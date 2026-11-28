//! The live control channel to each real Computer.
//!
//! A Computer connects out over one WebSocket. The Space keeps the sending
//! half in a [`ComputerLink`] and routes every action and every file request
//! through it: the Space assigns an id, the Computer's answer names that id,
//! and the waiting request resolves with it.
//!
//! One channel serves one Computer and carries the work of every Bot on it, so
//! each frame names its Bot. A channel that goes away fails every pending
//! request with [`OFFLINE`] and marks the Computer offline; when a Computer
//! reconnects before the old connection noticed, the new link replaces it and
//! the old one fails its own requests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha_computer::channel::{DeviceFrame, HostFrame};
use neha_computer::{Action, ActionResult, ActionStatus};
use tokio::sync::{mpsc, oneshot, watch};

/// Why a request failed because its Computer left.
pub const OFFLINE: &str = "the Computer went offline";

/// How many frames may wait for one Computer's WebSocket writer.
pub const FRAME_QUEUE: usize = 64;

/// How long one frame may wait to enter the queue or leave the socket.
///
/// Ten seconds in a release build; the test build uses a short limit so the
/// blocked-writer paths are asserted without slowing the suite. A frame that
/// misses its deadline fails its request and closes the channel, so a Computer
/// that stops reading cannot grow the Space's memory or strand a run.
pub const FRAME_TIMEOUT: Duration = if cfg!(any(test, feature = "test-support")) {
    Duration::from_millis(250)
} else {
    Duration::from_secs(10)
};

/// What a Computer's greeting said its machine can serve.
///
/// The Space records this from the channel handshake and keeps it for the
/// Computer: an offline machine must not offer a control its runtime refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessCapabilities {
    /// Whether the owner may grant folders and name a run's directory.
    pub grants: bool,
    /// Whether the owner may turn a Bot's full access on.
    pub full_access: bool,
    /// Whether the machine sandboxes runs; a client shows a view of unknown
    /// access until the machine greets, so this is only read when known.
    pub sandboxed: bool,
}

impl AccessCapabilities {
    /// The capabilities a Computer that declared nothing had.
    pub const SUPPORTED: Self = Self {
        grants: true,
        full_access: true,
        sandboxed: true,
    };

    /// What a Local Computer can serve until its handshake says otherwise.
    ///
    /// A Space restart forgets what a machine declared; until the daemon
    /// reconnects the machine is treated as serving none of it, so no recorded
    /// command widens access the Space cannot ask about.
    pub const UNSUPPORTED: Self = Self {
        grants: false,
        full_access: false,
        // Unknown access is shown as `None`, not as this placeholder.
        sandboxed: true,
    };
}

/// What a request waits for.
#[derive(Debug)]
pub enum Answer {
    /// An action finished.
    ActionResult(ActionResult),
    /// A File's bytes arrived.
    Bytes(Vec<u8>),
    /// The Computer refused the request.
    Failed(String),
    /// The channel went away before it answered.
    Gone(String),
}

/// Resolves one waiting request.
type Pending = oneshot::Sender<Answer>;

/// One Computer's control channel.
pub struct ComputerLink {
    /// The Computer this channel serves.
    pub computer: Id,
    /// The writer half of the Computer's WebSocket.
    frames: mpsc::Sender<HostFrame>,
    /// The requests whose answers are still owed, by Space-assigned id.
    pending: Mutex<HashMap<u64, Pending>>,
    /// Set once the channel is closing; no new request may then enter.
    closed: AtomicBool,
    /// Tells the connection handler that the channel is going away.
    closer: watch::Sender<bool>,
}

impl ComputerLink {
    /// Builds a link over a frame writer.
    fn new(computer: Id, frames: mpsc::Sender<HostFrame>) -> Self {
        let (closer, _) = watch::channel(false);

        Self {
            computer,
            frames,
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closer,
        }
    }

    /// Returns a receiver that turns `true` once the channel is closing.
    ///
    /// The connection handler watches it: a link that a reconnect replaced, or
    /// one the Space disconnected, must stop reading and close its socket
    /// instead of lingering as a second connection for the same Computer.
    #[must_use]
    pub fn closed_signal(&self) -> watch::Receiver<bool> {
        self.closer.subscribe()
    }

