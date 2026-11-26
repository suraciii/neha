//! The in-process Computer runtime.
//!
//! It executes real POSIX shell commands and real file operations. In managed
//! mode everything stays inside one workspace directory: not isolated, not
//! transported, and only for a Space on the same machine. In real mode it is
//! one Bot's view of a real machine: reads reach what the machine user can
//! read, writes reach one folder, and the shell runs under bubblewrap. Hidden
//! roots are the exception to both reads and writes: the Computer's own
//! directory, which holds its channel credential, is passed that way so a Bot
//! never reaches it.

use std::ffi::OsString;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::action::{Action, ActionResult, ActionStatus, ByteError, ComputerRuntime, ToolOutput};
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
/// The path a shell searches for its commands, in both modes.
const SHELL_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// The writable root outside a Bot's folder that a real shell also gets.
const SANDBOX_TMPDIR: &str = "/tmp";

/// The reach of a Bot on a real machine: it reads the machine and writes one
/// folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineAccess {
    /// This Bot's folder: its default working directory and its only writable
    /// place.
    pub folder: PathBuf,
}

/// A Computer runtime backed by one directory.
pub struct LocalRuntime {
    /// The only root writes may reach: the managed workspace, or the Bot's
    /// folder on a real machine.
    root: PathBuf,
    mode: Mode,
    /// The directories a real Bot must not read or write: the Computer's own
    /// configuration directory, which holds its channel credential. Resolved,
    /// so an opened file's real path can be compared against them.
    hidden: Vec<PathBuf>,
    timeout: Duration,
    /// The process group id of the shell that is running, or 0.
    ///
    /// A channel that goes away must kill the work it asked for, and it cannot
    /// see the child: this slot is the hook. [`cancel_in_flight`] reads it.
    ///
    /// [`cancel_in_flight`]: LocalRuntime::cancel_in_flight
    in_flight: AtomicU32,
}

/// Which kind of machine a runtime drives.
enum Mode {
    /// A managed workspace: the root is also everything a read may reach.
    Managed,
    /// A real machine, fenced by bubblewrap.
    Machine {
        /// The resolved `bwrap` binary.
        bwrap: PathBuf,
    },
}

impl LocalRuntime {
    /// Opens a managed workspace, creating it when absent.
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let workspace = workspace.into();
        std::fs::create_dir_all(&workspace)?;
        let workspace = std::fs::canonicalize(&workspace)?;
        let temporary = workspace.join(".tmp");
        std::fs::create_dir_all(&temporary)?;

