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
use tracing::Instrument as _;

use crate::commands;
use crate::diagnostics::scrub_error;
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
        #[cfg(feature = "test-support")]
        let waiting = AdmitWaiter::new(bot);
        let admit = admit.lock_owned().await;
        #[cfg(feature = "test-support")]
        drop(waiting);

        admit
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

/// A test seam: counts the callers queued for one Bot's admit lock.
///
/// A command that takes the lock can be interleaved with a paused run start
/// deterministically: a test waits until the command is queued on the lock
/// instead of sleeping and hoping.
#[cfg(feature = "test-support")]
static ADMIT_WAITERS: Mutex<Option<HashMap<Id, usize>>> = Mutex::new(None);

#[cfg(feature = "test-support")]
struct AdmitWaiter(Id);

#[cfg(feature = "test-support")]
impl AdmitWaiter {
    fn new(bot: Id) -> Self {
        if let Ok(mut waiters) = ADMIT_WAITERS.lock() {
            *waiters
                .get_or_insert_with(HashMap::new)
                .entry(bot)
                .or_insert(0) += 1;
        }
        Self(bot)
    }
}

#[cfg(feature = "test-support")]
impl Drop for AdmitWaiter {
    fn drop(&mut self) {
        if let Ok(mut waiters) = ADMIT_WAITERS.lock()
            && let Some(counts) = waiters.as_mut()
            && let Some(count) = counts.get_mut(&self.0)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.0);
            }
        }
    }
}

/// Returns how many callers wait for one Bot's admit lock.
#[cfg(feature = "test-support")]
pub(crate) fn admit_waiters(bot: Id) -> usize {
    ADMIT_WAITERS
        .lock()
        .ok()
        .and_then(|waiters| {
            waiters
                .as_ref()
                .and_then(|counts| counts.get(&bot).copied())
        })
        .unwrap_or(0)
}

/// A test seam: when a test pauses a Bot, its pump waits between snapshotting
/// the run's Computer and recording `RunStarted`, so a move can be
/// interleaved deterministically.
#[cfg(feature = "test-support")]
type StartPause = (Arc<Notify>, Arc<Notify>);

#[cfg(feature = "test-support")]
static START_PAUSES: Mutex<Option<HashMap<Id, StartPause>>> = Mutex::new(None);

/// Installs a pause for one Bot's next start; returns (reached, release).
#[cfg(feature = "test-support")]
pub(crate) fn pause_start_for_test(bot: Id) -> StartPause {
    let pause = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut pauses = START_PAUSES.lock().expect("start pause lock");
    pauses
        .get_or_insert_with(HashMap::new)
        .insert(bot, pause.clone());

    pause
}