    /// Runs one action on the Computer and waits for its result.
    pub async fn act(
        &self,
        id: u64,
        bot: Id,
        action: Action,
        working_dir: Option<&str>,
        access: &neha_computer::channel::AccessFrame,
        env: &[(String, String)],
    ) -> Answer {
        self.request(
            id,
            HostFrame::Action {
                id,
                bot: bot.to_string(),
                action: action.into(),
                working_dir: working_dir.map(ToOwned::to_owned),
                access: access.clone(),
                env: env.to_vec(),
            },
        )
        .await
    }

    /// Reads a File's bytes from the Computer and waits for them.
    ///
    /// The access the run pinned travels with the request: a read that
    /// verifies an action from an interrupted run must reach what that action
    /// could.
    pub async fn read(
        &self,
        id: u64,
        bot: Id,
        path: &str,
        working_dir: Option<&str>,
        access: &neha_computer::channel::AccessFrame,
    ) -> Answer {
        self.request(
            id,
            HostFrame::Read {
                id,
                bot: bot.to_string(),
                path: path.to_owned(),
                working_dir: working_dir.map(ToOwned::to_owned),
                access: access.clone(),
            },
        )
        .await
    }

    /// Writes a File's bytes on the Computer and waits for its answer.
    ///
    /// The run's access travels with the request, as for a read; the Computer
    /// keeps a File delivery inside the Bot's folder whatever it says.
    pub async fn write(
        &self,
        id: u64,
        bot: Id,
        path: &str,
        bytes: Vec<u8>,
        access: &neha_computer::channel::AccessFrame,
    ) -> Answer {
        self.request(
            id,
            HostFrame::Write {
                id,
                bot: bot.to_string(),
                path: path.to_owned(),
                bytes: URL_SAFE_NO_PAD.encode(bytes),
                access: access.clone(),
            },
        )
        .await
    }

    /// Sends one request and waits for the answer that names its id.
    async fn request(&self, id: u64, frame: HostFrame) -> Answer {
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("computer link lock");
            if self.is_closed() {
                return Answer::Gone(OFFLINE.to_owned());
            }
            if pending.insert(id, sender).is_some() {
                return Answer::Failed(format!("request {id} is already pending"));
            }
        }
        // `close` may run between the insert and this check: it drains the map,
        // so a request that lost the race is answered here or by that drain.
        if self.is_closed() {
            self.cancel(id);
            return Answer::Gone(OFFLINE.to_owned());
        }
        // The writer's queue is bounded: a Computer that stops reading must
        // not grow the Space's memory. A frame that cannot enter the queue in
        // time fails this request and closes the channel, so its run ends
        // instead of waiting forever.
        let queued = tokio::time::timeout(FRAME_TIMEOUT, self.frames.send(frame)).await;
        if !matches!(queued, Ok(Ok(()))) {
            self.close();
            return Answer::Gone(OFFLINE.to_owned());
        }
        match receiver.await {
            Ok(answer) => answer,
            Err(_) => Answer::Gone(OFFLINE.to_owned()),
        }
    }

    /// Forgets a request whose caller stopped waiting.
    pub fn cancel(&self, id: u64) {
        let _ = self.pending.lock().expect("computer link lock").remove(&id);
    }

    /// Resolves the request an answer names; an unknown id is ignored.
    pub fn deliver(&self, id: u64, answer: Answer) {
        let sender = self.pending.lock().expect("computer link lock").remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(answer);
        }
    }

    /// Fails every pending request and refuses new ones.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // `send_replace` also works when no handler subscribed yet: it stores
        // the value, so a later subscriber still sees the closed channel.
        let _ = self.closer.send_replace(true);
        let pending: Vec<Pending> = self
            .pending
            .lock()
            .expect("computer link lock")
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for sender in pending {
            let _ = sender.send(Answer::Gone(OFFLINE.to_owned()));
        }
    }

    /// Returns whether the channel is closing.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Returns the request id and answer a Computer frame carries.
