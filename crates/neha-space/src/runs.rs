//! The queue of runs, one pump per Bot.
//!
//! The record is the queue: a run enters it as `neha.run.queued`, and a Bot's
//! pump starts the waiting run with the smallest cursor. A pump wakes when a
//! run is admitted, when the Space starts, and when a Computer connects or
//! leaves, then drains everything its Bot has waiting before it sleeps again.
//! Losing a wake-up or replacing a pump therefore never strands a run: the next
//! wake, and the next start of the Space, read the backlog from the record.
//!
//! A real Computer that is offline holds its Bot's queue: the pump stops in
//! front of the waiting run instead of skipping it, so the record's order is
//! the order the Computer runs the work in.
//!
//! One pump works the runs of one Bot at a time. A run it cannot prepare, or
//! one it cannot finish, is recorded by the pump itself; a store failure ends
//! the pump, and the next start of the Space recovers what it left.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use neha::id::Id;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::commands;
use crate::server::SpaceServer;
use crate::state::{RunStatus, RunView};

/// The pumps that work the runs of each Bot.
pub struct RunQueue {
    bots: Mutex<HashMap<Id, Pump>>,
    /// Tells every pump that the Space stops.
    stop: watch::Sender<bool>,
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

struct Pump {
    /// Serializes taking a place in the queue with recording it, so the place
    /// a command confirms is the place the record holds.
    admit: Arc<tokio::sync::Mutex<()>>,
    /// Wakes the pump when the record may hold a run for its Bot.
    wake: Arc<Notify>,
    worker: Option<JoinHandle<()>>,
}

impl Pump {
    fn new() -> Self {
        Self {
            admit: Arc::new(tokio::sync::Mutex::new(())),
            wake: Arc::new(Notify::new()),
            worker: None,
        }
    }
}

impl RunQueue {
    /// Builds an empty queue.
    #[must_use]
    pub fn new() -> Self {
        let (stop, _) = watch::channel(false);

        Self {
            bots: Mutex::new(HashMap::new()),
            stop,
        }
    }

    /// Waits until this Bot may take the next place in its queue.
    ///
    /// The caller holds the guard while it computes the place, records the
    /// run, and wakes the pump, so two commands never read the same place.
    pub async fn admit(&self, bot: Id) -> tokio::sync::OwnedMutexGuard<()> {
        let admit = {
            let mut bots = self.bots.lock().expect("run queue lock");
            Arc::clone(&bots.entry(bot).or_insert_with(Pump::new).admit)
        };

        admit.lock_owned().await
    }

    /// Wakes the Bot's pump, starting one when it is not running.
    ///
    /// The record already holds the run, so a lost wake-up, or a pump that
    /// ended after a store failure, is healed by the next wake or start.
    pub fn enqueue(&self, server: &Arc<SpaceServer>, bot: Id) {
        let mut bots = self.bots.lock().expect("run queue lock");
        // A shutdown that began before this call must not see a new pump.
        if server.is_closed() {
            return;
        }
        let pump = bots.entry(bot).or_insert_with(Pump::new);
        if pump.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            pump.worker = None;
        }
        if pump.worker.is_none() {
            let wake = Arc::clone(&pump.wake);
            let stop = self.stop.subscribe();
            pump.worker = Some(tokio::spawn(work(Arc::clone(server), bot, wake, stop)));
        }
        pump.wake.notify_one();
    }

    /// Wakes every Bot that has a run waiting in the record.
    ///
    /// A start after a stop and a Computer connecting or leaving need it: the
    /// pumps then work the record in its own order, so no wake-up has to be
    /// remembered.
    pub fn wake_all(&self, server: &Arc<SpaceServer>) {
        let bots: Vec<Id> = {
            let Ok(state) = server.state.lock() else {
                return;
            };
            let mut bots: Vec<Id> = state
                .runs
                .values()
                .filter(|run| {
                    run.status == RunStatus::Queued
                        || state.is_recovering(run)
                        || state
                            .resumable_run(run.bot)
                            .is_some_and(|view| view.id == run.id)
                        || state
                            .denied_run(run.bot)
                            .is_some_and(|view| view.id == run.id)
                })
                .map(|run| run.bot)
                .collect();
            bots.sort_unstable();
            bots.dedup();
            bots
        };
        for bot in bots {
            self.enqueue(server, bot);
        }
    }

