//! The in-process Computer runtime.
//!
//! It executes real POSIX shell commands and real file operations. In managed
//! mode everything stays inside one workspace directory: not isolated, not
//! transported, and only for a Space on the same machine. In real mode it is
//! one Bot's view of a real machine: reads reach what the machine user can
//! read, writes reach the Bot's folder, the owner's grants, and the run's own
//! directory, and the shell runs under bubblewrap. Hidden roots are the
//! exception to both reads and writes: the Computer's own directory, which
//! holds its channel credential, is passed that way so a Bot never reaches it.
//!
//! The owner can instead give a Bot full access, the `danger-full-access`
//! model: the shell runs directly as the machine user with no sandbox at all,
//! and the file tools read and write with the machine user's own rights. The
//! hidden roots and the write fence do not apply then; only the approvals a
//! Space asks for still stand.

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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

/// Returns the directory holding this build's `neha` binary, when known.
fn default_cli_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// A Computer runtime backed by one directory.
pub struct LocalRuntime {
    /// The only root writes may reach: the managed workspace, or the Bot's
    /// folder on a real machine.
    root: PathBuf,
    mode: Mode,
    /// The directories a real Bot must not read or write: the Computer's own
    /// configuration directory, which holds its channel credential. Resolved,
    /// so an opened file's real path can be compared against them. They only
    /// take part in the granted mode's sandbox mounts; a full-access runtime
    /// ignores them, because the owner gave the Bot the machine user's rights.
    hidden: Vec<PathBuf>,
    /// Paths the file tools must refuse even when they do not exist yet: a
    /// credential or ticket file the daemon created after the runtime started.
    /// An entry matches itself and anything below it.
    denied: Vec<PathBuf>,
    timeout: Duration,
    /// The process group id of the shell that is running, or 0.
    ///
    /// A channel that goes away must kill the work it asked for, and it cannot
    /// see the child: this slot is the hook. [`cancel_in_flight`] reads it.
    ///
    /// [`cancel_in_flight`]: LocalRuntime::cancel_in_flight
    in_flight: AtomicU32,
    /// Whether the owner gave this Bot the daemon's own rights. A full-access
    /// runtime runs the shell with no sandbox and lets the file tools use the
    /// machine user's own rights, so the write fence and the hidden roots do
    /// not apply.
    full_access: bool,
    /// The folders writes may reach besides `root`: the owner's grants and,
    /// without full access, the run's own directory. Each is opened as a
    /// directory descriptor so a later path swap cannot redirect a write, and
    /// no root sits inside `root`. They are candidates, not all mounted: the
    /// shell mounts only the roots that pass its own scan, and an inner root
    /// stays a candidate because an outer root the scan refuses must not take
    /// it down too.
    write_roots: Vec<(PathBuf, std::fs::File)>,
    /// The directory a run works in. It is where relative paths resolve and
    /// where the shell starts; without full access it is also one of the write
    /// roots.
    cwd: Option<PathBuf>,
    /// Environment entries a shell command sees, on top of the fixed PATH,
    /// HOME, and TMPDIR. A Space puts its short-lived Bot token here. The
    /// daemon reuses one runtime across runs, so its actions update this slot
    /// before they run instead of holding one token forever.
    env: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// The directory holding this build's `neha` binary. A Bot's shell finds
    /// the CLI by name, so the skill's commands run where a `neha` install
    /// is not on the fixed system PATH.
    cli_dir: Option<PathBuf>,
}

/// Which kind of machine a runtime drives.
enum Mode {
    /// A managed workspace: the root is also everything a read may reach.
    Managed,
    /// A real machine, fenced by bubblewrap.
    Machine {
        /// The resolved `bwrap` binary a fenced shell starts. A full-access
        /// runtime opens no sandbox, so it has none.
        bwrap: Option<PathBuf>,
        /// The Bot's folder, opened without following a symlink and checked
        /// once. Every write and every listing starts from this descriptor, so
        /// a swap on disk cannot move the root between calls.
        folder: std::fs::File,
    },
}