///
/// `Pong` and `Hello` answer no request, so they return `None`; the caller
/// uses `Pong` for the heartbeat and refuses a repeated `Hello`.
#[must_use]
pub fn answer(frame: DeviceFrame) -> Option<(u64, Answer)> {
    match frame {
        DeviceFrame::Result { id, status, output } => {
            let error = match status {
                ActionStatus::Succeeded => None,
                // The wire result carries no separate failure text; the output
                // is what the Computer says about the failure.
                _ if output.text.trim().is_empty() => {
                    Some("the action failed on the Computer".to_owned())
                }
                _ => Some(output.text.clone()),
            };
            Some((
                id,
                Answer::ActionResult(ActionResult {
                    status,
                    output,
                    error,
                }),
            ))
        }
        DeviceFrame::Bytes { id, bytes } => Some((
            id,
            match URL_SAFE_NO_PAD.decode(bytes) {
                Ok(bytes) => Answer::Bytes(bytes),
                Err(_) => Answer::Failed("the Computer sent bytes that are not base64".to_owned()),
            },
        )),
        DeviceFrame::Failed { id, message } => Some((id, Answer::Failed(message))),
        DeviceFrame::Hello { .. } | DeviceFrame::Pong => None,
    }
}

/// Every Computer channel a Space has, by Computer.
pub struct ComputerLinks {
    links: Mutex<HashMap<Id, Arc<ComputerLink>>>,
    /// Whether Hosted Computers run in worker containers, so a Hosted
    /// Computer's channel is as necessary as a Local Computer's.
    hosted_workers: bool,
    /// The access capabilities each Computer last declared. Kept after the
    /// channel goes away: a machine's runtime does not change with its
    /// connection, and a Bot update while it is offline still must not widen
    /// access the machine cannot serve.
    access: Mutex<HashMap<Id, AccessCapabilities>>,
    /// The Space-wide request counter; every request on every channel uses it.
    next_id: Arc<AtomicU64>,
}

impl Default for ComputerLinks {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputerLinks {
    /// Builds a Space with no connected Computer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            links: Mutex::new(HashMap::new()),
            hosted_workers: false,
            access: Mutex::new(HashMap::new()),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Builds a Space whose Hosted Computers are worker-backed.
    #[must_use]
    pub fn with_hosted_workers() -> Self {
        Self {
            hosted_workers: true,
            ..Self::new()
        }
    }

    /// Returns whether Hosted Computers need a connected worker channel.
    #[must_use]
    pub fn hosted_workers(&self) -> bool {
        self.hosted_workers
    }

    /// Returns whether the Computer's channel is up.
    #[must_use]
    pub fn online(&self, computer: Id) -> bool {
        self.links
            .lock()
            .expect("computer links lock")
            .contains_key(&computer)
    }

    /// Returns the Computer's channel, when it is up.
    #[must_use]
    pub fn link(&self, computer: Id) -> Option<Arc<ComputerLink>> {
        self.links
            .lock()
            .expect("computer links lock")
            .get(&computer)
            .cloned()
    }

    /// Returns the access capabilities the Computer last declared.
    #[must_use]
    pub fn access_capabilities(&self, computer: Id) -> Option<AccessCapabilities> {
        self.access
            .lock()
            .expect("computer access lock")
            .get(&computer)
            .copied()
    }

    /// Registers a Computer's new channel, replacing one that still lingers.
    pub fn connect(
        &self,
        computer: Id,
        frames: mpsc::Sender<HostFrame>,
        access: AccessCapabilities,
    ) -> Arc<ComputerLink> {
        self.access
            .lock()
            .expect("computer access lock")
            .insert(computer, access);
        let link = Arc::new(ComputerLink::new(computer, frames));
        let replaced = self
            .links
            .lock()
            .expect("computer links lock")
            .insert(computer, Arc::clone(&link));
        if let Some(replaced) = replaced {
            replaced.close();
        }
        link
    }

    /// Removes a channel that stopped and fails its pending requests.
    ///
    /// Returns whether this link was the Computer's current one; a link a
    /// reconnect already replaced leaves the new one in place.
    pub fn disconnect(&self, link: &Arc<ComputerLink>) -> bool {
        let current = {
            let mut links = self.links.lock().expect("computer links lock");
            let current = links
                .get(&link.computer)
                .is_some_and(|current| Arc::ptr_eq(current, link));
            if current {
                links.remove(&link.computer);
            }
            current
        };
        link.close();

        current
    }