        Ok(Self {
            root: workspace,
            mode: Mode::Managed,
            hidden: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
        })
    }

    /// Opens one Bot's folder on a real machine.
    ///
    /// Reads reach everything the machine user can read, writes reach only the
    /// folder, and the shell runs under bubblewrap. The constructor refuses
    /// when bubblewrap is missing or cannot start a sandbox, so a daemon never
    /// serves a Computer it cannot fence.
    pub fn machine(folder: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        Self::machine_with_bwrap(folder.into(), find_bwrap(), Vec::new())
    }

    /// Opens one Bot's folder on a real machine, with directories it must not
    /// reach.
    ///
    /// A hidden root is invisible to both of this Bot's read paths: the file
    /// tools open a file and resolve the opened descriptor's real path, and
    /// the shell gets a fresh tmpfs mounted over each root. A write never
    /// leaves the Bot's folder, and a target that lands under a hidden root is
    /// refused before it is opened.
    pub fn machine_with_hidden(
        folder: impl Into<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        Self::machine_with_bwrap(folder.into(), find_bwrap(), hidden)
    }

    /// Opens a real-machine runtime with a specific `bwrap`, for tests.
    fn machine_with_bwrap(
        folder: impl Into<PathBuf>,
        bwrap: Option<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let folder = folder.into();
        std::fs::create_dir_all(&folder)?;
        let folder = std::fs::canonicalize(&folder)?;
        // A hidden root must exist: it is the directory the daemon wrote its
        // own configuration into, and a missing one would mean the tmpfs that
        // hides it cannot be mounted.
        let hidden = hidden
            .into_iter()
            .map(|root| std::fs::canonicalize(&root))
            .collect::<Result<Vec<_>, _>>()?;
        let bwrap = bwrap.ok_or_else(|| {
            RuntimeError::NoSandbox("bwrap was not found on PATH; install bubblewrap".to_owned())
        })?;
        probe_bwrap(&bwrap, &folder, &hidden).map_err(RuntimeError::NoSandbox)?;

        Ok(Self {
            root: folder,
            mode: Mode::Machine { bwrap },
            hidden,
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
        })
    }

    /// Kills the process group of the shell that is running, if one is.
    ///
    /// A caller whose channel closed has no result to wait for: this ends the
    /// work instead of leaving it running. The running [`execute`] sees its
    /// child die and returns its normal failed or timed-out result. Returns
    /// whether a shell was running.
    ///
    /// [`execute`]: ComputerRuntime::execute
    pub fn cancel_in_flight(&self) -> bool {
        let pid = self.in_flight.swap(0, Ordering::SeqCst);
        if pid == 0 {
            return false;
        }
        kill_group(Some(pid));
        true
    }

    /// Changes the shell time limit.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the only root writes reach: the workspace in managed mode, the
    /// Bot's folder in real mode.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.root
    }

    /// The path a model asked for, before any mode's rules apply.
    fn requested(&self, input: &str) -> Result<PathBuf, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("the path is empty".to_owned());
        }
        let requested = Path::new(input);
        Ok(if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        })
    }

    /// Resolves a path in managed mode: everything must stay in the workspace,
    /// and no symlink is followed.
    fn resolve_managed(&self, input: &str) -> Result<PathBuf, String> {
        let joined = self.requested(input)?;
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
        if !normalized.starts_with(&self.root) {
            return Err("the path escapes the workspace".to_owned());
        }

        // A symlink anywhere in the requested path is refused. Checking
        // `exists` would follow a dangling link, and a later write would
        // follow it too.
        let relative = normalized
            .strip_prefix(&self.root)
            .map_err(|_| "the path escapes the workspace".to_owned())?;
        let mut current = self.root.clone();
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

    /// Resolves a read on a real machine: a relative path sits in the Bot's
    /// folder and an absolute path is the machine's own. Reads may reach
    /// anything the machine user can read.
    fn resolve_machine_read(&self, input: &str) -> Result<PathBuf, String> {
        Ok(normalize_path(&self.requested(input)?))
    }

    /// Resolves a read for this mode's rules.
    fn resolve_read(&self, input: &str) -> Result<PathBuf, String> {
        match &self.mode {
            Mode::Managed => self.resolve_managed(input),
            Mode::Machine { .. } => self.resolve_machine_read(input),
        }
    }

    /// Writes bytes to a relative path, following this mode's write rules.
    ///
    /// Returns the path to show a reader (relative to the write root). A
    /// managed workspace keeps its check-then-write path; a real machine writes
    /// only through directory file descriptors, so no symlink is followed and
    /// no check-to-open window exists.
    fn write_to_root(&self, input: &str, bytes: &[u8]) -> Result<String, ByteError> {
        match &self.mode {
            Mode::Managed => {
                let path = self.resolve_managed(input).map_err(ByteError::Failed)?;
                if let Some(parent) = path.parent()
                    && let Err(error) = std::fs::create_dir_all(parent)
                {
                    return Err(ByteError::Failed(format!(
                        "cannot create {}: {error}",
                        parent.display()
                    )));
                }
                std::fs::write(&path, bytes).map_err(|error| {
                    ByteError::Failed(format!("cannot write {}: {error}", path.display()))
                })?;

                Ok(self.relative(&path))
            }
            Mode::Machine { .. } => {
                // A write cannot leave the Bot's folder, so a hidden root is
                // only reachable when that folder itself sits under one; that
                // is a misconfiguration the daemon refuses, and a write is
                // refused here before any descriptor is opened.
                if self.is_hidden(&normalize_path(&self.root.join(input.trim()))) {
                    return Err(ByteError::Failed(hidden_error(input.trim())));
                }
                write_machine_file(&self.root, input, bytes)?;
                Ok(input.trim().to_owned())
            }
        }
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// Returns whether a path lies under one of this runtime's hidden roots.
    fn is_hidden(&self, path: &Path) -> bool {
        self.hidden.iter().any(|root| path.starts_with(root))
    }

    /// Opens a path for a read, refusing a target under a hidden root.
    ///
    /// The hidden check resolves the opened descriptor's real path, not the
    /// path a caller typed, so a symlink or a swap between resolving and
    /// opening cannot move a hidden file past it: the descriptor decides.
    fn open_read(&self, path: &Path) -> Result<std::fs::File, String> {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        self.refuse_hidden(&file, path)?;

        Ok(file)
    }

    /// Opens a directory for listing, refusing a target under a hidden root.
    fn open_directory(&self, path: &Path) -> Result<(), String> {
        if self.hidden.is_empty() {
            return Ok(());
        }
        let directory = std::fs::File::open(path)
            .map_err(|error| format!("cannot list {}: {error}", self.relative(path)))?;
        self.refuse_hidden(&directory, path)
    }

    /// Refuses an opened file or directory that resolves under a hidden root.
    fn refuse_hidden(&self, opened: &std::fs::File, path: &Path) -> Result<(), String> {
        if self.hidden.is_empty() {
            return Ok(());
        }
        let shown = path.display();
        let real = opened_path(opened)
            .map_err(|error| format!("cannot verify what {shown} names: {error}"))?;
        if self.is_hidden(&without_deleted_suffix(real)) {
            return Err(hidden_error(shown));
        }

        Ok(())
    }

    fn read_file(&self, path: &str) -> ActionResult {
        let path = match self.resolve_read(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        let mut file = match self.open_read(&path) {
            Ok(file) => file,
            Err(error) => return ActionResult::failed(error),
        };
        let metadata = match file.metadata() {
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
            (&mut file)
                .take(MAX_READ_BYTES as u64)
                .read_to_end(&mut bytes)
                .map(|_| None)
        } else {
            read_head_and_tail(&mut file, length, &mut bytes)
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
        match self.write_to_root(path, content.as_bytes()) {
            Ok(shown) => ActionResult::succeeded(ToolOutput::full(format!(
                "wrote {} bytes to {shown}",
                content.len()
            ))),
            Err(error) => ActionResult::failed(error.to_string()),
        }
    }

    fn list_files(&self, path: &str) -> ActionResult {
        let path = match self.resolve_read(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        if let Err(error) = self.open_directory(&path) {
            return ActionResult::failed(error);
        }
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

    /// Builds the shell process for this mode, without starting it.
    fn shell_command(&self, command: &str) -> Command {
        let mut process = match &self.mode {
            Mode::Managed => {
                let mut process = Command::new("sh");
                process.arg("-c").arg(command).current_dir(&self.root);
                process
            }
            Mode::Machine { bwrap } => {
                let mut process = sandbox_command(bwrap, &self.root, &self.hidden);
                process
                    .arg("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(&self.root);
                process
            }
        };
        let temporary = match &self.mode {
            Mode::Managed => self.root.join(".tmp"),
            Mode::Machine { .. } => PathBuf::from(SANDBOX_TMPDIR),
        };
        process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PATH", SHELL_PATH)
            .env("HOME", &self.root)
            .env("TMPDIR", temporary)
            .kill_on_drop(true);
        process
    }

    async fn shell(&self, command: &str) -> ActionResult {
        if command.trim().is_empty() {
            return ActionResult::failed("the command is empty");
        }
        if command.len() > MAX_COMMAND_BYTES {
            return ActionResult::failed("the command is too long");
        }
        let mut process = self.shell_command(command);
        #[cfg(unix)]
        process.process_group(0);

        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => return ActionResult::failed(format!("cannot start a shell: {error}")),
        };
        let pid = child.id();
        let _in_flight = InFlight::start(&self.in_flight, pid);
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
        let code = status
            .as_ref()
            .and_then(std::process::ExitStatus::code)
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        let output = render_shell_output(
            &stdout,
            &stderr,
            &code,
            timed_out,
            stdout_lost || stderr_lost,
        );
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

    // A File take reads what the runtime can reach: the managed workspace, or
    // anything the real machine's user can read. One File is at most
    // `neha::file::MAX_FILE_BYTES`, the same limit the managed mode has always
    // applied; the tool action above keeps its own 8 MiB read cap.
    #[allow(clippy::manual_async_fn)]
    fn read_bytes(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ByteError>> + Send {
        let resolved = self.resolve_read(path);
        async move {
            let path = resolved.map_err(ByteError::Failed)?;
            let mut file = self.open_read(&path).map_err(ByteError::Failed)?;
            let metadata = file.metadata().map_err(|error| {
                ByteError::Failed(format!("cannot read {}: {error}", path.display()))
            })?;
            if !metadata.is_file() {
                return Err(ByteError::Failed(format!(
                    "{} is not a file",
                    path.display()
                )));
            }
            if metadata.len() > neha::file::MAX_FILE_BYTES {
                return Err(ByteError::Failed(format!(
                    "the file is larger than {} bytes",
                    neha::file::MAX_FILE_BYTES
                )));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                ByteError::Failed(format!("cannot read {}: {error}", path.display()))
            })?;

            Ok(bytes)
        }
    }

    // A File delivery writes where the runtime lets it write: the managed
    // workspace, or only the Bot's folder on a real machine. The limit is one
    // `neha::file::MAX_FILE_BYTES` File; the write tool above keeps its own
    // 64 KiB cap.
    #[allow(clippy::manual_async_fn)]
    fn write_bytes(
        &self,
        path: &str,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), ByteError>> + Send {
        let path = path.to_owned();
        async move {
            if bytes.len() as u64 > neha::file::MAX_FILE_BYTES {
                return Err(ByteError::Failed(format!(
                    "the content is larger than {} bytes",
                    neha::file::MAX_FILE_BYTES
                )));
            }
            self.write_to_root(&path, &bytes).map(|_shown| ())
        }
    }
}

/// Finds `bwrap` on the machine user's `PATH`.
fn find_bwrap() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

/// Starts one empty sandbox, so a Computer never serves a machine where
/// bubblewrap is installed but cannot run.
fn probe_bwrap(bwrap: &Path, folder: &Path, hidden: &[PathBuf]) -> Result<(), String> {
    let version = std::process::Command::new(bwrap)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("bwrap cannot run: {error}; install bubblewrap"))?;
    if !version.status.success() {
        return Err("bwrap --version failed; install a working bubblewrap".to_owned());
    }
    let output = std::process::Command::new(bwrap)
        .args(sandbox_args(folder, hidden))
        .arg("/bin/sh")
        .arg("-c")
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("bwrap cannot run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "bwrap cannot start a sandbox: {}",
        stderr.lines().next().unwrap_or("no error output")
    ))
}

/// The bubblewrap arguments that fence one real shell.
///
/// The machine is read-only, `/dev` is a minimal device set of the sandbox's
/// own (with its own `/dev/shm`), `/tmp` is fresh, the PID namespace is
/// private, the sandbox dies with its daemon, and the Bot's folder is the one
/// writable place and the working directory. Each hidden root gets a fresh
/// empty tmpfs, so the Computer's own directory and its credential are absent
/// from the sandbox. The network is not unshared: a real Computer uses its
/// machine's network as before.
fn sandbox_args(folder: &Path, hidden: &[PathBuf]) -> Vec<OsString> {
    let folder = folder.as_os_str();
    let mut arguments: Vec<OsString> = ["--ro-bind", "/", "/"]
        .into_iter()
        .map(OsString::from)
        .collect();
    // The tmpfs mounts come before /tmp's: a hidden root inside /tmp must be
    // mounted while the host's /tmp is still what the sandbox sees.
    for root in hidden {
        arguments.push(OsString::from("--tmpfs"));
        arguments.push(root.as_os_str().to_owned());
    }
    arguments.extend(
        [
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/dev/shm",
            "--unshare-pid",
            "--die-with-parent",
        ]
        .into_iter()
        .map(OsString::from),
    );
    arguments.push(OsString::from("--bind"));
    arguments.push(folder.to_owned());
    arguments.push(folder.to_owned());
    arguments.push(OsString::from("--chdir"));
    arguments.push(folder.to_owned());
    arguments.push(OsString::from("--"));

    arguments
}

/// Builds the fenced shell process without starting it.
fn sandbox_command(bwrap: &Path, folder: &Path, hidden: &[PathBuf]) -> Command {
    let mut process = Command::new(bwrap);
    process.args(sandbox_args(folder, hidden));
    process
}

/// Normalizes `.` and `..` without touching the file system; an absolute path
/// stops at `/`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

/// The message a real-mode read or write returns for a hidden path.
///
/// It names the path but never carries any of its bytes: a refused read must
/// not put the credential it protects into the model's context.
fn hidden_error(shown: impl std::fmt::Display) -> String {
    format!("the path is hidden from this Computer: {shown}")
}

/// Resolves the real path of an opened file or directory.
///
/// On Linux the descriptor itself names the file, so a path that changed
/// between resolution and opening cannot hide what was actually opened. A
/// machine without `/proc` cannot answer, and its reads are refused rather
/// than checked against a name.
#[cfg(target_os = "linux")]
fn opened_path(opened: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    std::fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd()))
}

/// Refuses to guess an opened file's real path on a system without `/proc`.
#[cfg(not(target_os = "linux"))]
fn opened_path(_opened: &std::fs::File) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this system does not name an opened file's path",
    ))
}