/// How many entries one Bot folder scan may visit.
const MAX_SCAN_ENTRIES: usize = 10_000;
/// How many directories below the Bot folder one scan may descend.
const MAX_SCAN_DEPTH: usize = 8;

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
            denied: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
            full_access: false,
            write_roots: Vec::new(),
            cwd: None,
            env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            cli_dir: default_cli_dir(),
        })
    }

    /// Opens one Bot's folder on a real machine.
    ///
    /// The folder is opened without following a symlink and is scanned once
    /// for links and special files. See
    /// [`machine_at`](LocalRuntime::machine_at) for the form a daemon uses.
    pub fn machine(folder: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        Self::machine_with_bwrap(folder.into(), find_bwrap(), Vec::new())
    }

    /// Opens one Bot's folder on a real machine, with directories it must not
    /// reach.
    ///
    /// A hidden root is invisible to both of this Bot's read paths: the file
    /// tools resolve the opened descriptor's real path, and the shell gets a
    /// fresh tmpfs mounted over each root. A write never leaves the Bot's
    /// folder, and a target that lands under a hidden root is refused before
    /// it is opened.
    pub fn machine_with_hidden(
        folder: impl Into<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        Self::machine_with_bwrap(folder.into(), find_bwrap(), hidden)
    }

    /// Opens one Bot's folder under a workspace root the caller opened.
    ///
    /// `workspace` is a directory descriptor (a daemon opens it with
    /// `O_DIRECTORY | O_NOFOLLOW`), and `bot` is one path component under it.
    /// The folder is created when absent and opened with `O_DIRECTORY |
    /// O_NOFOLLOW`; every symlink on the way is refused. The folder is then
    /// scanned once, and refused when it holds a hard link, a symlink, or a
    /// special file.
    pub fn machine_at(
        workspace: &std::fs::File,
        bot: &str,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        Self::machine_at_with(workspace, OsStr::new(bot), find_bwrap(), hidden)
    }

    /// Opens one Bot's folder on a real machine with full access.
    ///
    /// The owner gave the Bot the machine user's own rights, so this path
    /// probes no sandbox and scans nothing: the folder is opened without
    /// following a symlink and created when absent, and a Bot's own symlink,
    /// hard link, or special file is not this runtime's business. A machine
    /// without bubblewrap can still run a full-access Bot, and a Bot that made
    /// itself a symlink is still served after a restart.
    pub fn full_access(workspace: &std::fs::File, bot: &str) -> Result<Self, RuntimeError> {
        Self::full_access_at(workspace, OsStr::new(bot))
    }

    /// Opens one Bot's folder with full access under a verified workspace root
    /// descriptor.
    #[cfg(unix)]
    fn full_access_at(workspace: &std::fs::File, bot: &OsStr) -> Result<Self, RuntimeError> {
        if !is_one_component(bot) {
            return Err(RuntimeError::UnsafeFolder(format!(
                "{} is not one folder name; clean it and start again (the default is ~/Neha/<bot-id>)",
                bot.to_string_lossy()
            )));
        }
        let workspace_path = opened_path(workspace).map_err(RuntimeError::Io)?;
        let folder_path = workspace_path.join(bot);
        let folder = open_or_create_machine_folder(workspace, bot, &folder_path)?;

        Ok(Self {
            root: folder_path,
            mode: Mode::Machine {
                bwrap: None,
                folder,
            },
            hidden: Vec::new(),
            denied: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
            full_access: true,
            write_roots: Vec::new(),
            cwd: None,
            env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            cli_dir: default_cli_dir(),
        })
    }

    /// A real Computer runs only on Unix, where a folder is a descriptor.
    #[cfg(not(unix))]
    fn full_access_at(_workspace: &std::fs::File, _bot: &OsStr) -> Result<Self, RuntimeError> {
        Err(RuntimeError::NoSandbox(
            "a real Computer runs only on Unix".to_owned(),
        ))
    }

    /// Opens a machine's workspace root without following a symlink, creating
    /// it when absent.
    pub fn open_workspace(workspace: impl AsRef<Path>) -> Result<std::fs::File, RuntimeError> {
        let workspace = workspace.as_ref();
        std::fs::create_dir_all(workspace)?;
        open_directory_path(workspace).map_err(RuntimeError::Io)
    }

    /// Checks that this machine can start a fenced shell.
    ///
    /// A daemon calls this before it serves, so a machine without bubblewrap
    /// never accepts a Computer.
    pub fn probe_sandbox() -> Result<(), RuntimeError> {
        let bwrap = find_bwrap().ok_or_else(no_sandbox)?;
        probe_bwrap(&bwrap, Path::new("/"), &[]).map_err(RuntimeError::NoSandbox)
    }

    /// Opens a real-machine runtime with a specific `bwrap`, for tests.
    fn machine_with_bwrap(
        folder: impl Into<PathBuf>,
        bwrap: Option<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let folder = folder.into();
        let parent = folder
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = folder.file_name().ok_or_else(|| {
            RuntimeError::UnsafeFolder(format!(
                "{} does not name one folder; clean it and start again (the default is ~/Neha/<bot-id>)",
                folder.display()
            ))
        })?;
        // The configured parent may be a new path; its final component is
        // still opened without following a symlink.
        std::fs::create_dir_all(parent)?;
        let parent = open_directory_path(parent).map_err(RuntimeError::Io)?;

        Self::machine_at_with(&parent, name, bwrap, hidden)
    }

    /// Opens a real-machine runtime under a verified workspace root descriptor.
    #[cfg(unix)]
    fn machine_at_with(
        workspace: &std::fs::File,
        bot: &OsStr,
        bwrap: Option<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        if !is_one_component(bot) {
            return Err(RuntimeError::UnsafeFolder(format!(
                "{} is not one folder name; clean it and start again (the default is ~/Neha/<bot-id>)",
                bot.to_string_lossy()
            )));
        }
        let workspace_path = opened_path(workspace).map_err(RuntimeError::Io)?;
        let folder_path = workspace_path.join(bot);
        let folder = open_or_create_machine_folder(workspace, bot, &folder_path)?;
        // A hidden root must exist: it is the directory the daemon wrote its
        // own configuration into, and a missing one would mean the tmpfs that
        // hides it cannot be mounted.
        let hidden = hidden
            .into_iter()
            .map(|root| std::fs::canonicalize(&root).map_err(RuntimeError::Io))
            .collect::<Result<Vec<_>, _>>()?;
        for root in &hidden {
            let metadata = std::fs::metadata(root).map_err(RuntimeError::Io)?;
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(RuntimeError::NoSandbox(format!(
                    "the hidden path {} is not a regular file or a directory",
                    root.display()
                )));
            }
            if root.starts_with(&folder_path) {
                return Err(unsafe_folder(
                    &folder_path,
                    format!("it contains the configuration directory {}", root.display()),
                ));
            }
            if folder_path.starts_with(root) {
                return Err(unsafe_folder(
                    &folder_path,
                    format!("it sits inside the hidden directory {}", root.display()),
                ));
            }
        }
        scan_bot_folder(&folder, &folder_path)?;
        let bwrap = bwrap.ok_or_else(no_sandbox)?;
        probe_bwrap(&bwrap, &folder_path, &hidden).map_err(RuntimeError::NoSandbox)?;

        Ok(Self {
            root: folder_path,
            mode: Mode::Machine {
                bwrap: Some(bwrap),
                folder,
            },
            hidden,
            denied: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
            full_access: false,
            write_roots: Vec::new(),
            cwd: None,
            env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            cli_dir: default_cli_dir(),
        })
    }

    /// Opens a real-machine runtime under a verified workspace root descriptor.
    ///
    /// A real Computer runs only on Unix, where a folder is a descriptor and
    /// bubblewrap fences its shell; every other platform refuses it.
    #[cfg(not(unix))]
    fn machine_at_with(
        _workspace: &std::fs::File,
        _bot: &OsStr,
        _bwrap: Option<PathBuf>,
        _hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        Err(RuntimeError::NoSandbox(
            "a real Computer runs only on Unix".to_owned(),
        ))
    }

    /// Marks paths the file tools must refuse even when they do not exist yet.
    ///
    /// The daemon passes its configuration, session credential, and ticket
    /// locations here: they are hidden from the sandbox by directory mounts,
    /// and the file tools read with the machine user's rights, so they must
    /// refuse the paths themselves even when a file appears later.
    #[must_use]
    pub fn with_denied(mut self, denied: Vec<PathBuf>) -> Self {
        self.denied = denied;
        self
    }

    /// Sets the environment every shell command sees.
    #[must_use]
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = Arc::new(std::sync::Mutex::new(env));
        self
    }

    /// Replaces the environment for the next action on this runtime.
    pub fn set_env(&self, env: Vec<(String, String)>) {
        if let Ok(mut slot) = self.env.lock() {
            *slot = env;
        }
    }

    /// Applies the owner's access choice and this run's directory.
    ///
    /// Returns a runtime over this machine and Bot folder with one request's
    /// access. A daemon caches a runtime per Bot and calls this for every
    /// request: the grants and the run's directory are opened for that request
    /// alone, so a directory archived and rebuilt between two runs is reopened
    /// instead of being served from an older descriptor.
    ///
    /// Grant roots and the run's directory are opened now, so a later rename
    /// cannot point a write somewhere else, and every later write and shell
    /// bind uses the opened descriptor.
    ///
    /// Full access is the `danger-full-access` model: the runtime takes the
    /// machine user's own rights, so it keeps no write roots and ignores the
    /// hidden roots. Without it, `cwd` is this run's one-off write root: the
    /// shell binds it read-write and the file tools write through its
    /// descriptor. It is never persisted as a grant.
    ///
    /// A root inside the Bot's folder is dropped, because the folder covers
    /// it. Every other opened root stays a candidate for the file tools; the
    /// shell mounts only the candidates its scan accepts, and an outer accepted
    /// root covers an inner one.
    ///
    /// A grant that cannot be opened is skipped: the Space checked it when the
    /// owner added it. A run directory that cannot be opened, or one that is
    /// hidden, is an error: the run named a directory it cannot use.
    pub fn with_machine_access(
        &self,
        full_access: bool,
        grants: &[String],
        cwd: Option<&str>,
    ) -> Result<Self, RuntimeError> {
        let mut runtime = self.reopened()?;
        runtime.apply_machine_access(full_access, grants, cwd)?;
        Ok(runtime)
    }

    /// Copies this runtime's opened machine and Bot folder without an access
    /// decision: the copy starts with no grants and no run directory.
    fn reopened(&self) -> Result<Self, RuntimeError> {
        let mode = match &self.mode {
            Mode::Managed => Mode::Managed,
            Mode::Machine { bwrap, folder } => Mode::Machine {
                bwrap: bwrap.clone(),
                folder: folder.try_clone().map_err(RuntimeError::Io)?,
            },
        };
        let env = self.env.lock().map(|env| env.clone()).unwrap_or_default();

        Ok(Self {
            root: self.root.clone(),
            mode,
            hidden: self.hidden.clone(),
            denied: self.denied.clone(),
            timeout: self.timeout,
            in_flight: AtomicU32::new(0),
            full_access: false,
            write_roots: Vec::new(),
            cwd: None,
            env: Arc::new(std::sync::Mutex::new(env)),
            cli_dir: self.cli_dir.clone(),
        })
    }

    /// Opens one request's grants and run directory on a copied runtime.
    fn apply_machine_access(
        &mut self,
        full_access: bool,
        grants: &[String],
        cwd: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let cwd = match cwd {
            Some(directory) => Some(self.open_run_directory(directory, full_access)?),
            None => None,
        };
        if full_access {
            self.full_access = true;
            self.write_roots.clear();
            self.cwd = cwd.map(|(path, _file)| path);
            return Ok(());
        }

        let mut roots = Vec::new();
        for grant in grants {
            // Open first, then judge the descriptor: a path replaced between a
            // check and the open could otherwise point the grant somewhere
            // else. The open never blocks and requires a directory, so a path
            // swapped or turned into a FIFO after the Space's check cannot
            // stall the daemon here.
            let Ok((root, file)) = open_write_root(grant.trim()) else {
                continue;
            };
            if self.is_hidden(&root) || self.is_denied(&root) {
                return Err(RuntimeError::Access(format!(
                    "the granted folder is hidden from this Computer: {}",
                    root.display()
                )));
            }
            // A grant that contains a hidden root would re-expose it after the
            // hidden tmpfs is mounted.
            if self.hidden.iter().any(|hidden| hidden.starts_with(&root)) {
                return Err(RuntimeError::Access(format!(
                    "the granted folder contains a hidden path: {}",
                    root.display()
                )));
            }
            roots.push((root, file));
        }
        if let Some((path, file)) = cwd {
            self.cwd = Some(path.clone());
            if !path.starts_with(&self.root) {
                roots.push((path, file));
            }
        }
        // The Bot's folder is already a write root; a root inside it adds
        // nothing. The rest stay as candidates because the shell decides which
        // of them it can mount after its own scan.
        roots.retain(|(root, _)| !root.starts_with(&self.root));
        self.write_roots = roots;
        Ok(())
    }

    /// Opens the directory a run works in.
    ///
    /// The descriptor names the directory, so a rename after the open cannot
    /// point the run somewhere else. The directory must already exist. Without
    /// full access a hidden directory is refused, and so is one that contains a
    /// hidden path: binding it read-write would re-expose that path after the
    /// hidden tmpfs is mounted.
    fn open_run_directory(
        &self,
        directory: &str,
        full_access: bool,
    ) -> Result<(PathBuf, std::fs::File), RuntimeError> {
        let shown = directory.trim();
        let (path, file) = open_write_root(shown).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotADirectory {
                RuntimeError::Access(format!("the working directory is not a folder: {shown}"))
            } else {
                RuntimeError::Io(error)
            }
        })?;
        if full_access {
            return Ok((path, file));
        }
        if self.is_hidden(&path) || self.is_denied(&path) {
            return Err(RuntimeError::Access(format!(
                "the folder is hidden from this Computer: {}",
                path.display()
            )));
        }
        if self.hidden.iter().any(|hidden| hidden.starts_with(&path)) {
            return Err(RuntimeError::Access(format!(
                "the working directory contains a hidden path: {}",
                path.display()
            )));
        }
        Ok((path, file))
    }

    /// Kills the running shell and every process below it, if one is.
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
        kill_tree(Some(pid));
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
    /// The directory relative paths resolve against.
    fn base(&self) -> &Path {
        self.cwd.as_deref().unwrap_or(&self.root)
    }

    fn requested(&self, input: &str) -> Result<PathBuf, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("the path is empty".to_owned());
        }
        let requested = Path::new(input);
        Ok(if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.base().join(requested)
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
            Mode::Machine { folder, .. } => {
                let shown = input.trim();
                let path = Path::new(shown);
                if !path.is_absolute()
                    && path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(ByteError::Failed(format!(
                        "the path uses `..`, which a real Computer does not write through: {shown}"
                    )));
                }
                let target = if path.is_absolute() {
                    normalize_path(path)
                } else {
                    normalize_path(&self.base().join(path))
                };
                if !self.full_access && (self.is_hidden(&target) || self.is_denied(&target)) {
                    return Err(ByteError::Failed(hidden_error(shown)));
                }
                if self.full_access {
                    // Full access writes with the machine user's own rights, so
                    // the machine's root is the write root. The write still goes
                    // through an opened descriptor and no-follow components, so
                    // a path is never re-resolved as a string. A relative path
                    // was resolved against the run's directory above.
                    let relative = target
                        .strip_prefix("/")
                        .map_err(|_| ByteError::Failed("the path is not absolute".to_owned()))?;
                    let root = std::fs::File::open("/").map_err(|error| {
                        ByteError::Failed(format!("cannot open the machine root: {error}"))
                    })?;
                    write_machine_file(&root, &relative.to_string_lossy(), bytes)?;
                    return Ok(shown.to_owned());
                }
                let written = if let Ok(relative) = target.strip_prefix(&self.root) {
                    let relative = relative.to_string_lossy();
                    write_machine_file(folder, &relative, bytes)?;
                    shown.to_owned()
                } else if let Some((root, write_root)) = self
                    .write_roots
                    .iter()
                    .find(|(root, _)| target.starts_with(root))
                {
                    let relative = target
                        .strip_prefix(root)
                        .map_err(|_| ByteError::Failed("the path left its write root".to_owned()))?
                        .to_string_lossy();
                    write_machine_file(write_root, &relative, bytes)?;
                    shown.to_owned()
                } else if path.is_absolute() {
                    return Err(ByteError::Failed(format!(
                        "the path is absolute and outside the Bot's workspace, its grants, and the run's directory: {shown}"
                    )));
                } else {
                    return Err(ByteError::Failed(format!(
                        "the path is outside the Bot's workspace, its grants, and the run's directory: {shown}"
                    )));
                };
                Ok(written)
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

    /// Returns whether a path is denied for the file tools.
    fn is_denied(&self, path: &Path) -> bool {
        self.denied
            .iter()
            .any(|denied| path == denied || path.starts_with(denied))
    }

    /// Opens a file whose bytes this runtime may read.
    ///
    /// A managed read is an ordinary open. A real-machine read opens with
    /// `O_NONBLOCK | O_NOFOLLOW` — a FIFO cannot block it and a symlink is not
    /// followed — requires a regular file, and refuses a descriptor that
    /// resolves under a hidden root.
    fn open_read(&self, path: &Path) -> Result<(std::fs::File, std::fs::Metadata), String> {
        match &self.mode {
            Mode::Managed => {
                let file = std::fs::File::open(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let metadata = file
                    .metadata()
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                Ok((file, metadata))
            }
            Mode::Machine { .. } => self.open_machine_read(path),
        }
    }

    /// A real-machine read of one opened regular file.
    ///
    /// The open follows a symlink, as the machine user's own tools do: reading
    /// is limited by the machine user's permissions, not by the write fence.
    /// The hidden check still sees through it, because it reads the opened
    /// descriptor's real path. The open is non-blocking and the descriptor
    /// must hold a regular file, so a FIFO or a device is refused instead of
    /// waited on.
    #[cfg(unix)]
    fn open_machine_read(&self, path: &Path) -> Result<(std::fs::File, std::fs::Metadata), String> {
        use std::os::fd::FromRawFd as _;

        let shown = self.relative(path);
        let name = c_name(path.as_os_str())
            .map_err(|message| format!("cannot read {shown}: {message}"))?;
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: `name` is NUL-terminated; a successful return is a fresh
        // descriptor this function owns.
        let fd = unsafe { libc::openat(libc::AT_FDCWD, name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(format!("cannot read {shown}: {error}"));
        }
        // SAFETY: `fd` is a fresh descriptor from `openat` and is owned here.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot read {shown}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        self.refuse_hidden(&file, path)?;

        Ok((file, metadata))
    }

    /// Refuses an opened file or directory that is hidden or denied.
    ///
    /// The opened descriptor's real path is what counts, not the path the
    /// caller typed: a symlink inside the Bot's folder cannot aim at a hidden
    /// file and have the check miss. A full-access runtime skips the check:
    /// the owner gave the Bot the machine user's own rights, including the
    /// Computer's configuration directory.
    fn refuse_hidden(&self, opened: &std::fs::File, path: &Path) -> Result<(), String> {
        if self.full_access || (self.hidden.is_empty() && self.denied.is_empty()) {
            return Ok(());
        }
        let shown = path.display();
        let real = opened_path(opened)
            .map_err(|error| format!("cannot verify what {shown} names: {error}"))?;
        let real = without_deleted_suffix(real);
        if self.is_hidden(&real) || self.is_denied(&real) {
            return Err(hidden_error(shown));
        }

        Ok(())
    }

    fn read_file(&self, path: &str) -> ActionResult {
        let path = match self.resolve_read(path) {
            Ok(path) => path,
            Err(error) => return ActionResult::failed(error),
        };
        let (mut file, metadata) = match self.open_read(&path) {
            Ok(opened) => opened,
            Err(error) => return ActionResult::failed(error),
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
        let lines = match &self.mode {
            Mode::Managed => match std::fs::read_dir(&path) {
                Ok(entries) => list_entries(entries),
                Err(error) => {
                    return ActionResult::failed(format!(
                        "cannot list {}: {error}",
                        self.relative(&path)
                    ));
                }
            },
            Mode::Machine { .. } => match self.list_machine_directory(&path) {
                Ok(lines) => lines,
                Err(error) => return ActionResult::failed(error),
            },
        };

        ActionResult::succeeded(truncate(lines.join("\n")))
    }

    /// Lists one directory of a real machine through the descriptor it opened.
    ///
    /// The directory is opened once with `O_DIRECTORY | O_NOFOLLOW`, checked
    /// against the hidden roots, and then walked through that same descriptor:
    /// a path swapped in between the check and the walk cannot change what is
    /// listed.
    #[cfg(unix)]
    fn list_machine_directory(&self, path: &Path) -> Result<Vec<String>, String> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        let shown = self.relative(path);
        let name = c_name(path.as_os_str())
            .map_err(|message| format!("cannot list {shown}: {message}"))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `name` is NUL-terminated; a successful return is a fresh
        // descriptor this function owns.
        let fd = unsafe { libc::openat(libc::AT_FDCWD, name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ELOOP) => {
                    format!("the path is a symlink, which is not listed: {shown}")
                }
                _ => format!("cannot list {shown}: {error}"),
            });
        }
        // SAFETY: `fd` is a fresh descriptor from `openat` and is owned here.
        let directory = unsafe { std::fs::File::from_raw_fd(fd) };
        self.refuse_hidden(&directory, path)?;
        let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
        if duplicate < 0 {
            return Err(format!(
                "cannot list {shown}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            // SAFETY: `duplicate` is still ours when `fdopendir` fails.
            unsafe { libc::close(duplicate) };
            return Err(format!("cannot list {shown}: {error}"));
        }
        let mut lines = Vec::new();
        let result = (|| -> std::io::Result<()> {
            while let Some(name) = next_directory_entry(stream)? {
                if lines.len() == MAX_LIST_ENTRIES {
                    break;
                }
                let kind = match fstatat(&directory, &name) {
                    Ok(metadata) => match metadata.st_mode & libc::S_IFMT {
                        libc::S_IFDIR => "dir ",
                        libc::S_IFLNK => "link",
                        libc::S_IFREG => "file",
                        _ => "?   ",
                    },
                    Err(_) => "?   ",
                };
                lines.push(format!("{kind} {}", name.to_string_lossy()));
            }

            Ok(())
        })();
        // SAFETY: `stream` is the open directory stream created above.
        unsafe { libc::closedir(stream) };
        result.map_err(|error| format!("cannot list {shown}: {error}"))?;
        lines.sort();
        if lines.is_empty() {
            lines.push("(empty)".to_owned());
        }

        Ok(lines)
    }

    /// Lists a directory on a system without real-drive descriptors.
    #[cfg(not(unix))]
    fn list_machine_directory(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("a real Computer lists only on Unix".to_owned())
    }

    /// Builds the shell process for this mode, without starting it.
    ///
    /// A granted shell rechecks what it is about to mount: the Bot's folder and
    /// every candidate write root are scanned now, because the machine's host
    /// may have added a link since the runtime opened them, and only the
    /// accepted roots are mounted.
    fn shell_command(&self, command: &str) -> Result<Command, ActionResult> {
        let mut process = match &self.mode {
            Mode::Managed => {
                let mut process = Command::new("sh");
                process.arg("-c").arg(command).current_dir(&self.root);
                process
            }
            // Full access has no fence to build: the command runs as the
            // machine user from the run's directory, and bubblewrap is never
            // started. Its wrapper ends every process the command leaves
            // behind, including one that moved to its own process group.
            Mode::Machine { .. } if self.full_access => {
                full_access_shell_command(command, self.base())
            }
            Mode::Machine { bwrap, folder } => {
                let Some(bwrap) = bwrap else {
                    return Err(ActionResult::failed(
                        "this runtime has no sandbox to start a fenced shell",
                    ));
                };
                // A host that added a hard link or a symlink below the Bot's
                // folder since the runtime started would otherwise be mounted
                // writable for this shell: scan the folder again for every
                // shell, exactly as the write roots are scanned.
                scan_bot_folder(folder, &self.root)
                    .map_err(|error| ActionResult::failed(error.to_string()))?;
                #[cfg(unix)]
                let source = folder_mount_source();
                #[cfg(not(unix))]
                let source = self.root.as_os_str().to_owned();
                #[cfg(unix)]
                let (write_roots, root_fds): (
                    Vec<(OsString, PathBuf)>,
                    Vec<libc::c_int>,
                ) = {
                    // A write root the scan refuses is not mounted writable for
                    // the shell: a hard link or symlink in it could otherwise
                    // change a file outside the root. The accepted roots are
                    // deduped here, after the scan: an outer root the scan
                    // refuses must not take a clean inner root down with it.
                    let mut mounted: Vec<&(PathBuf, std::fs::File)> = self
                        .write_roots
                        .iter()
                        .filter(|(root, file)| scan_bot_folder(file, root).is_ok())
                        .collect();
                    mounted.sort_by(|(left, _), (right, _)| left.cmp(right));
                    mounted.dedup_by(|inner, outer| inner.0.starts_with(&outer.0));
                    mounted
                        .into_iter()
                        .map(|(root, file)| {
                            use std::os::fd::AsRawFd as _;
                            (
                                (
                                    OsString::from(format!("/proc/self/fd/{}", file.as_raw_fd())),
                                    root.clone(),
                                ),
                                file.as_raw_fd(),
                            )
                        })
                        .unzip()
                };
                #[cfg(not(unix))]
                let (write_roots, root_fds): (
                    Vec<(OsString, PathBuf)>,
                    Vec<libc::c_int>,
                ) = (Vec::new(), Vec::new());
                let mut process = sandbox_command(
                    bwrap,
                    &source,
                    &self.root,
                    self.base(),
                    &self.hidden,
                    &write_roots,
                );
                // The parent keeps this descriptor close-on-exec, so other
                // runtimes' folders are never inherited. In the child, after
                // fork and before exec, move this runtime's folder descriptor
                // to a one-digit number and clear its close-on-exec flag, so
                // bwrap can bind `/proc/self/fd/<fd>` without trusting a path
                // a later rename could point somewhere else.
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd as _;

                    let folder_fd = folder.as_raw_fd();
                    // SAFETY: the closure runs in the child between `fork` and
                    // `exec`, where only async-signal-safe calls are allowed;
                    // `dup2` and `fcntl` are, they touch only this child's
                    // descriptor table, and they allocate nothing. A failure is
                    // returned so `spawn` reports it instead of exec'ing
                    // without the descriptor.
                    unsafe {
                        process.pre_exec(move || {
                            if folder_fd != MOUNT_FD && libc::dup2(folder_fd, MOUNT_FD) < 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                            if libc::fcntl(MOUNT_FD, libc::F_SETFD, 0) < 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                            for fd in &root_fds {
                                if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }
                            Ok(())
                        });
                    }
                    // The wrapper is our own shell script: it closes the mount
                    // descriptor first and only then execs the user's command,
                    // which arrives as a separate argv element and is never
                    // interpolated into the script.
                    process
                        .arg("/bin/sh")
                        .arg("-c")
                        .arg(format!("exec {MOUNT_FD}>&-; exec /bin/sh -c \"$1\""))
                        .arg("neha")
                        .arg(command);
                }
                #[cfg(not(unix))]
                process.arg("/bin/sh").arg("-c").arg(command);
                process.current_dir(self.base());
                process
            }
        };
        let temporary = match &self.mode {
            Mode::Managed => self.root.join(".tmp"),
            Mode::Machine { .. } => PathBuf::from(SANDBOX_TMPDIR),
        };
        let path = match &self.cli_dir {
            Some(directory) => format!("{}:{SHELL_PATH}", directory.display()),
            None => SHELL_PATH.to_owned(),
        };
        process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("TMPDIR", temporary)
            .envs(self.env.lock().map(|env| env.clone()).unwrap_or_default())
            .kill_on_drop(true);
        Ok(process)
    }

    async fn shell(&self, command: &str) -> ActionResult {
        if command.trim().is_empty() {
            return ActionResult::failed("the command is empty");
        }
        if command.len() > MAX_COMMAND_BYTES {
            return ActionResult::failed("the command is too long");
        }
        let mut process = match self.shell_command(command) {
            Ok(process) => process,
            Err(result) => return result,
        };
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
                kill_tree(pid);
                return ActionResult::failed(format!("the shell failed: {error}"));
            }
            Err(_) => {
                timed_out = true;
                kill_tree(pid);
                let _ = child.wait().await;
                None
            }
        };
        // The shell may have left children holding the pipes open. The
        // command is over, so the group goes; the readers then drain what
        // the pipes still hold. A full-access shell's wrapper has already
        // ended the descendants that left its process group.
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
            let (file, metadata) = self.open_read(&path).map_err(ByteError::Failed)?;
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
            read_file_bounded(
                file,
                neha::file::MAX_FILE_BYTES,
                &path.display().to_string(),
            )
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
        .args(sandbox_args(
            folder.as_os_str(),
            folder,
            folder,
            hidden,
            &[],
        ))
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

/// Returns whether one hidden root is a file rather than a directory.
fn hidden_is_file(root: &Path) -> bool {
    std::fs::metadata(root).is_ok_and(|metadata| metadata.is_file())
}

/// The bubblewrap arguments that fence one real shell.
///
/// The machine is read-only, `/dev` is a minimal device set of the sandbox's
/// own (with its own `/dev/shm`), `/tmp` is fresh, the PID namespace is
/// private, the sandbox dies with its daemon, and the Bot's folder is a
/// writable root. Each hidden root gets a fresh empty tmpfs, so the Computer's
/// own directory and its credential are absent from the sandbox. The network is
/// not unshared: a real Computer uses its machine's network as before.
///
/// `write_roots` are the owner's grants and the run's own directory; each is
/// bound read-write after the machine. The writable mount's source is `source`
/// — the runtime's opened folder by descriptor — while its target, and the
/// working directory, stay the folder's path.
fn sandbox_args(
    source: &OsStr,
    folder: &Path,
    cwd: &Path,
    hidden: &[PathBuf],
    write_roots: &[(OsString, PathBuf)],
) -> Vec<OsString> {
    let folder = folder.as_os_str();
    let mut arguments: Vec<OsString> = ["--ro-bind", "/", "/"]
        .into_iter()
        .map(OsString::from)
        .collect();
    // Hidden roots are mounted before /tmp's: a root inside /tmp must be
    // mounted while the host's /tmp is still what the sandbox sees. A
    // directory root is replaced by an empty tmpfs; a single hidden file (a
    // credential file) is replaced by `/dev/null`, so reading it yields
    // nothing and writing it still reaches nothing on the machine.
    for root in hidden {
        if hidden_is_file(root) {
            arguments.push(OsString::from("--ro-bind"));
            arguments.push(OsString::from("/dev/null"));
            arguments.push(root.as_os_str().to_owned());
        } else {
            arguments.push(OsString::from("--tmpfs"));
            arguments.push(root.as_os_str().to_owned());
        }
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
    arguments.push(source.to_owned());
    arguments.push(folder.to_owned());
    for (source, target) in write_roots {
        arguments.push(OsString::from("--bind"));
        arguments.push(source.clone());
        arguments.push(target.as_os_str().to_owned());
    }
    arguments.push(OsString::from("--chdir"));
    arguments.push(cwd.as_os_str().to_owned());
    arguments.push(OsString::from("--"));

    arguments
}

/// The one-digit descriptor the shell mounts as the Bot's folder.
///
/// POSIX shells only accept a one-digit `IO_NUMBER` (`dash` rejects
/// `exec 10>&-`), so the child process moves the folder descriptor here before
/// bwrap starts and the wrapper shell closes it before the user command runs.
#[cfg(unix)]
const MOUNT_FD: libc::c_int = 9;

/// Names the opened Bot folder as a path the sandbox can bind.
#[cfg(unix)]
fn folder_mount_source() -> OsString {
    OsString::from(format!("/proc/self/fd/{MOUNT_FD}"))
}

/// Builds the fenced shell process without starting it.
fn sandbox_command(
    bwrap: &Path,
    source: &OsStr,
    folder: &Path,
    cwd: &Path,
    hidden: &[PathBuf],
    write_roots: &[(OsString, PathBuf)],
) -> Command {
    let mut process = Command::new(bwrap);
    process.args(sandbox_args(source, folder, cwd, hidden, write_roots));
    process
}

/// The shell script a full-access command runs under.
///
/// A full-access shell has no sandbox, so a descendant that moves to its own
/// process group (a `setsid` daemon) or double-forks out of it would outlive
/// the command. This wrapper is a child subreaper: every orphaned descendant is
/// reparented to it instead of to init, and its exit trap kills those children
/// before it exits. The command arrives as its own argv element (`$1`) and is
/// never interpolated into this script.
#[cfg(target_os = "linux")]
const FULL_ACCESS_WRAPPER: &str = r#"
cleanup() {
    neha_round=0
    while [ "$neha_round" -lt 100 ]; do
        neha_found=0
        for neha_stat in /proc/[0-9]*/stat; do
            neha_pid=${neha_stat#/proc/}
            neha_pid=${neha_pid%/stat}
            [ "$neha_pid" = "$$" ] && continue
            read -r neha_line < "$neha_stat" 2>/dev/null || continue
            neha_rest=${neha_line##*') '}
            set -- $neha_rest
            if [ "$2" = "$$" ]; then
                kill -KILL "$neha_pid" 2>/dev/null
                neha_found=1
            fi
        done
        [ "$neha_found" = 0 ] && return 0
        neha_round=$((neha_round + 1))
    done
}
trap 'neha_status=$?; cleanup; exit $neha_status' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
/bin/sh -c "$1"
"#;

/// Builds a full-access shell process: no bubblewrap, and a wrapper that ends
/// the command's whole process tree when the command ends.
///
/// The wrapper needs `/proc` to find descendants that left the process group,
/// and only Linux has the tracking that a timeout or a closed channel uses; on
/// another Unix the command keeps the plain process-group rule.
fn full_access_shell_command(command: &str, base: &Path) -> Command {
    let mut process = Command::new("sh");
    #[cfg(target_os = "linux")]
    {
        process
            .arg("-c")
            .arg(FULL_ACCESS_WRAPPER)
            .arg("neha")
            .arg(command);
        // SAFETY: the closure runs in the child between `fork` and `exec`,
        // where only async-signal-safe calls are allowed; `prctl` is a plain
        // syscall that changes only this child's attribute and allocates
        // nothing. The attribute survives `exec`, so the wrapper script below
        // is the subreaper that collects the command's orphaned descendants.
        unsafe {
            process.pre_exec(|| {
                if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        process.arg("-c").arg(command);
    }
    process.current_dir(base);
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

/// Renders the entries of a managed directory, one per line.
fn list_entries(entries: std::fs::ReadDir) -> Vec<String> {
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

    lines
}

/// Reads at most `limit` bytes, refusing a file that grew past the limit after
/// its length was checked.
///
/// The read stops after `limit + 1` bytes, so a file another process keeps
/// appending to can never put more than one File's worth of bytes in memory.
fn read_file_bounded(
    reader: impl std::io::Read,
    limit: u64,
    shown: &str,
) -> Result<Vec<u8>, ByteError> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ByteError::Failed(format!("cannot read {shown}: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(ByteError::Failed(format!(
            "the file is larger than {limit} bytes"
        )));
    }

    Ok(bytes)
}

/// The error a real-machine constructor returns when bubblewrap is absent.
fn no_sandbox() -> RuntimeError {
    RuntimeError::NoSandbox("bwrap was not found on PATH; install bubblewrap".to_owned())
}

/// Returns whether a name is exactly one normal path component.
fn is_one_component(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(part)) if part == name)
        && components.next().is_none()
}

/// Builds the refusal a Bot folder that cannot be served gets.
fn unsafe_folder(folder_path: &Path, problem: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::UnsafeFolder(format!(
        "{problem}; clean {} and start again (the default is ~/Neha/<bot-id>)",
        folder_path.display()
    ))
}

/// Opens the Bot's folder under `workspace`, creating it when absent.
///
/// `open_directory_at` never follows a symlink, so neither a Bot folder that
/// is a symlink nor one that is not a directory is ever served.
#[cfg(unix)]
fn open_or_create_machine_folder(
    workspace: &std::fs::File,
    name: &OsStr,
    folder_path: &Path,
) -> Result<std::fs::File, RuntimeError> {
    use std::os::fd::AsRawFd as _;

    match open_directory_at(workspace, name) {
        Ok(folder) => Ok(folder),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name_c = c_name(name).map_err(|message| {
                unsafe_folder(folder_path, format!("its name is not usable: {message}"))
            })?;
            // SAFETY: `workspace` is a live directory descriptor and `name_c`
            // is NUL-terminated; `mkdirat` takes no ownership of it.
            let created = unsafe { libc::mkdirat(workspace.as_raw_fd(), name_c.as_ptr(), 0o755) };
            if created != 0 {
                let error = std::io::Error::last_os_error();
                // Another writer may have created it between open and mkdir.
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(RuntimeError::Io(error));
                }
            }
            open_directory_at(workspace, name)
                .map_err(|error| folder_open_error(workspace, name, error, folder_path))
        }
        Err(error) => Err(folder_open_error(workspace, name, error, folder_path)),
    }
}

/// Names the reason a Bot folder could not be opened as a directory.
#[cfg(unix)]
fn folder_open_error(
    workspace: &std::fs::File,
    name: &OsStr,
    error: std::io::Error,
    folder_path: &Path,
) -> RuntimeError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => unsafe_folder(folder_path, "it is a symlink"),
        // A symlink opened with `O_DIRECTORY | O_NOFOLLOW` is not a directory,
        // and Linux may name it either way.
        Some(libc::ENOTDIR) if is_symlink_at(workspace, name) => {
            unsafe_folder(folder_path, "it is a symlink")
        }
        Some(libc::ENOTDIR) => unsafe_folder(folder_path, "it is not a directory"),
        _ => RuntimeError::Io(error),
    }
}

