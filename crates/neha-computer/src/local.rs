//! The in-process Computer runtime.
//!
//! It executes real POSIX shell commands and real file operations inside one
//! workspace directory. It is the M1 implementation: not isolated, not
//! transported, and only for a Space on the same machine.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::action::{Action, ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
use crate::output::{HEAD_BYTES, TAIL_BYTES, truncate};

/// The time a shell command may run before its process group is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest file a write tool accepts.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIST_ENTRIES: usize = 1000;
/// The largest shell command a tool accepts.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A Computer runtime backed by one workspace directory.
pub struct LocalRuntime {
    workspace: PathBuf,
    timeout: Duration,
}

impl LocalRuntime {
    /// Opens a workspace, creating it when absent.
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let workspace = workspace.into();
        std::fs::create_dir_all(&workspace)?;
        let workspace = std::fs::canonicalize(&workspace)?;
        let temporary = workspace.join(".tmp");
        std::fs::create_dir_all(&temporary)?;

        Ok(Self {
            workspace,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Changes the shell time limit.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the workspace.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("the path is empty".to_owned());
        }
        let requested = Path::new(input);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.workspace.join(requested)
        };
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    normalized.push(component.as_os_str());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err("the path escapes the workspace".to_owned());
                    }
                }
            }
        }
        if !normalized.starts_with(&self.workspace) {
            return Err("the path escapes the workspace".to_owned());
        }

        // A symlink anywhere in the requested path is refused. Checking
        // `exists` would follow a dangling link, and a later write would
        // follow it too.
        let relative = normalized
            .strip_prefix(&self.workspace)
            .map_err(|_| "the path escapes the workspace".to_owned())?;
        let mut current = self.workspace.clone();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("the path uses a symlink, which is not followed".to_owned());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!("cannot resolve {}: {error}", current.display()));
                }
            }
        }

        Ok(normalized)
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn read_file(&self, path: &str) -> ActionResult {
        let path = match self.resolve(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return ActionResult::failed(format!("cannot read {}: {error}", path.display()));
            }
        };
        if !metadata.is_file() {
            return ActionResult::failed(format!("{} is not a file", self.relative(&path)));
        }
        let length = metadata.len();
        let mut bytes = Vec::new();
        let read: std::io::Result<Option<Vec<u8>>> = if length <= MAX_READ_BYTES as u64 {
            std::fs::File::open(&path)
                .and_then(|file| file.take(MAX_READ_BYTES as u64).read_to_end(&mut bytes))
                .map(|_| None)
        } else {
            read_head_and_tail(&path, length, &mut bytes)
        };
        let tail = match read {
            Ok(Some(tail)) => tail,
            Ok(None) => {
                return ActionResult::succeeded(truncate(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }
            Err(error) => {
                return ActionResult::failed(format!("cannot read {}: {error}", path.display()));
            }
        };
        let output = join_head_and_tail(&bytes, &tail, length);

        ActionResult::succeeded(output)
    }

    fn write_file(&self, path: &str, content: &str) -> ActionResult {
        if content.len() > MAX_WRITE_BYTES {
            return ActionResult::failed(format!(
                "the content is larger than {MAX_WRITE_BYTES} bytes"
            ));
        }
        let path = match self.resolve(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return ActionResult::failed(format!("cannot create {}: {error}", parent.display()));
        }
        match std::fs::write(&path, content) {
            Ok(()) => ActionResult::succeeded(ToolOutput::full(format!(
                "wrote {} bytes to {}",
                content.len(),
                self.relative(&path)
            ))),
            Err(error) => ActionResult::failed(format!("cannot write {}: {error}", path.display())),
        }
    }

    fn list_files(&self, path: &str) -> ActionResult {
        let path = match self.resolve(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                return ActionResult::failed(format!(
                    "cannot list {}: {error}",
                    self.relative(&path)
                ));
            }
        };
        let mut lines = Vec::new();
        for entry in entries.take(MAX_LIST_ENTRIES) {
            let Ok(entry) = entry else { continue };
            let kind = match entry.file_type() {
                Ok(kind) if kind.is_dir() => "dir ",
                Ok(kind) if kind.is_symlink() => "link",
                Ok(_) => "file",
                Err(_) => "?   ",
            };
            lines.push(format!("{kind} {}", entry.file_name().to_string_lossy()));
        }
        lines.sort();
        if lines.is_empty() {
            lines.push("(empty)".to_owned());
        }

        ActionResult::succeeded(truncate(lines.join("\n")))
    }

    async fn shell(&self, command: &str) -> ActionResult {
        if command.trim().is_empty() {
            return ActionResult::failed("the command is empty");
        }
        if command.len() > MAX_COMMAND_BYTES {
            return ActionResult::failed("the command is too long");
        }
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.workspace)
            .env("TMPDIR", self.workspace.join(".tmp"))
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);

        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => return ActionResult::failed(format!("cannot start a shell: {error}")),
        };
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = stdout.map(|stdout| tokio::spawn(read_bounded(stdout)));
        let stderr_task = stderr.map(|stderr| tokio::spawn(read_bounded(stderr)));
        let deadline = tokio::time::Instant::now() + self.timeout;

        let mut timed_out = false;
        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                kill_group(pid);
                return ActionResult::failed(format!("the shell failed: {error}"));
            }
            Err(_) => {
                timed_out = true;
                kill_group(pid);
                let _ = child.wait().await;
                None
            }
        };
        // The shell may have left children holding the pipes open. The
        // command is over, so the group goes; the readers then drain what
        // the pipes still hold.
        kill_group(pid);
        let grace = if timed_out {
            DRAIN_GRACE
        } else {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .clamp(Duration::from_millis(50), DRAIN_GRACE)
        };
        let (stdout, stdout_lost) = drain(stdout_task, grace).await;
        let (stderr, stderr_lost) = drain(stderr_task, grace).await;
        let (mut stdout_text, stdout_truncated, stdout_omitted) = stdout.render();
        let (stderr_text, stderr_truncated, stderr_omitted) = stderr.render();
        if stdout_lost || stderr_lost {
            stdout_text.push_str("\n[the shell's output could not be drained]");
        }
        let code = status
            .as_ref()
            .and_then(std::process::ExitStatus::code)
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        let mut text = format!("exit code: {code}\nstdout:\n{stdout_text}");
        if !stderr_text.is_empty() {
            text.push_str(&format!("\nstderr:\n{stderr_text}"));
        }
        if timed_out {
            text = format!("the command ran past its time limit and was killed\n{text}");
        }
        let truncated = stdout_truncated || stderr_truncated;
        let captured = truncate(text);
        let output = ToolOutput {
            text: captured.text,
            truncated: truncated || captured.truncated,
            omitted_bytes: stdout_omitted
                .saturating_add(stderr_omitted)
                .saturating_add(captured.omitted_bytes),
        };
        if timed_out {
            return ActionResult {
                status: ActionStatus::TimedOut,
                output,
                error: Some("timed out".to_owned()),
            };
        }
        if status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success)
        {
            ActionResult::succeeded(output)
        } else {
            ActionResult {
                status: ActionStatus::Failed,
                output,
                error: Some(format!("the command exited with code {code}")),
            }
        }
    }
}