/// Drops the ` (deleted)` marker `/proc/self/fd` adds to an unlinked file, so
/// a root that still names the file's place is still recognized.
fn without_deleted_suffix(path: PathBuf) -> PathBuf {
    let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
        return path;
    };
    let Some(name) = name.strip_suffix(" (deleted)") else {
        return path;
    };

    path.with_file_name(name)
}

/// Writes bytes to a relative path inside `root` without following a symlink
/// or writing a hard link.
///
/// Every component is opened with `O_NOFOLLOW` relative to the descriptor of
/// its parent, so a shell that swaps in a symlink between two tool calls
/// cannot move the write out of the folder and there is no check-then-open
/// window. The target is opened without `O_TRUNC`, checked with `fstat` — a
/// file this Computer wrote itself has one link — and only then truncated and
/// written.
#[cfg(unix)]
fn write_machine_file(root: &Path, input: &str, bytes: &[u8]) -> Result<(), ByteError> {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;

    let (shown, mut components) = write_components(input).map_err(ByteError::Failed)?;
    let Some(last) = components.pop() else {
        return Err(ByteError::Failed(format!(
            "the path does not name a file: {shown}"
        )));
    };
    let mut directory = open_root(root)?;
    for name in components {
        directory = open_or_create_directory(&directory, &name, &shown)?;
    }
    let mut file = open_target_file(&directory, &last, &shown)?;
    let metadata = file
        .metadata()
        .map_err(|error| ByteError::Failed(format!("cannot write {shown}: {error}")))?;
    if metadata.nlink() > 1 {
        return Err(ByteError::Failed(format!(
            "the path is a hard link, which is not written through: {shown}"
        )));
    }
    file.set_len(0)
        .and_then(|()| file.write_all(bytes))
        .map_err(|error| ByteError::Failed(format!("cannot write {shown}: {error}")))
}