/// Scans a Bot's folder once, refusing links and special files.
///
/// The scan is bounded: at most [`MAX_SCAN_ENTRIES`] entries and
/// [`MAX_SCAN_DEPTH`] directories below the folder. A folder beyond either
/// bound is refused rather than trusted.
#[cfg(unix)]
fn scan_bot_folder(folder: &std::fs::File, folder_path: &Path) -> Result<(), RuntimeError> {
    let mut budget = MAX_SCAN_ENTRIES;
    scan_directory(folder, folder_path, Path::new(""), 0, &mut budget)
}

/// Scans one directory of a Bot folder, refusing what a write must not meet.
#[cfg(unix)]
fn scan_directory(
    directory: &std::fs::File,
    folder_path: &Path,
    relative: &Path,
    depth: usize,
    budget: &mut usize,
) -> Result<(), RuntimeError> {
    if depth > MAX_SCAN_DEPTH {
        return Err(unsafe_folder(
            folder_path,
            format!(
                "it has more than {MAX_SCAN_DEPTH} nested directories: {}",
                relative.display()
            ),
        ));
    }
    // Open `.` afresh instead of `dup`: a duplicate shares the directory
    // offset, so a second scan of a cached descriptor would start at EOF and
    // silently see no entries at all.
    use std::os::fd::IntoRawFd as _;
    let opened = match open_directory_at(directory, std::ffi::OsStr::new(".")) {
        Ok(opened) => opened,
        Err(error) => return Err(RuntimeError::Io(error)),
    };
    let stream = unsafe { libc::fdopendir(opened.into_raw_fd()) };
    if stream.is_null() {
        return Err(RuntimeError::Io(std::io::Error::last_os_error()));
    }
    let result = scan_entries(directory, folder_path, relative, depth, budget, stream);
    // SAFETY: `stream` is the open directory stream created above.
    unsafe { libc::closedir(stream) };

    result
}

