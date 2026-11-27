//! The runtime that drives one Bot on a real Computer.
//!
//! Every action and every File request crosses the Computer's channel, so the
//! runtime is the Space's end of that channel: it assigns a request id, waits
//! for the answer, and turns a channel that went away into a failure with the
//! same reason the Space shows a User.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use neha::id::Id;
use neha_computer::{Action, ActionResult, ActionStatus, ByteError, ComputerRuntime, ToolOutput};

use crate::computer_link::{Answer, ComputerLink};

/// How long past the tool timeout a real Computer may take to answer.
///
/// The Computer runs the same tool under the same limit; the grace covers the
/// frame crossing the channel so a slow machine is not reported as a timeout
/// before its own limit fires.
pub const TIMEOUT_GRACE: Duration = Duration::from_secs(5);

/// A runtime whose work runs on a real Computer.
pub struct RemoteRuntime {
    link: Arc<ComputerLink>,
    /// The Bot whose folder the work uses.
    bot: Id,
    /// The Space-wide request counter.
    ids: Arc<AtomicU64>,
    /// How long one request may wait for its answer.
    timeout: Duration,
    /// The directory the run works in.
    working_dir: Option<String>,
    /// The owner-granted write access the run carries.
    access: neha_computer::channel::AccessFrame,
    /// A folder allowed for the next action only.
    once_grant: std::sync::Mutex<Option<String>>,
    /// Environment entries every action of this run adds to its shell.
    env: Vec<(String, String)>,
}

impl RemoteRuntime {
    /// Builds a runtime over a live channel.
    #[must_use]
    pub fn new(link: Arc<ComputerLink>, bot: Id, ids: Arc<AtomicU64>, timeout: Duration) -> Self {
        Self {
            link,
            bot,
            ids,
            timeout,
            working_dir: None,
            access: neha_computer::channel::AccessFrame::default(),
            once_grant: std::sync::Mutex::new(None),
            env: Vec::new(),
        }
    }

    /// Sets the directory this run's actions work in.
    #[must_use]
    pub fn with_working_dir(mut self, working_dir: impl Into<String>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }

    /// Sets the owner-granted write access this run carries.
    #[must_use]
    pub fn with_access(mut self, access: neha_computer::channel::AccessFrame) -> Self {
        self.access = access;
        self
    }

    /// Sets the environment every shell action of this run sees.
    #[must_use]
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// Allows one extra folder for the next action only.
    #[must_use]
    pub fn with_once_grant(self, folder: impl Into<String>) -> Self {
        if let Ok(mut once) = self.once_grant.lock() {
            *once = Some(folder.into());
        }
        self
    }

    /// Returns the id for the next request.
    fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }
}

