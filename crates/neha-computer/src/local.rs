//! The in-process Computer runtime.
//!
//! It executes real POSIX shell commands and real file operations inside one
//! workspace directory. It is the M1 implementation: not isolated, not
//! transported, and only for a Space on the same machine.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::action::{Action, ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
use crate::output::truncate;

/// The time a shell command may run before its process group is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_WRITE_BYTES: usize = 1024 * 1024;
const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_COMMAND_BYTES: usize = 64 * 1024;

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

        // An existing ancestor may be a symlink that points outside.
        let mut ancestor = normalized.clone();
        loop {
            if ancestor.exists() {
                let real = std::fs::canonicalize(&ancestor)
                    .map_err(|error| format!("cannot resolve {}: {error}", ancestor.display()))?;
                if !real.starts_with(&self.workspace) {
                    return Err("the path escapes the workspace".to_owned());
                }
                break;
            }
            if !ancestor.pop() {
                break;
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
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&path)
            .and_then(|file| file.take(MAX_READ_BYTES as u64).read_to_end(&mut bytes))
            .map_err(|error| format!("cannot read {}: {error}", path.display()));
        if let Err(error) = read {
            return ActionResult::failed(error);
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut output = truncate(text);
        if metadata.len() > MAX_READ_BYTES as u64 {
            output.truncated = true;
            output.omitted_bytes = output
                .omitted_bytes
                .saturating_add(metadata.len() - MAX_READ_BYTES as u64);
        }

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

        let mut timed_out = false;
        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => return ActionResult::failed(format!("the shell failed: {error}")),
            Err(_) => {
                timed_out = true;
                kill_group(pid);
                let _ = child.wait().await;
                None
            }
        };
        let mut stdout = String::new();
        if let Some(task) = stdout_task
            && let Ok(Ok(text)) = task.await
        {
            stdout = text;
        }
        let mut stderr = String::new();
        if let Some(task) = stderr_task
            && let Ok(Ok(text)) = task.await
        {
            stderr = text;
        }
        let code = status
            .as_ref()
            .and_then(std::process::ExitStatus::code)
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        let mut text = format!("exit code: {code}\nstdout:\n{stdout}");
        if !stderr.is_empty() {
            text.push_str(&format!("\nstderr:\n{stderr}"));
        }
        if timed_out {
            text = format!("the command ran past its time limit and was killed\n{text}");
        }
        let output = truncate(text);
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

async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if bytes.len() < MAX_READ_BYTES {
            let room = MAX_READ_BYTES - bytes.len();
            bytes.extend_from_slice(&buffer[..read.min(room)]);
        }
    }

    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        assert!(result.output.text.contains("escapes"));
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
        let result = runtime
            .execute(Action::Shell {
                command: "printenv NEHA_PROVIDER_KEY; echo done".to_owned(),
            })
            .await;

        assert!(!result.output.text.contains("sk-"));
        assert!(result.output.text.contains("done"));
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