/// Visits the entries of one open directory, bounded by `budget`.
#[cfg(unix)]
fn scan_entries(
    directory: &std::fs::File,
    folder_path: &Path,
    relative: &Path,
    depth: usize,
    budget: &mut usize,
    stream: *mut libc::DIR,
) -> Result<(), RuntimeError> {
    while let Some(name) = next_directory_entry(stream).map_err(RuntimeError::Io)? {
        if *budget == 0 {
            return Err(unsafe_folder(
                folder_path,
                format!("it has more than {MAX_SCAN_ENTRIES} entries"),
            ));
        }
        *budget -= 1;
        let child = relative.join(&name);
        let metadata = match fstatat(directory, &name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(RuntimeError::Io(error)),
        };
        match metadata.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                return Err(unsafe_folder(
                    folder_path,
                    format!("it contains a symlink: {}", child.display()),
                ));
            }
            libc::S_IFDIR => {
                let child_directory = open_directory_at(directory, &name)
                    .map_err(|error| folder_open_error(directory, &name, error, folder_path))?;
                scan_directory(&child_directory, folder_path, &child, depth + 1, budget)?;
            }
            libc::S_IFREG => {
                if metadata.st_nlink > 1 {
                    return Err(unsafe_folder(
                        folder_path,
                        format!(
                            "it contains a hard link ({} links): {}",
                            metadata.st_nlink,
                            child.display()
                        ),
                    ));
                }
            }
            _ => {
                return Err(unsafe_folder(
                    folder_path,
                    format!("it contains a special file: {}", child.display()),
                ));
            }
        }
    }

    Ok(())
}

/// Reads one entry from an open directory stream, or `None` at its end.
#[cfg(unix)]
fn next_directory_entry(stream: *mut libc::DIR) -> std::io::Result<Option<OsString>> {
    use std::os::unix::ffi::OsStrExt as _;

    loop {
        clear_errno();
        // SAFETY: `stream` is an open directory stream owned by the caller.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            // A cleared errno of 0 means the end of the stream, not a failure.
            return if error.raw_os_error() == Some(0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: `entry` points at a valid entry for the life of `stream`.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = OsStr::from_bytes(name.to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        return Ok(Some(name.to_owned()));
    }
}

/// Clears the thread's errno before `readdir`, which reports its end that way.
#[cfg(all(unix, target_os = "linux"))]
fn clear_errno() {
    // SAFETY: the thread's errno slot is always valid to write.
    unsafe { *libc::__errno_location() = 0 };
}

/// Clears the thread's errno before `readdir`, which reports its end that way.
///
/// A real Computer runs only on Linux, where the helper above is compiled; on
/// another Unix the scan never runs and a null `readdir` is treated as the
/// end.
#[cfg(all(unix, not(target_os = "linux")))]
fn clear_errno() {}

/// Stats one entry of a directory without following a symlink.
#[cfg(unix)]
fn fstatat(directory: &std::fs::File, name: &OsStr) -> std::io::Result<libc::stat> {
    use std::os::fd::AsRawFd as _;

    let name = c_name(name)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    // SAFETY: `stat` is a plain C structure owned here.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `directory` is a live descriptor, `name` is NUL-terminated, and
    // `stat` outlives the call.
    let found = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if found != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(stat)
}

/// Opens a directory without following a symlink.
#[cfg(unix)]
fn open_directory_path(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("the path contains a NUL byte: {}", path.display()),
        )
    })?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated; a successful return is a fresh
    // descriptor this function owns.
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor from `open` and is owned here.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Opens a directory without following a symlink, on a system without them.
#[cfg(not(unix))]
fn open_directory_path(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a real Computer runs only on Unix",
    ))
}