    /// Stops every pump without waiting for the run it works to finish.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        let workers: Vec<JoinHandle<()>> = {
            let mut bots = self.bots.lock().expect("run queue lock");
            bots.drain().filter_map(|(_, pump)| pump.worker).collect()
        };
        for worker in workers {
            worker.abort();
            let _ = worker.await;
        }
    }
}

/// A test seam: when a test pauses a Bot, its pump waits between snapshotting
/// the run's Computer and recording `RunStarted`, so a move can be
/// interleaved deterministically.
#[cfg(test)]
type StartPause = (Arc<Notify>, Arc<Notify>);

#[cfg(test)]
static START_PAUSES: Mutex<Option<HashMap<Id, StartPause>>> = Mutex::new(None);

/// Installs a pause for one Bot's next start; returns (reached, release).
#[cfg(test)]
pub(crate) fn pause_start_for_test(bot: Id) -> StartPause {
    let pause = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut pauses = START_PAUSES.lock().expect("start pause lock");
    pauses
        .get_or_insert_with(HashMap::new)
        .insert(bot, pause.clone());

    pause
}

#[cfg(test)]
async fn pause_before_start(bot: Id) {
    let pause = START_PAUSES
        .lock()
        .ok()
        .and_then(|mut pauses| pauses.as_mut().and_then(|pauses| pauses.remove(&bot)));
    if let Some((reached, release)) = pause {
        reached.notify_one();
        release.notified().await;
    }
}

