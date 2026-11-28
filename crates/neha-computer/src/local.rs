//! The in-process Computer runtime.
//!
//! It executes real POSIX shell commands and real file operations. In managed
//! mode everything stays inside one workspace directory: not isolated, not
//! transported, and only for a Space on the same machine. A hosted runtime
//! keeps the same file rules but runs its shell in a minimal bubblewrap rootfs,
//! so a Space can give a Bot an isolated Computer on its own machine. In real
//! mode it is one Bot's view of a real machine: reads reach what the machine
//! user can read, writes reach the Bot's folder, the owner's grants, and the
//! run's own directory, and the shell runs under bubblewrap. Hidden roots are
//! the exception to both reads and writes: the Computer's own directory, which
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
/// The system directories a shell searches for its commands.
const SHELL_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// The sandbox's own `/tmp`, which a fenced shell gets instead of the host's.
const SANDBOX_TMPDIR: &str = "/tmp";
/// The directory a Hosted sandbox mounts this build's `neha` binary at. The
/// skill's commands call `neha` by name, and the Space's installation is not
/// inside the minimal rootfs.
const HOSTED_CLI_DIR: &str = "/opt/neha-cli";
/// The one writable folder in a Hosted sandbox: the runtime's workspace.
const HOSTED_WORKSPACE: &str = "/workspace";

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
    /// The bubblewrap binary a Hosted runtime starts its shell under, or
    /// `None` for a managed workspace or a real machine. The file tools keep
    /// the managed rules either way; this field changes only the shell.
    hosted: Option<PathBuf>,
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
        Self::managed(workspace.into())
    }

    /// Opens a Hosted workspace: a managed workspace whose shell runs in a
    /// minimal bubblewrap rootfs.
    ///
    /// The workspace is created and resolved like a managed one, and it is the
    /// only writable place in the sandbox, where it appears at `/workspace`.
    /// The sandbox is probed here, so a Space never promises a Hosted Computer
    /// on a machine where bubblewrap cannot start it.
    pub fn hosted(workspace: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        Self::hosted_with_hidden(workspace, Vec::new())
    }

    /// Opens a Hosted workspace with the roots its sandbox must not expose.
    ///
    /// A Hosted sandbox mounts the Space machine's system directories and the
    /// CLI's directory read-only. If the Space home lives under one of those
    /// mounts, the sandbox would expose the record, secrets, or another
    /// Workspace despite the read-only mount, so this constructor refuses that
    /// layout instead of starting it. The Space passes its home here.
    pub fn hosted_with_hidden(
        workspace: impl Into<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let requested = workspace.into();
        std::fs::create_dir_all(&requested)?;
        let workspace = std::fs::canonicalize(&requested)?;
        // The path is built from the Space's resolved home and a Computer id,
        // so a difference after resolving means a symlink sits on it. A later
        // bind would follow that link and expose or write another directory.
        Self::hosted_checked(requested, workspace, hidden)
    }

    fn hosted_checked(
        requested: PathBuf,
        workspace: PathBuf,
        hidden: Vec<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        if requested != workspace {
            return Err(RuntimeError::UnsafeFolder(format!(
                "the Hosted Workspace path uses a symlink: {}",
                requested.display()
            )));
        }
        let hidden: Vec<PathBuf> = hidden
            .into_iter()
            .map(|root| std::fs::canonicalize(&root).map_err(RuntimeError::Io))
            .collect::<Result<_, _>>()?;
        validate_hosted_mounts(&hidden, default_cli_dir().as_deref())?;
        // The old managed runtime ran with the Space user's rights, so a
        // Workspace can already hold a hard link or symlink to a host file.
        // Refuse it before the writable bind: a reset is the owner's repair.
        #[cfg(unix)]
        {
            let folder = Self::open_workspace(&workspace)?;
            scan_bot_folder(&folder, &workspace).map_err(|error| {
                RuntimeError::UnsafeFolder(format!(
                    "{error}; reset the Hosted Computer to clear its Workspace"
                ))
            })?;
        }
        let mut runtime = Self::managed(workspace)?;
        let bwrap = find_bwrap().ok_or_else(no_sandbox)?;
        probe_hosted(&bwrap, &runtime.root)?;
        runtime.hosted = Some(bwrap);

        Ok(runtime)
    }

    /// Creates and resolves a managed workspace and its temporary folder.
    fn managed(workspace: PathBuf) -> Result<Self, RuntimeError> {
        std::fs::create_dir_all(&workspace)?;
        let workspace = std::fs::canonicalize(&workspace)?;
        let temporary = workspace.join(".tmp");
        std::fs::create_dir_all(&temporary)?;

        Ok(Self {
            root: workspace,
            mode: Mode::Managed,
            hosted: None,
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
            hosted: None,
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

        // Windows has no bubblewrap. A Windows Computer uses a managed folder:
        // its file tools stay inside the Bot's folder, while its shell runs
        // with the machine user's rights and is not fenced. The daemon must
        // not claim the Linux sandbox on this path.
        #[cfg(windows)]
        {
            let _ = bwrap;
            std::fs::create_dir_all(&folder)?;
            let root = std::fs::canonicalize(&folder)?;
            let hidden = hidden
                .into_iter()
                .map(|path| std::fs::canonicalize(path).map_err(RuntimeError::Io))
                .collect::<Result<Vec<_>, _>>()?;
            let mut runtime = Self::managed(root)?;
            runtime.hidden = hidden;
            return Ok(runtime);
        }

        #[cfg(not(windows))]
        {
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
            hosted: None,
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
            hosted: self.hosted.clone(),
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
        // The managed Windows runtime has no OS fence and no directory
        // descriptors. Refuse a widened access model instead of silently
        // serving a narrower one than the owner asked for.
        #[cfg(windows)]
        {
            if full_access {
                return Err(RuntimeError::Access(
                    "full access is not available for a Windows Local Computer".to_owned(),
                ));
            }
            if cwd.is_some() {
                return Err(RuntimeError::Access(
                    "a run directory is not available for a Windows Local Computer".to_owned(),
                ));
            }
            if !grants.is_empty() {
                return Err(RuntimeError::Access(
                    "granted folders are not available for a Windows Local Computer".to_owned(),
                ));
            }
        }
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
            // The Space already trimmed the grant at the edge where the owner
            // typed it and folds the path without touching component names
            // (`logs ` and `logs` stay two folders). Trimming again here would
            // grant the wrong folder for a name with a trailing space.
            //
            // Open first, then judge the descriptor: a path replaced between a
            // check and the open could otherwise point the grant somewhere
            // else. The open never blocks, requires a directory at every
            // component, and follows no symlink, so a grant that is or became
            // a link is skipped instead of authorizing the link's target.
            let Ok((root, file)) = open_write_root(grant) else {
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
        // As with a grant, the Space hands over an already normalized path:
        // this edge trims nothing, so a name with a trailing space still names
        // its own folder.
        let (path, file) = open_write_root(directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotADirectory {
                RuntimeError::Access(format!(
                    "the working directory is not a folder: {directory}"
                ))
            } else if is_symlink_error(&error) {
                RuntimeError::Access(format!(
                    "the working directory uses a symlink, which is not followed: {directory}"
                ))
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

    /// Sets the directory a run works in.
    ///
    /// The directory must exist; it is never created and never becomes a
    /// writable root. Relative file paths resolve against it and the shell
    /// starts there. A machine runtime takes its per-request directory through
    /// [`with_machine_access`](LocalRuntime::with_machine_access) instead: that
    /// one also opens it now, so a rename cannot move the run, and, without
    /// full access, makes it this run's one-off write root.
    ///
    /// A Hosted runtime only accepts a directory inside its workspace: the
    /// sandbox sees the workspace at `/workspace` and no other host path
    /// reaches it, so a host working directory would name nothing there.
    pub fn with_working_dir(mut self, directory: &str) -> Result<Self, RuntimeError> {
        let path = std::fs::canonicalize(directory.trim()).map_err(RuntimeError::Io)?;
        let metadata = std::fs::metadata(&path).map_err(RuntimeError::Io)?;
        if !metadata.is_dir() {
            return Err(RuntimeError::Access(format!(
                "the working directory is not a folder: {}",
                path.display()
            )));
        }
        if self.is_hidden(&path) || self.is_denied(&path) {
            return Err(RuntimeError::Access(format!(
                "the folder is hidden from this Computer: {}",
                path.display()
            )));
        }
        if self.hosted.is_some() && !path.starts_with(&self.root) {
            return Err(RuntimeError::Access(format!(
                "a Hosted working directory must stay inside its workspace: {}",
                path.display()
            )));
        }
        self.cwd = Some(path);
        Ok(self)
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
                Ok(metadata) if is_reparse_point(&metadata) => {
                    return Err("the path uses a reparse point, which is not followed".to_owned());
                }
                Ok(metadata) if is_hard_link(&current, &metadata) => {
                    return Err("the path uses a hard link, which is not followed".to_owned());
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
                let relative = path
                    .strip_prefix(&self.root)
                    .map_err(|_| ByteError::Failed("the path escapes the workspace".to_owned()))?;
                #[cfg(unix)]
                {
                    // Walk from the Workspace root descriptor and refuse every
                    // symlink on the way: no path is checked and then opened.
                    let root = std::fs::File::open(&self.root).map_err(|error| {
                        ByteError::Failed(format!("cannot open {}: {error}", self.root.display()))
                    })?;
                    write_machine_file(&root, &relative.to_string_lossy(), bytes)?;
                }
                #[cfg(not(unix))]
                {
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
                }

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
            Mode::Managed => self.open_managed_read(path),
            Mode::Machine { .. } => self.open_machine_read(path),
        }
    }

    /// Opens a managed file by walking from the Workspace root through
    /// directory descriptors with `O_NOFOLLOW`, so another Bot that swaps a
    /// path component between the check and the open cannot move the read
    /// outside the Computer.
    #[cfg(unix)]
    fn open_managed_read(&self, path: &Path) -> Result<(std::fs::File, std::fs::Metadata), String> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| "the path escapes the workspace".to_owned())?;
        let mut directory = std::fs::File::open(&self.root)
            .map_err(|error| format!("cannot open {}: {error}", self.root.display()))?;
        let mut components: Vec<_> = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_os_string()),
                _ => None,
            })
            .collect();
        let name = components
            .pop()
            .ok_or_else(|| "the path does not name a file".to_owned())?;
        for component in components {
            directory = open_directory_at(&directory, &component)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        }
        let name = c_name(&name)?;
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `directory` is a live directory descriptor and `name` is
        // NUL-terminated; a successful return is a fresh descriptor owned here.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ELOOP) => {
                    format!(
                        "the path uses a symlink, which is not followed: {}",
                        path.display()
                    )
                }
                Some(libc::ENXIO) => format!("{} is not a regular file", path.display()),
                _ => format!("cannot read {}: {error}", path.display()),
            });
        }
        // SAFETY: `fd` is a fresh descriptor from `openat` and is owned here.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() > 1 {
                return Err(format!(
                    "the path uses a hard link, which is not followed: {}",
                    path.display()
                ));
            }
        }
        Ok((file, metadata))
    }

    /// Managed reads are Unix's descriptor walk; another platform keeps the
    /// check-then-open path because it has no Hosted worker.
    #[cfg(not(unix))]
    fn open_managed_read(&self, path: &Path) -> Result<(std::fs::File, std::fs::Metadata), String> {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Ok((file, metadata))
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

    /// Windows has no `/proc/self/fd`, and its managed runtime does not use
    /// this method. Keep the match exhaustive with a clear refusal.
    #[cfg(not(unix))]
    fn open_machine_read(&self, path: &Path) -> Result<(std::fs::File, std::fs::Metadata), String> {
        Err(format!(
            "a real-machine read is not available on this platform: {}",
            path.display()
        ))
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
            Mode::Managed => match self.list_managed_directory(&path) {
                Ok(lines) => lines,
                Err(error) => return ActionResult::failed(error),
            },
            Mode::Machine { .. } => match self.list_machine_directory(&path) {
                Ok(lines) => lines,
                Err(error) => return ActionResult::failed(error),
            },
        };

        ActionResult::succeeded(truncate(lines.join("\n")))
    }

    /// Lists one open directory, so an entry's kind and the listing come from
    /// the same descriptor.
    #[cfg(unix)]
    fn list_managed_directory(&self, path: &Path) -> Result<Vec<String>, String> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| "the path escapes the workspace".to_owned())?;
        let mut directory = std::fs::File::open(&self.root)
            .map_err(|error| format!("cannot open {}: {error}", self.root.display()))?;
        for component in relative.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            directory = open_directory_at(&directory, part)
                .map_err(|error| format!("cannot list {}: {error}", self.relative(path)))?;
        }
        list_opened_directory(&directory, &self.relative(path))
    }

    /// Managed listings use the descriptor walk; another platform keeps the
    /// path check because it has no Hosted worker.
    #[cfg(not(unix))]
    fn list_managed_directory(&self, path: &Path) -> Result<Vec<String>, String> {
        match std::fs::read_dir(path) {
            Ok(entries) => Ok(list_entries(entries)),
            Err(error) => Err(format!("cannot list {}: {error}", self.relative(path))),
        }
    }

    /// Lists one directory of a real machine through the descriptor it opened.
    ///
    /// The directory is opened once with `O_DIRECTORY | O_NOFOLLOW`, checked
    /// against the hidden roots, and then walked through that same descriptor:
    /// a path swapped in between the check and the walk cannot change what is
    /// listed.
    #[cfg(unix)]
    fn list_machine_directory(&self, path: &Path) -> Result<Vec<String>, String> {
        use std::os::fd::FromRawFd as _;

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
        list_opened_directory(&directory, &self.relative(path))
    }

    /// Lists a directory on a system without real-drive descriptors.
    #[cfg(not(unix))]
    fn list_machine_directory(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("a real Computer lists only on Unix".to_owned())
    }

    /// Builds the shell process for this mode, without starting it.
    ///
    /// A Hosted shell runs in a minimal rootfs: bubblewrap mounts the workspace
    /// at `/workspace` and changes into it, so the host process's own working
    /// directory is never involved. A granted shell rechecks what it is about to
    /// mount: the Bot's folder and every candidate write root are scanned now,
    /// because the machine's host may have added a link since the runtime opened
    /// them, and only the accepted roots are mounted. A full-access shell builds
    /// no fence at all.
    fn shell_command(&self, command: &str) -> Result<Command, ActionResult> {
        let mut process = if let Some(bwrap) = &self.hosted {
            let mut process = Command::new(bwrap);
            process
                .args(hosted_sandbox_args(
                    bwrap,
                    &self.root,
                    self.cwd.as_deref(),
                    self.cli_dir.as_deref(),
                ))
                .arg("/usr/bin/sh")
                .arg("-c")
                .arg(command);
            process
        } else {
            match &self.mode {
                Mode::Managed => {
                    #[cfg(windows)]
                    {
                        windows_shell(command, &self.root)
                    }
                    #[cfg(not(windows))]
                    {
                        let mut process = Command::new("sh");
                        process.arg("-c").arg(command).current_dir(&self.root);
                        process
                    }
                }
                // Full access has no fence to build: the command runs as the
                // machine user from the run's directory, and bubblewrap is never
                // started. Its wrapper ends every process the command leaves
                // behind, including one that moved to its own process group.
                Mode::Machine { .. } if self.full_access => {
                    full_access_shell_command(command, self.base())
                }
                #[cfg(windows)]
                Mode::Machine { .. } => {
                    // Windows never constructs a Machine runtime; it uses the
                    // managed folder. Keep the arm unreachable rather than
                    // compiling Unix descriptor and bubblewrap code here.
                    let mut process = Command::new("cmd.exe");
                    process.args(["/C", "exit 1"]);
                    process
                }
                #[cfg(not(windows))]
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
                    let (write_roots, grant_fds): (
                        Vec<(OsString, PathBuf)>,
                        Vec<libc::c_int>,
                    ) = {
                        // A write root the scan refuses is not mounted writable
                        // for the shell: a hard link or symlink in it could
                        // otherwise change a file outside the root. The accepted
                        // roots are deduped here, after the scan: an outer root
                        // the scan refuses must not take a clean inner root down
                        // with it.
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
                                        OsString::from(format!(
                                            "/proc/self/fd/{}",
                                            file.as_raw_fd()
                                        )),
                                        root.clone(),
                                    ),
                                    file.as_raw_fd(),
                                )
                            })
                            .unzip()
                    };
                    #[cfg(not(unix))]
                    let (write_roots, grant_fds): (
                        Vec<(OsString, PathBuf)>,
                        Vec<i32>,
                    ) = (Vec::new(), Vec::new());
                    #[cfg(windows)]
                    let _ = &grant_fds;
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
                                for fd in &grant_fds {
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
            }
        };
        let (home, temporary) = if self.hosted.is_some() {
            let home = self
                .cwd
                .as_deref()
                .and_then(|cwd| cwd.strip_prefix(&self.root).ok())
                .map(|relative| {
                    let mut path = PathBuf::from(HOSTED_WORKSPACE);
                    path.push(relative);
                    path
                })
                .unwrap_or_else(|| PathBuf::from(HOSTED_WORKSPACE));
            (home, PathBuf::from(SANDBOX_TMPDIR))
        } else {
            let temporary = match &self.mode {
                Mode::Managed => self.root.join(".tmp"),
                Mode::Machine { .. } => PathBuf::from(SANDBOX_TMPDIR),
            };
            (self.root.clone(), temporary)
        };
        #[cfg(windows)]
        let path = {
            let inherited = std::env::var_os("PATH").unwrap_or_default();
            match &self.cli_dir {
                Some(directory) => {
                    format!("{};{}", directory.display(), inherited.to_string_lossy())
                }
                None => inherited.to_string_lossy().into_owned(),
            }
        };
        #[cfg(not(windows))]
        let path = if self.hosted.is_some() {
            match &self.cli_dir {
                // The CLI is mounted at a fixed sandbox path, not where it
                // sits on the host.
                Some(_) => format!("{HOSTED_CLI_DIR}:{SHELL_PATH}"),
                None => SHELL_PATH.to_owned(),
            }
        } else {
            match &self.cli_dir {
                Some(directory) => format!("{}:{SHELL_PATH}", directory.display()),
                None => SHELL_PATH.to_owned(),
            }
        };
        process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A Bot gets Windows' system variables it needs to start a shell, not
        // the daemon's environment: NEHA_MASTER_KEY, provider keys, and proxy
        // credentials must not reach a command.
        #[cfg(windows)]
        {
            process.env_clear();
            for key in [
                "SystemRoot",
                "SystemDrive",
                "windir",
                "COMSPEC",
                "PATHEXT",
                "APPDATA",
                "LOCALAPPDATA",
                "PROGRAMDATA",
                "PROGRAMFILES",
                "PROGRAMFILES(X86)",
                "COMMONPROGRAMFILES",
                "COMMONPROGRAMFILES(X86)",
                "PSModulePath",
                "OS",
                "NUMBER_OF_PROCESSORS",
                "PROCESSOR_ARCHITECTURE",
                "PROCESSOR_IDENTIFIER",
                "USERNAME",
                "USERDOMAIN",
                "COMPUTERNAME",
            ] {
                if let Some(value) = std::env::var_os(key) {
                    process.env(key, value);
                }
            }
            process
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("TMP", &temporary)
                .env("TEMP", &temporary)
                .env("PATH", path);
        }
        #[cfg(not(windows))]
        {
            process
                .env_clear()
                .env("PATH", path)
                .env("HOME", home)
                .env("TMPDIR", temporary);
        }
        process
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
        // The guard owns the shell until this task disarms it after its own
        // cleanup. A future dropped mid-run (a closed channel, a shut-down
        // worker) then finds the pid still registered and ends the whole
        // process tree before the child below it is released.
        let mut in_flight = InFlight::start(&self.in_flight, pid);
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
                in_flight.disarm();
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
        // ended the descendants that left its process group. The tree is
        // cleaned here, so the guard no longer owns it.
        kill_group(pid);
        in_flight.disarm();
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