#[cfg(feature = "test-support")]
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
                        crate::metrics::install().store_error("fail_denied_run");
                        tracing::error!(
                            target: "neha::store",
                            event = "store.fail",
                            operation = "fail_denied_run",
                            run_id = %run.id,
                            error_message = scrub_error(&error.to_string()).as_str(),
                            "cannot record a denied run"
                        );
                        return;
                    }
                }
                Work::Resume(run) => {
                    drop(admit);
                    let started_at = std::time::Instant::now();
                    let run_span = tracing::info_span!(
                        target: "neha::runs",
                        "run",
                        run_id = %run.id,
                        bot_id = %run.bot,
                        chat_id = %run.chat,
                        resumed = true,
                        otel.kind = "internal",
                    );
                    run_span.in_scope(|| {
                        tracing::info!(
                            target: "neha::runs",
                            event = "run.start",
                            run_id = %run.id,
                            bot_id = %run.bot,
                            chat_id = %run.chat,
                            resumed = true,
                            "a run resumed"
                        );
                    });
                    match commands::resume_run(
                        &server.store,
                        &server.state,
                        &server.home,
                        &server.blobs,
                        server.instructions_only,
                        &server.model,
                        &server.secrets,
                        &server.events,
                        server.tool_timeout,
                        &server.links,
                        &server.approvals,
                        &run,
                    )
                    .instrument(run_span.clone())
                    .await
                    {
                        Ok(commands::Finished::Completed(events)) => {
                            log_run_finish(&server, &run, started_at, &run_span);
                            for event in events {
                                let _ = server.events.send(event);
                            }
                        }
                        // The run may wait again on a later action.
                        Ok(commands::Finished::Waiting) => {
                            log_run_finish(&server, &run, started_at, &run_span);
                        }
                        Err(error) => {
                            run_span.in_scope(|| {
                                tracing::error!(
                                    target: "neha::runs",
                                    event = "run.fail",
                                    run_id = %run.id,
                                    resumed = true,
                                    error_kind = ?error.code(),
                                    duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                                    "a resumed run could not finish"
                                );
                            });
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
                                crate::metrics::install().store_error("fail_queued_run");
                                tracing::error!(
                                    target: "neha::store",
                                    event = "store.fail",
                                    operation = "fail_queued_run",
                                    run_id = %run.id,
                                    error_message = scrub_error(&failure.to_string()).as_str(),
                                    "cannot fail queued run"
                                );
                                return;
                            }
                            continue;
                        }
                    };
                    #[cfg(feature = "test-support")]
                    pause_before_start(bot).await;
                    // A revoke may land between selecting this run and
                    // recording its start. Take the same gate the revoke
                    // holds and re-check the channel under it: revocation wins
                    // leaves the run queued instead of starting and failing it
                    // after the owner already saw success.
                    let revoking = server.links.lock_revocations().await;
                    let still_online = server.state.lock().is_ok_and(|state| {
                        bot_computer_online(&state, &server.links, pending.bot.id)
                    });
                    if !still_online {
                        drop(revoking);
                        drop(admit);
                        continue;
                    }
                    if commands::start_run(&server.store, &server.state, &server.events, &pending)
                        .await
                        .is_err()
                    {
                        drop(revoking);
                        drop(admit);
                        return;
                    }
                    drop(revoking);
                    drop(admit);
                    let started_at = std::time::Instant::now();
                    let run_span = tracing::info_span!(
                        target: "neha::runs",
                        "run",
                        run_id = %pending.run,
                        bot_id = %pending.bot.id,
                        chat_id = %pending.chat.id,
                        resumed = false,
                        otel.kind = "internal",
                    );
                    run_span.in_scope(|| {
                        tracing::info!(
                            target: "neha::runs",
                            event = "run.start",
                            run_id = %pending.run,
                            bot_id = %pending.bot.id,
                            chat_id = %pending.chat.id,
                            resumed = false,
                            "a run started"
                        );
                    });
                    let outcome = commands::finish_run(
                        &server.store,
                        &server.state,
                        &server.home,
                        &server.blobs,
                        server.instructions_only,
                        &server.secrets,
                        &server.model,
                        &server.events,
                        server.tool_timeout,
                        &server.links,
                        &server.approvals,
                        pending,
                    )
                    .instrument(run_span.clone())
                    .await;
                    match outcome {
                        Ok(commands::Finished::Completed(events)) => {
                            log_run_finish(&server, &run, started_at, &run_span);
                            for event in events {
                                let _ = server.events.send(event);
                            }
                        }
                        Ok(commands::Finished::Waiting) => {
                            log_run_finish(&server, &run, started_at, &run_span);
                        }
                        Err(error) => {
                            // The run stays in the record; the next start reports it.
                            run_span.in_scope(|| {
                                tracing::error!(
                                    target: "neha::runs",
                                    event = "run.fail",
                                    run_id = %run.id,
                                    error_kind = ?error.code(),
                                    duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                                    "a run could not finish"
                                );
                            });
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Records the end of a run, with the usage the record now holds.
fn log_run_finish(
    server: &SpaceServer,
    run: &RunView,
    started: std::time::Instant,
    span: &tracing::Span,
) {
    let (outcome, usage) = server
        .state
        .lock()
        .ok()
        .and_then(|state| state.runs.get(&run.id).cloned())
        .map_or(("unknown", None), |record| {
            let outcome = match record.status {
                RunStatus::Succeeded => "succeeded",
                RunStatus::Failed => "failed",
                RunStatus::Waiting => "waiting",
                RunStatus::Running => "running",
                RunStatus::Queued => "queued",
            };
            (outcome, record.usage)
        });
    let elapsed = started.elapsed();
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    span.in_scope(|| match usage {
        Some(usage) => tracing::info!(
            target: "neha::runs",
            event = "run.finish",
            run_id = %run.id,
            bot_id = %run.bot,
            chat_id = %run.chat,
            outcome,
            duration_ms,
            input_tokens = usage.uncached_input,
            cache_read_tokens = usage.cache_read,
            output_tokens = usage.output,
            reasoning_tokens = usage.reasoning,
            "a run finished"
        ),
        None => tracing::info!(
            target: "neha::runs",
            event = "run.finish",
            run_id = %run.id,
            bot_id = %run.bot,
            chat_id = %run.chat,
            outcome,
            duration_ms,
            "a run finished"
        ),
    });
}

/// Watches the events a successful record append wrote and counts a run when
/// its `RunFinished` first lands. The start time comes from the observed
/// `RunStarted`, so a run that waited for an approval keeps the whole duration.
pub(crate) fn observe_recorded_events(events: &[neha::record::Event]) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static STARTS: OnceLock<Mutex<HashMap<Id, std::time::Instant>>> = OnceLock::new();
    let starts = STARTS.get_or_init(|| Mutex::new(HashMap::new()));

    for event in events {
        let Some(run) = event
            .data
            .get("run")
            .and_then(|run| serde_json::from_value::<Id>(run.clone()).ok())
        else {
            continue;
        };
        match event.kind {
            neha::record::EventKind::RunStarted => {
                starts
                    .lock()
                    .expect("run start map")
                    .insert(run, std::time::Instant::now());
            }
            neha::record::EventKind::RunFinished => {
                let status = event
                    .data
                    .get("status")
                    .and_then(|status| serde_json::from_value::<RunStatus>(status.clone()).ok());
                let Some(status) = status.and_then(terminal_status) else {
                    continue;
                };
                let started = starts.lock().expect("run start map").remove(&run);
                match started {
                    Some(started) => crate::metrics::install()
                        .run_finished(status, started.elapsed().as_secs_f64()),
                    None => crate::metrics::install().run_finished_without_duration(status),
                }
            }
            _ => {}
        }
    }
}

/// The metric label for a persisted terminal run status.
///
/// A queued, running, or waiting run is not finished: counting it here would
/// count a run twice after it resumes and skew the failure rate.
fn terminal_status(status: RunStatus) -> Option<&'static str> {
    match status {
        RunStatus::Succeeded => Some("succeeded"),
        RunStatus::Failed => Some("failed"),
        RunStatus::Queued | RunStatus::Running | RunStatus::Waiting => None,
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

/// Returns whether the Bot's current Computer can take a queued run now.
///
/// A queued run may not name a Computer yet: the Bot's assignment decides,
/// which is also the assignment `next_queued` read when it selected the run.
fn bot_computer_online(
    state: &crate::state::State,
    links: &crate::computer_link::ComputerLinks,
    bot: Id,
) -> bool {
    let Some(computer) = state.bots.get(&bot).map(|bot| bot.computer) else {
        return false;
    };
    if state.is_resetting(computer) {
        return false;
    }
    let remote = state.computers.get(&computer).is_some_and(|view| {
        view.kind == "local" || (view.kind == "hosted" && links.hosted_workers())
    });

    !remote || links.online(computer)
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
    // A reset clears the Workspace between two actions: no run may start on it
    // until the clear finished.
    if state.is_resetting(computer) {
        return false;
    }
    let remote = state.computers.get(&computer).is_some_and(|view| {
        view.kind == "local" || (view.kind == "hosted" && links.hosted_workers())
    });

    !remote || links.online(computer)
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
    if state.is_resetting(computer) {
        return None;
    }
    let remote = state.computers.get(&computer).is_some_and(|view| {
        view.kind == "local" || (view.kind == "hosted" && links.hosted_workers())
    });
    if remote && !links.online(computer) {
        return None;
    }

    Some(run)
}

#[cfg(test)]
mod tests {
    use super::{observe_recorded_events, terminal_status};
    use crate::state::RunStatus;

    fn event(
        kind: neha::record::EventKind,
        run: neha::id::Id,
        status: Option<RunStatus>,
    ) -> neha::record::Event {
        neha::record::Event {
            seq: neha::seq::Seq::ZERO,
            kind,
            version: 1,
            at: neha::time::Timestamp::now(),
            author: neha::record::Author::Bot(neha::id::Id::new()),
            acting: None,
            command: None,
            data: match status {
                Some(status) => serde_json::json!({"run": run, "status": status}),
                None => serde_json::json!({"run": run}),
            },
        }
    }

    fn finished_total() -> u64 {
        crate::metrics::install()
            .render()
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter(|line| line.starts_with("neha_runs_finished_total{"))
            .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
            .sum::<f64>() as u64
    }

    #[test]
    fn a_run_counts_once_at_its_terminal_event() {
        let run = neha::id::Id::new();
        observe_recorded_events(&[event(neha::record::EventKind::RunStarted, run, None)]);
        observe_recorded_events(&[event(
            neha::record::EventKind::RunFinished,
            run,
            Some(RunStatus::Waiting),
        )]);
        let waiting = finished_total();
        observe_recorded_events(&[event(
            neha::record::EventKind::RunFinished,
            run,
            Some(RunStatus::Succeeded),
        )]);
        assert_eq!(finished_total(), waiting + 1);
    }

    #[test]
    fn only_a_persisted_terminal_status_counts() {
        assert_eq!(terminal_status(RunStatus::Succeeded), Some("succeeded"));
        assert_eq!(terminal_status(RunStatus::Failed), Some("failed"));
        assert_eq!(terminal_status(RunStatus::Queued), None);
        assert_eq!(terminal_status(RunStatus::Running), None);
        assert_eq!(
            terminal_status(RunStatus::Waiting),
            None,
            "a run waiting for an approval is not finished"
        );
    }
}