impl ComputerRuntime for LocalRuntime {
    // The trait promises a `Send` future; `async fn` would not state that.
    #[allow(clippy::manual_async_fn)]
    fn execute(&self, action: Action) -> impl std::future::Future<Output = ActionResult> + Send {
        async move {
            match action {
                Action::Shell { command } => self.shell(&command).await,
                Action::ReadFile { path } => self.read_file(&path),
                Action::WriteFile { path, content } => self.write_file(&path, &content),
                Action::ListFiles { path } => self.list_files(&path),
            }
        }
    }
}

#[derive(Default)]
struct Bounded {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
}

impl Bounded {
    fn render(&self) -> (String, bool, u64) {
        if self.total <= (HEAD_BYTES + TAIL_BYTES) as u64 {
            let mut text = String::from_utf8_lossy(&self.head).into_owned();
            let tail: Vec<u8> = self.tail.iter().copied().collect();
            text.push_str(&String::from_utf8_lossy(&tail));
            return (text, false, 0);
        }
        let omitted = self
            .total
            .saturating_sub(self.head.len() as u64)
            .saturating_sub(self.tail.len() as u64);
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        text.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        text.push_str(&String::from_utf8_lossy(&tail));

        (text, true, omitted)
    }
}

async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Bounded> {
    let mut bounded = Bounded::default();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bounded.total = bounded.total.saturating_add(read as u64);
        let mut rest = &buffer[..read];
        if bounded.head.len() < HEAD_BYTES {
            let room = HEAD_BYTES - bounded.head.len();
            let take = rest.len().min(room);
            bounded.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        for byte in rest {
            if bounded.tail.len() == TAIL_BYTES {
                bounded.tail.pop_front();
            }
            bounded.tail.push_back(*byte);
        }
    }

    Ok(bounded)
}