/// Opens one folder writes may reach and names it by its descriptor.
///
/// The open does not block on a FIFO and requires a directory, and the path is
/// read back from the descriptor: a rename after the open cannot make the root
/// name something else.
#[cfg(unix)]
fn open_write_root(directory: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_DIRECTORY)
        .open(directory)?;
    // The shell moves the Bot's folder descriptor to a fixed one-digit number.
    // A root that landed there is moved to another descriptor so the two
    // mounts can never name each other.
    while file.as_raw_fd() == MOUNT_FD {
        file = file.try_clone()?;
    }
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;

    Ok((path, file))
}

/// A real Computer runs only on Unix, where a write root can be a descriptor.
#[cfg(not(unix))]
fn open_write_root(_directory: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a real Computer runs only on Unix",
    ))
}

/// Writes bytes to a relative path inside the Bot's folder descriptor without
/// following a symlink, writing a hard link, or opening a special file.
///
/// Every component is opened with `O_NOFOLLOW` relative to the descriptor of
/// its parent, so a shell that swaps in a symlink between two tool calls
/// cannot move the write out of the folder and there is no check-then-open
/// window. The target is opened without `O_TRUNC`, checked with `fstat` — it
/// must be a regular file with one link — and only then truncated and written.
#[cfg(unix)]
fn write_machine_file(folder: &std::fs::File, input: &str, bytes: &[u8]) -> Result<(), ByteError> {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;

    let (shown, mut components) = write_components(input).map_err(ByteError::Failed)?;
    let Some(last) = components.pop() else {
        return Err(ByteError::Failed(format!(
            "the path does not name a file: {shown}"
        )));
    };
    let mut directory = folder
        .try_clone()
        .map_err(|error| ByteError::Failed(format!("cannot use the Bot's folder: {error}")))?;
    for name in components {
        directory = open_or_create_directory(&directory, &name, &shown)?;
    }
    let mut file = open_target_file(&directory, &last, &shown)?;
    let metadata = file
        .metadata()
        .map_err(|error| ByteError::Failed(format!("cannot write {shown}: {error}")))?;
    if !metadata.is_file() {
        return Err(ByteError::Failed(format!("{shown} is not a regular file")));
    }
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
fn write_machine_file(
    _folder: &std::fs::File,
    _input: &str,
    _bytes: &[u8],
) -> Result<(), ByteError> {
    Err(ByteError::Failed(
        "a real Computer writes only on Unix".to_owned(),
    ))
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

/// Opens the file a write ends at, without following a symlink, without
/// blocking on a FIFO, and without truncating it yet.
#[cfg(unix)]
fn open_target_file(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    shown: &str,
) -> Result<std::fs::File, ByteError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    // Refuse a special file before opening it: a write end of a FIFO would
    // otherwise wait for a reader, and opening a device can have effects.
    match fstatat(parent, name) {
        Ok(metadata) => match metadata.st_mode & libc::S_IFMT {
            libc::S_IFREG => {}
            libc::S_IFLNK => {
                return Err(ByteError::Failed(format!(
                    "the path uses a symlink, which is not written through: {shown}"
                )));
            }
            libc::S_IFDIR => {
                return Err(ByteError::Failed(format!(
                    "the path is a directory, not a file: {shown}"
                )));
            }
            _ => {
                return Err(ByteError::Failed(format!("{shown} is not a regular file")));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
    let name = c_name(name).map_err(ByteError::Failed)?;
    let flags =
        libc::O_WRONLY | libc::O_CREAT | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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
        // A FIFO with no reader refuses a non-blocking write this way.
        Some(libc::ENXIO) => Err(ByteError::Failed(format!("{shown} is not a regular file"))),
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

/// Kills one shell and every process below it.
///
/// A fenced shell dies with its bubblewrap namespace and a shell's own group
/// with it, but an unsandboxed command can move a descendant to another process
/// group (a `setsid` daemon) or double-fork it out of reach of the group signal.
/// The descendants are recorded before the group goes, while their parent links
/// still exist, and killed after it; the recorded start time refuses a process
/// id the kernel reused in between.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(target_os = "linux")]
    let descendants = process_descendants(pid);
    kill_group(Some(pid));
    #[cfg(target_os = "linux")]
    for (child, start_time) in descendants {
        if process_start_time(child) != Some(start_time) {
            continue;
        }
        // SAFETY: `child` names a live process with the recorded start time, so
        // it is the descendant the snapshot saw and not a reused process id.
        unsafe {
            libc::kill(child as i32, libc::SIGKILL);
        }
    }
}

/// Returns the live descendants of `root` and their start times.
///
/// A `/proc` snapshot is what lets an unsandboxed shell's whole tree be found:
/// the parent links name every process, including one in its own process group.
/// A machine without `/proc` records nothing and falls back to the group.
#[cfg(target_os = "linux")]
fn process_descendants(root: u32) -> Vec<(u32, u64)> {
    let mut parents: std::collections::HashMap<u32, (u32, u64)> = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(parent) = process_parent_and_start(pid) {
            parents.insert(pid, parent);
        }
    }
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for (pid, (parent, _)) in &parents {
        children.entry(*parent).or_default().push(*pid);
    }
    let mut descendants = Vec::new();
    let mut seen = std::collections::HashSet::from([root]);
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        let Some(kids) = children.get(&parent) else {
            continue;
        };
        for child in kids {
            if !seen.insert(*child) {
                continue;
            }
            if let Some((_, start_time)) = parents.get(child) {
                descendants.push((*child, *start_time));
            }
            frontier.push(*child);
        }
    }

    descendants
}

/// A process's parent and start time from one `/proc/<pid>/stat` snapshot.
///
/// The command name is parenthesized and may itself hold spaces or `)`, so the
/// fields are read after its last `)`.
#[cfg(target_os = "linux")]
fn process_parent_and_start(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    // `starttime` is the 22nd field of /proc/<pid>/stat; `state` and `ppid` are
    // the 3rd and 4th, so it is the 18th field after `ppid`.
    let start_time = fields.nth(17)?.parse().ok()?;

    Some((parent, start_time))
}

/// Returns a process's start time from `/proc`, or `None` when it is gone.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    process_parent_and_start(pid).map(|(_, start_time)| start_time)
}
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
    /// The Bot's folder cannot be served because of what it holds or where it
    /// sits; the message names the folder and what to clean.
    #[error("{0}")]
    UnsafeFolder(String),
    /// A folder the owner granted, or a run's directory, cannot be used.
    #[error("access error: {0}")]
    Access(String),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    use neha::file::MAX_FILE_BYTES;

    use super::{
        DEFAULT_TIMEOUT, LocalRuntime, MAX_SCAN_DEPTH, Mode, RuntimeError, open_directory_path,
        sandbox_args,
    };
    use crate::action::{Action, ActionStatus, ByteError, ComputerRuntime};

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

    /// Returns whether the bubblewrap tests are switched on.
    fn bwrap_enabled() -> bool {
        if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
            eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
            return false;
        }
        true
    }

    /// Makes a FIFO without opening it.
    #[cfg(unix)]
    fn make_fifo(path: &std::path::Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo exists");
        assert!(status.success(), "mkfifo failed");
    }

    /// Returns a real-machine runtime, or `None` when the bwrap tests are off.
    ///
    /// `NEHA_TEST_BWRAP` opens them. Once it is set, a missing or broken
    /// bubblewrap fails the test instead of skipping: the gate promises a
    /// working sandbox.
    fn machine_runtime() -> Option<(tempfile::TempDir, LocalRuntime)> {
        if !bwrap_enabled() {
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

    /// Returns a real-machine runtime that hides one file outside `/tmp`,
    /// or `None` when the bwrap tests are off.
    fn machine_runtime_with_hidden_file()
    -> Option<(tempfile::TempDir, LocalRuntime, std::path::PathBuf)> {
        if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
            eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
            return None;
        }
        let directory = outside_tempdir();
        let secret = directory.path().join("credential.json");
        std::fs::write(&secret, "session-secret-must-not-leak").unwrap();
        let runtime =
            LocalRuntime::machine_with_hidden(directory.path().join("bot"), vec![secret.clone()])
                .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

        Some((directory, runtime, secret))
    }

    /// Builds a real-machine runtime for the file tools alone.
    ///
    /// The file tools never start a sandbox, so a test of their rules does not
    /// need bubblewrap; the shell rules have their own gated tests. Both paths
    /// are resolved, as the constructor resolves them. The `bwrap` path does
    /// not exist, so code that wrongly started a sandbox would fail the test.
    fn file_runtime(root: &std::path::Path, hidden: &std::path::Path) -> LocalRuntime {
        LocalRuntime {
            root: std::fs::canonicalize(root).unwrap(),
            mode: Mode::Machine {
                bwrap: Some(std::path::PathBuf::from("/nonexistent/neha-test-bwrap")),
                folder: open_directory_path(&std::fs::canonicalize(root).unwrap()).unwrap(),
            },
            hidden: vec![std::fs::canonicalize(hidden).unwrap()],
            denied: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: AtomicU32::new(0),
            full_access: false,
            write_roots: Vec::new(),
            cwd: None,
            env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            cli_dir: None,
        }
    }

    /// Builds a real-machine runtime for the file tools that hides nothing.
    fn file_runtime_without_hidden(root: &std::path::Path) -> LocalRuntime {
        let mut runtime = file_runtime(root, root);
        runtime.hidden.clear();
        runtime
    }

    /// Builds a full-access runtime.
    ///
    /// Full access needs no sandbox, so the runtime's `bwrap` path is absent:
    /// code that tried to start one would fail the test instead of hiding the
    /// mistake. The hidden directory keeps its meaning only for the granted
    /// mode, so a test can pass one and see full access ignore it.
    fn full_access_runtime(root: &std::path::Path, hidden: &std::path::Path) -> LocalRuntime {
        file_runtime(root, hidden)
            .with_machine_access(true, &[], None)
            .expect("full access opens no sandbox")
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

        // An absolute path reads the machine; writing one is allowed when it
        // names the Bot's own folder.
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
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
            "absolute"
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

    #[cfg(unix)]
    #[test]
    fn a_bot_folder_that_is_a_symlink_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, directory.path().join("bot")).unwrap();

        let error =
            LocalRuntime::machine_with_bwrap(directory.path().join("bot"), None, Vec::new())
                .err()
                .expect("a symlinked Bot folder is refused");
        assert!(matches!(error, RuntimeError::UnsafeFolder(_)), "{error}");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(error.to_string().contains("clean"), "{error}");

        std::fs::write(directory.path().join("file"), "not a directory").unwrap();
        let error =
            LocalRuntime::machine_with_bwrap(directory.path().join("file"), None, Vec::new())
                .err()
                .expect("a file that is not a directory is refused");
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_root_opens_without_following_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let root = LocalRuntime::open_workspace(&workspace).unwrap();
        assert!(workspace.is_dir());
        drop(root);

        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, directory.path().join("linked")).unwrap();
        assert!(
            LocalRuntime::open_workspace(directory.path().join("linked")).is_err(),
            "a symlinked workspace root is not followed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_holding_a_hard_link_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let bot = directory.path().join("bot");
        std::fs::create_dir_all(bot.join("sub")).unwrap();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "host").unwrap();
        std::fs::hard_link(&outside, bot.join("sub/inside.txt")).unwrap();

        let error = LocalRuntime::machine_with_bwrap(&bot, None, Vec::new())
            .err()
            .expect("a folder holding a hard link is refused");
        assert!(error.to_string().contains("hard link"), "{error}");
        assert!(error.to_string().contains("clean"), "{error}");
        assert!(
            error.to_string().contains("~/Neha/<bot-id>"),
            "the refusal tells the user what to clean: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_holding_a_symlink_or_a_special_file_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let bot = directory.path().join("bot");
        std::fs::create_dir(&bot).unwrap();
        std::os::unix::fs::symlink(directory.path().join("elsewhere"), bot.join("link")).unwrap();
        let error = LocalRuntime::machine_with_bwrap(&bot, None, Vec::new())
            .err()
            .expect("a folder holding a symlink is refused");
        assert!(error.to_string().contains("symlink"), "{error}");

        std::fs::remove_file(bot.join("link")).unwrap();
        make_fifo(&bot.join("pipe"));
        let error = LocalRuntime::machine_with_bwrap(&bot, None, Vec::new())
            .err()
            .expect("a folder holding a FIFO is refused");
        assert!(error.to_string().contains("special file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_nested_past_the_scan_limit_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let bot = directory.path().join("bot");
        let mut deep = bot.clone();
        for _ in 0..=MAX_SCAN_DEPTH {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).unwrap();

        let error = LocalRuntime::machine_with_bwrap(&bot, None, Vec::new())
            .err()
            .expect("a folder nested past the scan limit is refused");
        assert!(error.to_string().contains("nested"), "{error}");
    }

    #[test]
    fn a_hidden_root_inside_the_bot_folder_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let bot = directory.path().join("bot");
        let hidden = bot.join("config");
        std::fs::create_dir_all(&hidden).unwrap();

        let error = LocalRuntime::machine_with_bwrap(&bot, None, vec![hidden])
            .err()
            .expect("a configuration directory inside the Bot folder is refused");
        assert!(
            error.to_string().contains("configuration directory"),
            "{error}"
        );
    }

    #[test]
    fn a_bot_folder_inside_a_hidden_root_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let hidden = directory.path().join("config");
        let bot = hidden.join("bot");
        std::fs::create_dir_all(&bot).unwrap();

        let error = LocalRuntime::machine_with_bwrap(&bot, None, vec![hidden])
            .err()
            .expect("a Bot folder inside the configuration directory is refused");
        assert!(error.to_string().contains("hidden directory"), "{error}");
    }

    #[test]
    fn a_machine_is_probed_before_it_serves() {
        if !bwrap_enabled() {
            return;
        }
        LocalRuntime::probe_sandbox().expect("bubblewrap starts one empty sandbox");
    }

    #[tokio::test]
    async fn a_workspace_root_descriptor_opens_one_bot_folder() {
        if !bwrap_enabled() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let root = LocalRuntime::open_workspace(&workspace).unwrap();
        let runtime = LocalRuntime::machine_at(&root, "bot-id", Vec::new())
            .expect("the workspace root opens a Bot folder");
        assert!(workspace.join("bot-id").is_dir());

        let written = runtime
            .execute(Action::WriteFile {
                path: "hello.txt".to_owned(),
                content: "hi".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("bot-id/hello.txt")).unwrap(),
            "hi"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_special_file_does_not_block_a_read_or_a_write() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let fifo = runtime.workspace().join("pipe");
        make_fifo(&fifo);

        let read = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.execute(Action::ReadFile {
                path: "pipe".to_owned(),
            }),
        )
        .await
        .expect("a FIFO read must not block");
        assert_eq!(read.status, ActionStatus::Failed);
        assert!(
            read.output.text.contains("not a regular file"),
            "{}",
            read.output.text
        );

        let write = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.execute(Action::WriteFile {
                path: "pipe".to_owned(),
                content: "pwned".to_owned(),
            }),
        )
        .await
        .expect("a FIFO write must not block");
        assert_eq!(write.status, ActionStatus::Failed);
        assert!(
            write.output.text.contains("not a regular file"),
            "{}",
            write.output.text
        );

        let bytes = tokio::time::timeout(Duration::from_secs(5), runtime.read_bytes("pipe"))
            .await
            .expect("a FIFO byte read must not block");
        assert!(matches!(bytes, Err(ByteError::Failed(_))), "{bytes:?}");
        let bytes = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.write_bytes("pipe", b"pwned".to_vec()),
        )
        .await
        .expect("a FIFO byte write must not block");
        assert!(matches!(bytes, Err(ByteError::Failed(_))), "{bytes:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_read_follows_a_symlink_to_a_machine_file() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "readable through a link").unwrap();
        std::os::unix::fs::symlink(&outside, runtime.workspace().join("link.txt")).unwrap();

        let result = runtime
            .execute(Action::ReadFile {
                path: "link.txt".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert_eq!(result.output.text, "readable through a link");

        let bytes = runtime.read_bytes("link.txt").await.unwrap();
        assert_eq!(bytes, b"readable through a link");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_read_through_a_symlink_still_refuses_a_hidden_root() {
        let Some((_directory, runtime, hidden)) = machine_runtime_with_hidden() else {
            return;
        };
        let secret = hidden.join("computer.json");
        std::fs::write(&secret, "credential").unwrap();
        std::os::unix::fs::symlink(&secret, runtime.workspace().join("link.json")).unwrap();

        let result = runtime
            .execute(Action::ReadFile {
                path: "link.json".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(
            result.output.text.contains("hidden"),
            "{}",
            result.output.text
        );

        let bytes = runtime.read_bytes("link.json").await;
        assert!(
            bytes
                .as_ref()
                .is_err_and(|error| error.to_string().contains("hidden")),
            "{bytes:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_inherits_only_its_own_bot_folder() {
        let Some((_dir_a, runtime_a)) = machine_runtime() else {
            return;
        };
        let Some((_dir_b, runtime_b)) = machine_runtime() else {
            return;
        };
        let own = runtime_a.workspace().to_path_buf();
        let other = runtime_b.workspace().to_path_buf();
        // Files a Bot folder can have; the other runtime is only cached, never
        // used, so its descriptor must still be close-on-exec.
        let result = runtime_a
            .execute(Action::Shell {
                command: "ls -l /proc/self/fd".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        let text = result.output.text;
        for line in text.lines() {
            let Some(target) = line.split(" -> ").nth(1) else {
                continue;
            };
            let target = target.trim();
            // Only the sandbox's own stdio and /proc enumeration handles may
            // be open; no host directory, and no mount source descriptor.
            assert!(
                target == "/dev/null"
                    || target.starts_with("pipe:")
                    || target.starts_with("/proc/"),
                "the user command inherited a descriptor: {line}"
            );
        }
        assert!(
            !text.contains(own.to_string_lossy().as_ref()),
            "the Bot's own folder descriptor leaked into the command: {text}"
        );
        assert!(
            !text.contains(other.to_string_lossy().as_ref()),
            "another Bot's folder leaked into this shell: {text}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_spawned_again_mounts_the_same_folder() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let written = runtime
            .execute(Action::WriteFile {
                path: "marker.txt".to_owned(),
                content: "twice".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        for _ in 0..2 {
            let result = runtime
                .execute(Action::Shell {
                    command: "cat marker.txt".to_owned(),
                })
                .await;
            assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
            assert!(
                result.output.text.contains("twice"),
                "the folder is still mounted on this spawn: {}",
                result.output.text
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_returns_the_command_exit_code_and_stdout() {
        let Some((_dir, runtime)) = machine_runtime() else {
            return;
        };
        let result = runtime
            .execute(Action::Shell {
                command: "echo hi; exit 7".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
        assert!(
            result.output.text.contains("exit code: 7"),
            "{}",
            result.output.text
        );
        assert!(result.output.text.contains("hi"), "{}", result.output.text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_mounts_the_opened_bot_folder_after_its_path_moves() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let written = runtime
            .execute(Action::WriteFile {
                path: "marker.txt".to_owned(),
                content: "original".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        let folder = runtime.workspace().to_path_buf();
        let moved = directory.path().join("moved");
        std::fs::rename(&folder, &moved).unwrap();
        std::fs::create_dir(&folder).unwrap();

        let result = runtime
            .execute(Action::Shell {
                command: "cat marker.txt".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(
            result.output.text.contains("original"),
            "the shell mounts the opened folder, not the new path: {}",
            result.output.text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_does_not_follow_a_replaced_bot_folder_path() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let written = runtime
            .execute(Action::WriteFile {
                path: "marker.txt".to_owned(),
                content: "original".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        let folder = runtime.workspace().to_path_buf();
        let moved = directory.path().join("moved");
        std::fs::rename(&folder, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &folder).unwrap();

        let result = runtime
            .execute(Action::Shell {
                command: "cat marker.txt".to_owned(),
            })
            .await;
        match result.status {
            // The mounted object is still the opened folder, whose marker is
            // only reachable through the old root.
            ActionStatus::Succeeded => assert!(
                result.output.text.contains("original"),
                "{}",
                result.output.text
            ),
            // A replacement the sandbox cannot mount is refused, not followed.
            ActionStatus::Failed => {}
            other => panic!("unexpected status {other:?}: {}", result.output.text),
        }
    }

    #[test]
    fn a_byte_read_stops_a_file_that_grows_past_the_limit() {
        use std::io::Read as _;

        // A file that reported a small length and then grew: the reader keeps
        // producing bytes past the limit.
        let growing = std::io::repeat(0u8).take(MAX_FILE_BYTES + 1);
        let error = super::read_file_bounded(growing, MAX_FILE_BYTES, "growing.bin").unwrap_err();
        assert!(error.to_string().contains("larger than"), "{error}");
    }

    #[tokio::test]
    async fn a_byte_read_of_a_file_that_grows_never_returns_more_than_one_file() {
        use std::io::Write as _;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (_dir, runtime) = runtime();
        let path = runtime.workspace().join("growing.bin");
        std::fs::write(&path, "start").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let path = path.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) else {
                    return;
                };
                let chunk = vec![b'x'; 256 * 1024];
                let mut written = 0u64;
                while !stop.load(Ordering::Relaxed) && written <= MAX_FILE_BYTES + 2 * 1024 * 1024 {
                    if file.write_all(&chunk).is_err() {
                        break;
                    }
                    written += chunk.len() as u64;
                }
            })
        };
        let mut refused = false;
        for _ in 0..500 {
            match runtime.read_bytes("growing.bin").await {
                Ok(bytes) => {
                    assert!(
                        bytes.len() as u64 <= MAX_FILE_BYTES,
                        "a byte read returned more than one File"
                    );
                }
                Err(error) => {
                    assert!(error.to_string().contains("larger than"), "{error}");
                    refused = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(refused, "a file that grew past the limit is refused");
    }

    #[cfg(unix)]
    #[test]
    fn a_hidden_file_is_mounted_as_dev_null() {
        let directory = tempfile::tempdir().unwrap();
        let hidden_file = directory.path().join("credential.json");
        std::fs::write(&hidden_file, "session-secret").unwrap();
        let hidden_directory = directory.path().join("computer");
        std::fs::create_dir(&hidden_directory).unwrap();
        let folder = directory.path().join("bot");
        std::fs::create_dir(&folder).unwrap();

        let arguments = sandbox_args(
            folder.as_os_str(),
            &folder,
            &folder,
            &[hidden_file.clone(), hidden_directory.clone()],
            &[],
        );
        let shown: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let file = hidden_file.to_string_lossy().into_owned();
        let file_at = shown
            .iter()
            .position(|argument| argument == &file)
            .expect("the hidden file is mounted");
        assert_eq!(
            &shown[file_at - 2..file_at],
            ["--ro-bind", "/dev/null"],
            "a hidden file is replaced by /dev/null: {shown:?}"
        );
        let directory = hidden_directory.to_string_lossy().into_owned();
        let directory_at = shown
            .iter()
            .position(|argument| argument == &directory)
            .expect("the hidden directory is mounted");
        assert_eq!(shown[directory_at - 1], "--tmpfs", "{shown:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_machine_file_tool_refuses_a_single_hidden_file() {
        let directory = tempfile::tempdir().unwrap();
        let secret = directory.path().join("credential.json");
        std::fs::write(&secret, "session-secret-must-not-leak").unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let visible = directory.path().join("visible.txt");
        std::fs::write(&visible, "readable").unwrap();
        let runtime = file_runtime(&root, &secret);

        // A hidden file is refused by both read paths, and its bytes never
        // come back in the refusal.
        let read = runtime
            .execute(Action::ReadFile {
                path: secret.to_string_lossy().into_owned(),
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
        assert!(!read.output.text.contains("session-secret-must-not-leak"));

        let error = runtime
            .read_bytes(&secret.to_string_lossy())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the path is hidden from this Computer"),
            "{error}"
        );
        assert!(!error.contains("session-secret-must-not-leak"));

        let read = runtime
            .execute(Action::ReadFile {
                path: visible.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "readable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_shell_cannot_read_a_hidden_file() {
        let Some((_directory, runtime, secret)) = machine_runtime_with_hidden_file() else {
            return;
        };
        let command = format!("cat '{}'", secret.display());
        let result = runtime
            .execute(Action::Shell {
                command: command.clone(),
            })
            .await;
        // The sandbox mounts /dev/null over the file, so the command reads
        // nothing (an empty read, or a refusal) — but the secret never
        // crosses into the output.
        assert!(
            !result.output.text.contains("session-secret-must-not-leak"),
            "the hidden file's bytes must never reach the shell: {}",
            result.output.text
        );
        // The host file is untouched: only the sandbox's view changed.
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "session-secret-must-not-leak"
        );
    }

    /// A denied path that does not exist yet is refused once it appears.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_denied_file_is_refused_even_when_it_appears_later() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let hidden = directory.path().join("computer");
        std::fs::create_dir(&hidden).unwrap();
        let visible = directory.path().join("visible.txt");
        std::fs::write(&visible, "readable").unwrap();
        let late = directory.path().join("credential.json");

        // The runtime is built while the credential file does not exist; the
        // daemon's file tools must refuse it when it appears later.
        let runtime = file_runtime(&root, &hidden).with_denied(vec![late.clone()]);
        std::fs::write(&late, "session-secret-must-not-leak").unwrap();

        let read = runtime
            .execute(Action::ReadFile {
                path: late.to_string_lossy().into_owned(),
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
        assert!(!read.output.text.contains("session-secret-must-not-leak"));
        let error = runtime
            .read_bytes(&late.to_string_lossy())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the path is hidden from this Computer"),
            "{error}"
        );
        assert!(!error.contains("session-secret-must-not-leak"));

        // A write to a denied path inside the Bot's folder is refused too,
        // even before the file exists.
        let inside = root.join("credential.json");
        let runtime = file_runtime(&root, &hidden).with_denied(vec![inside.clone()]);
        let write = runtime
            .execute(Action::WriteFile {
                path: "credential.json".to_owned(),
                content: "new".to_owned(),
            })
            .await;
        assert_eq!(write.status, ActionStatus::Failed, "{write:?}");
        assert!(
            write
                .output
                .text
                .contains("the path is hidden from this Computer"),
            "{}",
            write.output.text
        );
        assert!(
            !inside.exists(),
            "the denied write must not create the file"
        );

        // A normal file stays readable and writable.
        let read = runtime
            .execute(Action::ReadFile {
                path: visible.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "readable");
    }

    /// A file created inside a hidden directory after the runtime is built is
    /// still out of the shell's reach, and the Workspace stays writable.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_created_later_inside_a_hidden_directory_stays_hidden() {
        let Some((_directory, runtime, hidden)) = machine_runtime_with_hidden() else {
            return;
        };
        let late = hidden.join("token.json");
        std::fs::write(&late, "channel-secret-must-not-leak").unwrap();

        let command = format!("ls {} && cat {}", hidden.display(), late.display());
        let result = runtime
            .execute(Action::Shell {
                command: command.clone(),
            })
            .await;
        assert!(
            !result.output.text.contains("channel-secret-must-not-leak"),
            "the hidden directory's late file must stay hidden: {}",
            result.output.text
        );
        // The Bot's Workspace is still writable.
        let write = runtime
            .execute(Action::WriteFile {
                path: "note.txt".to_owned(),
                content: "written".to_owned(),
            })
            .await;
        assert_eq!(write.status, ActionStatus::Succeeded, "{write:?}");
        let read = runtime
            .execute(Action::ReadFile {
                path: "note.txt".to_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "written");
    }
    #[tokio::test]
    async fn a_granted_folder_is_writable_outside_the_workspace() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let granted = directory.path().join("shared");
        std::fs::create_dir(&granted).unwrap();
        let runtime = runtime
            .with_machine_access(false, &[granted.to_string_lossy().into_owned()], None)
            .unwrap();
        let file = granted.join("written.txt");
        let written = runtime
            .execute(Action::WriteFile {
                path: file.to_string_lossy().into_owned(),
                content: "shared".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "shared");

        // A file outside both the workspace and the grant is refused.
        let outside = directory.path().join("outside.txt");
        let refused = runtime
            .execute(Action::WriteFile {
                path: outside.to_string_lossy().into_owned(),
                content: "no".to_owned(),
            })
            .await;
        assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");

        // The shell sees the same grant through the fence.
        let command = format!("echo shell > {}", granted.join("shell.txt").display());
        let shell = runtime.execute(Action::Shell { command }).await;
        assert_eq!(shell.status, ActionStatus::Succeeded, "{shell:?}");
        assert_eq!(
            std::fs::read_to_string(granted.join("shell.txt")).unwrap(),
            "shell\n"
        );
    }

    #[tokio::test]
    async fn a_hard_link_in_a_grant_is_not_writable_by_the_shell() {
        if !bwrap_enabled() {
            return;
        }
        let directory = outside_tempdir();
        let granted = directory.path().join("granted");
        std::fs::create_dir(&granted).unwrap();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "safe").unwrap();
        let runtime = LocalRuntime::machine(directory.path().join("bot"))
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
            .with_machine_access(false, &[granted.to_string_lossy().into_owned()], None)
            .unwrap();
        // The first scan leaves the cached grant descriptor at EOF; a second
        // scan must open a fresh directory stream, not read from that offset.
        let first = runtime
            .execute(Action::Shell {
                command: format!("printf ok > {}/ok.txt", granted.display()),
            })
            .await;
        assert_eq!(first.status, ActionStatus::Succeeded, "{first:?}");

        std::fs::hard_link(&outside, granted.join("late.txt")).unwrap();
        let second = runtime
            .execute(Action::Shell {
                command: format!("printf changed > {}/late.txt", granted.display()),
            })
            .await;
        assert_eq!(second.status, ActionStatus::Failed, "{second:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
    }

    #[tokio::test]
    async fn a_shell_mounts_every_grant_separately() {
        if !bwrap_enabled() {
            return;
        }
        let directory = outside_tempdir();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let runtime = LocalRuntime::machine(directory.path().join("bot"))
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
            .with_machine_access(
                false,
                &[
                    first.to_string_lossy().into_owned(),
                    second.to_string_lossy().into_owned(),
                ],
                None,
            )
            .unwrap();
        let command = format!(
            "printf first > {}/one; printf second > {}/two; printf own > {}/own.txt",
            first.display(),
            second.display(),
            directory.path().join("bot").display()
        );
        let result = runtime.execute(Action::Shell { command }).await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert_eq!(std::fs::read_to_string(first.join("one")).unwrap(), "first");
        assert_eq!(
            std::fs::read_to_string(second.join("two")).unwrap(),
            "second"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("bot").join("own.txt")).unwrap(),
            "own"
        );
        assert!(!first.join("two").exists());
        assert!(!second.join("one").exists());
    }

    /// Full access runs the shell as the machine user with no sandbox. The
    /// runtime's bwrap path does not exist, so a shell that started one would
    /// fail instead of writing.
    #[cfg(unix)]
    #[tokio::test]
    async fn full_access_runs_the_shell_without_a_sandbox() {
        let directory = outside_tempdir();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = full_access_runtime(&root, &root);
        let target = directory.path().join("outside.txt");
        let written = runtime
            .execute(Action::Shell {
                command: format!("printf free > {}", target.display()),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "free");

        // The shell still starts in the Bot's folder.
        let pwd = runtime
            .execute(Action::Shell {
                command: "pwd".to_owned(),
            })
            .await;
        assert_eq!(pwd.status, ActionStatus::Succeeded, "{pwd:?}");
        assert!(
            pwd.output.text.contains(root.to_str().unwrap()),
            "{}",
            pwd.output.text
        );
    }

    /// Full access writes with the machine user's own rights: any path the
    /// user can write is a write root.
    #[cfg(unix)]
    #[tokio::test]
    async fn full_access_writes_outside_the_workspace_without_a_grant() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = full_access_runtime(&root, &root);
        let file = directory.path().join("anywhere/note.txt");
        let written = runtime
            .execute(Action::WriteFile {
                path: file.to_string_lossy().into_owned(),
                content: "full".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "full");

        // A File take uses the same writer.
        let delivered = directory.path().join("delivered.bin");
        runtime
            .write_bytes(&delivered.to_string_lossy(), vec![0, 1, 2])
            .await
            .unwrap();
        assert_eq!(std::fs::read(&delivered).unwrap(), vec![0, 1, 2]);
    }

    /// Full access ignores the hidden roots: the owner accepted that the Bot
    /// can read the Computer's own configuration directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn full_access_reads_and_writes_a_hidden_root() {
        let directory = tempfile::tempdir().unwrap();
        let hidden = directory.path().join("computer");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("computer.json"), "dummy-credential").unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = full_access_runtime(&root, &hidden);

        let read = runtime
            .execute(Action::ReadFile {
                path: hidden.join("computer.json").to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "dummy-credential");
        let bytes = runtime
            .read_bytes(&hidden.join("computer.json").to_string_lossy())
            .await
            .unwrap();
        assert_eq!(bytes, b"dummy-credential");
        let listed = runtime
            .execute(Action::ListFiles {
                path: hidden.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(listed.status, ActionStatus::Succeeded, "{listed:?}");
        assert!(
            listed.output.text.contains("computer.json"),
            "{}",
            listed.output.text
        );

        let note = hidden.join("note.txt");
        let written = runtime
            .execute(Action::WriteFile {
                path: note.to_string_lossy().into_owned(),
                content: "written".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "written");
    }
    #[tokio::test]
    async fn a_run_directory_is_a_write_root() {
        if !bwrap_enabled() {
            return;
        }
        let directory = outside_tempdir();
        let work = directory.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let runtime = LocalRuntime::machine(directory.path().join("bot"))
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
            .with_machine_access(false, &[], Some(work.to_str().unwrap()))
            .unwrap();

        // The shell starts in the run's directory and writes relative paths
        // there.
        let pwd = runtime
            .execute(Action::Shell {
                command: "pwd".to_owned(),
            })
            .await;
        assert_eq!(pwd.status, ActionStatus::Succeeded, "{pwd:?}");
        assert!(
            pwd.output.text.contains(work.to_str().unwrap()),
            "{}",
            pwd.output.text
        );
        let shell_write = runtime
            .execute(Action::Shell {
                command: "printf shell > note.txt".to_owned(),
            })
            .await;
        assert_eq!(
            shell_write.status,
            ActionStatus::Succeeded,
            "{shell_write:?}"
        );
        assert_eq!(
            std::fs::read_to_string(work.join("note.txt")).unwrap(),
            "shell"
        );

        // The file tools resolve relative paths there and write through it.
        let tool_write = runtime
            .execute(Action::WriteFile {
                path: "tool.txt".to_owned(),
                content: "tool".to_owned(),
            })
            .await;
        assert_eq!(tool_write.status, ActionStatus::Succeeded, "{tool_write:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("tool.txt")).unwrap(),
            "tool"
        );
        let read = runtime
            .execute(Action::ReadFile {
                path: "tool.txt".to_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "tool");

        // The run's directory is the only extra write root: a sibling stays
        // outside the fence.
        let outside = directory.path().join("outside.txt");
        let refused = runtime
            .execute(Action::WriteFile {
                path: outside.to_string_lossy().into_owned(),
                content: "no".to_owned(),
            })
            .await;
        assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
        assert!(!outside.exists());
        let error = runtime
            .write_bytes(&outside.to_string_lossy(), b"no".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("absolute"), "{error}");
    }

    /// A run's directory is a write root for the file tools without a sandbox:
    /// relative paths resolve there and writes below it succeed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_directory_is_a_write_root_for_the_file_tools() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let work = directory.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("report.txt"), "from the run").unwrap();
        let runtime = file_runtime_without_hidden(&root)
            .with_machine_access(false, &[], Some(work.to_str().unwrap()))
            .unwrap();

        let read = runtime
            .execute(Action::ReadFile {
                path: "report.txt".to_owned(),
            })
            .await;
        assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
        assert_eq!(read.output.text, "from the run");
        let written = runtime
            .execute(Action::WriteFile {
                path: "note.txt".to_owned(),
                content: "note".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("note.txt")).unwrap(),
            "note"
        );

        // A File take's relative path goes through the same write root.
        runtime
            .write_bytes("deep/report.bin", vec![9])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(work.join("deep/report.bin")).unwrap(),
            vec![9]
        );

        // A path outside the run's directory is still refused.
        let outside = directory.path().join("outside.bin");
        let error = runtime
            .write_bytes(&outside.to_string_lossy(), b"x".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("absolute"), "{error}");
        assert!(!outside.exists());
    }

    /// A run directory must name an existing folder.
    #[cfg(unix)]
    #[test]
    fn a_run_directory_that_is_not_a_folder_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();

        let missing = directory.path().join("missing");
        let error = file_runtime_without_hidden(&root)
            .with_machine_access(false, &[], Some(missing.to_str().unwrap()))
            .err()
            .expect("a missing run directory is refused");
        assert!(matches!(error, RuntimeError::Io(_)), "{error}");

        let file = directory.path().join("plain.txt");
        std::fs::write(&file, "not a folder").unwrap();
        let error = file_runtime_without_hidden(&root)
            .with_machine_access(false, &[], Some(file.to_str().unwrap()))
            .err()
            .expect("a plain file is not a run directory");
        assert!(
            matches!(&error, RuntimeError::Access(message) if message.contains("not a folder")),
            "{error}"
        );
    }

    /// A hidden run directory is refused: its write root would re-expose what
    /// the sandbox hides.
    #[cfg(unix)]
    #[test]
    fn a_hidden_run_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let hidden = directory.path().join("computer");
        std::fs::create_dir(&hidden).unwrap();
        let error = file_runtime(&root, &hidden)
            .with_machine_access(false, &[], Some(hidden.to_str().unwrap()))
            .err()
            .expect("a hidden run directory is refused");
        assert!(
            error
                .to_string()
                .contains("the folder is hidden from this Computer"),
            "{error}"
        );
    }

    /// Write roots are kept as candidates: the Bot's folder covers a root
    /// inside it, but an outer root that the shell's scan refuses must not take
    /// a clean inner root down with it.
    #[cfg(unix)]
    #[test]
    fn nested_write_roots_stay_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let granted = directory.path().join("granted");
        std::fs::create_dir(&granted).unwrap();

        // A run directory inside the Bot's folder adds no root: the folder
        // already covers it.
        let inside_root = root.join("work");
        std::fs::create_dir(&inside_root).unwrap();
        let runtime = file_runtime_without_hidden(&root)
            .with_machine_access(false, &[], Some(inside_root.to_str().unwrap()))
            .unwrap();
        assert!(runtime.write_roots.is_empty());

        // A run directory inside a grant stays a candidate: the shell decides
        // after its scan which of the two it can mount.
        let inside_grant = granted.join("work");
        std::fs::create_dir(&inside_grant).unwrap();
        let runtime = file_runtime_without_hidden(&root)
            .with_machine_access(
                false,
                &[granted.to_string_lossy().into_owned()],
                Some(inside_grant.to_str().unwrap()),
            )
            .unwrap();
        let canonical_granted = std::fs::canonicalize(&granted).unwrap();
        let canonical_inside = std::fs::canonicalize(&inside_grant).unwrap();
        assert_eq!(runtime.write_roots.len(), 2);
        assert_eq!(runtime.write_roots[0].0, canonical_granted);
        assert_eq!(runtime.write_roots[1].0, canonical_inside);

        // Of two roots, the outer one covers the inner one for the shell, but
        // both stay as file-tool candidates because the outer scan may fail.
        let inner = granted.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let runtime = file_runtime_without_hidden(&root)
            .with_machine_access(
                false,
                &[
                    inner.to_string_lossy().into_owned(),
                    granted.to_string_lossy().into_owned(),
                ],
                None,
            )
            .unwrap();
        assert_eq!(runtime.write_roots.len(), 2);
        assert!(
            runtime
                .write_roots
                .iter()
                .any(|(path, _)| path == &canonical_granted)
        );
        assert!(
            runtime
                .write_roots
                .iter()
                .any(|(path, _)| path == &std::fs::canonicalize(&inner).unwrap())
        );
    }

    /// The fenced constructor refuses what a full-access one must accept: a Bot
    /// folder holding a symlink or a hard link, on a machine where bubblewrap
    /// is not even looked for.
    #[cfg(unix)]
    #[test]
    fn a_full_access_folder_opens_without_a_scan_or_a_probe() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let bot = "01890a5d-ac96-774b-bcce-b302099a8057";
        let folder = workspace.join(bot);
        std::fs::create_dir(&folder).unwrap();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("link")).unwrap();
        std::fs::hard_link(&outside, folder.join("hard")).unwrap();

        let root = LocalRuntime::open_workspace(&workspace).unwrap();
        assert!(
            LocalRuntime::machine_at(&root, bot, Vec::new()).is_err(),
            "the fenced path refuses a folder that holds a link"
        );
        let runtime = LocalRuntime::full_access(&root, bot)
            .expect("full access opens the folder without a scan or a probe");

        assert!(runtime.full_access);
        assert!(runtime.hidden.is_empty());
        assert!(matches!(runtime.mode, Mode::Machine { bwrap: None, .. }));
        assert_eq!(runtime.workspace(), std::fs::canonicalize(&folder).unwrap());
    }

    /// A full-access command ends the descendants it left behind in another
    /// process group or below a second fork, not only the group the shell
    /// started.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_full_access_shell_ends_a_background_process_in_its_own_session() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = full_access_runtime(&root, &root);
        let session = directory.path().join("session-alive");
        let double_fork = directory.path().join("double-fork-alive");
        let command = format!(
            "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & \
             (setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 &)",
            session.display(),
            double_fork.display()
        );

        let result = runtime.execute(Action::Shell { command }).await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !session.exists(),
            "a full-access command ends its own-session descendant when it ends"
        );
        assert!(
            !double_fork.exists(),
            "a full-access command ends a double-forked descendant when it ends"
        );
    }

    /// A timed-out full-access command also ends a descendant in another
    /// process group: only the process group would not reach it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_full_access_timeout_ends_a_background_process_in_its_own_session() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = full_access_runtime(&root, &root).with_timeout(Duration::from_millis(200));
        let marker = directory.path().join("alive");
        let command = format!(
            "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & sleep 30",
            marker.display()
        );

        let result = runtime.execute(Action::Shell { command }).await;
        assert_eq!(result.status, ActionStatus::TimedOut, "{result:?}");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !marker.exists(),
            "a timed-out full-access command ends its whole process tree"
        );
    }

    /// A channel close kills a full-access command's whole tree, including a
    /// descendant that left its process group.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelling_a_full_access_shell_ends_its_whole_tree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let runtime = std::sync::Arc::new(full_access_runtime(&root, &root));
        let started = directory.path().join("started");
        let marker = directory.path().join("alive");
        let command = format!(
            "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & echo started > {}; sleep 30",
            marker.display(),
            started.display()
        );
        let running = tokio::spawn({
            let runtime = std::sync::Arc::clone(&runtime);
            async move {
                runtime
                    .execute(Action::Shell {
                        command: command.clone(),
                    })
                    .await
            }
        });
        for _ in 0..200 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started.exists(), "the shell started");
        assert!(runtime.cancel_in_flight());
        let result = running.await.unwrap();
        assert_ne!(result.status, ActionStatus::Succeeded, "{result:?}");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !marker.exists(),
            "a cancelled full-access command ends its whole process tree"
        );
    }

    /// The cached runtime is reused between commands, so the Bot's folder must
    /// be scanned again for every shell: the host may have added a hard link
    /// after the runtime opened the folder.
    #[tokio::test]
    async fn a_cached_runtime_scans_the_bot_folder_before_every_shell() {
        let Some((directory, runtime)) = machine_runtime() else {
            return;
        };
        let folder = runtime.workspace().to_path_buf();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "safe").unwrap();

        let first = runtime
            .execute(Action::Shell {
                command: format!("printf first > {}/first.txt", folder.display()),
            })
            .await;
        assert_eq!(first.status, ActionStatus::Succeeded, "{first:?}");

        // The host adds a hard link to a file outside the folder between two
        // commands that reuse this runtime.
        std::fs::hard_link(&outside, folder.join("late.txt")).unwrap();
        let second = runtime
            .execute(Action::Shell {
                command: format!("printf changed > {}/late.txt", folder.display()),
            })
            .await;
        assert_eq!(second.status, ActionStatus::Failed, "{second:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
    }

    /// A clean inner grant still gets its own mount when an outer grant the
    /// shell's scan refuses would cover it.
    #[tokio::test]
    async fn a_clean_inner_root_survives_a_refused_outer_root() {
        if !bwrap_enabled() {
            return;
        }
        // The grants sit outside `/tmp`, which the fence replaces with a fresh
        // tmpfs; only a visible directory proves the shell's write.
        let directory = outside_tempdir();
        let outer = directory.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "safe").unwrap();
        std::fs::hard_link(&outside, outer.join("linked.txt")).unwrap();
        let runtime = LocalRuntime::machine(directory.path().join("bot"))
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
            .with_machine_access(
                false,
                &[
                    outer.to_string_lossy().into_owned(),
                    inner.to_string_lossy().into_owned(),
                ],
                None,
            )
            .unwrap();

        // The outer root holds a hard link, so it is not mounted; the inner
        // root is clean and must still be writable for the shell.
        let shell = runtime
            .execute(Action::Shell {
                command: format!("printf inner > {}/note.txt", inner.display()),
            })
            .await;
        assert_eq!(shell.status, ActionStatus::Succeeded, "{shell:?}");
        assert_eq!(
            std::fs::read_to_string(inner.join("note.txt")).unwrap(),
            "inner"
        );

        // The refused outer root is not a shell write root.
        let refused = runtime
            .execute(Action::Shell {
                command: format!("printf changed > {}/linked.txt", outer.display()),
            })
            .await;
        assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
    }

    /// Access roots are opened for each access decision, not held from an older
    /// one: a run directory archived and rebuilt under the same path must be
    /// written through its new inode.
    #[cfg(unix)]
    #[tokio::test]
    async fn access_roots_are_reopened_for_every_decision() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bot");
        std::fs::create_dir(&root).unwrap();
        let work = directory.path().join("run");
        std::fs::create_dir(&work).unwrap();
        let base = file_runtime_without_hidden(&root);

        let first = base
            .with_machine_access(false, &[], Some(work.to_str().unwrap()))
            .unwrap();
        let written = first
            .execute(Action::WriteFile {
                path: "note.txt".to_owned(),
                content: "first".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("note.txt")).unwrap(),
            "first"
        );

        // Between two runs the directory is archived and rebuilt under the same
        // path. A descriptor held by a cached runtime would still name the old
        // inode.
        let archived = directory.path().join("archived");
        std::fs::rename(&work, &archived).unwrap();
        std::fs::create_dir(&work).unwrap();
        let second = base
            .with_machine_access(false, &[], Some(work.to_str().unwrap()))
            .unwrap();
        let written = second
            .execute(Action::WriteFile {
                path: "note.txt".to_owned(),
                content: "second".to_owned(),
            })
            .await;
        assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("note.txt")).unwrap(),
            "second"
        );
        assert_eq!(
            std::fs::read_to_string(archived.join("note.txt")).unwrap(),
            "first"
        );
    }
}