/// Writes bytes to a relative path inside a real machine's folder.
///
/// A real Computer only exists on Unix, where its folder can be opened as a
/// directory and stepped into; the constructor already refuses any other
/// platform.
#[cfg(not(unix))]
fn write_machine_file(_root: &Path, _input: &str, _bytes: &[u8]) -> Result<(), ByteError> {
    Err(ByteError::Failed(
        "a real Computer writes only on Unix".to_owned(),
    ))
}

/// Opens the Bot's folder itself as a directory, refusing a symlink there.
#[cfg(unix)]
fn open_root(root: &Path) -> Result<std::fs::File, ByteError> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::ffi::CString::new(root.as_os_str().as_bytes()).map_err(|_| {
        ByteError::Failed(format!(
            "the Bot's folder path contains a NUL byte: {}",
            root.display()
        ))
    })?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated; a successful return is a fresh
    // descriptor this function owns.
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(ByteError::Failed(format!(
            "cannot open the Bot's folder: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` is a fresh descriptor from `open` and is owned here.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Splits a real machine's write path into safe components.
///
/// A real Computer writes only through its folder, so an absolute path or a
/// `..` is refused before any descriptor is opened.
#[cfg(unix)]
fn write_components(input: &str) -> Result<(String, Vec<OsString>), String> {
    let shown = input.trim();
    if shown.is_empty() {
        return Err("the path is empty".to_owned());
    }
    let path = Path::new(shown);
    if path.is_absolute() {
        return Err(format!(
            "the path is absolute; a real Computer writes only inside the Bot's folder: {shown}"
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "the path uses `..`, which a real Computer does not write through: {shown}"
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!(
                    "the path is absolute; a real Computer writes only inside the Bot's folder: {shown}"
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(format!("the path does not name a file: {shown}"));
    }

    Ok((shown.to_owned(), components))
}

#[cfg(unix)]
fn c_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt as _;

    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| format!("the path contains a NUL byte: {}", name.to_string_lossy()))
}

/// Opens one subdirectory of `parent` without following a symlink.
#[cfg(unix)]
fn open_directory_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name = c_name(name)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `parent` is a live directory descriptor and `name` is
    // NUL-terminated; a successful return is a fresh descriptor this function
    // owns.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor from `openat` and is owned here.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Opens one subdirectory of `parent`, creating it when absent.
#[cfg(unix)]
fn open_or_create_directory(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    shown: &str,
) -> Result<std::fs::File, ByteError> {
    use std::os::fd::AsRawFd as _;

    match open_directory_at(parent, name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name_c = c_name(name).map_err(ByteError::Failed)?;
            // SAFETY: `parent` is a live directory descriptor and `name_c` is
            // NUL-terminated; `mkdirat` takes no ownership of it.
            let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o755) };
            if created != 0 {
                let error = std::io::Error::last_os_error();
                // Another writer may have created it between open and mkdir.
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(ByteError::Failed(format!(
                        "cannot create {} in the Bot's folder: {error}",
                        name.to_string_lossy()
                    )));
                }
            }
            open_directory_at(parent, name)
                .map_err(|error| directory_error(parent, name, shown, &error))
        }
        Err(error) => Err(directory_error(parent, name, shown, &error)),
    }
}

/// Returns whether one directory entry is a symlink, without following it.
#[cfg(unix)]
fn is_symlink_at(parent: &std::fs::File, name: &std::ffi::OsStr) -> bool {
    use std::os::fd::AsRawFd as _;

    let Ok(name) = c_name(name) else {
        return false;
    };
    // SAFETY: `parent` is a live directory descriptor, `name` is
    // NUL-terminated, and `stat` is a plain C structure owned here.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the pointers above outlive the call and name a live descriptor
    // and a NUL-terminated path.
    let found = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };

    found == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

#[cfg(unix)]
fn directory_error(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    shown: &str,
    error: &std::io::Error,
) -> ByteError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => ByteError::Failed(format!(
            "the path uses a symlink, which is not written through: {shown}"
        )),
        // A symlink opened with `O_DIRECTORY | O_NOFOLLOW` is not a directory,
        // and either error may name it; the message says symlink for both.
        Some(libc::ENOTDIR) if is_symlink_at(parent, name) => ByteError::Failed(format!(
            "the path uses a symlink, which is not written through: {shown}"
        )),
        Some(libc::ENOTDIR) => ByteError::Failed(format!(
            "cannot write {shown}: {} is not a directory",
            name.to_string_lossy()
        )),
        _ => ByteError::Failed(format!("cannot write {shown}: {error}")),
    }
}

/// Opens the file a write ends at, without following a symlink and without
/// truncating it yet.
#[cfg(unix)]
fn open_target_file(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    shown: &str,
) -> Result<std::fs::File, ByteError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name = c_name(name).map_err(ByteError::Failed)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `parent` is a live directory descriptor and `name` is
    // NUL-terminated; a successful return is a fresh descriptor owned here.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd >= 0 {
        // SAFETY: `fd` is a fresh descriptor from `openat` and is owned here.
        return Ok(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ELOOP) => Err(ByteError::Failed(format!(
            "the path uses a symlink, which is not written through: {shown}"
        ))),
        Some(libc::EISDIR) => Err(ByteError::Failed(format!(
            "the path is a directory, not a file: {shown}"
        ))),
        _ => Err(ByteError::Failed(format!("cannot write {shown}: {error}"))),
    }
}

/// Marks the shell whose process group is running until it is gone.
struct InFlight<'a> {
    slot: &'a AtomicU32,
    pid: u32,
}