impl ComputerRuntime for RemoteRuntime {
    // The trait promises a `Send` future; `async fn` would not state that.
    #[allow(clippy::manual_async_fn)]
    fn execute(&self, action: Action) -> impl std::future::Future<Output = ActionResult> + Send {
        async move {
            let id = self.next_id();
            let mut access = self.access.clone();
            if let Ok(mut once) = self.once_grant.lock()
                && let Some(folder) = once.take()
            {
                access.write_roots.push(folder);
            }

            let answer = tokio::time::timeout(
                self.timeout,
                self.link.act(
                    id,
                    self.bot,
                    action,
                    self.working_dir.as_deref(),
                    &access,
                    &self.env,
                ),
            )
            .await;
            match answer {
                Ok(Answer::ActionResult(result)) => result,
                Ok(Answer::Bytes(_)) => {
                    ActionResult::failed("the Computer answered an action with bytes")
                }
                Ok(Answer::Failed(message)) => ActionResult::failed(message),
                Ok(Answer::Gone(message)) => ActionResult {
                    status: ActionStatus::Unknown,
                    output: ToolOutput::full(message.clone()),
                    error: Some(message),
                },
                Err(_) => {
                    self.link.cancel(id);
                    ActionResult {
                        status: ActionStatus::TimedOut,
                        output: ToolOutput::full("timed out"),
                        error: Some("timed out".to_owned()),
                    }
                }
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn read_bytes(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ByteError>> + Send {
        let path = path.to_owned();
        async move {
            let id = self.next_id();
            let answer = tokio::time::timeout(
                self.timeout,
                self.link.read(
                    id,
                    self.bot,
                    &path,
                    self.working_dir.as_deref(),
                    &self.access,
                ),
            )
            .await;
            match answer {
                Ok(Answer::Bytes(bytes)) => Ok(bytes),
                Ok(Answer::ActionResult(_)) => Err(ByteError::Failed(
                    "the Computer answered a File take with an action result".to_owned(),
                )),
                Ok(Answer::Failed(message)) => Err(ByteError::Failed(message)),
                Ok(Answer::Gone(message)) => Err(ByteError::Lost(message)),
                Err(_) => {
                    self.link.cancel(id);
                    Err(ByteError::TimedOut(
                        "the Computer did not answer in time".to_owned(),
                    ))
                }
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn write_bytes(
        &self,
        path: &str,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), ByteError>> + Send {
        let path = path.to_owned();
        async move {
            let id = self.next_id();
            let answer = tokio::time::timeout(
                self.timeout,
                self.link.write(id, self.bot, &path, bytes, &self.access),
            )
            .await;
            match answer {
                // A File delivery is acknowledged with no bytes back, or with
                // a successful action result.
                Ok(Answer::Bytes(_)) => Ok(()),
                Ok(Answer::ActionResult(result)) if result.status == ActionStatus::Succeeded => {
                    Ok(())
                }
                Ok(Answer::ActionResult(result)) => {
                    Err(ByteError::Failed(result.error.unwrap_or_else(|| {
                        "the Computer refused the File".to_owned()
                    })))
                }
                Ok(Answer::Failed(message)) => Err(ByteError::Failed(message)),
                Ok(Answer::Gone(message)) => Err(ByteError::Lost(message)),
                Err(_) => {
                    self.link.cancel(id);
                    Err(ByteError::TimedOut(
                        "the Computer did not answer in time".to_owned(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use neha::id::Id;
    use neha_computer::channel::{DeviceFrame, HostFrame};
    use neha_computer::{Action, ActionStatus, ByteError, ComputerRuntime, ToolOutput};
    use tokio::sync::mpsc;

    use super::{RemoteRuntime, TIMEOUT_GRACE};
    use crate::computer_link::{Answer, ComputerLink, ComputerLinks, OFFLINE, answer};

    /// A runtime over a fresh channel, with a 50ms answer limit.
    fn remote() -> (
        RemoteRuntime,
        Arc<ComputerLink>,
        Id,
        mpsc::Receiver<HostFrame>,
    ) {
        let links = ComputerLinks::new();
        let computer = Id::new();
        let bot = Id::new();
        let (frames, outgoing) = mpsc::channel(crate::computer_link::FRAME_QUEUE);
        let link = links.connect(
            computer,
            frames,
            crate::computer_link::AccessCapabilities::SUPPORTED,
        );
        let runtime = RemoteRuntime::new(
            Arc::clone(&link),
            bot,
            links.ids(),
            Duration::from_millis(50),
        );

        (runtime, link, bot, outgoing)
    }

    #[tokio::test]
    async fn a_runs_write_roots_cross_the_channel_with_its_once_grant() {
        let (runtime, link, _bot, mut outgoing) = remote();
        let runtime = Arc::new(
            runtime
                .with_working_dir("/data/work")
                .with_access(neha_computer::channel::AccessFrame {
                    full_access: false,
                    write_roots: vec!["/data/shared".to_owned(), "/data/work".to_owned()],
                })
                .with_once_grant("/data/once"),
        );

        // The run's directory, its grants, and the one-time grant travel with
        // the action.
        let first = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .execute(Action::Shell {
                        command: "pwd".to_owned(),
                    })
                    .await
            }
        });
        let HostFrame::Action {
            id,
            working_dir,
            access,
            ..
        } = outgoing.recv().await.unwrap()
        else {
            panic!("expected an action frame");
        };
        assert_eq!(working_dir.as_deref(), Some("/data/work"));
        assert_eq!(
            access,
            neha_computer::channel::AccessFrame {
                full_access: false,
                write_roots: vec![
                    "/data/shared".to_owned(),
                    "/data/work".to_owned(),
                    "/data/once".to_owned(),
                ],
            }
        );
        link.deliver(
            id,
            Answer::ActionResult(neha_computer::ActionResult::succeeded(ToolOutput::full(
                "/data/work",
            ))),
        );
        assert_eq!(first.await.unwrap().status, ActionStatus::Succeeded);

        // The one-time grant opened one folder for one action only.
        let second = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .execute(Action::Shell {
                        command: "pwd".to_owned(),
                    })
                    .await
            }
        });
        let HostFrame::Action { id, access, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected an action frame");
        };
        assert_eq!(
            access,
            neha_computer::channel::AccessFrame {
                full_access: false,
                write_roots: vec!["/data/shared".to_owned(), "/data/work".to_owned()],
            },
            "the spent one-time grant opens nothing more"
        );
        link.deliver(
            id,
            Answer::ActionResult(neha_computer::ActionResult::succeeded(ToolOutput::full(
                "/data/work",
            ))),
        );
        assert_eq!(second.await.unwrap().status, ActionStatus::Succeeded);
    }

    #[tokio::test]
    async fn an_action_crosses_the_channel_and_returns_its_result() {
        let (runtime, link, bot, mut outgoing) = remote();
        let waiting = tokio::spawn(async move {
            runtime
                .execute(Action::Shell {
                    command: "pwd".to_owned(),
                })
                .await
        });
        let HostFrame::Action { id, bot: named, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected an action frame");
        };
        assert_eq!(named, bot.to_string());
        link.deliver(
            id,
            Answer::ActionResult(neha_computer::ActionResult::succeeded(ToolOutput::full(
                "/bot-folder",
            ))),
        );
        let result = waiting.await.unwrap();
        assert_eq!(result.status, ActionStatus::Succeeded);
        assert_eq!(result.output.text, "/bot-folder");
    }

    #[tokio::test]
    async fn a_failed_frame_answers_the_action_with_its_reason() {
        let (runtime, link, _bot, mut outgoing) = remote();
        let waiting = tokio::spawn(async move {
            runtime
                .execute(Action::ReadFile {
                    path: "notes.txt".to_owned(),
                })
                .await
        });
        let HostFrame::Action { id, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected an action frame");
        };
        let (_, refusal) = answer(DeviceFrame::Failed {
            id,
            message: "the path is outside the writable folder".to_owned(),
        })
        .unwrap();
        link.deliver(id, refusal);
        let result = waiting.await.unwrap();
        assert_eq!(result.status, ActionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some("the path is outside the writable folder")
        );
    }

    #[tokio::test]
    async fn a_silent_computer_times_out_and_forgets_the_request() {
        let (runtime, link, _bot, mut outgoing) = remote();
        let result = runtime
            .execute(Action::Shell {
                command: "sleep 60".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::TimedOut);
        assert_eq!(result.error.as_deref(), Some("timed out"));
        let HostFrame::Action { id, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected an action frame");
        };
        // The caller stopped waiting, so the late answer resolves nothing.
        link.deliver(
            id,
            Answer::ActionResult(neha_computer::ActionResult::succeeded(ToolOutput::full(
                "late",
            ))),
        );
    }

    #[tokio::test]
    async fn read_bytes_and_write_bytes_cross_the_channel() {
        let (runtime, link, _bot, mut outgoing) = remote();
        // The run's pinned access crosses with every File request, so a read
        // that verifies an interrupted write sees what that write could reach.
        let access = neha_computer::channel::AccessFrame {
            full_access: true,
            write_roots: vec!["/data/shared".to_owned()],
        };
        let runtime = Arc::new(runtime.with_access(access.clone()));

        let reader = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.read_bytes("report.pdf").await }
        });
        let HostFrame::Read {
            id,
            access: read_access,
            ..
        } = outgoing.recv().await.unwrap()
        else {
            panic!("expected a read frame");
        };
        assert_eq!(read_access, access);
        link.deliver(id, Answer::Bytes(b"%PDF".to_vec()));
        assert_eq!(reader.await.unwrap(), Ok(b"%PDF".to_vec()));

        let writer = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.write_bytes("out.bin", b"hello".to_vec()).await }
        });
        let HostFrame::Write {
            id,
            bytes,
            access: write_access,
            ..
        } = outgoing.recv().await.unwrap()
        else {
            panic!("expected a write frame");
        };
        assert_eq!(write_access, access);
        assert_eq!(URL_SAFE_NO_PAD.decode(bytes).unwrap(), b"hello");
        // A File delivery is acknowledged with no bytes back.
        link.deliver(id, Answer::Bytes(Vec::new()));
        assert_eq!(writer.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_channel_that_goes_away_fails_work_and_file_requests() {
        let (runtime, link, _bot, _outgoing) = remote();
        let runtime = Arc::new(runtime);
        let link = Arc::clone(&link);
        let action = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .execute(Action::Shell {
                        command: "pwd".to_owned(),
                    })
                    .await
            }
        });
        let read = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.read_bytes("notes.txt").await }
        });
        let write = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.write_bytes("out.bin", b"hello".to_vec()).await }
        });
        tokio::task::yield_now().await;
        link.close();

        let result = action.await.unwrap();
        assert_eq!(
            result.status,
            ActionStatus::Unknown,
            "an action whose answer never returned is unknown"
        );
        assert_eq!(result.error.as_deref(), Some(OFFLINE));
        assert_eq!(
            read.await.unwrap(),
            Err(ByteError::Lost(OFFLINE.to_owned())),
            "a File take whose channel went away is lost, not refused"
        );
        assert_eq!(
            write.await.unwrap(),
            Err(ByteError::Lost(OFFLINE.to_owned())),
            "a File delivery whose channel went away is lost, not refused"
        );
    }

    #[tokio::test]
    async fn a_refused_file_request_is_failed_not_lost() {
        let (runtime, link, _bot, mut outgoing) = remote();
        let runtime = Arc::new(runtime);

        let reader = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.read_bytes("missing.txt").await }
        });
        let HostFrame::Read { id, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected a read frame");
        };
        link.deliver(id, Answer::Failed("cannot read missing.txt".to_owned()));
        assert_eq!(
            reader.await.unwrap(),
            Err(ByteError::Failed("cannot read missing.txt".to_owned()))
        );

        let writer = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.write_bytes("out.bin", b"hello".to_vec()).await }
        });
        let HostFrame::Write { id, .. } = outgoing.recv().await.unwrap() else {
            panic!("expected a write frame");
        };
        link.deliver(id, Answer::Failed("the folder is full".to_owned()));
        assert_eq!(
            writer.await.unwrap(),
            Err(ByteError::Failed("the folder is full".to_owned()))
        );
    }

    #[tokio::test]
    async fn a_file_request_that_never_answers_times_out() {
        let (runtime, _link, _bot, _outgoing) = remote();
        let runtime = Arc::new(runtime);

        let reader = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.read_bytes("notes.txt").await }
        });
        let writer = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            async move { runtime.write_bytes("out.bin", b"hello".to_vec()).await }
        });

        let read = reader.await.unwrap().map(|_bytes| ());
        let write = writer.await.unwrap();
        for answer in [read, write] {
            assert_eq!(
                answer,
                Err(ByteError::TimedOut(
                    "the Computer did not answer in time".to_owned()
                ))
            );
        }
    }

    #[test]
    fn the_remote_limit_adds_five_seconds_to_the_tool_timeout() {
        assert_eq!(TIMEOUT_GRACE, Duration::from_secs(5));
    }
}