/// Works the runs of one Bot, one at a time.
async fn work(
    server: Arc<SpaceServer>,
    bot: Id,
    wake: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = wake.notified() => {}
        }
        // A wake stands for the whole backlog: keep working while the record
        // holds work of this Bot. A waiting run does not hold the pump: it is
        // skipped until the acting User answers.
        loop {
            // The Bot's admit lock covers choosing the work, snapshotting the
            // Computer it starts on, and recording `RunStarted`. A `bot_moved`
            // takes the same lock, so the record's order decides which Computer
            // a run uses: a run started before the move keeps the old one, and
            // one started after it uses the new one. The lock is released before
            // any wait and before the run itself, so commands and the run never
            // block each other.
            let admit = server.runs.admit(bot).await;
            let work = {
                let state = server.state.lock().ok();
                state.and_then(|state| {
                    // An allowed run that can run now resumes first. One whose
                    // Computer is away is skipped instead of holding the Bot:
                    // a later move, a new run, or another run's denial must
                    // still make progress. It resumes when its Computer
                    // reconnects, because the run keeps the Computer it began
                    // with.

                    if let Some(run) = next_resumable(&state, &server.links, bot) {
                        return Some(Work::Resume(run));
                    }
                    if let Some(run) = state.denied_run(bot) {
                        return Some(Work::Deny(run.clone()));
                    }
                    next_queued(&state, bot, &server.links).map(Work::Queued)
                })
            };
            let Some(work) = work else {
                drop(admit);
                break;
            };
            match work {
                Work::Deny(run) => {
                    drop(admit);
                    if let Err(error) = commands::fail_denied_run(
                        &server.store,
                        &server.state,
                        &server.events,
                        &run,
                    )
                    .await
                    {
                        eprintln!("neha: cannot record a denied run {}: {error}", run.id);
                        return;
                    }
                }
                Work::Resume(run) => {
                    drop(admit);
                    match commands::resume_run(
                        &server.store,
                        &server.state,
                        &server.home,
                        &server.model,
                        &server.secrets,
                        &server.events,
                        server.tool_timeout,
                        &server.links,
                        &run,
                    )
                    .await
                    {
                        Ok(commands::Finished::Completed(events)) => {
                            for event in events {
                                let _ = server.events.send(event);
                            }
                        }
                        // The run may wait again on a later action.
                        Ok(commands::Finished::Waiting) => {}
                        Err(error) => {
                            eprintln!("neha: a resumed run could not finish: {error}");
                            return;
                        }
                    }
                }
                Work::Queued(run) => {
                    let pending = match commands::resume_pending(
                        &server.state,
                        &server.secrets,
                        &run,
                    )
                    .await
                    {
                        Ok(pending) => pending,
                        Err(error) => {
                            drop(admit);
                            let reason = error.to_string();
                            if let Err(failure) = commands::fail_queued_run(
                                &server.store,
                                &server.state,
                                &server.events,
                                &run,
                                &reason,
                            )
                            .await
                            {
                                eprintln!("neha: cannot fail queued run {}: {failure}", run.id);
                                return;
                            }
                            continue;
                        }
                    };
                    #[cfg(test)]
                    pause_before_start(bot).await;
                    if commands::start_run(&server.store, &server.state, &server.events, &pending)
                        .await
                        .is_err()
                    {
                        drop(admit);
                        return;
                    }
                    drop(admit);
                    match commands::finish_run(
                        &server.store,
                        &server.state,
                        &server.home,
                        &server.model,
                        &server.events,
                        server.tool_timeout,
                        &server.links,
                        pending,
                    )
                    .await
                    {
                        Ok(commands::Finished::Completed(events)) => {
                            for event in events {
                                let _ = server.events.send(event);
                            }
                        }
                        Ok(commands::Finished::Waiting) => {}
                        Err(error) => {
                            // The run stays in the record; the next start reports it.
                            eprintln!("neha: a run could not finish: {error}");
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// The work one turn of a pump does.
enum Work {
    /// Continue a run whose approval was allowed.
    Resume(RunView),
    /// Fail a run whose approval was denied.
    Deny(RunView),
    /// Start the earliest queued run.
    Queued(RunView),
}

/// Returns the earliest allowed run of a Bot whose Computer is online.
fn next_resumable(
    state: &crate::state::State,
    links: &crate::computer_link::ComputerLinks,
    bot: Id,
) -> Option<RunView> {
    state
        .runs
        .values()
        .filter(|run| run.bot == bot && (state.is_resumable(run) || state.is_recovering(run)))
        .filter(|run| computer_online(state, links, run))
        .min_by_key(|run| run.seq)
        .cloned()
}

/// Returns whether the Computer a run works on can take the run now.
fn computer_online(
    state: &crate::state::State,
    links: &crate::computer_link::ComputerLinks,
    run: &RunView,
) -> bool {
    // A recorded final answer needs no machine: it finishes from the record.
    if state.is_final_recovered(run.id) {
        return true;
    }
    let Some(computer) = run.computer else {
        return true;
    };
    let local = state
        .computers
        .get(&computer)
        .is_some_and(|view| view.kind == "local");

    !local || links.online(computer)
}

/// Returns the earliest queued run of a Bot whose Computer can start it.
///
/// A real Computer that is offline holds the queue: the run keeps its place in
/// the record and its pump waits for the channel, so a later run never
/// overtakes it. A waiting run is not queued and never holds the pump.
fn next_queued(
    state: &crate::state::State,
    bot: Id,
    links: &crate::computer_link::ComputerLinks,
) -> Option<RunView> {
    let run = state
        .runs
        .values()
        .filter(|run| run.bot == bot && run.status == RunStatus::Queued)
        .min_by_key(|run| run.seq)?
        .clone();
    let computer = state.bots.get(&bot).map(|bot| bot.computer)?;
    let local = state
        .computers
        .get(&computer)
        .is_some_and(|view| view.kind == "local");
    if local && !links.online(computer) {
        return None;
    }

    Some(run)
}