async fn drain(
    task: Option<tokio::task::JoinHandle<std::io::Result<Bounded>>>,
    grace: Duration,
) -> (Bounded, bool) {
    let Some(task) = task else {
        return (Bounded::default(), false);
    };
    let abort = task.abort_handle();
    match tokio::time::timeout(grace, task).await {
        Ok(Ok(Ok(bounded))) => (bounded, false),
        Ok(_) => (Bounded::default(), true),
        Err(_) => {
            abort.abort();
            (Bounded::default(), true)
        }
    }
}

fn read_head_and_tail(
    path: &Path,
    length: u64,
    bytes: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut file = std::fs::File::open(path)?;
    let head = HEAD_BYTES.min(length as usize);
    bytes.resize(head, 0);
    file.read_exact(bytes)?;
    let tail = TAIL_BYTES.min(length as usize - head);
    let mut tail_bytes = vec![0_u8; tail];
    file.seek(SeekFrom::End(-(tail as i64)))?;
    file.read_exact(&mut tail_bytes)?;

    Ok(Some(tail_bytes))
}

fn join_head_and_tail(head: &[u8], tail: &[u8], length: u64) -> ToolOutput {
    let omitted = length
        .saturating_sub(head.len() as u64)
        .saturating_sub(tail.len() as u64);
    let mut text = String::from_utf8_lossy(head).into_owned();
    text.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
    text.push_str(&String::from_utf8_lossy(tail));

    ToolOutput {
        text,
        truncated: true,
        omitted_bytes: omitted,
    }
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // The child was spawned as its own process group leader.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

/// The runtime cannot open its workspace.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// A directory could not be created or read.
    #[error("workspace error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::LocalRuntime;
    use crate::action::{Action, ActionStatus, ComputerRuntime};

    fn runtime() -> (tempfile::TempDir, LocalRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = LocalRuntime::new(directory.path().join("workspace")).unwrap();

        (directory, runtime)
    }

    #[tokio::test]
    async fn writes_reads_and_lists_a_file() {
        let (_dir, runtime) = runtime();
        let written = runtime
            .execute(Action::WriteFile {
                path: "hello.txt".to_owned(),
                content: "hello neha".to_owned(),
            })
            .await;
        let read = runtime
            .execute(Action::ReadFile {
                path: "hello.txt".to_owned(),
            })
            .await;
        let listed = runtime
            .execute(Action::ListFiles {
                path: ".".to_owned(),
            })
            .await;

        assert_eq!(written.status, ActionStatus::Succeeded);
        assert_eq!(read.output.text, "hello neha");
        assert!(
            listed.output.text.contains("hello.txt"),
            "{}",
            listed.output.text
        );
        assert!(runtime.workspace().join("hello.txt").is_file());
    }

    #[tokio::test]
    async fn refuses_paths_outside_the_workspace() {
        let (directory, runtime) = runtime();
        std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
        for path in ["../outside.txt", "/etc/passwd"] {
            let result = runtime
                .execute(Action::ReadFile {
                    path: path.to_owned(),
                })
                .await;
            assert_eq!(result.status, ActionStatus::Failed, "{path}");
            assert!(
                result.output.text.contains("escapes"),
                "{path}: {}",
                result.output.text
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_symlink_outside_the_workspace() {
        let (directory, runtime) = runtime();
        std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();
        let result = runtime
            .execute(Action::ReadFile {
                path: "escape/outside.txt".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed);
        assert!(result.output.text.contains("symlink"));
    }

    #[tokio::test]
    async fn refuses_to_write_through_a_dangling_symlink() {
        let (directory, runtime) = runtime();
        let outside = directory.path().join("created-outside.txt");
        std::os::unix::fs::symlink(&outside, runtime.workspace().join("escape")).unwrap();
        let result = runtime
            .execute(Action::WriteFile {
                path: "escape".to_owned(),
                content: "pwned".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed);
        assert!(result.output.text.contains("symlink"));
        assert!(!outside.exists(), "the write followed a dangling symlink");
    }

    #[tokio::test]
    async fn runs_a_shell_command_and_reports_its_output() {
        let (_dir, runtime) = runtime();
        let result = runtime
            .execute(Action::Shell {
                command: "printf 42".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded);
        assert!(result.output.text.contains("42"), "{}", result.output.text);
    }

    #[tokio::test]
    async fn the_shell_does_not_see_the_space_environment() {
        let (_dir, runtime) = runtime();
        // The Space process holds a Provider key in this test; the shell
        // must not inherit it.
        unsafe { std::env::set_var("NEHA_PROVIDER_KEY", "sk-must-not-leak") };
        let result = runtime
            .execute(Action::Shell {
                command: "printenv NEHA_PROVIDER_KEY || true; echo done".to_owned(),
            })
            .await;
        unsafe { std::env::remove_var("NEHA_PROVIDER_KEY") };

        assert!(!result.output.text.contains("sk-must-not-leak"));
        assert!(result.output.text.contains("done"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_that_exits_does_not_leave_its_background_child_holding_the_pipes() {
        let (_dir, runtime) = runtime();
        let runtime = runtime.with_timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let result = runtime
            .execute(Action::Shell {
                command: "sleep 30 & echo started".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(result.output.text.contains("started"));
    }

    #[tokio::test]
    async fn timeout_kills_a_grandchild_too() {
        let (_dir, runtime) = runtime();
        let runtime = runtime.with_timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let result = runtime
            .execute(Action::Shell {
                command: "sleep 30 & wait".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn stdout_and_stderr_share_one_output_budget() {
        let (_dir, runtime) = runtime();
        let result = runtime
            .execute(Action::Shell {
                command: "head -c 30000 /dev/zero | tr '\\0' 'a'; \
                          head -c 30000 /dev/zero | tr '\\0' 'b' >&2"
                    .to_owned(),
            })
            .await;

        assert!(result.output.truncated);
        assert!(result.output.omitted_bytes > 0);
        assert!(
            result.output.text.len() < 40_000,
            "the combined result is {} bytes",
            result.output.text.len()
        );
        assert!(result.output.text.contains("bytes omitted"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_output_over_the_cap_keeps_its_real_tail_and_count() {
        let (_dir, runtime) = runtime();
        let result = runtime
            .execute(Action::Shell {
                command: "yes y | head -c 9000000; printf 'END'".to_owned(),
            })
            .await;

        assert!(result.output.truncated);
        assert!(result.output.omitted_bytes > 8_000_000);
        assert!(result.output.text.contains("bytes omitted"));
        assert!(result.output.text.ends_with("END") || result.output.text.contains("END"));
    }

    #[tokio::test]
    async fn a_large_file_keeps_a_head_a_tail_and_the_true_omitted_count() {
        let (_dir, runtime) = runtime();
        let path = runtime.workspace().join("large.txt");
        let mut content = "a".repeat(9 * 1024 * 1024);
        content.push_str("TAIL");
        std::fs::write(&path, &content).unwrap();
        let result = runtime
            .execute(Action::ReadFile {
                path: "large.txt".to_owned(),
            })
            .await;

        assert!(result.output.truncated);
        assert!(result.output.omitted_bytes > 8_000_000);
        assert!(result.output.text.ends_with("TAIL"));
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_is_killed() {
        let (_dir, runtime) = runtime();
        let runtime = runtime.with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let result = runtime
            .execute(Action::Shell {
                command: "sleep 30".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(result.output.text.contains("time limit"));
    }

    #[tokio::test]
    async fn large_output_keeps_a_head_and_a_tail() {
        let (_dir, runtime) = runtime();
        let result = runtime
            .execute(Action::Shell {
                command: "head -c 40000 /dev/zero | tr '\\0' 'a'".to_owned(),
            })
            .await;

        assert!(result.output.truncated);
        assert!(result.output.omitted_bytes > 0);
        assert!(result.output.text.contains("bytes omitted"));
    }
}
