//! The queue of runs, one pump per Bot.
//!
//! The record is the queue: a run enters it as `neha.run.queued`, and a Bot's
//! pump starts the waiting run with the smallest cursor. A pump wakes when a
//! run is admitted or when the Space starts, then drains everything its Bot
//! has waiting before it sleeps again. Losing a wake-up or replacing a pump
//! therefore never strands a run: the next wake, and the next start of the
//! Space, read the backlog from the record.
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
    /// This is what a start after a stop needs: the pumps then work the record
    /// in its own order, so that recovery needs no stored work of its own.
    pub fn resume(&self, server: &Arc<SpaceServer>) {
        let bots: Vec<Id> = {
            let Ok(state) = server.state.lock() else {
                return;
            };
            let mut bots: Vec<Id> = state
                .runs
                .values()
                .filter(|run| run.status == RunStatus::Queued)
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
        // still holds a run of this Bot that waits.
        while let Some(run) = next_queued(&server, bot) {
            let pending = match commands::resume_pending(&server.state, &server.secrets, &run).await
            {
                Ok(pending) => pending,
                Err(error) => {
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
            if commands::start_run(&server.store, &server.state, &server.events, &pending)
                .await
                .is_err()
            {
                return;
            }
            match commands::finish_run(
                &server.store,
                &server.state,
                &server.home,
                &server.blobs,
                &server.model,
                &server.events,
                server.tool_timeout,
                pending,
            )
            .await
            {
                Ok((_result, events)) => {
                    for event in events {
                        let _ = server.events.send(event);
                    }
                }
                Err(error) => {
                    // The run stays in the record; the next start reports it.
                    eprintln!("neha: a run could not finish: {error}");
                    return;
                }
            }
        }
    }
}

/// Returns the waiting run of a Bot that the record holds first.
fn next_queued(server: &SpaceServer, bot: Id) -> Option<RunView> {
    let state = server.state.lock().ok()?;

    state
        .runs
        .values()
        .filter(|run| run.bot == bot && run.status == RunStatus::Queued)
        .min_by_key(|run| run.seq)
        .cloned()
}