/// Checks that one `bwrap` binary runs at all.
fn check_bwrap_version(bwrap: &Path) -> Result<(), String> {
    let version = std::process::Command::new(bwrap)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("bwrap cannot run: {error}; install bubblewrap"))?;
    if !version.status.success() {
        return Err("bwrap --version failed; install a working bubblewrap".to_owned());
    }

    Ok(())
}

/// Starts one empty sandbox, so a Computer never serves a machine where
/// bubblewrap is installed but cannot run.
fn probe_bwrap(bwrap: &Path, folder: &Path, hidden: &[PathBuf]) -> Result<(), String> {
    check_bwrap_version(bwrap)?;
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

/// Starts one empty Hosted sandbox, so a Space never promises a Hosted
/// Computer on a machine where bubblewrap cannot mount the minimal rootfs.
///
/// `workspace` must exist: it is bound into the sandbox as `/workspace`, and
/// the probe fails the way a real action would if the path cannot be mounted.
pub fn probe_hosted(bwrap: &Path, workspace: &Path) -> Result<(), RuntimeError> {
    check_bwrap_version(bwrap).map_err(RuntimeError::NoSandbox)?;
    let output = std::process::Command::new(bwrap)
        .args(hosted_sandbox_args(bwrap, workspace, None, None))
        .arg("/usr/bin/sh")
        .arg("-c")
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            RuntimeError::NoSandbox(format!("bwrap cannot run: {error}; install bubblewrap"))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(RuntimeError::NoSandbox(format!(
        "bwrap cannot start a Hosted sandbox: {}",
        stderr.lines().next().unwrap_or("no error output")
    )))
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

/// Refuses a Hosted layout whose read-only mounts would expose a protected
/// root.
///
/// The sandbox mounts the system directories and the CLI's directory. The
/// Space home must not sit inside either mount, and the CLI directory must not
/// sit inside the Space home: in both cases the read-only mount would still
/// reveal the record, a secret, or another Workspace.
fn validate_hosted_mounts(hidden: &[PathBuf], cli_dir: Option<&Path>) -> Result<(), RuntimeError> {
    let cli_dir = cli_dir
        .map(std::fs::canonicalize)
        .transpose()
        .map_err(RuntimeError::Io)?;
    for root in hidden {
        if root == Path::new("/") {
            return Err(RuntimeError::NoSandbox(
                "NEHA_HOME cannot be the file-system root".to_owned(),
            ));
        }
        for system in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
            let Ok(system) = std::fs::canonicalize(system) else {
                continue;
            };
            if root.starts_with(&system) {
                return Err(RuntimeError::NoSandbox(format!(
                    "NEHA_HOME is inside {}; a Hosted sandbox would mount that directory. Move NEHA_HOME outside the system directories",
                    system.display()
                )));
            }
        }
        if let Some(cli_dir) = &cli_dir
            && (root.starts_with(cli_dir) || cli_dir.starts_with(root))
        {
            return Err(RuntimeError::NoSandbox(
                "NEHA_HOME and the CLI directory overlap; a Hosted sandbox would mount one of them"
                    .to_owned(),
            ));
        }
    }

    Ok(())
}

/// The bubblewrap arguments that give one Hosted shell a minimal rootfs.
///
/// A Hosted Computer is the Space's own environment, so its sandbox starts
/// from an empty root rather than the machine: only the host's system
/// directories are mounted, read-only, because that is what the shell and the
/// programs it starts need to run. Nothing else of the host reaches the
/// sandbox — not the Space's home, its record, its credentials, or another
/// Computer's workspace. The workspace is the one writable folder, bound at
/// `/workspace`, and it is the working directory. `/dev`, `/proc`, and a fresh
/// `/tmp` are the sandbox's own, the PID namespace is private, and the sandbox
/// dies with its daemon. The network is not unshared: a Hosted shell reaches
/// the Space's own loopback for the Bot CLI's token API.
///
/// The CLI this build runs is mounted read-only at [`HOSTED_CLI_DIR`], so the
/// skill's `neha` commands work in the shell; the mount is skipped when the
/// caller has no CLI directory. `_bwrap` is the binary that will run these
/// arguments; they are the same for every bwrap path.
fn hosted_sandbox_args(
    _bwrap: &Path,
    workspace: &Path,
    cwd: Option<&Path>,
    cli_dir: Option<&Path>,
) -> Vec<OsString> {
    let mut arguments = Vec::new();
    // A system directory that does not exist is skipped, so the same rootfs
    // works on a distribution where it is missing.
    for directory in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if Path::new(directory).is_dir() {
            arguments.push(OsString::from("--ro-bind"));
            arguments.push(OsString::from(directory));
            arguments.push(OsString::from(directory));
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
    if let Some(directory) = cli_dir {
        arguments.push(OsString::from("--dir"));
        arguments.push(OsString::from("/opt"));
        arguments.push(OsString::from("--ro-bind"));
        arguments.push(directory.as_os_str().to_owned());
        arguments.push(OsString::from(HOSTED_CLI_DIR));
    }
    arguments.push(OsString::from("--bind"));
    arguments.push(workspace.as_os_str().to_owned());
    arguments.push(OsString::from(HOSTED_WORKSPACE));
    // The Computer's Workspace is mounted whole, so Bots on it can see each
    // other's files. The run starts in its Bot's directory when the caller
    // named one; a probe starts at the root.
    let mut start = PathBuf::from(HOSTED_WORKSPACE);
    if let Some(relative) = cwd.and_then(|cwd| cwd.strip_prefix(workspace).ok())
        && !relative.as_os_str().is_empty()
    {
        start.push(relative);
    }
    arguments.push(OsString::from("--chdir"));
    arguments.push(start.into_os_string());
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

/// Windows reparse points, including junctions, are not followed by the
/// managed file tools. The attribute check also covers a link without a
/// recognisable symlink tag.
#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// A file with more than one hard link may name an object outside the Bot's
/// folder; the managed tools refuse it rather than write through it.
#[cfg(windows)]
fn is_hard_link(path: &std::path::Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    if !metadata.is_file() {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        // A file that exists but cannot be inspected is not safe to write.
        return true;
    };
    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let inspected =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) };
    inspected == 0 || information.nNumberOfLinks > 1
}

#[cfg(unix)]
fn is_hard_link(_path: &std::path::Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    metadata.is_file() && metadata.nlink() > 1
}

#[cfg(not(any(unix, windows)))]
fn is_hard_link(_path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    false
}

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

/// Lists one open directory through its descriptor.
#[cfg(unix)]
fn list_opened_directory(directory: &std::fs::File, shown: &str) -> Result<Vec<String>, String> {
    use std::os::fd::AsRawFd as _;

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
            let kind = match fstatat(directory, &name) {
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

/// Renders the entries of a managed directory, one per line.
#[cfg(not(unix))]
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

/// Opens one folder writes may reach and names it by its lexical path.
///
/// Every component is opened from `/` without following a symlink and must be a
/// directory, so a grant that is or became a link (or sits below one) opens
/// nothing and authorizes nothing: the link's target is never a write root. A
/// path that fails to open is skipped by the caller. The descriptors name the
/// opened directories, so a rename after the open cannot make the root name
/// something else.
#[cfg(unix)]
fn open_write_root(directory: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::os::fd::AsRawFd as _;

    let path = normalize_path(Path::new(directory));
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("a write root must be an absolute folder: {directory}"),
        ));
    }
    let mut file = open_directory_path(Path::new("/"))?;
    let mut components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .peekable();
    while let Some(name) = components.next() {
        // A component on the way to the granted folder only has to be
        // traversable, as in a plain path lookup; the folder itself is opened
        // as a readable directory. Neither follows a symlink.
        file = if components.peek().is_some() {
            open_traversal_at(&file, name)?
        } else {
            open_directory_at(&file, name)?
        };
    }
    // The shell moves the Bot's folder descriptor to a fixed one-digit number.
    // A root that landed there is moved to another descriptor so the two
    // mounts can never name each other.
    while file.as_raw_fd() == MOUNT_FD {
        file = file.try_clone()?;
    }

    Ok((path, file))
}