impl<'a> InFlight<'a> {
    fn start(slot: &'a AtomicU32, pid: Option<u32>) -> Self {
        let pid = pid.unwrap_or(0);
        slot.store(pid, Ordering::SeqCst);
        Self { slot, pid }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // A cancel may have taken the slot first; only clear our own pid.
        let _ = self
            .slot
            .compare_exchange(self.pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Bounded {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
}

impl Bounded {
    fn full(&self) -> Vec<u8> {
        let mut bytes = self.head.clone();
        bytes.extend(self.tail.iter().copied());
        bytes
    }

    fn prefix(&self, length: usize) -> Vec<u8> {
        let length = length.min(self.total as usize).min(self.head.len());
        self.head[..length].to_vec()
    }

    fn suffix(&self, length: usize) -> Vec<u8> {
        let length = length.min(self.total as usize);
        if length == 0 {
            return Vec::new();
        }
        if length <= self.tail.len() {
            return self
                .tail
                .iter()
                .skip(self.tail.len() - length)
                .copied()
                .collect();
        }
        let from_head = length - self.tail.len();
        let mut bytes = self.head[self.head.len() - from_head..].to_vec();
        bytes.extend(self.tail.iter().copied());
        bytes
    }
}

fn render_shell_output(
    stdout: &Bounded,
    stderr: &Bounded,
    code: &str,
    timed_out: bool,
    drain_lost: bool,
) -> ToolOutput {
    let total = stdout.total.saturating_add(stderr.total);
    let mut text = String::new();
    if timed_out {
        text.push_str("the command ran past its time limit and was killed\n");
    }
    text.push_str(&format!("exit code: {code}\n"));
    if drain_lost {
        text.push_str("[the shell's output could not be drained]\n");
    }

    if total <= (HEAD_BYTES + TAIL_BYTES) as u64 {
        if stdout.total > 0 {
            text.push_str("stdout:\n");
            text.push_str(&String::from_utf8_lossy(&stdout.full()));
            text.push('\n');
        }
        if stderr.total > 0 {
            text.push_str("stderr:\n");
            text.push_str(&String::from_utf8_lossy(&stderr.full()));
            text.push('\n');
        }
        return ToolOutput {
            text,
            truncated: false,
            omitted_bytes: 0,
        };
    }

    // One shared budget: the first HEAD_BYTES of the output and then its
    // last TAIL_BYTES. The omitted count is the real number of bytes
    // between what is shown.
    let head_stdout = HEAD_BYTES.min(stdout.total as usize);
    let head_stderr = (HEAD_BYTES - head_stdout).min(stderr.total as usize);
    let tail_stderr = TAIL_BYTES.min(stderr.total as usize);
    let tail_stdout = (TAIL_BYTES - tail_stderr).min(stdout.total as usize);
    if head_stdout > 0 {
        text.push_str("stdout:\n");
        text.push_str(&String::from_utf8_lossy(&stdout.prefix(head_stdout)));
        text.push('\n');
    }
    if head_stderr > 0 {
        text.push_str("stderr:\n");
        text.push_str(&String::from_utf8_lossy(&stderr.prefix(head_stderr)));
        text.push('\n');
    }
    let omitted = total
        .saturating_sub(head_stdout as u64 + head_stderr as u64)
        .saturating_sub(tail_stdout as u64 + tail_stderr as u64);
    text.push_str(&format!("[... {omitted} bytes omitted ...]\n"));
    if tail_stdout > 0 {
        text.push_str("stdout tail:\n");
        text.push_str(&String::from_utf8_lossy(&stdout.suffix(tail_stdout)));
        text.push('\n');
    }
    if tail_stderr > 0 {
        text.push_str("stderr tail:\n");
        text.push_str(&String::from_utf8_lossy(&stderr.suffix(tail_stderr)));
        text.push('\n');
    }

    ToolOutput {
        text,
        truncated: true,
        omitted_bytes: omitted,
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

/// Reads the head and the tail of an opened file, which the caller already
/// checked against the hidden roots.
fn read_head_and_tail(
    file: &mut std::fs::File,
    length: u64,
    bytes: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>> {
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

/// The runtime cannot open its workspace or fence a real machine.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// A directory could not be created or read.
    #[error("workspace error: {0}")]
    Io(#[from] std::io::Error),
    /// Bubblewrap is missing or cannot start a sandbox on this machine, so a
    /// real Computer would run unfenced.
    #[error("the machine cannot be fenced: {0}")]
    NoSandbox(String),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    use neha::file::MAX_FILE_BYTES;

    use super::{DEFAULT_TIMEOUT, LocalRuntime, Mode};
    use crate::action::{Action, ActionStatus, ComputerRuntime};

    fn omitted_in_text(text: &str) -> u64 {
        let start = text.find("[... ").expect("an omission marker") + 5;
        let end = text[start..].find(" bytes omitted").unwrap() + start;
        text[start..end].parse().unwrap()
    }

    fn runtime() -> (tempfile::TempDir, LocalRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = LocalRuntime::new(directory.path().join("workspace")).unwrap();

        (directory, runtime)
    }

    /// Returns a real-machine runtime, or `None` when the bwrap tests are off.
    ///
    /// `NEHA_TEST_BWRAP` opens them. Once it is set, a missing or broken
    /// bubblewrap fails the test instead of skipping: the gate promises a
    /// working sandbox.
    fn machine_runtime() -> Option<(tempfile::TempDir, LocalRuntime)> {
        if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
            eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
            return None;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = LocalRuntime::machine(directory.path().join("bot"))
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

        Some((directory, runtime))
    }

    /// Returns a real-machine runtime that hides one directory outside `/tmp`,
    /// or `None` when the bwrap tests are off.
    ///
    /// `/var/tmp` stays visible in the sandbox, so a hidden root there proves
    /// the sandbox hides it; a root under the replaced `/tmp` would prove
    /// nothing.
    fn machine_runtime_with_hidden() -> Option<(tempfile::TempDir, LocalRuntime, std::path::PathBuf)>
    {
        if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
            eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
            return None;
        }
        let directory = outside_tempdir();
        let hidden = directory.path().join("computer");
        std::fs::create_dir(&hidden).unwrap();
        let runtime = LocalRuntime::machine_with_hidden(
            directory.path().join("bot"),
            vec![std::path::PathBuf::from(&hidden)],
        )
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

        Some((directory, runtime, hidden))
    }

    /// Builds a real-machine runtime for the file tools alone.
    ///
    /// The file tools never start a sandbox, so a test of their hidden-root
    /// rules does not need bubblewrap; the shell rules have their own gated
    /// test. Both paths are resolved, as the constructor resolves them.
    fn file_runtime(root: &std::path::Path, hidden: &std::path::Path) -> LocalRuntime {
        LocalRuntime {
            root: std::fs::canonicalize(root).unwrap(),
            mode: Mode::Machine {
                bwrap: std::path::PathBuf::from("bwrap"),
            },
            hidden: vec![std::fs::canonicalize(hidden).unwrap()],
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
        }
    }

    /// A temporary directory outside `/tmp`, which the sandbox replaces with a
    /// fresh tmpfs. A real shell test writes "outside" there.
    fn outside_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("neha-outside-")
            .tempdir_in("/var/tmp")
            .expect("the fence test needs a writable /var/tmp outside /tmp")
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
    async fn bytes_are_written_and_read_with_parent_directories() {
        let (_dir, runtime) = runtime();
        let bytes = vec![0_u8, 1, 2, 255];
        runtime
            .write_bytes("out/report.bin", bytes.clone())
            .await
            .unwrap();

        let read = runtime.read_bytes("out/report.bin").await.unwrap();
        assert_eq!(read, bytes);
        assert!(runtime.workspace().join("out").is_dir());

        // An absolute path inside the workspace names the same file.
        let absolute = runtime.workspace().join("out/report.bin");
        let read = runtime
            .read_bytes(&absolute.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(read, bytes);
    }

    #[tokio::test]
    async fn byte_transfers_stay_inside_the_workspace() {
        let (directory, runtime) = runtime();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        for path in ["../outside.txt", &outside.to_string_lossy()] {
            assert!(runtime.read_bytes(path).await.is_err(), "read {path}");
            assert!(
                runtime.write_bytes(path, b"pwned".to_vec()).await.is_err(),
                "write {path}"
            );
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn byte_transfers_refuse_a_symlink() {
        let (directory, runtime) = runtime();
        std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();
        let dangling = runtime.workspace().join("dangling");
        let created_outside = directory.path().join("created-outside.txt");
        std::os::unix::fs::symlink(&created_outside, &dangling).unwrap();

        for path in ["escape/outside.txt", "dangling"] {
            let error = runtime.read_bytes(path).await.unwrap_err().to_string();
            assert!(error.contains("symlink"), "{path}: {error}");
        }
        for path in ["escape/created-by-link.txt", "dangling"] {
            let error = runtime
                .write_bytes(path, b"x".to_vec())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("symlink"), "{path}: {error}");
        }
        assert!(!directory.path().join("created-by-link.txt").exists());
        assert!(!created_outside.exists());
    }

    #[tokio::test]
    async fn read_bytes_refuses_what_is_not_a_file() {
        let (_dir, runtime) = runtime();
        std::fs::create_dir(runtime.workspace().join("dir")).unwrap();

        let error = runtime.read_bytes("dir").await.unwrap_err().to_string();
        assert!(error.contains("not a file"), "{error}");
        let error = runtime
            .read_bytes("missing.txt")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot read"), "{error}");
    }

    #[tokio::test]
    async fn byte_transfers_refuse_more_than_the_file_limit() {
        let (_dir, runtime) = runtime();
        for (name, length) in [
            ("at-limit.bin", MAX_FILE_BYTES),
            ("over.bin", MAX_FILE_BYTES + 1),
        ] {
            std::fs::File::create(runtime.workspace().join(name))
                .unwrap()
                .set_len(length)
                .unwrap();
        }

        let error = runtime
            .read_bytes("over.bin")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("larger than"), "{error}");
        let error = runtime
            .write_bytes("over.bin", vec![0; MAX_FILE_BYTES as usize + 1])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("larger than"), "{error}");
        assert_eq!(
            std::fs::metadata(runtime.workspace().join("over.bin"))
                .unwrap()
                .len(),
            MAX_FILE_BYTES + 1,
            "a refused write leaves the file alone"
        );

        let at_limit = vec![0_u8; MAX_FILE_BYTES as usize];
        runtime
            .write_bytes("writable.bin", at_limit.clone())
            .await
            .unwrap();
        assert_eq!(runtime.read_bytes("at-limit.bin").await.unwrap(), at_limit);
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
        assert_eq!(result.output.omitted_bytes, 60_000 - 32_768);
        assert_eq!(
            omitted_in_text(&result.output.text),
            result.output.omitted_bytes
        );
        assert!(
            result.output.text.len() < 40_000,
            "the combined result is {} bytes",
            result.output.text.len()
        );
    }

    #[tokio::test]
    async fn a_result_slightly_over_the_budget_names_the_true_omitted_count() {
        let (_dir, runtime) = runtime();
        for (bytes, omitted) in [(33_000_u64, 232_u64), (40_000, 7_232)] {
            let result = runtime
                .execute(Action::Shell {
                    command: format!("head -c {bytes} /dev/zero | tr '\\0' 'a'"),
                })
                .await;

            assert!(result.output.truncated);
            assert_eq!(result.output.omitted_bytes, omitted, "{bytes} bytes");
            assert_eq!(
                omitted_in_text(&result.output.text),
                omitted,
                "the marker must name the true omitted count"
            );
        }
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
        assert_eq!(result.output.omitted_bytes, 9_000_003 - 32_768);
        assert_eq!(
            omitted_in_text(&result.output.text),
            result.output.omitted_bytes
        );
        assert!(result.output.text.contains("END"));
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

    #[tokio::test]
    async fn cancel_in_flight_kills_the_running_shell() {
        let (_dir, runtime) = runtime();
        let runtime = std::sync::Arc::new(runtime.with_timeout(Duration::from_secs(30)));
        assert!(!runtime.cancel_in_flight(), "nothing runs yet");

        let running = tokio::spawn({
            let runtime = std::sync::Arc::clone(&runtime);
            async move {
                runtime
                    .execute(Action::Shell {
                        command: "sleep 30".to_owned(),
                    })
                    .await
            }
        });
        // The shell needs a moment to exist before it can be cancelled.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(runtime.cancel_in_flight(), "the running shell is cancelled");

        let started = std::time::Instant::now();
        let result = running.await.unwrap();
        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = runtime.cancel_in_flight();
    }

    #[cfg(unix)]
    #[test]
    fn a_machine_without_a_working_bubblewrap_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        use super::RuntimeError;

        let directory = tempfile::tempdir().unwrap();
        let error =
            LocalRuntime::machine_with_bwrap(directory.path().join("bot"), None, Vec::new())
                .err()
                .expect("a missing bwrap must refuse the machine");
        assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");

        let fake = directory.path().join("bwrap");
        std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error =
            LocalRuntime::machine_with_bwrap(directory.path().join("bot2"), Some(fake), Vec::new())
                .err()
                .expect("a bwrap that cannot start a sandbox must refuse the machine");
        assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");
    }

    #[tokio::test]
    async fn a_machine_tool_writes_and_reads_inside_the_bot_folder() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let folder = runtime.workspace().to_path_buf();
        let written = runtime
            .execute(Action::WriteFile {
                path: "notes/hello.txt".to_owned(),
                content: "hello neha".to_owned(),
            })
            .await;

        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
            "hello neha"
        );

        // Overwriting truncates: a shorter write replaces the whole file.
        let written = runtime
            .execute(Action::WriteFile {
                path: "notes/hello.txt".to_owned(),
                content: "hi".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
            "hi"
        );

        // `.` is allowed and creates the parent directories on the way.
        let written = runtime
            .execute(Action::WriteFile {
                path: "./top.txt".to_owned(),
                content: "top".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(folder.join("top.txt")).unwrap(),
            "top"
        );

        // An absolute path reads the machine; a real Computer never writes one.
        let absolute = folder.join("notes/hello.txt");
        let read = runtime
            .execute(Action::ReadFile {
                path: absolute.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "hi");
        let written = runtime
            .execute(Action::WriteFile {
                path: absolute.to_string_lossy().into_owned(),
                content: "absolute".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Failed, "{written:?}");
        assert!(
            written.output.text.contains("absolute"),
            "{}",
            written.output.text
        );

        let listed = runtime
            .execute(Action::ListFiles {
                path: ".".to_owned(),
            })
            .await;
        assert!(
            listed.output.text.contains("notes"),
            "{}",
            listed.output.text
        );
        assert!(
            listed.output.text.contains("top.txt"),
            "{}",
            listed.output.text
        );
    }

    #[tokio::test]
    async fn a_machine_tool_refuses_to_write_outside_the_bot_folder() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let outside = directory.path().join("outside.txt");
        for path in ["../outside.txt", "nested/../../outside.txt"] {
            let result = runtime
                .execute(Action::WriteFile {
                    path: path.to_owned(),
                    content: "pwned".to_owned(),
                })
                .await;
            assert_eq!(result.status, ActionStatus::Failed, "{path}");
            assert!(
                result.output.text.contains("`..`"),
                "{path}: {}",
                result.output.text
            );
        }
        let result = runtime
            .execute(Action::WriteFile {
                path: outside.to_string_lossy().into_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed);
        assert!(
            result.output.text.contains("absolute"),
            "{}",
            result.output.text
        );
        assert!(!outside.exists(), "a refused write leaves no file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_tool_refuses_to_write_through_a_symlink() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();

        let result = runtime
            .execute(Action::WriteFile {
                path: "escape/created.txt".to_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed);
        assert!(
            result.output.text.contains("symlink"),
            "{}",
            result.output.text
        );
        assert!(!directory.path().join("created.txt").exists());

        // A symlink as the target is never written through, dangling or not.
        let dangling = runtime.workspace().join("dangling");
        std::os::unix::fs::symlink(runtime.workspace().join("created-by-link.txt"), &dangling)
            .unwrap();
        let result = runtime
            .execute(Action::WriteFile {
                path: "dangling".to_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed);
        assert!(
            result.output.text.contains("symlink"),
            "{}",
            result.output.text
        );
        assert!(!runtime.workspace().join("created-by-link.txt").exists());

        let error = runtime
            .write_bytes("escape/created.bin", b"pwned".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink"), "{error}");
        assert!(!directory.path().join("created.bin").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_tool_refuses_to_write_a_hard_link() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "host content").unwrap();
        let linked = runtime.workspace().join("linked.txt");
        std::fs::hard_link(&outside, &linked).unwrap();

        let result = runtime
            .execute(Action::WriteFile {
                path: "linked.txt".to_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed);
        assert!(
            result.output.text.contains("hard link"),
            "{}",
            result.output.text
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "host content",
            "the host inode was written"
        );

        let error = runtime
            .write_bytes("linked.txt", b"pwned".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("hard link"), "{error}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host content");
        // The link is still there: a refusal changes nothing.
        assert_eq!(std::fs::metadata(&linked).unwrap().len(), 12);
    }

    #[tokio::test]
    async fn a_machine_tool_reads_the_whole_machine() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "readable").unwrap();
        let read = runtime
            .execute(Action::ReadFile {
                path: outside.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "readable");

        let read = runtime
            .execute(Action::ReadFile {
                path: "/etc/hostname".to_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert!(!read.output.text.trim().is_empty());

        let missing = runtime
            .execute(Action::ReadFile {
                path: directory
                    .path()
                    .join("missing.txt")
                    .to_string_lossy()
                    .into_owned(),
            })
            .await;
        assert_eq!(missing.status, ActionStatus::Failed);
        assert!(
            missing.output.text.contains("cannot read"),
            "{}",
            missing.output.text
        );
    }

    #[tokio::test]
    async fn a_machine_byte_transfer_reads_anywhere_and_writes_inside_the_folder() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let folder = runtime.workspace().to_path_buf();
        let outside = directory.path().join("outside.bin");
        std::fs::write(&outside, b"take me").unwrap();

        let read = runtime
            .read_bytes(&outside.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(read, b"take me");

        runtime
            .write_bytes("deliver/report.bin", vec![0, 1, 2])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(folder.join("deliver/report.bin")).unwrap(),
            vec![0, 1, 2]
        );

        let error = runtime
            .write_bytes("../pwned.bin", b"x".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("`..`"), "{error}");
        assert!(!directory.path().join("pwned.bin").exists());
        let error = runtime
            .write_bytes(&outside.to_string_lossy(), b"x".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("absolute"), "{error}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"take me");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_file_tool_refuses_a_hidden_root() {
        let directory = tempfile::tempdir().unwrap();
        let hidden = directory.path().join("computer");
        std::fs::create_dir(&hidden).unwrap();
        let credential = hidden.join("computer.json");
        std::fs::write(&credential, "channel-secret-must-not-leak").unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let visible = directory.path().join("visible.txt");
        std::fs::write(&visible, "readable").unwrap();
        let runtime = file_runtime(&root, &hidden);

        // A hidden root is refused by both read paths, and its bytes never
        // come back in the refusal.
        let read = runtime
            .execute(Action::ReadFile {
                path: credential.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
        assert!(
            read.output
                .text
                .contains("the path is hidden from this Computer"),
            "{}",
            read.output.text
        );
        assert!(!read.output.text.contains("channel-secret-must-not-leak"));
        let error = runtime
            .read_bytes(&credential.to_string_lossy())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the path is hidden from this Computer"),
            "{error}"
        );
        assert!(!error.contains("channel-secret-must-not-leak"));
        let listed = runtime
            .execute(Action::ListFiles {
                path: hidden.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(listed.status, ActionStatus::Failed, "{listed:?}");
        assert!(
            listed
                .output
                .text
                .contains("the path is hidden from this Computer"),
            "{}",
            listed.output.text
        );
        assert!(!listed.output.text.contains("computer.json"));

        // A normal machine file next to the hidden root is still readable.
        let read = runtime
            .execute(Action::ReadFile {
                path: visible.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "readable");

        // A symlink in the Bot's folder is caught after the open: the check
        // follows the opened descriptor, not the path the caller typed.
        std::os::unix::fs::symlink(&hidden, root.join("leak")).unwrap();
        let read = runtime
            .execute(Action::ReadFile {
                path: "leak/computer.json".to_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
        assert!(
            read.output
                .text
                .contains("the path is hidden from this Computer"),
            "{}",
            read.output.text
        );
        assert!(!read.output.text.contains("channel-secret-must-not-leak"));
    }

    #[tokio::test]
    async fn a_machine_write_refuses_a_hidden_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        // A write can only reach a hidden root through a misconfigured folder;
        // that is what the daemon refuses, and the write refuses it as well.
        let runtime = file_runtime(&root, &root);

        let written = runtime
            .execute(Action::WriteFile {
                path: "notes.txt".to_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Failed, "{written:?}");
        assert!(
            written
                .output
                .text
                .contains("the path is hidden from this Computer"),
            "{}",
            written.output.text
        );
        assert!(!root.join("notes.txt").exists());
        let error = runtime
            .write_bytes("notes.bin", b"pwned".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the path is hidden from this Computer"),
            "{error}"
        );
        assert!(!root.join("notes.bin").exists());
    }

    #[tokio::test]
    async fn a_real_shell_cannot_read_a_hidden_root() {
        let Some((directory, runtime, hidden)) = machine_runtime_with_hidden() else {
            return;
        };
        let credential = hidden.join("computer.json");
        std::fs::write(&credential, "channel-secret-must-not-leak").unwrap();
        let visible = directory.path().join("visible.txt");
        std::fs::write(&visible, "readable").unwrap();

        let hidden_read = runtime
            .execute(Action::Shell {
                command: format!("cat {}", credential.display()),
            })
            .await;
        assert_eq!(hidden_read.status, ActionStatus::Failed, "{hidden_read:?}");
        assert!(
            !hidden_read
                .output
                .text
                .contains("channel-secret-must-not-leak"),
            "the sandbox read the credential: {}",
            hidden_read.output.text
        );

        // The shell still reads the machine outside the hidden root.
        let visible_read = runtime
            .execute(Action::Shell {
                command: format!("cat {}", visible.display()),
            })
            .await;
        assert_eq!(
            visible_read.status,
            ActionStatus::Succeeded,
            "{visible_read:?}"
        );
        assert!(visible_read.output.text.contains("readable"));
    }

    #[tokio::test]
    async fn a_real_shell_starts_in_the_bot_folder() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let result = runtime
            .execute(Action::Shell {
                command: "pwd".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(
            result
                .output
                .text
                .contains(&runtime.workspace().to_string_lossy().into_owned()),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_real_shell_writes_inside_the_bot_folder() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let folder = runtime.workspace().to_path_buf();
        let result = runtime
            .execute(Action::Shell {
                command: "printf hi > inside.txt && printf hi > \"$PWD/absolute.txt\"".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(folder.join("inside.txt")).unwrap(),
            "hi"
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("absolute.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn a_real_shell_cannot_write_outside_the_bot_folder() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let outside = outside_tempdir();
        let sink = outside.path().join("pwned.txt");
        let result = runtime
            .execute(Action::Shell {
                command: format!("printf hi > {}", sink.display()),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(!sink.exists(), "the sandbox wrote outside the folder");
    }

    #[tokio::test]
    async fn a_real_shell_reads_the_whole_machine() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let result = runtime
            .execute(Action::Shell {
                command: "cat /etc/hostname".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(!result.output.text.trim().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_cannot_escape_through_a_symlink() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let outside = outside_tempdir();
        let sink = outside.path().join("pwned.txt");
        std::os::unix::fs::symlink(outside.path(), runtime.workspace().join("escape")).unwrap();
        let result = runtime
            .execute(Action::Shell {
                command: "printf hi > escape/pwned.txt".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(!sink.exists(), "the sandbox escaped through a symlink");
    }

    #[tokio::test]
    async fn a_real_shell_has_its_own_dev_shm() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        if !std::path::Path::new("/dev/shm").is_dir() {
            eprintln!("skipping: this machine has no /dev/shm");
            return;
        }
        let id = std::process::id();
        let host = format!("/dev/shm/neha-host-{id}");
        let sandbox = format!("/dev/shm/neha-sandbox-{id}");
        std::fs::write(&host, "host bytes").unwrap();
        let result = runtime
            .execute(Action::Shell {
                command: format!("cat {host} 2>/dev/null; printf sandbox > {sandbox}"),
            })
            .await;
        let _ = std::fs::remove_file(&host);

        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(
            !result.output.text.contains("host bytes"),
            "the host /dev/shm is visible in the sandbox: {}",
            result.output.text
        );
        assert!(
            !std::path::Path::new(&sandbox).exists(),
            "the sandbox wrote the host's /dev/shm"
        );
    }

    #[tokio::test]
    async fn a_real_shell_has_its_own_pid_namespace() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let host = std::fs::read_link("/proc/self/ns/pid").expect("the host has a pid namespace");
        let result = runtime
            .execute(Action::Shell {
                command: "readlink /proc/self/ns/pid".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(
            !result
                .output
                .text
                .contains(&host.to_string_lossy().into_owned()),
            "the sandbox shares the host's PID namespace: {}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_real_shell_cannot_signal_a_host_process() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let mut host = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("the host spawns a sleep");
        let pid = host.id();
        let result = runtime
            .execute(Action::Shell {
                command: format!("kill -0 {pid}"),
            })
            .await;
        let _ = host.kill();
        let _ = host.wait();

        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(
            result.output.text.contains("No such process")
                || result.output.text.contains("not permitted"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_real_shell_that_runs_too_long_is_killed() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let runtime = runtime.with_timeout(Duration::from_millis(500));
        let started = std::time::Instant::now();
        let result = runtime
            .execute(Action::Shell {
                command: "sleep 30".to_owned(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::TimedOut, "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