    /// Returns the id for the next request on any channel.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Returns the Space-wide request counter a [`RemoteRuntime`] assigns from.
    ///
    /// [`RemoteRuntime`]: crate::remote::RemoteRuntime
    #[must_use]
    pub fn ids(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.next_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use neha::id::Id;
    use neha_computer::channel::{DeviceFrame, HostFrame};
    use neha_computer::{Action, ActionStatus, ToolOutput};
    use tokio::sync::mpsc;

    use super::{Answer, ComputerLink, ComputerLinks, FRAME_QUEUE, OFFLINE, answer};

    fn connected() -> (
        ComputerLinks,
        Id,
        Arc<ComputerLink>,
        mpsc::Receiver<HostFrame>,
    ) {
        let links = ComputerLinks::new();
        let computer = Id::new();
        let (frames, outgoing) = mpsc::channel(FRAME_QUEUE);
        let link = links.connect(computer, frames, super::AccessCapabilities::SUPPORTED);

        (links, computer, link, outgoing)
    }

    #[test]
    fn an_answer_names_its_request_and_converts_the_wire_shape() {
        let (id, result) = answer(DeviceFrame::Result {
            id: 7,
            status: ActionStatus::Succeeded,
            output: ToolOutput::full("done"),
        })
        .unwrap();
        assert_eq!(id, 7);
        match result {
            Answer::ActionResult(result) => {
                assert_eq!(result.status, ActionStatus::Succeeded);
                assert_eq!(result.output.text, "done");
                assert!(result.error.is_none());
            }
            other => panic!("expected an action result, got {other:?}"),
        }

        let (_, failed) = answer(DeviceFrame::Result {
            id: 8,
            status: ActionStatus::Failed,
            output: ToolOutput::full("the Computer refused"),
        })
        .unwrap();
        assert!(
            matches!(failed, Answer::ActionResult(result) if result.error.as_deref() == Some("the Computer refused"))
        );

        let bytes = URL_SAFE_NO_PAD.encode([0_u8, 1, 2, 255]);
        let (_, bytes_answer) = answer(DeviceFrame::Bytes { id: 9, bytes }).unwrap();
        assert!(matches!(bytes_answer, Answer::Bytes(value) if value == [0, 1, 2, 255]));

        let (_, failure) = answer(DeviceFrame::Failed {
            id: 10,
            message: "the Computer refused".to_owned(),
        })
        .unwrap();
        assert!(matches!(failure, Answer::Failed(message) if message == "the Computer refused"));

        assert!(answer(DeviceFrame::Pong).is_none());
    }

    #[tokio::test]
    async fn an_action_goes_out_and_its_answer_resolves_it() {
        let (links, computer, link, mut outgoing) = connected();
        let bot = Id::new();
        assert!(links.online(computer));

        let sent = tokio::spawn({
            let link = Arc::clone(&link);
            async move {
                link.act(
                    1,
                    bot,
                    Action::Shell {
                        command: "pwd".to_owned(),
                    },
                    None,
                    &neha_computer::channel::AccessFrame::default(),
                    &[],
                )
                .await
            }
        });
        let frame = outgoing.recv().await.unwrap();
        assert_eq!(
            frame,
            HostFrame::Action {
                id: 1,
                bot: bot.to_string(),
                action: neha_computer::channel::WireAction::Shell {
                    command: "pwd".to_owned(),
                },
                working_dir: None,
                access: Default::default(),
                env: Vec::new(),
            }
        );

        link.deliver(
            1,
            Answer::ActionResult(neha_computer::ActionResult::succeeded(ToolOutput::full(
                "/bot",
            ))),
        );
        assert!(matches!(
            sent.await.unwrap(),
            Answer::ActionResult(result) if result.output.text == "/bot"
        ));
    }

    #[tokio::test]
    async fn reads_and_writes_name_their_bot_and_encode_bytes() {
        let (_links, _computer, link, mut outgoing) = connected();
        let bot = Id::new();
        let access = neha_computer::channel::AccessFrame {
            full_access: true,
            write_roots: vec!["/data/shared".to_owned()],
        };
        let reader = tokio::spawn({
            let link = Arc::clone(&link);
            let access = access.clone();
            async move { link.read(2, bot, "notes.txt", None, &access).await }
        });
        assert_eq!(
            outgoing.recv().await.unwrap(),
            HostFrame::Read {
                id: 2,
                bot: bot.to_string(),
                path: "notes.txt".to_owned(),
                working_dir: None,
                access: access.clone(),
            }
        );
        link.deliver(2, Answer::Bytes(b"hello".to_vec()));
        assert!(matches!(reader.await.unwrap(), Answer::Bytes(bytes) if bytes == b"hello"));

        let writer = tokio::spawn({
            let link = Arc::clone(&link);
            let access = access.clone();
            async move {
                link.write(3, bot, "out.bin", b"hello".to_vec(), &access)
                    .await
            }
        });
        assert_eq!(
            outgoing.recv().await.unwrap(),
            HostFrame::Write {
                id: 3,
                bot: bot.to_string(),
                path: "out.bin".to_owned(),
                bytes: URL_SAFE_NO_PAD.encode(b"hello"),
                access,
            }
        );
        link.deliver(3, Answer::Bytes(Vec::new()));
        assert!(matches!(writer.await.unwrap(), Answer::Bytes(bytes) if bytes.is_empty()));
    }

    #[tokio::test]
    async fn a_disconnect_marks_the_computer_offline_and_fails_pending_requests() {
        let (links, computer, link, _outgoing) = connected();
        let waiting = tokio::spawn({
            let link = Arc::clone(&link);
            async move {
                link.read(4, Id::new(), "/etc/hostname", None, &Default::default())
                    .await
            }
        });
        tokio::task::yield_now().await;

        assert!(links.disconnect(&link));
        assert!(!links.online(computer));
        assert!(matches!(waiting.await.unwrap(), Answer::Gone(message) if message == OFFLINE));
        assert!(
            matches!(
                link
                    .act(
                        5,
                        Id::new(),
                        Action::Shell { command: "true".to_owned() },
                        None,
                        &neha_computer::channel::AccessFrame::default(),
                        &[],
                    )
                    .await,
                Answer::Gone(message) if message == OFFLINE
            ),
            "a closed channel refuses new requests"
        );
    }

    #[tokio::test]
    async fn a_reconnect_replaces_the_old_channel_and_fails_its_work() {
        let (links, computer, old, _old_outgoing) = connected();
        let signal = old.closed_signal();
        let waiting = tokio::spawn({
            let old = Arc::clone(&old);
            async move {
                old.read(4, Id::new(), "/etc/hostname", None, &Default::default())
                    .await
            }
        });
        tokio::task::yield_now().await;

        let (frames, _new_outgoing) = mpsc::channel(FRAME_QUEUE);
        let new = links.connect(
            computer,
            frames,
            super::AccessCapabilities {
                grants: false,
                full_access: false,
                sandboxed: true,
            },
        );

        assert!(old.is_closed());
        assert!(*signal.borrow(), "the old handler is told to stop reading");
        assert!(matches!(
            waiting.await.unwrap(),
            Answer::Gone(message) if message == OFFLINE
        ));
        assert!(
            !links.disconnect(&old),
            "the old channel no longer speaks for the Computer"
        );
        assert!(links.online(computer));
        assert!(matches!(
            links.link(computer),
            Some(link) if Arc::ptr_eq(&link, &new)
        ));
        assert_eq!(
            links.access_capabilities(computer),
            Some(super::AccessCapabilities {
                grants: false,
                full_access: false,
                sandboxed: true,
            }),
            "the reconnect replaced what the Computer declared"
        );
    }

    #[test]
    fn a_disconnected_computer_keeps_the_access_it_declared() {
        let (links, computer, link, _outgoing) = connected();
        assert!(links.disconnect(&link));
        assert!(!links.online(computer));
        assert_eq!(
            links.access_capabilities(computer),
            Some(super::AccessCapabilities::SUPPORTED),
            "an offline Computer still answers for its runtime"
        );
    }

    #[tokio::test]
    async fn a_full_send_queue_fails_requests_and_closes_the_link() {
        let (_links, _computer, link, mut outgoing) = connected();
        let bot = Id::new();

        // Nobody reads the queue in this test, so the writer's channel fills at
        // its capacity and the request past it cannot enter before its
        // deadline. Closing the link then fails every request still waiting.
        let requests: Vec<_> = (1..=FRAME_QUEUE as u64 + 1)
            .map(|id| {
                let link = Arc::clone(&link);
                async move {
                    link.act(
                        id,
                        bot,
                        Action::Shell {
                            command: format!("sleep {id}"),
                        },
                        None,
                        &neha_computer::channel::AccessFrame::default(),
                        &[],
                    )
                    .await
                }
            })
            .collect();
        let answers = futures_util::future::join_all(requests).await;

        assert!(
            link.is_closed(),
            "a frame past the deadline closes the link"
        );
        assert!(
            answers
                .iter()
                .all(|answer| matches!(answer, Answer::Gone(_))),
            "every request that was waiting fails when the link closes"
        );
        let mut queued = 0;
        while outgoing.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(
            queued, FRAME_QUEUE,
            "the send queue never grows past its capacity"
        );
    }
}