/// Returns whether an open failed because a component is a symlink.
#[cfg(unix)]
fn is_symlink_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

/// A system without descriptor-relative opens has no symlink error to name.
#[cfg(not(unix))]
fn is_symlink_error(_error: &std::io::Error) -> bool {
    false
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

/// Opens one intermediate directory of a write root without following a
/// symlink.
///
/// On Linux this uses `O_PATH`: the component is only traversed, never read, so
/// a granted folder below an execute-only ancestor still opens as it did with a
/// plain path lookup. Other Unix systems open it as a directory instead.
#[cfg(target_os = "linux")]
fn open_traversal_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name = c_name(name)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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

/// Opens one intermediate directory of a write root without following a
/// symlink, on a Unix without `O_PATH`.
#[cfg(all(unix, not(target_os = "linux")))]
fn open_traversal_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    open_directory_at(parent, name)
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

    /// Marks the shell finished and its process tree already cleaned by the
    /// caller, so dropping the guard kills nothing.
    fn disarm(&mut self) {
        let _ = self
            .slot
            .compare_exchange(self.pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // Whoever takes the pid from the slot owns the cleanup: the running
        // task disarms after its own kill, a cancel takes the slot first, and
        // a future dropped mid-run lands here. A second taker sees the slot
        // empty and does nothing, so every path cleans once.
        if self
            .slot
            .compare_exchange(self.pid, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            kill_tree(Some(self.pid));
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

#[cfg(windows)]
fn windows_shell(command: &str, root: &Path) -> Command {
    // Real Windows machines have Windows PowerShell 5.1 beside the system
    // directory. Use it with an absolute path so a Bot's PATH cannot shadow
    // the shell; fall back to cmd.exe on a minimal image.
    let powershell = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        });
    let mut process = if powershell.as_ref().is_some_and(|path| path.is_file()) {
        let mut process = Command::new(powershell.expect("checked above"));
        process
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(command);
        process
    } else {
        let mut process = Command::new("cmd.exe");
        process.args(["/D", "/S", "/C"]).arg(command);
        process
    };
    process.current_dir(root);
    process
}

#[cfg(windows)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Windows has no process-group kill in the standard library. taskkill
        // /T walks the child tree and /F ends it, matching the Unix path's
        // intent for a timed-out or cancelled command.
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(all(not(unix), not(windows)))]
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

/// What the integration tests need from inside this module.
///
/// Integration tests live in `tests/` and see only the crate's public
/// interface. The fixtures that build a runtime by hand and the accessors for
/// the fields the fence tests inspect live here, behind the `test-support`
/// feature, so a production build keeps them out.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    pub use super::{DEFAULT_TIMEOUT, LocalRuntime, RuntimeError};

    /// How deep the folder scan goes before it refuses a tree.
    pub const MAX_SCAN_DEPTH: usize = super::MAX_SCAN_DEPTH;

    /// Builds a real-machine runtime for the file tools alone.
    ///
    /// The file tools never start a sandbox, so a test of their rules does not
    /// need bubblewrap; the shell rules have their own gated tests. Both paths
    /// are resolved, as the constructor resolves them. The `bwrap` path does
    /// not exist, so code that wrongly started a sandbox would fail the test.
    pub fn file_runtime(root: &Path, hidden: &Path) -> LocalRuntime {
        LocalRuntime {
            root: std::fs::canonicalize(root).unwrap(),
            mode: super::Mode::Machine {
                bwrap: Some(PathBuf::from("/nonexistent/neha-test-bwrap")),
                folder: super::open_directory_path(&std::fs::canonicalize(root).unwrap()).unwrap(),
            },
            hosted: None,
            hidden: vec![std::fs::canonicalize(hidden).unwrap()],
            denied: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            in_flight: std::sync::atomic::AtomicU32::new(0),
            full_access: false,
            write_roots: Vec::new(),
            cwd: None,
            env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            cli_dir: None,
        }
    }

    /// Builds a real-machine runtime for the file tools that hides nothing.
    pub fn file_runtime_without_hidden(root: &Path) -> LocalRuntime {
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
    pub fn full_access_runtime(root: &Path, hidden: &Path) -> LocalRuntime {
        file_runtime(root, hidden)
            .with_machine_access(true, &[], None)
            .expect("full access opens no sandbox")
    }

    /// The folders writes may reach besides the root.
    pub fn write_roots(runtime: &LocalRuntime) -> &[(PathBuf, std::fs::File)] {
        &runtime.write_roots
    }

    /// The directories a granted runtime must not read or write.
    pub fn hidden(runtime: &LocalRuntime) -> &[PathBuf] {
        &runtime.hidden
    }

    /// Whether the owner gave this Bot the machine user's rights.
    pub fn is_full_access(runtime: &LocalRuntime) -> bool {
        runtime.full_access
    }

    /// Whether a real-machine runtime starts a shell without a sandbox.
    pub fn machine_mode_is_bwrap_less(runtime: &LocalRuntime) -> bool {
        matches!(runtime.mode, super::Mode::Machine { bwrap: None, .. })
    }

    /// The arguments that start a fenced shell.
    pub fn sandbox_args(
        source: &OsStr,
        folder: &Path,
        cwd: &Path,
        hidden: &[PathBuf],
        write_roots: &[(OsString, PathBuf)],
    ) -> Vec<OsString> {
        super::sandbox_args(source, folder, cwd, hidden, write_roots)
    }

    /// Opens a folder a write may reach.
    pub fn open_write_root(directory: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
        super::open_write_root(directory)
    }

    /// Opens a real-machine runtime with an explicit bubblewrap binary.
    pub fn machine_with_bwrap(
        folder: impl Into<PathBuf>,
        bwrap: Option<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Result<LocalRuntime, RuntimeError> {
        LocalRuntime::machine_with_bwrap(folder, bwrap, hidden)
    }

    /// Finds the bubblewrap binary the sandbox needs.
    pub fn find_bwrap() -> Option<PathBuf> {
        super::find_bwrap()
    }

    /// Probes a Hosted sandbox.
    pub fn probe_hosted(bwrap: &Path, workspace: &Path) -> Result<(), RuntimeError> {
        super::probe_hosted(bwrap, workspace)
    }

    /// Reads a file up to a byte limit.
    pub fn read_file_bounded(
        reader: impl std::io::Read,
        limit: u64,
        shown: &str,
    ) -> Result<Vec<u8>, crate::action::ByteError> {
        super::read_file_bounded(reader, limit, shown)
    }

    /// Refuses hosted mounts that would expose the Computer's own state.
    pub fn validate_hosted_mounts(
        hidden: &[PathBuf],
        cli_dir: Option<&Path>,
    ) -> Result<(), RuntimeError> {
        super::validate_hosted_mounts(hidden, cli_dir)
    }
}
