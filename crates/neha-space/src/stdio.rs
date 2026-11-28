//! The Model Context Protocol client for the stdio transport.
//!
//! A `stdio` server is a program on the Space's machine. The Space runs it
//! under a sandbox that mounts the system read-only, the package read-only,
//! and the server's data directory read-write; the two speak JSON-RPC 2.0
//! over the child's standard input and output, one message per line.
//!
//! This module owns the wire protocol, the sandbox arguments, and the child's
//! lifetime. A client is one use: [`StdioClient::open`] performs the handshake,
//! and the other methods work only after it. A server that misses a deadline,
//! closes a pipe, or writes a line this client cannot trust is killed, and its
//! credential never enters an error message.
//!
//! The sandbox policy is stated once, in `sandbox_arguments`: the system
//! directories are mounted read-only, no directory of the Space's home is
//! mounted, the package directory is read-only, its data directory is the only
//! writable place, and the working directory is inside one of them. A home a
//! system mount would expose refuses the launch.
//!
//! The package's data stays out of the pre-sandbox process: bubblewrap itself
//! starts with only values the Space builds, and the package's environment
//! enters the sandbox as `--setenv` pairs. The credential is the exception
//! that never travels as an argument: bubblewrap inherits it and the server
//! reads it from its environment, so `/proc/<pid>/cmdline` never holds it.

use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(any(target_os = "linux", windows))]
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader,
};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::mcp::{self, APP_MIME_TYPE, MAX_ANSWER_BYTES, McpAnswer, McpResource, McpTool};

/// The client's name in the handshake.
const CLIENT_NAME: &str = "neha";

/// The client's version in the handshake.
const CLIENT_VERSION: &str = "1";

/// How long the handshake and one discovery request may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one tool call or one resource read may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of a server's standard error this transport keeps.
///
/// Only draining the pipe matters; the bytes are dropped once the buffer is
/// full. They are never logged or returned in an error: a server may print its
/// credential to standard error.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// The sanitized `PATH` a sandboxed server starts from.
#[cfg(target_os = "linux")]
const BASE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The directories a sandboxed server needs to run, mounted read-only when
/// this machine has them.
#[cfg(target_os = "linux")]
const SYSTEM_DIRECTORIES: [&str; 8] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/usr/local",
    "/opt",
];

/// The fixed reason a Linux machine without bubblewrap reports.
#[cfg(target_os = "linux")]
const NO_BUBBLEWRAP: &str =
    "this machine has no bubblewrap, which a sandboxed stdio MCP server needs";

/// The fixed reason a Linux machine whose sandbox cannot start one reports.
#[cfg(target_os = "linux")]
const SANDBOX_SMOKE_FAILED: &str =
    "this machine's bubblewrap sandbox cannot start a stdio MCP server";

/// The fixed reason a platform without a sandbox reports.
#[cfg(not(any(target_os = "linux", windows)))]
const NO_SANDBOX: &str = "a sandboxed stdio MCP server runs only under bubblewrap on Linux, which this platform does not provide";

/// The fixed reason a Windows machine without the opt-in reports.
#[cfg(windows)]
const NO_WINDOWS_SANDBOX: &str =
    "a stdio MCP server on Windows runs unsandboxed only when NEHA_ALLOW_UNSANDBOXED_MCP=1";

/// The fixed reason a Windows launch cannot manage its child's lifetime.
///
/// The job is the only thing that ends a server and every process it starts
/// after the launcher exits, so a launch that cannot hand the child to a job
/// starts nothing.
#[cfg(windows)]
const JOB_UNMANAGED: &str =
    "the MCP server's process could not be put under the job that ends it with this client";

/// The fixed reason a command cannot be executed.
const NOT_EXECUTABLE: &str = "the MCP server's command is not an executable file";

/// The fixed reason a system mount would expose the Space's home.
#[cfg(target_os = "linux")]
const HOME_EXPOSED: &str =
    "a system directory the sandbox mounts contains the Space's home, so it would expose it";

/// The fixed reason a working directory leaves the directory the sandbox
/// mounts.
const WORKING_DIRECTORY_ESCAPES: &str =
    "the MCP server's working directory leaves the directory the sandbox mounts";

/// The fixed reason a working directory cannot be read.
#[cfg(any(target_os = "linux", windows))]
const WORKING_DIRECTORY_UNREADABLE: &str = "the MCP server's working directory cannot be read";

/// The fixed reason the directory a working directory hangs from cannot be
/// read.
#[cfg(any(target_os = "linux", windows))]
const MOUNT_UNREADABLE: &str = "the MCP server's package or data directory cannot be read";

/// The fixed reason a working directory cannot be created.
#[cfg(any(target_os = "linux", windows))]
const WORKING_DIRECTORY_UNCREATABLE: &str = "the MCP server's working directory cannot be created";

/// The fixed reason a package-rooted working directory is not there.
#[cfg(any(target_os = "linux", windows))]
const WORKING_DIRECTORY_MISSING: &str = "the MCP server's working directory is not a directory";

/// A stdio MCP server failed.
/// Names a stdio MCP failure without its display text.
fn stdio_error_kind(error: &StdioError) -> &'static str {
    match error {
        StdioError::Unusable(_) => "unusable",
        StdioError::SandboxUnavailable(_) => "sandbox_unavailable",
        StdioError::Unreachable(_) => "unreachable",
        StdioError::Protocol(_) => "protocol",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StdioError {
    /// The server cannot be run with the specification it was given.
    Unusable(String),
    /// This machine has no sandbox a stdio server may run in.
    SandboxUnavailable(String),
    /// The server process could not be started, ended early, or missed a
    /// deadline.
    Unreachable(String),
    /// The server's answer did not fit the protocol.
    Protocol(String),
}

impl std::fmt::Display for StdioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unusable(message)
            | Self::SandboxUnavailable(message)
            | Self::Unreachable(message)
            | Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StdioError {}

impl From<mcp::McpError> for StdioError {
    fn from(error: mcp::McpError) -> Self {
        match error {
            mcp::McpError::Unreachable(message) => Self::Unreachable(message),
            // The parsing helpers this module shares with the HTTP client
            // report only protocol failures; the other variants cannot occur
            // while reading an answer, and mapping them keeps one error type.
            mcp::McpError::Protocol(message) => Self::Protocol(message),
            mcp::McpError::NotConnected { status } => {
                Self::Protocol(format!("the MCP server is not connected (HTTP {status})"))
            }
        }
    }
}

/// How to start one stdio MCP server.
///
/// The address is already resolved: [`resolve_command`] turns a package's
/// command name into an executable path. The environment values are the
/// package's own, already expanded; the sandbox adds the Space's variables
/// after them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdioSpec {
    /// The executable to start.
    pub command: PathBuf,
    /// The arguments the package gives the executable.
    pub args: Vec<String>,
    /// The environment the package declares, already expanded.
    pub env: BTreeMap<String, String>,
    /// The directory the server starts in; inside the package or its data.
    pub cwd: PathBuf,
    /// The package directory, mounted read-only.
    pub plugin_root: PathBuf,
    /// The server's data directory, mounted read-write and its `HOME`.
    pub plugin_data: PathBuf,
    /// The Space's home directory; no system mount may expose it.
    pub space_home: PathBuf,
    /// The bearer token the server may use, when the Space has one.
    pub credential: Option<String>,
}

/// The placeholder that stands for the plugin root in one launch value.
const PLUGIN_ROOT_PLACEHOLDER: &str = "${PLUGIN_ROOT}";

/// The placeholder that stands for the plugin data directory in one launch
/// value.
const PLUGIN_DATA_PLACEHOLDER: &str = "${PLUGIN_DATA}";

/// The first recognized placeholder in `value`, with the index it starts at,
/// the text it is, and the root it names.
fn next_placeholder<'a>(
    value: &str,
    plugin_root: &'a Path,
    plugin_data: &'a Path,
) -> Option<(usize, &'static str, &'a Path)> {
    [
        (
            value.find(PLUGIN_ROOT_PLACEHOLDER),
            PLUGIN_ROOT_PLACEHOLDER,
            plugin_root,
        ),
        (
            value.find(PLUGIN_DATA_PLACEHOLDER),
            PLUGIN_DATA_PLACEHOLDER,
            plugin_data,
        ),
    ]
    .into_iter()
    .filter_map(|(index, placeholder, path)| index.map(|index| (index, placeholder, path)))
    .min_by_key(|(index, _, _)| *index)
}

/// Expands the two Agent Plugins placeholders in one launch value.
///
/// The expansion is one pass, left to right: a replacement is never expanded
/// again, and `${...}` text that names no placeholder this Space defines stays
/// literal. The package loader keeps a symbolic working directory, and an
/// install and a launch resolve it against the Space's own roots through this
/// function, so there is one rule for what is expanded.
pub(crate) fn expand_placeholders(value: &str, plugin_root: &Path, plugin_data: &Path) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((index, placeholder, path)) = next_placeholder(rest, plugin_root, plugin_data) {
        expanded.push_str(&rest[..index]);
        expanded.push_str(&path.to_string_lossy());
        rest = &rest[index + placeholder.len()..];
    }
    expanded.push_str(rest);
    expanded
}

/// One piece of a working directory component, in the order the expansion
/// writes it: text the package wrote, or a placeholder's root.
enum LaunchPiece<'a> {
    /// Text the package wrote, with no `/` in it.
    Literal(&'a str),
    /// A recognized placeholder, naming the root it expands to.
    Root(&'a Path),
}

/// Resolves one recorded working directory expression under the root it hangs
/// from.
///
/// `symbolic_relative` is the record's symbolic path: `/`-separated and
/// relative to `root`, the plugin root or the plugin data directory. It may
/// hold the two recognized placeholders and `..` components, so the directory
/// it names is known only after the expansion. The expansion is textual —
/// every placeholder becomes the directory it names, a `.` is dropped, and a
/// `..` removes the component before it — and the result must be `root` or a
/// directory below it.
///
/// The path is built from `root` as a path, component by component. A
/// placeholder contributes the components of the root it names, not that
/// root's text: a canonical Windows home is a verbatim path (`\\?\C:\...`),
/// and a `/` written after one is a character of a file name rather than a
/// separator. Joining text would make `${PLUGIN_DATA}/work` one name directly
/// under the home, so the containment check would refuse an ordinary
/// configured working directory.
///
/// # Errors
///
/// Returns [`StdioError::Unusable`] when the expansion normalizes outside
/// `root`, when a `..` would remove `root` itself, or when the recorded path
/// holds text no path component may hold.
pub(crate) fn resolve_launch_cwd(
    root: &Path,
    symbolic_relative: &str,
    plugin_root: &Path,
    plugin_data: &Path,
) -> Result<PathBuf, StdioError> {
    let mut resolved = root.to_path_buf();
    for component in symbolic_relative.split('/') {
        expand_launch_component(&mut resolved, root, component, plugin_root, plugin_data)?;
    }
    if !resolved.starts_with(root) {
        return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
    }

    Ok(resolved)
}

/// Expands one `/`-separated component of a symbolic working directory.
///
/// A component is text the package wrote, one or more placeholders, or a mix:
/// `${PLUGIN_ROOT}/a${PLUGIN_DATA}b` expands to the plugin root, then `a`, then
/// the data root — with `b` glued to the data root's last component, because
/// the expansion is textual and nothing separates them.
fn expand_launch_component(
    resolved: &mut PathBuf,
    root: &Path,
    component: &str,
    plugin_root: &Path,
    plugin_data: &Path,
) -> Result<(), StdioError> {
    let mut pieces = Vec::new();
    let mut rest = component;
    while let Some((index, placeholder, path)) = next_placeholder(rest, plugin_root, plugin_data) {
        if index > 0 {
            pieces.push(LaunchPiece::Literal(&rest[..index]));
        }
        pieces.push(LaunchPiece::Root(path));
        rest = &rest[index + placeholder.len()..];
    }
    if !rest.is_empty() {
        pieces.push(LaunchPiece::Literal(rest));
    }
    let mut index = 0;
    while index < pieces.len() {
        match pieces[index] {
            LaunchPiece::Literal(text) => push_launch_component(resolved, root, text)?,
            LaunchPiece::Root(source) => {
                // A root's text starts at a separator, so a literal before it
                // is already one component. The literal right after it has no
                // separator before it, so the expansion glues it to the root's
                // last component.
                let trailing = match pieces.get(index + 1) {
                    Some(LaunchPiece::Literal(text)) => {
                        index += 1;
                        Some(*text)
                    }
                    _ => None,
                };
                push_launch_root(resolved, root, source, trailing)?;
            }
        }
        index += 1;
    }

    Ok(())
}

/// Pushes the path components a placeholder's root contributes.
///
/// `source`'s prefix and root directory name the filesystem, not a path text
/// the expansion writes: only its normal components continue the path. A
/// literal glued to the last of them makes that component's name longer.
fn push_launch_root(
    resolved: &mut PathBuf,
    root: &Path,
    source: &Path,
    trailing: Option<&str>,
) -> Result<(), StdioError> {
    let components: Vec<&OsStr> = source
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let Some((last, leading)) = components.split_last() else {
        // The root is a bare filesystem root, whose text is one separator:
        // text after it is the next component.
        if let Some(text) = trailing {
            push_launch_component(resolved, root, text)?;
        }
        return Ok(());
    };
    for name in leading {
        resolved.push(name);
    }
    match trailing {
        Some(text) => {
            let mut merged = last.to_os_string();
            merged.push(text);
            resolved.push(merged);
        }
        None => resolved.push(last),
    }

    Ok(())
}

/// Applies one expanded path component to the resolved path.
///
/// Empty text and `.` name the directory itself, `..` removes the last
/// component without passing `root`, and any other text is a component name. A
/// backslash or a control character cannot name a component on every platform
/// a record may reach, so it is a refusal rather than part of a name.
fn push_launch_component(
    resolved: &mut PathBuf,
    root: &Path,
    component: &str,
) -> Result<(), StdioError> {
    match component {
        "" | "." => Ok(()),
        ".." => {
            if resolved == root || !resolved.pop() || !resolved.starts_with(root) {
                return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
            }
            Ok(())
        }
        _ => {
            if component.contains('\\') || component.chars().any(char::is_control) {
                return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
            }
            resolved.push(component);
            Ok(())
        }
    }
}

/// Resolves a package's command name to an executable path.
///
/// A bare name is a program on this machine's `PATH`; a name with a separator
/// is a program inside the package's directory; an absolute path is used as
/// given. Only a file this machine may execute is a command.
///
/// # Errors
///
/// Returns [`StdioError::Unusable`] when the name is empty, leaves the
/// package's directory, or names no executable file.
pub fn resolve_command(name: &str, package_root: &Path) -> Result<PathBuf, StdioError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(unusable_error("the MCP server names no command"));
    }
    let command = Path::new(name);
    if command
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(unusable_error(
            "the MCP server's command leaves the package's directory",
        ));
    }
    if command.is_absolute() {
        return if is_executable(command) {
            Ok(command.to_path_buf())
        } else {
            Err(unusable_error(NOT_EXECUTABLE))
        };
    }
    if name.chars().any(std::path::is_separator) {
        if !package_root.is_absolute() {
            return Err(unusable_error(
                "the package's directory is not an absolute path",
            ));
        }
        return first_executable(program_candidates(package_root, name))
            .ok_or_else(|| unusable_error(NOT_EXECUTABLE));
    }
    let path = std::env::var_os("PATH")
        .ok_or_else(|| unusable_error("this machine has no PATH to resolve a command"))?;
    for directory in std::env::split_paths(&path) {
        if let Some(found) = first_executable(program_candidates(&directory, name)) {
            return Ok(found);
        }
    }
    Err(unusable_error(
        "the MCP server's command is not on this machine's PATH",
    ))
}

/// The files one program name in one directory may name.
///
/// Windows resolves a bare name by its `PATHEXT` extensions; every other
/// platform uses the name as written.
fn program_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    let candidate = directory.join(name);
    #[cfg(windows)]
    {
        let mut candidates = vec![candidate];
        if Path::new(name).extension().is_none() {
            let extensions = std::env::var_os("PATHEXT")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_owned());
            for extension in extensions.split(';').filter(|part| !part.trim().is_empty()) {
                candidates.push(directory.join(format!("{name}{}", extension.trim())));
            }
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![candidate]
    }
}

/// The first candidate this machine may execute.
fn first_executable(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|candidate| is_executable(candidate))
}

/// Whether one path names a file this machine may execute.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// One MCP server running as a child process.
///
/// A client is one use: [`StdioClient::open`] performs the handshake, and
/// [`StdioClient::tools`], [`StdioClient::call`], and
/// [`StdioClient::read_resource`] work only after it. The child and every
/// process it started are killed when the client drops.
pub struct StdioClient {
    /// The server process the sandbox started.
    child: Child,
    /// The Windows job that takes the server's descendants down when this
    /// client drops or times out; the sandbox covers Linux.
    #[cfg(windows)]
    job: Option<job::JobObject>,
    /// The child's standard input: one JSON-RPC message per line.
    stdin: ChildStdin,
    /// The child's standard output: one JSON-RPC message per line.
    stdout: BufReader<ChildStdout>,
    /// The token the server may use, when the Space has one.
    credential: Option<String>,
    /// The next JSON-RPC request id.
    next_id: u64,
    /// Whether the handshake completed.
    opened: bool,
    /// Whether the wire can no longer be trusted because the child was killed.
    failed: bool,
    /// How long the handshake and one discovery request may take.
    handshake_timeout: Duration,
    /// How long one tool call or resource read may take.
    call_timeout: Duration,
}

impl StdioClient {
    /// Starts one server under this platform's sandbox.
    ///
    /// The command is not spoken to until [`StdioClient::open`] runs. A client
    /// dropped without a shutdown kills the child and every process the
    /// sandbox started.
    ///
    /// # Errors
    ///
    /// Returns [`StdioError::Unusable`] when the specification cannot run,
    /// [`StdioError::SandboxUnavailable`] when this machine has no sandbox for
    /// it or a system mount would expose the Space's home, and
    /// [`StdioError::Unreachable`] when the process cannot be started.
    pub async fn start(spec: StdioSpec) -> Result<Self, StdioError> {
        // The Windows job is created before the process it holds. The child
        // starts suspended and is put in the job before its first instruction,
        // so it cannot start a descendant outside the job; closing the handle
        // kills everything the job holds, even after this launcher has exited.
        // A launch that cannot do both starts nothing.
        #[cfg(windows)]
        let job = job::JobObject::create().ok_or_else(|| unreachable_error(JOB_UNMANAGED))?;
        let mut command = sandbox_command(&spec)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A client that goes away without a shutdown never leaves its
            // server running: the child is killed with this struct.
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| unreachable_error("the MCP server could not be started"))?;
        #[cfg(windows)]
        let job = {
            let assigned = child
                .raw_handle()
                .is_some_and(|process| job.assign(process));
            if !assigned {
                return Err(stop_suspended_child(&mut child).await);
            }
            if !child.id().is_some_and(job::resume_suspended_child) {
                return Err(stop_suspended_child(&mut child).await);
            }
            job
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| unreachable_error("the MCP server has no input pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unreachable_error("the MCP server has no output pipe"))?;
        if let Some(stderr) = child.stderr.take() {
            drain_stderr(stderr);
        }

        Ok(Self {
            child,
            #[cfg(windows)]
            job: Some(job),
            stdin,
            stdout: BufReader::new(stdout),
            credential: spec.credential.filter(|credential| !credential.is_empty()),
            next_id: 1,
            opened: false,
            failed: false,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            call_timeout: CALL_TIMEOUT,
        })
    }

    /// Performs the `initialize` handshake and the `notifications/initialized`
    /// notification.
    ///
    /// Call it once, before [`StdioClient::tools`],
    /// [`StdioClient::call`], or [`StdioClient::read_resource`].
    ///
    /// # Errors
    ///
    /// Returns [`StdioError::Unreachable`] when the server cannot be started or
    /// does not answer in time, and [`StdioError::Protocol`] when its answer
    /// does not finish the handshake.
    pub async fn open(&mut self) -> Result<(), StdioError> {
        if self.opened {
            return Ok(());
        }
        self.require_live()?;
        // The extension tells a server this host renders an app's HTML; a
        // server that offers none still answers the handshake.
        let mut extensions = serde_json::Map::new();
        extensions.insert(
            mcp::UI_EXTENSION.to_owned(),
            json!({"mimeTypes": [APP_MIME_TYPE]}),
        );
        let params = json!({
            "protocolVersion": mcp::PROTOCOL_VERSION,
            "capabilities": {"extensions": extensions},
            "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
        });
        let result = self
            .rpc("initialize", Some(params), self.handshake_timeout)
            .await?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                protocol_error("the MCP server's handshake named no protocol version")
            })?;
        if negotiated != mcp::PROTOCOL_VERSION {
            // A server that speaks another revision cannot be used for this
            // client's remaining lifetime; the sandboxed process is stopped.
            self.kill().await;
            return Err(protocol_error(
                "the MCP server speaks a protocol version this Space does not",
            ));
        }
        self.notify("notifications/initialized").await?;
        self.opened = true;
        Ok(())
    }

    /// Lists every tool the server offers, following `nextCursor` pagination.
    ///
    /// # Errors
    ///
    /// Returns [`StdioError::Protocol`] when the client is not open or the
    /// server's answer does not fit the protocol, and the transport failures
    /// [`StdioClient::open`] reports.
    pub async fn tools(&mut self) -> Result<Vec<McpTool>, StdioError> {
        self.require_open()?;
        let mut tools = Vec::new();
        let mut declared = 0_usize;
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0_usize;
        loop {
            pages = pages.saturating_add(1);
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self
                .rpc("tools/list", Some(params), self.handshake_timeout)
                .await?;
            let page = result.as_object().ok_or_else(mcp::malformed)?;
            let listed = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(mcp::malformed)?;
            for tool in listed {
                if declared >= mcp::MAX_TOOLS {
                    return Err(protocol_error("the MCP server lists too many tools"));
                }
                declared = declared.saturating_add(1);
                if let Some(tool) = mcp::tool_from_json(tool)? {
                    tools.push(tool);
                }
            }
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            let Some(next) = next else {
                return Ok(tools);
            };
            if pages >= mcp::MAX_TOOL_PAGES {
                return Err(protocol_error("the MCP server lists too many pages"));
            }
            // A server that repeats a cursor would page forever.
            if !seen.insert(next.clone()) {
                return Err(protocol_error("the MCP server repeated a page cursor"));
            }
            cursor = Some(next);
        }
    }

    /// Calls one tool by its original name.
    ///
    /// A tool result with `isError: true` is a normal call: it returns an
    /// [`McpAnswer`] whose `is_error` is true.
    ///
    /// # Errors
    ///
    /// Returns [`StdioError::Protocol`] when the client is not open or the
    /// server's answer does not fit the protocol, and the transport failures
    /// [`StdioClient::open`] reports.
    pub async fn call(&mut self, tool: &str, arguments: Value) -> Result<McpAnswer, StdioError> {
        self.require_open()?;
        let params = json!({"name": tool, "arguments": arguments});
        let result = self
            .rpc("tools/call", Some(params), self.call_timeout)
            .await?;
        mcp::answer_from_json(&result).map_err(StdioError::from)
    }

    /// Reads one resource by its URI.
    ///
    /// The answer's `contents` may hold more than one resource; only the
    /// entries that name the requested URI count, and exactly one must.
    ///
    /// # Errors
    ///
    /// Returns [`StdioError::Protocol`] when the client is not open or the
    /// server's answer does not fit the protocol, and the transport failures
    /// [`StdioClient::open`] reports.
    pub async fn read_resource(&mut self, uri: &str) -> Result<McpResource, StdioError> {
        self.require_open()?;
        let params = json!({"uri": uri});
        let result = self
            .rpc("resources/read", Some(params), self.call_timeout)
            .await?;
        mcp::resource_from_answer(uri, &result).map_err(StdioError::from)
    }

    /// Rejects a request once the child is gone.
    fn require_live(&self) -> Result<(), StdioError> {
        if self.failed {
            Err(unreachable_error("the MCP server is no longer running"))
        } else {
            Ok(())
        }
    }

    /// Rejects a request that comes before the handshake.
    fn require_open(&self) -> Result<(), StdioError> {
        self.require_live()?;
        if self.opened {
            Ok(())
        } else {
            Err(protocol_error("the MCP client has not been opened"))
        }
    }

    /// Sends one JSON-RPC request and returns the answer's `result`.
    async fn rpc(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, StdioError> {
        self.require_live()?;
        use tracing::Instrument as _;

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let span = tracing::info_span!(
            target: "neha::mcp",
            "mcp.call",
            method,
            transport = "stdio",
            otel.kind = "client",
        );
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(mut params) = params {
            span.in_scope(|| crate::diagnostics::inject_json_meta(&mut params));
            message["params"] = params;
        }

        let result = match tokio::time::timeout(
            timeout,
            self.exchange(&message, id).instrument(span.clone()),
        )
        .await
        {
            Ok(answer) => answer,
            Err(_) => {
                self.kill().await;
                Err(unreachable_error("the MCP server did not answer in time"))
            }
        };
        crate::metrics::install().mcp_call(if result.is_ok() { "ok" } else { "failed" });
        span.in_scope(|| match &result {
            Ok(_) => tracing::debug!(target: "neha::mcp", event = "mcp.call", outcome = "ok", "an MCP stdio call answered"),
            Err(error) => tracing::warn!(target: "neha::mcp", event = "mcp.call", outcome = "failed", error_kind = stdio_error_kind(error), "an MCP stdio call failed"),
        });
        result
    }

    /// Sends one JSON-RPC notification; the protocol has no answer.
    async fn notify(&mut self, method: &str) -> Result<(), StdioError> {
        self.require_live()?;
        let message = json!({"jsonrpc": "2.0", "method": method});
        match tokio::time::timeout(self.handshake_timeout, self.write(&message)).await {
            Ok(result) => result,
            Err(_) => {
                self.kill().await;
                Err(unreachable_error(
                    "the MCP server did not accept the handshake in time",
                ))
            }
        }
    }

    /// Writes one request and reads the line that answers its id.
    ///
    /// A line that starts a server request is answered: `ping` succeeds with an
    /// empty result, as MCP requires, and every other method gets a JSON-RPC
    /// "method not found" error. A notification is ignored, and only an answer
    /// that names JSON-RPC 2.0 and this request's id is taken: anything else
    /// means the line framing can no longer be trusted, so no later request may
    /// run.
    async fn exchange(&mut self, message: &Value, id: u64) -> Result<Value, StdioError> {
        if let Err(error) = self.write(message).await {
            self.kill().await;
            return Err(error);
        }
        loop {
            let line = match read_line(&mut self.stdout).await {
                Ok(line) => line,
                Err(error) => {
                    self.kill().await;
                    return Err(error);
                }
            };
            let message: Value = match serde_json::from_slice(&line) {
                Ok(message) => message,
                Err(_) => {
                    self.kill().await;
                    return Err(protocol_error("the MCP server's answer is not JSON"));
                }
            };
            let Some(object) = message.as_object() else {
                self.kill().await;
                return Err(protocol_error(
                    "the MCP server's answer is not a JSON-RPC message",
                ));
            };
            if object.contains_key("method") {
                // A notification answers nothing and is ignored. A request
                // needs an answer of its own: `ping` is the one request MCP
                // requires every client to serve, and every other method is
                // reported as not found. The answer keeps the server's own id
                // so it can match the reply to its request, and the next line
                // may carry ours.
                let Some(request_id) = object.get("id") else {
                    continue;
                };
                let reply = if object.get("method").and_then(Value::as_str) == Some("ping") {
                    json!({"jsonrpc": "2.0", "id": request_id, "result": {}})
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": -32601_i64, "message": "method not found"},
                    })
                };
                if let Err(error) = self.write(&reply).await {
                    self.kill().await;
                    return Err(error);
                }
                continue;
            }
            if object.get("jsonrpc") != Some(&Value::from("2.0")) {
                self.kill().await;
                return Err(protocol_error(
                    "the MCP server's answer names another JSON-RPC version",
                ));
            }
            if object.get("id") != Some(&Value::from(id)) {
                self.kill().await;
                return Err(protocol_error(
                    "the MCP server's answer did not match the request",
                ));
            }
            if let Some(error) = object.get("error") {
                return Err(self.server_error(error));
            }
            if let Some(result) = object.get("result") {
                return Ok(result.clone());
            }
            self.kill().await;
            return Err(protocol_error(
                "the MCP server's answer is neither a result nor an error",
            ));
        }
    }

    /// Writes one JSON-RPC message as one line.
    async fn write(&mut self, message: &Value) -> Result<(), StdioError> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|_| protocol_error("the MCP request could not be encoded"))?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|_| unreachable_error("the MCP server closed its input"))?;
        self.stdin
            .flush()
            .await
            .map_err(|_| unreachable_error("the MCP server closed its input"))
    }

    /// The message of a JSON-RPC error a server reported.
    fn server_error(&self, error: &Value) -> StdioError {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the server reported an error");
        protocol_error(&format!(
            "the MCP server reported an error: {}",
            self.redact(message)
        ))
    }

    /// Removes the credential from text a server supplied.
    fn redact(&self, text: &str) -> String {
        match &self.credential {
            Some(credential) => text.replace(credential.as_str(), "[redacted]"),
            None => text.to_owned(),
        }
    }

    /// Kills the child and makes this client unusable.
    ///
    /// The line framing cannot be trusted after a timeout, a closed pipe, or a
    /// malformed line, so no later request is sent to this server. Windows has
    /// no sandbox to take the child's descendants with it, so the job's handle
    /// is closed here — its `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` ends every
    /// process the server started, even one whose launcher is already gone —
    /// and `taskkill /T` follows as the second layer. Linux's
    /// `--die-with-parent` covers the sandbox.
    async fn kill(&mut self) {
        self.failed = true;
        #[cfg(windows)]
        {
            self.job = None;
            if let Some(pid) = self.child.id() {
                let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
            }
        }
        let _ = self.child.kill().await;
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // `kill_on_drop` makes the same promise for a clean drop; stating the
        // kill next to the process also covers a client that never opened.
        #[cfg(windows)]
        {
            // Closing the job is what reaches a descendant the launcher no
            // longer has a parent link from; the taskkill walk follows it.
            self.job = None;
            if let Some(pid) = self.child.id() {
                kill_process_tree(pid);
            }
        }
        let _ = self.child.start_kill();
    }
}

/// Reads one newline-terminated message from the server's output.
///
/// The line is bounded by [`MAX_ANSWER_BYTES`]: bytes past it mean the server
/// wrote a message this client will not buffer, and the transport is unusable.
async fn read_line<R>(reader: &mut R) -> Result<Vec<u8>, StdioError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| unreachable_error("the MCP server closed its output"))?;
        if available.is_empty() {
            return Err(unreachable_error(
                "the MCP server closed its output before answering",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let taken = newline.map_or(available.len(), |position| position + 1);
        line.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if newline.is_some() {
            break;
        }
        if line.len() > MAX_ANSWER_BYTES {
            return Err(protocol_error("the MCP server's answer is too large"));
        }
    }
    // The terminating newline is framing, not message bytes: a message of
    // exactly the bound is read, and one byte more is refused.
    if line.len().saturating_sub(1) > MAX_ANSWER_BYTES {
        return Err(protocol_error("the MCP server's answer is too large"));
    }
    Ok(line)
}

/// Reads a server's standard error, keeping at most [`MAX_STDERR_BYTES`].
///
/// The read never lets a server block on a full pipe. The bounded buffer is
/// dropped once the pipe closes; standard error is the server's own output and
/// never enters the protocol, a log, or an error message, because it may hold
/// the credential.
fn drain_stderr(mut stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if buffer.len() < MAX_STDERR_BYTES {
                        let keep = (MAX_STDERR_BYTES - buffer.len()).min(read);
                        buffer.extend_from_slice(&chunk[..keep]);
                    }
                }
            }
        }
        drop(buffer);
    });
}

/// Builds the command that starts one server under this platform's sandbox.
///
/// The command is bubblewrap's. Its own environment is cleared and then built
/// from trusted values only: bubblewrap is dynamically linked, and its loader
/// reads variables such as `LD_PRELOAD` before any namespace exists. The
/// package's values enter the sandbox with `--setenv` arguments instead, and
/// the credential — the one value that must never be an argument — is inherited
/// from this process's environment.
///
/// # Errors
///
/// Returns [`StdioError::Unusable`] when the specification cannot run and
/// [`StdioError::SandboxUnavailable`] when this machine has no sandbox for it
/// or a system mount would expose the Space's home.
#[cfg(target_os = "linux")]
pub fn sandbox_command(spec: &StdioSpec) -> Result<Command, StdioError> {
    let arguments = sandbox_arguments(spec)?;
    let bwrap = find_executable("bwrap").ok_or_else(|| sandbox_unavailable_error(NO_BUBBLEWRAP))?;
    let mut command = Command::new(bwrap);
    command
        .args(&arguments)
        .env_clear()
        .envs(bwrap_environment(spec));
    Ok(command)
}

/// This platform has no sandbox a stdio server may run in.
///
/// # Errors
///
/// Returns [`StdioError::Unusable`] when the specification cannot run, and
/// always otherwise a [`StdioError::SandboxUnavailable`] with a fixed message.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn sandbox_command(spec: &StdioSpec) -> Result<Command, StdioError> {
    validate_spec(spec)?;
    Err(sandbox_unavailable_error(NO_SANDBOX))
}

/// Builds the command that starts one server unsandboxed on Windows.
///
/// Windows has no sandbox for these servers, so only an administrator's
/// explicit opt-in runs one. The child starts suspended and leads its own
/// process group, and gets an environment built from scratch: no variable of
/// the Space's process reaches it, so an ambient credential cannot arrive at a
/// server that was not given one. [`StdioClient::start`] puts it in the job that
/// owns its lifetime before it is resumed, and ending the launch ends every
/// process it started.
///
/// # Errors
///
/// Returns a [`StdioError::SandboxUnavailable`] with a fixed message when the
/// opt-in is absent, and [`StdioError::Unusable`] when the specification
/// cannot run.
#[cfg(windows)]
pub fn sandbox_command(spec: &StdioSpec) -> Result<Command, StdioError> {
    // The opt-in is checked before anything the launch would write: a refused
    // launch leaves the machine as it was.
    if !unsandboxed_allowed() {
        return Err(sandbox_unavailable_error(NO_WINDOWS_SANDBOX));
    }
    prepare_launch(spec)?;
    let environment = child_environment(spec);
    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(environment);
    // The child runs no instruction until the launch has put it in the job that
    // ends it, and Windows has no process-group kill in the standard library; a
    // child in its own group is one `taskkill /T` can walk back from.
    command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    Ok(command)
}

/// The `CreateProcess` flag that starts the child suspended.
///
/// The launch resumes it only after assigning it to the job that owns its
/// lifetime, so no descendant can start outside that job.
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// The `CreateProcess` flag that puts the unsandboxed child in its own group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Kills and reaps a child whose job could not take its lifetime.
///
/// The child started suspended, so it has run no instruction and started no
/// process of its own; the fixed reason names the job, never the package or a
/// credential.
#[cfg(windows)]
async fn stop_suspended_child(child: &mut Child) -> StdioError {
    let _ = child.start_kill();
    let _ = child.wait().await;
    unreachable_error(JOB_UNMANAGED)
}

/// Ends one unsandboxed child and every process it started.
///
/// This is the second layer on Windows: the job object in [`job`] is the one
/// that reaches descendants after the launcher itself exits, and `taskkill /T`
/// is the fallback that walks parent links while the child exists. It uses
/// `/F`, so a dropped or timed-out server never leaves a descendant behind.
#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    let pid = pid.to_string();
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The Windows Job Object that takes a server's descendants down with it.
///
/// `taskkill /T` walks parent links while the process that started them still
/// exists; a launcher that exits before its server does leaves nothing to walk
/// from. A Job Object holds every process the server starts, and a job created
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` kills all of them when its handle
/// closes — which is exactly when this client drops, times out, or is killed.
///
/// This module is the crate's only unsafe code: calling Win32 is inherently
/// unsafe. Each block states its safety argument, no other platform compiles
/// it, and no process handle outlives the client that owns it.
#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// A job object whose processes die when the handle closes.
    ///
    /// The handle is held as an integer so the client stays `Send`: a raw
    /// `HANDLE` is a pointer, and a kernel handle is usable from any thread
    /// anyway.
    pub(super) struct JobObject(usize);

    impl JobObject {
        /// Creates a job that kills every process in it when it closes.
        ///
        /// Answers `None` when Windows refuses; the launcher then starts
        /// nothing rather than a process whose lifetime it cannot manage.
        pub(super) fn create() -> Option<Self> {
            // SAFETY: `CreateJobObjectW` is called with no attributes and no
            // name; a non-null answer is an owned handle this value closes
            // exactly once.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return None;
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job` is the live handle just created, the information
            // class matches the struct, and the pointer is valid for the
            // struct's size for the duration of the call.
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                // SAFETY: the handle is still owned here and is not used after.
                unsafe { CloseHandle(job) };
                return None;
            }
            Some(Self(job as usize))
        }

        /// Puts one process in the job.
        ///
        /// A process assigned to a job joins every process it starts, so the
        /// server's descendants are in it too. Answers whether Windows accepted
        /// the process; the handle stays owned by this value either way. A
        /// refusal is fatal to the launch: the child is still suspended and is
        /// killed and reaped instead of run outside the job.
        pub(super) fn assign(&self, process: RawHandle) -> bool {
            // SAFETY: both handles are live: `self.0` is owned by this value and
            // `process` is the child's process handle while the child is not
            // reaped.
            unsafe { AssignProcessToJobObject(self.0 as HANDLE, process) != 0 }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the handle `create` returned and no other
            // value closes it. Closing it kills every process still in the job
            // because of `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
            unsafe { CloseHandle(self.0 as HANDLE) };
        }
    }

    /// Starts the primary thread of a child the launcher created suspended.
    ///
    /// A child created with `CREATE_SUSPENDED` runs no instruction until its
    /// primary thread is resumed, so a process started before the resume cannot
    /// leave the job it was just assigned to. The process handle does not name
    /// that thread, so it is found in a thread snapshot by the child's process
    /// id and opened with `THREAD_SUSPEND_RESUME`; every handle opened here is
    /// closed before the answer. Answers whether the thread was found and
    /// Windows resumed it.
    pub(super) fn resume_suspended_child(pid: u32) -> bool {
        // SAFETY: the snapshot call takes no pointers; a returned handle is
        // either `INVALID_HANDLE_VALUE` or a snapshot this function closes.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        // SAFETY: `THREADENTRY32` is a C struct of plain integers, so the
        // zeroed value is a valid initial one; `dwSize` is set as the API
        // requires and no pointer field is carried.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        // SAFETY: `snapshot` is live and `entry` is a valid, sized struct for
        // the call; the API writes into it and reads nothing else.
        let mut next = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        let mut resumed = false;
        while next {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: `entry` was filled by the snapshot API; the thread id
                // belongs to a live thread of the child, and a non-null answer
                // is an owned thread handle this branch closes.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    // SAFETY: `thread` is the live handle just opened and has
                    // the suspend/resume access right `ResumeThread` needs.
                    resumed = unsafe { ResumeThread(thread) } != u32::MAX;
                    // SAFETY: `thread` is owned here and is not used after.
                    unsafe { CloseHandle(thread) };
                    break;
                }
            }
            // SAFETY: `snapshot` is live and `entry` is the sized struct the
            // previous call left valid; the API reads and writes only it.
            next = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        // SAFETY: `snapshot` is an owned handle from the call above and is not
        // used after this point.
        unsafe { CloseHandle(snapshot) };
        resumed
    }
}

/// The bubblewrap arguments that fence one stdio server.
///
/// The list is one policy: the machine is read-only, `/dev` is a minimal
/// device set of the sandbox's own, `/tmp` is fresh, the sandbox dies with the
/// Space, the network is shared (a server reaches its own endpoint), the
/// package is read-only, and the package's data directory is the only writable
/// mount. No argument mounts the Space's home, the host's `/tmp`, or any other
/// directory of this machine beyond the system directories; a home that one of
/// those system directories would expose is refused before an argument is
/// built.
///
/// The server's environment is completed here too: the package's variables and
/// the Space's two roots enter as `--setenv` pairs, so they are data for the
/// sandbox rather than variables of the dynamically linked bubblewrap process.
/// `PLUGIN_ROOT` and `PLUGIN_DATA` come last, so a package cannot override
/// them.
#[cfg(target_os = "linux")]
fn sandbox_arguments(spec: &StdioSpec) -> Result<Vec<OsString>, StdioError> {
    // The Space's home is compared with the system mounts before anything the
    // launch writes: a launch that would expose it creates nothing.
    let system_mounts = mounted_system_directories();
    check_space_home(&spec.space_home, &system_mounts)?;
    prepare_launch(spec)?;
    let mut arguments = Vec::new();
    for flag in [
        "--die-with-parent",
        "--unshare-all",
        "--share-net",
        "--new-session",
    ] {
        arguments.push(OsString::from(flag));
    }
    // A directory this machine does not have (a distribution without
    // `/usr/local`, a merged `/bin`) is skipped rather than mounted empty.
    for directory in &system_mounts {
        arguments.push(OsString::from("--ro-bind"));
        arguments.push(directory.as_os_str().to_owned());
        arguments.push(directory.as_os_str().to_owned());
    }
    arguments.push(OsString::from("--proc"));
    arguments.push(OsString::from("/proc"));
    arguments.push(OsString::from("--dev"));
    arguments.push(OsString::from("/dev"));
    arguments.push(OsString::from("--tmpfs"));
    arguments.push(OsString::from("/tmp"));
    arguments.push(OsString::from("--ro-bind"));
    arguments.push(spec.plugin_root.as_os_str().to_owned());
    arguments.push(spec.plugin_root.as_os_str().to_owned());
    arguments.push(OsString::from("--bind"));
    arguments.push(spec.plugin_data.as_os_str().to_owned());
    arguments.push(spec.plugin_data.as_os_str().to_owned());
    for (name, value) in sandboxed_environment(spec) {
        arguments.push(OsString::from("--setenv"));
        arguments.push(name);
        arguments.push(value);
    }
    arguments.push(OsString::from("--chdir"));
    arguments.push(spec.cwd.as_os_str().to_owned());
    arguments.push(OsString::from("--"));
    arguments.push(spec.command.as_os_str().to_owned());
    arguments.extend(spec.args.iter().map(OsString::from));
    Ok(arguments)
}

/// The system directories this machine has, as they will be mounted.
#[cfg(target_os = "linux")]
fn mounted_system_directories() -> Vec<PathBuf> {
    SYSTEM_DIRECTORIES
        .iter()
        .map(PathBuf::from)
        .filter(|directory| directory.exists())
        .collect()
}

/// Refuses a Space home that one of the system mounts would expose.
///
/// Both sides of every comparison are resolved first, so a symlinked system
/// directory (`/bin` on a merged-`/usr` machine) cannot hide the home. A home
/// the mount equals, sits inside, or holds is refused: each of the three puts
/// at least the home's subtree in front of the sandbox.
#[cfg(target_os = "linux")]
fn check_space_home(space_home: &Path, system_mounts: &[PathBuf]) -> Result<(), StdioError> {
    let space_home = canonical_or_original(space_home);
    for mount in system_mounts {
        let mount = canonical_or_original(mount);
        if space_home.starts_with(&mount) || mount.starts_with(&space_home) {
            return Err(sandbox_unavailable_error(HOME_EXPOSED));
        }
    }
    Ok(())
}

/// The resolved path, or the path as written when it does not exist.
///
/// A path that does not exist cannot resolve to anything else, and keeping it
/// as written still lets a home that is not there yet be compared with the
/// directory that would expose it once it is.
fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The trusted variables every server's environment starts from.
///
/// These are values the Space builds, never a package's: a fixed sandbox
/// `PATH`, a `HOME` and a `TMPDIR` inside the mounts. A sandboxed server
/// inherits them from the bubblewrap process; an unsandboxed one gets them from
/// its launch environment.
#[cfg(any(target_os = "linux", windows))]
fn base_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("PATH"), sandbox_path()),
        (
            OsString::from("HOME"),
            spec.plugin_data.as_os_str().to_owned(),
        ),
        (OsString::from("TMPDIR"), sandbox_temp_directory(spec)),
    ]
}

/// The environment the bubblewrap process itself starts with.
///
/// Bubblewrap is a dynamically linked program, and its loader acts on variables
/// such as `LD_PRELOAD` before any namespace exists. So this list never holds a
/// package's value: only the trusted base and, when the Space has one, the
/// credential. The credential is inherited by bubblewrap — not passed as an
/// argument — and therefore reaches the server's environment without appearing
/// in `/proc/<pid>/cmdline`.
#[cfg(target_os = "linux")]
fn bwrap_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    let mut environment = base_environment(spec);
    if let Some(credential) = credential_of(spec) {
        environment.push((OsString::from("NEHA_MCP_TOKEN"), OsString::from(credential)));
    }
    environment
}

/// The package's variables and the Space's roots, as values the sandbox
/// receives.
///
/// The package's variables come first and the Space's `PLUGIN_ROOT` and
/// `PLUGIN_DATA` after them, so a package value with either name is overridden
/// by the Space's own. The token's name is removed: the credential is inherited
/// by bubblewrap and never an argument, so a package's value for that name must
/// not shadow it. These are package data, not Space secrets, so the rest may
/// travel as bubblewrap `--setenv` arguments.
#[cfg(target_os = "linux")]
fn sandboxed_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    // The credential is never a `--setenv` pair, so a package's value for the
    // token's name could only shadow the one the Space inherits: it is dropped
    // rather than allowed to replace the Space's credential in the server.
    let mut environment: Vec<(OsString, OsString)> = package_environment(spec)
        .into_iter()
        .filter(|(name, _)| name != "NEHA_MCP_TOKEN")
        .collect();
    environment.extend(space_roots(spec));
    environment
}

/// The package's variables, in the order the record holds them.
#[cfg(any(target_os = "linux", windows))]
fn package_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    spec.env
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

/// The Space's two roots, written after the package's variables so they win.
#[cfg(any(target_os = "linux", windows))]
fn space_roots(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("PLUGIN_ROOT"),
            spec.plugin_root.as_os_str().to_owned(),
        ),
        (
            OsString::from("PLUGIN_DATA"),
            spec.plugin_data.as_os_str().to_owned(),
        ),
    ]
}

/// The Space's credential, when this server has one.
#[cfg(any(target_os = "linux", windows))]
fn credential_of(spec: &StdioSpec) -> Option<&str> {
    spec.credential
        .as_deref()
        .filter(|credential| !credential.is_empty())
}

/// The complete environment a Windows server starts with.
///
/// The launcher's own environment is cleared, and this list is everything the
/// server sees: the trusted base, the package's variables, the Space's roots,
/// and the credential. Windows has no bubblewrap process whose loader the
/// values would pass through, so the list is the child's environment itself.
#[cfg(windows)]
fn child_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    let mut environment = base_environment(spec);
    environment.extend(package_environment(spec));
    environment.extend(space_roots(spec));
    if let Some(credential) = credential_of(spec) {
        environment.push((OsString::from("NEHA_MCP_TOKEN"), OsString::from(credential)));
    }
    environment
}

/// The `PATH` a sandboxed server starts from.
#[cfg(target_os = "linux")]
fn sandbox_path() -> OsString {
    OsString::from(BASE_PATH)
}

/// The `PATH` an unsandboxed server starts from.
///
/// The command is already resolved to an absolute path; the machine's own
/// `PATH` lets the server find the tools beside it without handing the child
/// any other variable of the Space's process.
#[cfg(windows)]
fn sandbox_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

/// The directory a sandboxed server may treat as its temporary directory.
#[cfg(target_os = "linux")]
fn sandbox_temp_directory(_spec: &StdioSpec) -> OsString {
    OsString::from("/tmp")
}

/// The directory an unsandboxed server may treat as its temporary directory.
#[cfg(windows)]
fn sandbox_temp_directory(spec: &StdioSpec) -> OsString {
    spec.plugin_data.as_os_str().to_owned()
}

/// Checks one specification and prepares the directories its launch needs.
///
/// Validation is static; this also resolves the working directory, creating a
/// missing data-rooted one a component at a time.
#[cfg(any(target_os = "linux", windows))]
fn prepare_launch(spec: &StdioSpec) -> Result<(), StdioError> {
    validate_spec(spec)?;
    resolve_working_directory(spec, true)?;
    Ok(())
}

/// Checks one specification the way a launch would, without writing anything.
///
/// A list row uses this: a data-rooted working directory may not be there until
/// the first launch creates it, but every component that is there must be a
/// directory inside the root the sandbox mounts. The checks are the launch's
/// own, minus the directory creation, so a listing reports the reason a launch
/// would and leaves the disk as it was.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn check_launch(spec: &StdioSpec) -> Result<(), StdioError> {
    #[cfg(target_os = "linux")]
    check_space_home(&spec.space_home, &mounted_system_directories())?;
    validate_spec(spec)?;
    resolve_working_directory(spec, false)?;
    Ok(())
}

/// This platform has no sandbox a listing can check a launch against.
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn check_launch(spec: &StdioSpec) -> Result<(), StdioError> {
    validate_spec(spec)
}

/// Resolves the working directory inside the directory the sandbox mounts.
///
/// A package-rooted working directory is published by the install and must
/// already be there. A data-rooted one may be named by a first launch: missing
/// components are then created one at a time. On Unix every step of that walk —
/// the check, the creation, and the next step — is anchored to an opened
/// directory descriptor, so another process renaming or linking a parent
/// between two steps cannot move the traversal, the creation, or the server's
/// working directory outside the canonical data root. A symlink or a
/// non-directory component is a fixed refusal.
///
/// `create` is false for the checks a listing makes: a data-rooted component
/// that does not exist yet is accepted there (the launch creates it), and
/// nothing on disk changes.
#[cfg(any(target_os = "linux", windows))]
fn resolve_working_directory(spec: &StdioSpec, create: bool) -> Result<PathBuf, StdioError> {
    let (root, relative, data_rooted) =
        if let Ok(relative) = spec.cwd.strip_prefix(&spec.plugin_root) {
            (&spec.plugin_root, relative, false)
        } else if let Ok(relative) = spec.cwd.strip_prefix(&spec.plugin_data) {
            (&spec.plugin_data, relative, true)
        } else {
            return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
        };
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| unusable_error(MOUNT_UNREADABLE))?;
    if data_rooted {
        return walk_data_root(&canonical_root, relative, create);
    }
    walk_package_working_directory(&canonical_root, relative)
}

/// The data root's walk, anchored to opened directory descriptors.
///
/// The root is opened first, and every step is relative to the descriptor of
/// the directory before it: `statat` with `AT_SYMLINK_NOFOLLOW` classifies the
/// component, and `openat` with `O_DIRECTORY | O_NOFOLLOW` opens it only when
/// it is a real directory. A symlink is a fixed escape refusal and any other
/// non-directory is the fixed "not a directory" refusal, so nothing is created
/// through a path the walk did not verify. A missing component is created with
/// `mkdirat` beside the same descriptor and opened the same way, and the
/// working directory is built from the canonical root plus the names the
/// descriptors verified.
///
/// The walk is the Linux sandbox's: only a Linux launch reaches it, and only
/// Linux builds the module's sandbox-scoped helpers.
#[cfg(target_os = "linux")]
fn walk_data_root(
    canonical_root: &Path,
    relative: &Path,
    create: bool,
) -> Result<PathBuf, StdioError> {
    use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, openat, statat};

    let directory_flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = openat(CWD, canonical_root, directory_flags, Mode::empty())
        .map_err(|_| unusable_error(MOUNT_UNREADABLE))?;
    let mut resolved = canonical_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
        };
        let path = Path::new(name);
        // Classify the component before opening it: with `O_DIRECTORY` a
        // symlink fails as "not a directory", and the fixed reason a link
        // deserves is the escape one.
        let exists = match statat(&directory, path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::Symlink => {
                    return Err(sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES));
                }
                FileType::Directory => true,
                _ => return Err(unusable_error(WORKING_DIRECTORY_MISSING)),
            },
            Err(error) if error == rustix::io::Errno::NOENT => false,
            Err(_) => return Err(unusable_error(WORKING_DIRECTORY_MISSING)),
        };
        if !exists {
            if !create {
                // A listing checks only what is there; the launch creates the
                // missing components and everything after them.
                return Ok(resolved);
            }
            create_data_component(&directory, path)?;
        }
        match openat(&directory, path, directory_flags, Mode::empty()) {
            Ok(opened) => {
                directory = opened;
                resolved.push(name);
            }
            Err(error) => return Err(working_directory_open_error(error)),
        }
    }
    finish_working_directory(&resolved)?;
    Ok(resolved)
}

/// Creates one missing data-rooted component beside its parent descriptor.
///
/// A component another launch created after the walk classified it as missing
/// is not a failure: `mkdirat` answers `EEXIST`, and the caller's `openat`
/// still decides whether the component is a directory it can enter. A file or
/// link that replaced it is refused there.
#[cfg(target_os = "linux")]
fn create_data_component(directory: &rustix::fd::OwnedFd, name: &Path) -> Result<(), StdioError> {
    use rustix::fs::{Mode, mkdirat};

    match mkdirat(directory, name, Mode::from_bits_truncate(0o777)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(_) => Err(unusable_error(WORKING_DIRECTORY_UNCREATABLE)),
    }
}

/// The data root's walk on a platform without `openat`.
///
/// Each component is resolved by name and the resolved path checked against the
/// canonical root. A symlink is refused as an escape in every direction, and a
/// non-directory is the fixed "not a directory" refusal. A missing component is
/// created by a launch and accepted by a listing; a component another launch
/// created in the meantime is not a failure, and the canonical read that
/// follows still refuses a link or a non-directory.
#[cfg(windows)]
fn walk_data_root(
    canonical_root: &Path,
    relative: &Path,
    create: bool,
) -> Result<PathBuf, StdioError> {
    let mut resolved = canonical_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
        };
        let candidate = resolved.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES));
                }
                let canonical = std::fs::canonicalize(&candidate)
                    .map_err(|_| unusable_error(WORKING_DIRECTORY_UNREADABLE))?;
                if !canonical.starts_with(canonical_root) {
                    return Err(sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES));
                }
                if !canonical.is_dir() {
                    return Err(unusable_error(WORKING_DIRECTORY_MISSING));
                }
                resolved = canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match std::fs::create_dir(&candidate) {
                    Ok(()) => {}
                    // Another launch created the component between the check
                    // and the call: the canonical read below decides what is
                    // there now.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(unusable_error(WORKING_DIRECTORY_UNCREATABLE)),
                }
                resolved = std::fs::canonicalize(&candidate)
                    .map_err(|_| unusable_error(WORKING_DIRECTORY_UNREADABLE))?;
                if !resolved.starts_with(canonical_root) {
                    return Err(sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES));
                }
                if !resolved.is_dir() {
                    return Err(unusable_error(WORKING_DIRECTORY_MISSING));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                // A listing accepts a data-rooted directory the first launch
                // creates; nothing on disk changes.
                return Ok(resolved);
            }
            Err(_) => return Err(unusable_error(WORKING_DIRECTORY_MISSING)),
        }
    }
    finish_working_directory(&resolved)?;
    Ok(resolved)
}

/// Maps an error opening one working-directory component to its fixed reason.
///
/// The component's type was checked with `statat` first, so these are the
/// races and permission failures left: either way the walk refuses instead of
/// creating or starting anything through the path.
#[cfg(target_os = "linux")]
fn working_directory_open_error(error: rustix::io::Errno) -> StdioError {
    match error {
        // `O_NOFOLLOW` refuses a symlink that replaced the checked directory
        // between the two calls, and a path below one could leave the root.
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
            sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES)
        }
        rustix::io::Errno::ACCESS | rustix::io::Errno::PERM => {
            unusable_error(WORKING_DIRECTORY_UNREADABLE)
        }
        _ => unusable_error(WORKING_DIRECTORY_MISSING),
    }
}

/// The package-rooted walk: the install published every component already.
///
/// The path is resolved and checked against the canonical package root, so the
/// server starts in the package and never outside it. Nothing is created: a
/// package directory is read-only to the Space.
#[cfg(any(target_os = "linux", windows))]
fn walk_package_working_directory(
    canonical_root: &Path,
    relative: &Path,
) -> Result<PathBuf, StdioError> {
    let mut resolved = canonical_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(unusable_error(WORKING_DIRECTORY_ESCAPES));
        };
        let candidate = resolved.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let canonical = std::fs::canonicalize(&candidate)
                    .map_err(|_| unusable_error(WORKING_DIRECTORY_UNREADABLE))?;
                if !canonical.starts_with(canonical_root) {
                    return Err(sandbox_unavailable_error(WORKING_DIRECTORY_ESCAPES));
                }
                if !canonical.is_dir() {
                    return Err(unusable_error(WORKING_DIRECTORY_MISSING));
                }
                resolved = canonical;
            }
            Err(_) => return Err(unusable_error(WORKING_DIRECTORY_MISSING)),
        }
    }
    finish_working_directory(&resolved)?;
    Ok(resolved)
}

/// Checks that the server can read the directory it will start in.
///
/// The final directory is opened as a directory, not merely named: the sandbox
/// `chdir`s the server into it, so a path that no longer opens is refused.
#[cfg(any(target_os = "linux", windows))]
fn finish_working_directory(resolved: &Path) -> Result<(), StdioError> {
    std::fs::read_dir(resolved).map_err(|_| unusable_error(WORKING_DIRECTORY_UNREADABLE))?;
    Ok(())
}

/// Resolves one program name on this machine's `PATH`.
#[cfg(target_os = "linux")]
fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether this Space can run a stdio server in the sandbox the contract
/// requires: bubblewrap on Linux, or the administrator's explicit opt-in on
/// Windows. Every other machine cannot launch one at all.
///
/// This is the cheap check: it finds the sandbox program but starts nothing.
/// [`sandbox_smoke`] is the answer a launch or a list row uses, because a
/// sandbox that is installed can still refuse to start a process.
pub(crate) fn sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_sandbox_available()
    }
    #[cfg(windows)]
    {
        unsandboxed_allowed()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_sandbox_available() -> bool {
    find_executable("bwrap").is_some()
}

/// Whether this Space's sandbox can actually start a program.
///
/// The install, a list row, and a launch all ask this instead of
/// [`sandbox_available`]: bubblewrap can be installed and still fail to start
/// anything, most often when the kernel leaves user namespaces disabled. The
/// probe runs bubblewrap with a trusted program this machine provides and the
/// sandbox flags a launch uses; no package code runs. The answer is asked once
/// and cached, so a listing that checks many servers starts one probe, and the
/// first check is the only one that may wait on it.
pub(crate) fn sandbox_smoke() -> bool {
    static SMOKE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    *SMOKE.get_or_init(|| {
        if !sandbox_available() {
            return false;
        }
        sandbox_probe()
    })
}

/// The fixed reason this machine cannot run a sandboxed stdio server.
///
/// It names no package, path, or credential: a list row and a failed launch may
/// both carry it.
pub(crate) fn sandbox_unavailable_reason() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        if linux_sandbox_available() {
            SANDBOX_SMOKE_FAILED
        } else {
            NO_BUBBLEWRAP
        }
    }
    #[cfg(windows)]
    {
        NO_WINDOWS_SANDBOX
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        NO_SANDBOX
    }
}

/// Runs the sandbox probe on the platform that has one.
///
/// Only Linux needs a probe: [`sandbox_available`] already answers without one
/// on Windows, and no other platform has a sandbox at all.
#[cfg(target_os = "linux")]
fn sandbox_probe() -> bool {
    let Some(bwrap) = find_executable("bwrap") else {
        return false;
    };
    let Some(program) = ["/bin/true", "/usr/bin/true"]
        .into_iter()
        .map(PathBuf::from)
        .find(|program| program.is_file())
    else {
        return false;
    };
    // The flags are the policy a launch uses, with the machine mounted at the
    // root in place of the package roots: a sandbox that cannot unshare a user
    // namespace, mount proc, or start a program fails here.
    std::process::Command::new(bwrap)
        .args([
            "--die-with-parent",
            "--unshare-all",
            "--share-net",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--",
        ])
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "linux"))]
fn sandbox_probe() -> bool {
    true
}

/// Whether the administrator accepted an unsandboxed stdio server.
///
/// Only the documented value counts: `1`. Any other spelling of a boolean is
/// not the opt-in the contract names.
#[cfg(windows)]
fn unsandboxed_allowed() -> bool {
    std::env::var("NEHA_ALLOW_UNSANDBOXED_MCP").is_ok_and(|value| value == "1")
}

/// Rejects a specification a sandbox cannot mount or start.
fn validate_spec(spec: &StdioSpec) -> Result<(), StdioError> {
    for (label, path) in [
        ("plugin root", &spec.plugin_root),
        ("plugin data", &spec.plugin_data),
        ("working directory", &spec.cwd),
        ("command", &spec.command),
        ("Space home", &spec.space_home),
    ] {
        if !path.is_absolute() {
            return Err(unusable_error(&format!(
                "the MCP server's {label} is not an absolute path"
            )));
        }
    }
    for (label, path) in [
        ("plugin root", &spec.plugin_root),
        ("plugin data", &spec.plugin_data),
        ("working directory", &spec.cwd),
        ("Space home", &spec.space_home),
    ] {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(unusable_error(&format!(
                "the MCP server's {label} contains a parent directory"
            )));
        }
    }
    // The sandbox mounts the package and its data; a working directory
    // outside both would be a directory the server cannot see, and a
    // containment check keeps a package from asking for one.
    if !spec.cwd.starts_with(&spec.plugin_root) && !spec.cwd.starts_with(&spec.plugin_data) {
        return Err(unusable_error(
            "the MCP server's working directory is outside the directories the sandbox mounts",
        ));
    }
    for (label, path) in [
        ("plugin root", &spec.plugin_root),
        ("plugin data", &spec.plugin_data),
    ] {
        if path.parent().is_none() {
            return Err(unusable_error(&format!(
                "the MCP server's {label} is the filesystem root"
            )));
        }
        // Mounting a directory that holds either home would show the sandbox
        // the home itself. A package inside a home is mounted by its subtree,
        // which stays fine: the home is the ancestor of the mount, never the
        // mount.
        for variable in ["NEHA_HOME", "HOME"] {
            if let Some(home) = std::env::var_os(variable)
                && Path::new(&home).starts_with(path)
            {
                return Err(unusable_error(&format!(
                    "the MCP server's {label} would mount a home directory"
                )));
            }
        }
    }
    // The explicit launch paths are the package's own subtree, which usually
    // lives inside the Space's home: that subtree is mounted intentionally.
    // The reverse would put the home itself in front of the sandbox, so a path
    // that is the home or holds it is refused.
    let space_home = canonical_or_original(&spec.space_home);
    for (label, path) in [
        ("plugin root", &spec.plugin_root),
        ("plugin data", &spec.plugin_data),
        ("working directory", &spec.cwd),
        ("command", &spec.command),
    ] {
        if space_home.starts_with(canonical_or_original(path)) {
            return Err(unusable_error(&format!(
                "the MCP server's {label} would mount the Space's home directory"
            )));
        }
    }
    for (label, path) in [
        ("plugin root", &spec.plugin_root),
        ("plugin data", &spec.plugin_data),
    ] {
        if !path.is_dir() {
            return Err(unusable_error(&format!(
                "the MCP server's {label} is not a directory"
            )));
        }
    }
    if !is_executable(&spec.command) {
        return Err(unusable_error(NOT_EXECUTABLE));
    }
    Ok(())
}

/// Builds a [`StdioError::Unusable`].
fn unusable_error(message: &str) -> StdioError {
    StdioError::Unusable(message.to_owned())
}

/// Builds a [`StdioError::SandboxUnavailable`].
fn sandbox_unavailable_error(message: &str) -> StdioError {
    StdioError::SandboxUnavailable(message.to_owned())
}

/// Builds a [`StdioError::Unreachable`].
fn unreachable_error(message: &str) -> StdioError {
    StdioError::Unreachable(message.to_owned())
}

/// Builds a [`StdioError::Protocol`].
fn protocol_error(message: &str) -> StdioError {
    StdioError::Protocol(message.to_owned())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    use super::{
        BASE_PATH, StdioClient, StdioSpec, WORKING_DIRECTORY_ESCAPES, WORKING_DIRECTORY_MISSING,
        check_launch, check_space_home, prepare_launch, sandbox_arguments, sandbox_command,
        sandboxed_environment,
    };
    use super::{StdioError, read_line, resolve_command, resolve_launch_cwd};
    #[cfg(target_os = "linux")]
    use crate::mcp::{self, APP_MIME_TYPE, McpContent};
    #[cfg(target_os = "linux")]
    use serde_json::{Value, json};
    #[cfg(target_os = "linux")]
    use std::collections::BTreeMap;
    #[cfg(target_os = "linux")]
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    /// The argument list as text; every path a test makes is UTF-8.
    #[cfg(target_os = "linux")]
    fn strings(arguments: &[std::ffi::OsString]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// Whether one flag and its one value are adjacent.
    #[cfg(target_os = "linux")]
    fn has_pair(arguments: &[String], flag: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|window| window[0].as_str() == flag && window[1].as_str() == value)
    }

    /// Whether one flag and its two values are adjacent.
    #[cfg(target_os = "linux")]
    fn has_triple(arguments: &[String], flag: &str, first: &str, second: &str) -> bool {
        arguments.windows(3).any(|window| {
            window[0].as_str() == flag
                && window[1].as_str() == first
                && window[2].as_str() == second
        })
    }

    /// The environment a command will hand to the process it starts.
    #[cfg(target_os = "linux")]
    fn environment_of(
        command: &tokio::process::Command,
    ) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        command
            .as_std()
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.unwrap_or_default().to_owned()))
            .collect()
    }

    /// The variable of one command environment, when it is set.
    #[cfg(target_os = "linux")]
    fn environment_value<'a>(
        environment: &'a [(std::ffi::OsString, std::ffi::OsString)],
        name: &str,
    ) -> Option<&'a std::ffi::OsStr> {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }

    /// One executable this machine has.
    #[cfg(target_os = "linux")]
    fn true_command() -> PathBuf {
        ["/bin/true", "/usr/bin/true"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .expect("this machine has a true(1)")
    }

    /// A specification that starts a harmless program in three fresh
    /// directories: a package root, its data directory, and the Space's home.
    #[cfg(target_os = "linux")]
    fn linux_spec() -> (TempDir, TempDir, TempDir, StdioSpec) {
        let root = TempDir::new().expect("a temporary package directory");
        let data = TempDir::new().expect("a temporary data directory");
        let home = TempDir::new().expect("a temporary Space home");
        let mut env = BTreeMap::new();
        env.insert("PLUGIN_ROOT".to_owned(), "/wrong/root".to_owned());
        env.insert("NEHA_TEST_MARKER".to_owned(), "marker".to_owned());
        let spec = StdioSpec {
            command: true_command(),
            args: vec!["--arg".to_owned()],
            env,
            cwd: data.path().to_path_buf(),
            plugin_root: root.path().to_path_buf(),
            plugin_data: data.path().to_path_buf(),
            space_home: home.path().to_path_buf(),
            credential: Some("stdio-credential".to_owned()),
        };
        (root, data, home, spec)
    }

    #[test]
    fn a_command_resolves_from_its_package_or_its_path() {
        let root = TempDir::new().unwrap();
        let executable = root.path().join("bin").join("tool");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert_eq!(
            resolve_command("./bin/tool", root.path()).unwrap(),
            executable
        );
        assert_eq!(
            resolve_command(executable.to_str().unwrap(), root.path()).unwrap(),
            executable
        );
        assert!(matches!(
            resolve_command("./bin/missing", root.path()),
            Err(StdioError::Unusable(_))
        ));
        assert!(matches!(
            resolve_command("../escape", root.path()),
            Err(StdioError::Unusable(_))
        ));
        assert!(matches!(
            resolve_command("neha-no-such-command-exists", root.path()),
            Err(StdioError::Unusable(_))
        ));
        assert!(matches!(
            resolve_command("", root.path()),
            Err(StdioError::Unusable(_))
        ));

        let plain = root.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                matches!(
                    resolve_command("./plain", root.path()),
                    Err(StdioError::Unusable(_))
                ),
                "a file this machine may not execute is not a command"
            );
        }
    }

    /// A path's components below its filesystem root: the text a textual
    /// placeholder expansion contributes when a placeholder stands for that
    /// path.
    fn below_filesystem_root(path: &std::path::Path) -> std::path::PathBuf {
        let mut below = std::path::PathBuf::new();
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                below.push(name);
            }
        }
        below
    }

    /// A recorded working directory is `/`-separated and relative to one root,
    /// so resolving it builds the path under that root: `.` is dropped, `..`
    /// removes the component before it, and a second placeholder contributes
    /// the components of the root it names. Resolution against the real roots is
    /// the containment decision, so the loader keeps the expression without
    /// judging where it lands.
    #[test]
    fn a_relative_working_directory_resolves_under_its_root() {
        let data = TempDir::new().unwrap();
        let package = TempDir::new().unwrap();

        assert_eq!(
            resolve_launch_cwd(data.path(), "work/nested", package.path(), data.path()).unwrap(),
            data.path().join("work").join("nested")
        );
        assert_eq!(
            resolve_launch_cwd(data.path(), "", package.path(), data.path()).unwrap(),
            data.path()
        );
        assert_eq!(
            resolve_launch_cwd(data.path(), "a/../work", package.path(), data.path()).unwrap(),
            data.path().join("work")
        );

        // A second placeholder expands where it stands, and a `..` after it
        // removes one of its components: the directory stays in the declared
        // data root.
        let mut expected = data.path().join("a");
        expected.push(below_filesystem_root(package.path()));
        expected.pop();
        expected.push("b");
        assert_eq!(
            resolve_launch_cwd(
                data.path(),
                "a/${PLUGIN_ROOT}/../b",
                package.path(),
                data.path(),
            )
            .unwrap(),
            expected
        );

        // A `..` that would remove the root itself is the sandbox's fixed
        // escape refusal.
        let error = resolve_launch_cwd(data.path(), "..", package.path(), data.path())
            .expect_err("a working directory above its root is refused");
        assert!(matches!(error, StdioError::Unusable(_)), "{error}");
    }

    /// A canonical Windows home is a verbatim path (`\\?\C:\...`), and after
    /// one a `/` is a character of a file name rather than a separator. Joining
    /// the configured directory as text would make `${PLUGIN_DATA}/work` one
    /// name directly under the home, and containment would refuse an ordinary
    /// configured working directory; building the path from the root fixes it.
    #[cfg(windows)]
    #[test]
    fn a_relative_working_directory_resolves_below_a_verbatim_root() {
        let data = std::path::Path::new(r"\\?\C:\space\plugin-data\weekly.tools");
        let package = std::path::Path::new(r"\\?\C:\space\plugins\weekly.tools@abc");

        // The record keeps `${PLUGIN_DATA}/work` as `work` under the data root.
        let resolved = resolve_launch_cwd(data, "work", package, data)
            .expect("an ordinary configured working directory resolves");
        assert_eq!(
            resolved,
            std::path::Path::new(r"\\?\C:\space\plugin-data\weekly.tools\work")
        );
        assert!(
            resolved.starts_with(data),
            "the directory stays in its root"
        );

        // Text concatenation is what used to build it: inside a verbatim path
        // the `/` stays a file-name character, so the result is not inside the
        // data root at all.
        let joined_text = std::path::PathBuf::from(format!("{}/work", data.display()));
        assert!(
            !joined_text.starts_with(data),
            "text concatenation does not resolve a verbatim root: {joined_text:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_command_resolves_against_the_path() {
        let root = TempDir::new().unwrap();
        let shell = resolve_command("sh", root.path()).expect("a shell is on the PATH");
        assert!(shell.is_absolute());
        assert!(shell.is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_argument_list_fences_the_server() {
        let (_root, _data, _home, spec) = linux_spec();
        let arguments = strings(&sandbox_arguments(&spec).expect("the specification is runnable"));

        for flag in [
            "--die-with-parent",
            "--unshare-all",
            "--share-net",
            "--new-session",
        ] {
            assert!(
                arguments.iter().any(|argument| argument == flag),
                "{flag} is missing"
            );
        }
        for (flag, value) in [("--proc", "/proc"), ("--dev", "/dev"), ("--tmpfs", "/tmp")] {
            assert!(
                has_pair(&arguments, flag, value),
                "{flag} {value} is missing"
            );
        }
        for directory in ["/usr", "/etc"] {
            assert!(
                has_triple(&arguments, "--ro-bind", directory, directory),
                "{directory} is not mounted read-only"
            );
        }
        let root = spec.plugin_root.to_str().unwrap();
        let data = spec.plugin_data.to_str().unwrap();
        assert!(
            has_triple(&arguments, "--ro-bind", root, root),
            "the package is not mounted read-only"
        );
        assert!(
            has_triple(&arguments, "--bind", data, data),
            "the data directory is not mounted read-write"
        );
        assert!(
            !has_triple(&arguments, "--bind", root, root),
            "the package must not be writable"
        );
        assert!(
            has_pair(&arguments, "--chdir", spec.cwd.to_str().unwrap()),
            "the working directory is not the sandbox's"
        );

        // The server starts after the last `--`, with the package's own
        // arguments.
        let separator = arguments
            .iter()
            .position(|argument| argument == "--")
            .expect("the list separates the server");
        assert_eq!(arguments[separator + 1], spec.command.to_str().unwrap());
        assert_eq!(&arguments[separator + 2..], ["--arg"]);

        // Nothing names the Space's home or this user's home, and no credential
        // is on the list: the environment is data, and the one value that is a
        // secret travels on the sandbox process.
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == spec.space_home.to_str().unwrap()),
            "no sandbox argument may name the Space's home"
        );
        for home in ["HOME", "NEHA_HOME"] {
            if let Ok(value) = std::env::var(home) {
                assert!(
                    !arguments.contains(&value),
                    "no sandbox argument may name {home}"
                );
            }
        }
        // The environment is not inherited at all: the launcher starts from an
        // empty environment, so no `--clearenv` is needed, and every non-secret
        // variable arrives as a `--setenv` pair.
        assert!(
            !arguments.iter().any(|argument| argument == "--clearenv"),
            "the launcher's own environment is empty, so nothing needs clearing"
        );
    }

    /// The bubblewrap process itself starts with trusted values only, so its
    /// dynamic loader never acts on a package's variable before the sandbox
    /// exists.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_process_environment_carries_only_trusted_values() {
        let (_root, _data, _home, mut spec) = linux_spec();
        // A package may declare the variables a dynamic loader acts on. They
        // are data for the sandbox, never variables of the bubblewrap process.
        spec.env
            .insert("LD_PRELOAD".to_owned(), "/package/loader.so".to_owned());
        spec.env
            .insert("LD_LIBRARY_PATH".to_owned(), "/package/lib".to_owned());
        let command = sandbox_command(&spec).expect("the sandbox command is built");
        let environment = environment_of(&command);

        let mut names: Vec<String> = environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["HOME", "NEHA_MCP_TOKEN", "PATH", "TMPDIR"],
            "bubblewrap starts with trusted values only"
        );
        assert_eq!(
            environment_value(&environment, "HOME"),
            Some(spec.plugin_data.as_os_str())
        );
        assert_eq!(
            environment_value(&environment, "PATH"),
            Some(std::ffi::OsStr::new(BASE_PATH))
        );
        assert_eq!(
            environment_value(&environment, "TMPDIR"),
            Some(std::ffi::OsStr::new("/tmp"))
        );
        assert_eq!(
            environment_value(&environment, "NEHA_MCP_TOKEN"),
            Some(std::ffi::OsStr::new("stdio-credential"))
        );
        for name in ["PLUGIN_ROOT", "PLUGIN_DATA", "NEHA_TEST_MARKER"] {
            assert!(
                environment_value(&environment, name).is_none(),
                "{name} is package data, not a variable of the sandbox process"
            );
        }
        for (name, _) in &environment {
            assert!(
                !name.to_string_lossy().starts_with("LD_"),
                "a loader variable must not reach the pre-sandbox process"
            );
        }
    }

    /// The package's variables and the Space's roots enter the sandbox as
    /// `--setenv` pairs, with the Space's roots last.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_package_environment_and_the_roots_enter_the_sandbox_as_setenv() {
        let (_root, _data, _home, spec) = linux_spec();
        let arguments = strings(&sandbox_arguments(&spec).expect("the specification is runnable"));
        let root = spec.plugin_root.to_str().unwrap();
        let data = spec.plugin_data.to_str().unwrap();

        assert!(
            has_triple(&arguments, "--setenv", "NEHA_TEST_MARKER", "marker"),
            "the package's environment does not reach the sandbox"
        );
        assert!(
            has_triple(&arguments, "--setenv", "PLUGIN_DATA", data),
            "the Space's data root does not reach the sandbox"
        );
        assert!(
            has_triple(&arguments, "--setenv", "PLUGIN_ROOT", root),
            "the Space's package root does not reach the sandbox"
        );

        // The package's own PLUGIN_ROOT is overridden because the Space's pair
        // comes later: bubblewrap applies the pairs in order, so the last one
        // for a name wins.
        let package_root = arguments
            .windows(3)
            .position(|window| {
                window[0] == "--setenv" && window[1] == "PLUGIN_ROOT" && window[2] == "/wrong/root"
            })
            .expect("the package's own PLUGIN_ROOT is passed through");
        let space_root = arguments
            .windows(3)
            .position(|window| {
                window[0] == "--setenv" && window[1] == "PLUGIN_ROOT" && window[2] == root
            })
            .expect("the Space's PLUGIN_ROOT is set");
        assert!(
            package_root < space_root,
            "the Space's PLUGIN_ROOT must come after the package's"
        );

        // Nothing clears the environment: bubblewrap's trusted variables are
        // inherited, and the pairs add the data on top.
        assert!(
            !arguments.iter().any(|argument| argument == "--clearenv"),
            "the environment is built from an empty launcher, so nothing needs clearing"
        );
    }

    /// The server sees exactly the trusted base, the package's variables, the
    /// Space's roots — the Space's values win — and the credential.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_child_environment_is_the_base_the_package_and_the_roots() {
        let (_root, _data, _home, spec) = linux_spec();
        let command = sandbox_command(&spec).expect("the sandbox command is built");

        // The child inherits bubblewrap's environment ...
        let mut child: BTreeMap<String, String> = environment_of(&command)
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        // ... and bubblewrap applies the `--setenv` pairs in order, so a later
        // pair for a name replaces the inherited value.
        for (name, value) in sandboxed_environment(&spec) {
            child.insert(
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }

        let data = spec.plugin_data.to_string_lossy().into_owned();
        let root = spec.plugin_root.to_string_lossy().into_owned();
        let expected = BTreeMap::from([
            ("HOME".to_owned(), data.clone()),
            ("NEHA_MCP_TOKEN".to_owned(), "stdio-credential".to_owned()),
            ("NEHA_TEST_MARKER".to_owned(), "marker".to_owned()),
            ("PATH".to_owned(), BASE_PATH.to_owned()),
            ("PLUGIN_DATA".to_owned(), data),
            // The Space's root, not the package's `/wrong/root`.
            ("PLUGIN_ROOT".to_owned(), root),
            ("TMPDIR".to_owned(), "/tmp".to_owned()),
        ]);
        assert_eq!(
            child, expected,
            "the server sees the base, the package's variables, and the Space's roots"
        );
    }

    /// A package cannot shadow the Space's credential: the token is inherited,
    /// never a `--setenv` pair, so a package's value for its name is dropped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_package_cannot_replace_the_credential() {
        let (_root, _data, _home, mut spec) = linux_spec();
        spec.env
            .insert("NEHA_MCP_TOKEN".to_owned(), "package-token".to_owned());
        let command = sandbox_command(&spec).expect("the sandbox command is built");

        let arguments = strings(&sandbox_arguments(&spec).expect("the specification is runnable"));
        assert!(
            !has_triple(&arguments, "--setenv", "NEHA_MCP_TOKEN", "package-token"),
            "the token's name is never a --setenv pair"
        );

        // The child's environment still holds the Space's credential.
        let mut child: BTreeMap<String, String> = environment_of(&command)
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for (name, value) in sandboxed_environment(&spec) {
            child.insert(
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
        assert_eq!(
            child.get("NEHA_MCP_TOKEN").map(String::as_str),
            Some("stdio-credential")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_credential_never_enters_the_sandbox_argv() {
        let (_root, _data, _home, spec) = linux_spec();
        let command = sandbox_command(&spec).expect("the sandbox command is built");
        let arguments: Vec<std::ffi::OsString> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_owned())
            .collect();

        // Every argument bubblewrap receives, before and after `--`, is free
        // of the credential: `/proc/<pid>/cmdline` would show it otherwise.
        for argument in &arguments {
            assert!(
                !argument.to_string_lossy().contains("stdio-credential"),
                "the credential is on the sandbox's command line"
            );
        }
        // Package data and the roots may be `--setenv` pairs; the credential is
        // inherited by bubblewrap and reaches the child's environment that way.
        assert!(
            !has_triple(
                &strings(&arguments),
                "--setenv",
                "NEHA_MCP_TOKEN",
                "stdio-credential"
            ),
            "the credential must not be a --setenv pair"
        );

        // It is delivered exactly once, through bubblewrap's environment.
        let environment = environment_of(&command);
        assert_eq!(
            environment
                .iter()
                .filter(|(name, _)| name == "NEHA_MCP_TOKEN")
                .count(),
            1
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_refuses_a_specification_that_escapes_its_directories() {
        let root = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let spec = || StdioSpec {
            command: true_command(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: data.path().to_path_buf(),
            plugin_root: root.path().to_path_buf(),
            plugin_data: data.path().to_path_buf(),
            space_home: home.path().to_path_buf(),
            credential: None,
        };

        // A working directory neither mounted directory holds is refused.
        let mut escaped = spec();
        escaped.cwd = outside.path().to_path_buf();
        assert!(matches!(
            sandbox_arguments(&escaped),
            Err(StdioError::Unusable(_))
        ));
        // A parent component is refused even when the resolved path would be
        // inside: the sandbox mounts paths, not resolutions.
        let mut escaping = spec();
        escaping.cwd = data.path().join("..").join("data");
        assert!(matches!(
            sandbox_arguments(&escaping),
            Err(StdioError::Unusable(_))
        ));

        // Relative mounts are refused: bubblewrap would resolve them against
        // the host's working directory.
        let mut relative_root = spec();
        relative_root.plugin_root = PathBuf::from("plugins/package");
        assert!(matches!(
            sandbox_arguments(&relative_root),
            Err(StdioError::Unusable(_))
        ));
        let mut relative_data = spec();
        relative_data.plugin_data = PathBuf::from("plugins/data");
        assert!(matches!(
            sandbox_arguments(&relative_data),
            Err(StdioError::Unusable(_))
        ));
        let mut relative_cwd = spec();
        relative_cwd.cwd = PathBuf::from("plugins/data");
        assert!(matches!(
            sandbox_arguments(&relative_cwd),
            Err(StdioError::Unusable(_))
        ));
        // The filesystem root would mount the whole machine.
        let mut rooted = spec();
        rooted.plugin_root = PathBuf::from("/");
        assert!(matches!(
            sandbox_arguments(&rooted),
            Err(StdioError::Unusable(_))
        ));

        // A working directory inside the package is accepted.
        let inside = root.path().join("work");
        std::fs::create_dir(&inside).unwrap();
        let mut contained = spec();
        contained.cwd = inside;
        assert!(sandbox_arguments(&contained).is_ok());

        // A package directory that is the Space's home would mount the home
        // itself; the normal layout mounts a subtree inside it.
        let mut mounting_home = spec();
        mounting_home.plugin_root = mounting_home.space_home.clone();
        mounting_home.cwd = mounting_home.plugin_data.clone();
        assert!(matches!(
            sandbox_arguments(&mounting_home),
            Err(StdioError::Unusable(_))
        ));
    }

    /// A Space home a system mount would expose refuses the launch with a fixed
    /// reason, before any argument is built; a normal home launches.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_space_home_inside_a_system_mount_is_refused() {
        // The documented case, through the pure check: a home under a system
        // directory the sandbox mounts read-only.
        let error = check_space_home(
            Path::new("/usr/local/share/neha-home"),
            &[PathBuf::from("/usr/local")],
        )
        .expect_err("a home under a mounted system directory is refused");
        assert!(matches!(error, StdioError::SandboxUnavailable(_)));

        // The same relation on directories a test may create: under, equal to,
        // and around the mount all expose at least the home's subtree.
        let mount = TempDir::new().unwrap();
        let home = mount.path().join("neha-home");
        assert!(matches!(
            check_space_home(&home, &[mount.path().to_path_buf()]),
            Err(StdioError::SandboxUnavailable(_))
        ));
        assert!(matches!(
            check_space_home(mount.path(), &[mount.path().to_path_buf()]),
            Err(StdioError::SandboxUnavailable(_))
        ));
        assert!(matches!(
            check_space_home(mount.path(), &[mount.path().join("usr")]),
            Err(StdioError::SandboxUnavailable(_))
        ));

        // A home under none of the mounts is a normal launch.
        let (_root, _data, _home, spec) = linux_spec();
        assert!(sandbox_arguments(&spec).is_ok());

        // Through the whole launcher, with the documented path: the machine's
        // own `/usr/local` mount would expose a home installed under it.
        if Path::new("/usr/local").is_dir() {
            let (_root, _data, _home, mut spec) = linux_spec();
            spec.space_home = PathBuf::from("/usr/local/share/neha-home");
            assert!(
                matches!(
                    sandbox_arguments(&spec),
                    Err(StdioError::SandboxUnavailable(_))
                ),
                "a spec whose home is under a system mount refuses the launch"
            );
        }
    }

    /// A link inside the data directory never moves the working directory, or
    /// a first launch's directories, outside the data root.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_working_directory_through_an_escaping_link_is_refused() {
        let (_root, _data, _home, mut spec) = linux_spec();
        let data = spec.plugin_data.clone();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), data.join("escape")).unwrap();
        spec.cwd = data.join("escape").join("work");

        let error = prepare_launch(&spec).expect_err("a link out of the data root is refused");
        assert!(
            matches!(&error, StdioError::SandboxUnavailable(reason) if reason == WORKING_DIRECTORY_ESCAPES),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "the launcher creates nothing outside the data root"
        );
        assert!(
            !data.join("escape").join("work").exists(),
            "the link is refused, never followed"
        );
    }

    /// A missing data-rooted working directory is created a component at a
    /// time, all of it inside the data root.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_data_working_directory_is_created_inside_its_root() {
        let (_root, _data, _home, mut spec) = linux_spec();
        let data = spec.plugin_data.clone();
        spec.cwd = data.join("work").join("nested");

        prepare_launch(&spec).expect("a data-rooted working directory is created");

        assert!(data.join("work").join("nested").is_dir());
    }

    /// Two launches that create the same missing component both succeed.
    ///
    /// The walk classifies a component as missing and then creates it: the
    /// second launch to reach `mkdirat` is answered `EEXIST`, which is the
    /// directory the first one made, not a failure. The create step is called
    /// twice here to simulate the two creators at the one call they race on.
    #[cfg(target_os = "linux")]
    #[test]
    fn creating_a_data_working_directory_component_twice_is_not_a_failure() {
        let root = TempDir::new().unwrap();
        let directory = rustix::fs::openat(
            rustix::fs::CWD,
            root.path(),
            rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .expect("the temporary root opens");
        let name = Path::new("work");

        super::create_data_component(&directory, name).expect("the first launch creates it");
        super::create_data_component(&directory, name)
            .expect("the second launch is answered EEXIST, not a failure");

        assert!(root.path().join("work").is_dir());

        // Accepting `EEXIST` is not accepting the component: a file another
        // creator left is refused by the `openat` the walk enters it with.
        std::fs::write(root.path().join("file"), b"not a directory").unwrap();
        super::create_data_component(&directory, Path::new("file"))
            .expect("a component that is already there is not created twice");
        assert!(
            rustix::fs::openat(
                &directory,
                Path::new("file"),
                rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .is_err(),
            "a non-directory component is still refused when the walk opens it"
        );
    }

    /// A link is refused even when it stays inside the data root: the walk
    /// never follows one, so another process cannot turn a checked directory
    /// into a path outside the root between two steps.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_inside_the_data_root_is_refused() {
        let (_root, _data, _home, mut spec) = linux_spec();
        let data = spec.plugin_data.clone();
        std::fs::create_dir(data.join("actual")).unwrap();
        std::os::unix::fs::symlink(data.join("actual"), data.join("link")).unwrap();
        spec.cwd = data.join("link").join("work");

        let error = prepare_launch(&spec).expect_err("a link is a fixed refusal");
        assert!(
            matches!(&error, StdioError::SandboxUnavailable(reason) if reason == WORKING_DIRECTORY_ESCAPES),
            "{error:?}"
        );
        assert!(
            !data.join("actual").join("work").exists(),
            "the launcher creates nothing through a link"
        );
    }

    /// A component a first launch would create is there as a regular file: the
    /// walk refuses it and creates nothing below it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_data_working_directory_component_that_is_a_file_is_refused() {
        let (_root, _data, _home, mut spec) = linux_spec();
        let data = spec.plugin_data.clone();
        std::fs::write(data.join("work"), b"not a directory").unwrap();
        spec.cwd = data.join("work").join("nested");

        let error = prepare_launch(&spec).expect_err("a file is not a working-directory component");
        assert!(
            matches!(&error, StdioError::Unusable(reason) if reason == WORKING_DIRECTORY_MISSING),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(data.join("work")).unwrap(),
            "not a directory",
            "the file the component names is untouched"
        );
        assert!(!data.join("work").join("nested").exists());
    }

    /// A listing checks the configured working directory without writing: an
    /// existing component must be a directory, and a missing one is left for
    /// the first launch to create.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_listing_check_writes_nothing_and_accepts_a_missing_data_directory() {
        let (_root, _data, _home, mut spec) = linux_spec();
        let data = spec.plugin_data.clone();

        // Every component is missing: the first launch creates them, so the
        // row is connected and nothing is written.
        spec.cwd = data.join("work").join("nested");
        check_launch(&spec).expect("a data-rooted working directory may not exist yet");
        assert!(!data.join("work").exists(), "a listing creates nothing");

        // The first component exists and is a directory; the rest are missing.
        std::fs::create_dir(data.join("work")).unwrap();
        check_launch(&spec).expect("the existing part of the working directory is fine");
        assert!(
            !data.join("work").join("nested").exists(),
            "a listing creates nothing"
        );

        // A component that is a regular file is not a directory: the launch
        // would refuse it, so the row is unconnected and the file stays.
        std::fs::write(data.join("work").join("nested"), b"file").unwrap();
        let error = check_launch(&spec).expect_err("a file component leaves the row unconnected");
        assert!(
            matches!(&error, StdioError::Unusable(reason) if reason == WORKING_DIRECTORY_MISSING),
            "{error:?}"
        );

        // A package-rooted working directory must already be there: the check
        // refuses a missing one without writing inside the package.
        let (_root2, _data2, _home2, mut package) = linux_spec();
        package.cwd = package.plugin_root.join("missing");
        let error = check_launch(&package).expect_err("a package-rooted directory must exist");
        assert!(matches!(error, StdioError::Unusable(_)));
        assert!(!package.cwd.exists());
    }

    /// A package-rooted working directory is published by the install: it must
    /// exist, and the launcher never writes inside the package.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_package_working_directory_is_refused_not_created() {
        let (_root, _data, _home, mut spec) = linux_spec();
        spec.cwd = spec.plugin_root.join("missing");

        let error =
            prepare_launch(&spec).expect_err("a package-rooted working directory must exist");

        assert!(matches!(error, StdioError::Unusable(_)));
        assert!(
            !spec.cwd.exists(),
            "the launcher never writes inside the package"
        );
    }

    /// The command that starts this machine's bubblewrap, when it has one.
    #[cfg(target_os = "linux")]
    fn bubblewrap() -> Option<PathBuf> {
        resolve_command("bwrap", Path::new("/")).ok()
    }

    /// The command that runs a Python 3 fixture, when this machine has one.
    #[cfg(target_os = "linux")]
    fn python() -> Option<PathBuf> {
        ["python3", "/usr/bin/python3"]
            .into_iter()
            .find_map(|name| resolve_command(name, Path::new("/")).ok())
    }

    /// The fixture MCP server every spawn test runs.
    #[cfg(target_os = "linux")]
    const FIXTURE: &str = r#"
import json, os, sys, time

mode = os.environ.get("NEHA_TEST_FIXTURE", "normal")
handshake = {}

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def answer(request_id, value):
    send({"jsonrpc": "2.0", "id": request_id, "result": value})

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        handshake = params
        if mode == "malformed":
            sys.stdout.write("this is not JSON\n")
            sys.stdout.flush()
        elif mode == "wrong-version":
            answer(request_id, {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}})
        elif mode == "wrong-jsonrpc":
            send({"jsonrpc": "1.0", "id": request_id, "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}}})
        else:
            answer(request_id, {"protocolVersion": "2025-06-18", "capabilities": {"extensions": {"io.modelcontextprotocol/ui": {"mimeTypes": ["text/html;profile=mcp-app"]}}}, "serverInfo": {"name": "fixture", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        answer(request_id, {"tools": [
            {"name": "echo", "description": "Echo text back.", "inputSchema": {"type": "object"}, "_meta": {"ui": {"resourceUri": "ui://fixture/app.html", "visibility": ["model"]}}},
        ]})
    elif method == "tools/call":
        if mode == "slow":
            time.sleep(30)
        name = params.get("name")
        arguments = params.get("arguments") or {}
        if name == "env":
            text = json.dumps({key: os.environ.get(key) for key in ["PLUGIN_ROOT", "PLUGIN_DATA", "HOME", "NEHA_MCP_TOKEN", "NEHA_TEST_MARKER"]})
        elif name == "env-all":
            text = json.dumps({"environment": dict(os.environ), "argv": sys.argv})
        elif name == "request":
            # A server may send a notification and requests of its own on this
            # transport. The client must ignore the notification, answer ping
            # with a success result, report every other method as not found, and
            # only then take this call's result.
            send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1}})
            send({"jsonrpc": "2.0", "id": "server-request-ping", "method": "ping", "params": {}})
            send({"jsonrpc": "2.0", "id": "server-request-1", "method": "sampling/createMessage", "params": {}})
            first = sys.stdin.readline().strip()
            second = sys.stdin.readline().strip()
            text = first + "\n" + second
        elif name == "probe":
            text = "visible" if os.path.exists(arguments.get("path", "")) else "hidden"
        elif name == "handshake":
            text = json.dumps({key: handshake.get(key) for key in ["protocolVersion", "clientInfo", "capabilities"]})
        else:
            text = str(arguments.get("text", ""))
        answer(request_id, {"content": [{"type": "text", "text": text}], "isError": False})
    elif method == "resources/read":
        answer(request_id, {"contents": [{"uri": params.get("uri"), "mimeType": "text/html;profile=mcp-app", "text": "<!doctype html><html><body>fixture app</body></html>", "_meta": {"ui": {"csp": {"connectDomains": ["https://example.test"]}}}}]})
"#;

    /// One fixture server with the directories it needs kept alive.
    #[cfg(target_os = "linux")]
    struct Fixture {
        _root: TempDir,
        _data: TempDir,
        _home: TempDir,
        spec: StdioSpec,
    }

    /// Writes the fixture server and the specification that starts it.
    ///
    /// The script lives in the data directory, the only place the sandbox
    /// mounts read-write and the one directory Python may read it from.
    #[cfg(target_os = "linux")]
    fn fixture(mode: &str) -> Option<Fixture> {
        let python = python()?;
        let root = TempDir::new().ok()?;
        let data = TempDir::new().ok()?;
        let home = TempDir::new().ok()?;
        let script = data.path().join("fixture.py");
        std::fs::write(&script, FIXTURE).ok()?;
        let mut env = BTreeMap::new();
        env.insert("NEHA_TEST_FIXTURE".to_owned(), mode.to_owned());
        env.insert("NEHA_TEST_MARKER".to_owned(), "marker".to_owned());
        env.insert("PLUGIN_ROOT".to_owned(), "/wrong/root".to_owned());
        env.insert("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned());
        let spec = StdioSpec {
            command: python,
            args: vec![script.to_str()?.to_owned()],
            env,
            cwd: data.path().to_path_buf(),
            plugin_root: root.path().to_path_buf(),
            plugin_data: data.path().to_path_buf(),
            space_home: home.path().to_path_buf(),
            credential: Some("stdio-secret-token".to_owned()),
        };
        Some(Fixture {
            _root: root,
            _data: data,
            _home: home,
            spec,
        })
    }

    /// The text of the first content block of an answer.
    #[cfg(target_os = "linux")]
    fn text_of(answer: &crate::mcp::McpAnswer) -> String {
        match answer.content.first() {
            Some(McpContent::Text(text)) => text.clone(),
            other => panic!("expected a text answer, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_smoke_check_answers_whether_a_probe_can_start() {
        if bubblewrap().is_none() {
            eprintln!("skipping the sandbox smoke check: bubblewrap is not installed");
            return;
        }
        assert!(
            super::sandbox_smoke(),
            "this environment's sandbox starts a trusted program"
        );
        assert!(
            super::sandbox_smoke(),
            "the answer is cached, so a listing probes once"
        );
        assert!(super::sandbox_available(), "the cheap check still answers");
        // The reason names the sandbox, never a package or a credential, so a
        // row may show it; a machine whose smoke passes needs none, but the
        // fixed text must stay credential-free either way.
        assert!(!super::sandbox_unavailable_reason().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_sandbox_starts_a_program() {
        if bubblewrap().is_none() {
            eprintln!("skipping the sandbox smoke test: bubblewrap is not installed");
            return;
        }
        let (_root, _data, _home, mut spec) = linux_spec();
        spec.command = true_command();
        let mut command = sandbox_command(&spec).expect("the sandbox command is built");
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status = command.status().await.expect("bubblewrap starts");
        assert!(status.success(), "the sandbox runs a program: {status}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_stdio_server_lists_tools_calls_one_and_reads_its_app() {
        if bubblewrap().is_none() {
            eprintln!("skipping the stdio round trip: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("normal") else {
            eprintln!("skipping the stdio round trip: python3 is not installed");
            return;
        };
        let root = fixture.spec.plugin_root.to_str().unwrap().to_owned();
        let data = fixture.spec.plugin_data.to_str().unwrap().to_owned();
        let mut client = StdioClient::start(fixture.spec.clone())
            .await
            .expect("the fixture server starts");

        client
            .open()
            .await
            .expect("the fixture answers the handshake");
        let tools = client.tools().await.expect("the fixture lists its tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo text back.");
        assert_eq!(
            tools[0].ui_resource.as_deref(),
            Some("ui://fixture/app.html")
        );

        // The handshake carried this client's extension and identity.
        let answer = client
            .call("handshake", json!({}))
            .await
            .expect("the fixture reports the handshake");
        let handshake: Value =
            serde_json::from_str(&text_of(&answer)).expect("the handshake is JSON");
        assert_eq!(handshake["protocolVersion"], "2025-06-18");
        assert_eq!(handshake["clientInfo"]["name"], "neha");
        assert_eq!(handshake["clientInfo"]["version"], "1");
        assert_eq!(
            handshake["capabilities"]["extensions"][mcp::UI_EXTENSION]["mimeTypes"][0],
            APP_MIME_TYPE
        );

        let answer = client
            .call("echo", json!({"text": "hello"}))
            .await
            .expect("the fixture echoes");
        assert!(!answer.is_error);
        assert_eq!(answer.content, vec![McpContent::Text("hello".to_owned())]);

        // The sandbox sets the Space's variables after the package's, so the
        // package's own PLUGIN_ROOT does not win.
        let answer = client
            .call("env", json!({}))
            .await
            .expect("the fixture reports its environment");
        let environment: Value =
            serde_json::from_str(&text_of(&answer)).expect("the environment is JSON");
        assert_eq!(environment["PLUGIN_ROOT"], root);
        assert_eq!(environment["PLUGIN_DATA"], data);
        assert_eq!(environment["HOME"], data);
        assert_eq!(environment["NEHA_MCP_TOKEN"], "stdio-secret-token");
        assert_eq!(environment["NEHA_TEST_MARKER"], "marker");

        // The child sees exactly the environment the launcher built: nothing
        // of the Space's process is inherited, and the credential is in the
        // environment alone.
        let answer = client
            .call("env-all", json!({}))
            .await
            .expect("the fixture reports its whole environment");
        let snapshot: Value =
            serde_json::from_str(&text_of(&answer)).expect("the environment is JSON");
        // The child sees exactly the environment the launcher built. Two
        // variables are added by the runtime, not inherited from the Space:
        // bubblewrap points `PWD` at the directory it chdirs into, and CPython
        // writes `LC_CTYPE` when it enables UTF-8 mode. Everything else must be
        // one of the launcher's own variables.
        let environment = snapshot["environment"]
            .as_object()
            .expect("the environment is an object");
        let mut names: Vec<&str> = environment
            .keys()
            .map(String::as_str)
            .filter(|name| !matches!(*name, "PWD" | "LC_CTYPE"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "HOME",
                "NEHA_MCP_TOKEN",
                "NEHA_TEST_FIXTURE",
                "NEHA_TEST_MARKER",
                "PATH",
                "PLUGIN_DATA",
                "PLUGIN_ROOT",
                "PYTHONDONTWRITEBYTECODE",
                "TMPDIR",
            ],
            "the server sees no variable the launcher did not set"
        );
        assert_eq!(environment["NEHA_MCP_TOKEN"], "stdio-secret-token");
        assert_eq!(environment["PATH"], BASE_PATH);
        for argument in snapshot["argv"].as_array().expect("argv is a list") {
            assert!(
                !argument.to_string().contains("stdio-secret-token"),
                "the credential is not on the server's own command line"
            );
        }

        // Nothing outside the two mounted directories is visible.
        let outside = TempDir::new().unwrap();
        let canary = outside.path().join("canary");
        std::fs::write(&canary, b"a file the sandbox must not see").unwrap();
        let answer = client
            .call("probe", json!({"path": canary.to_str().unwrap()}))
            .await
            .expect("the fixture probes the sandbox");
        assert_eq!(
            text_of(&answer),
            "hidden",
            "the sandbox mounts only the package and its data"
        );

        let resource = client
            .read_resource("ui://fixture/app.html")
            .await
            .expect("the fixture answers its app");
        assert_eq!(resource.mime_type.as_deref(), Some(APP_MIME_TYPE));
        let html = String::from_utf8(resource.data.clone()).expect("the app is text");
        assert!(html.contains("<html"), "the app is HTML: {html}");
        let policy = resource.content_security_policy(Some("stdio-secret-token"));
        assert!(
            policy.contains("connect-src https://example.test"),
            "the app's declared origin reaches the policy: {policy}"
        );
    }

    /// A server's own requests are answered before the call's result is taken:
    /// `ping` succeeds, every other method is "method not found", and a
    /// notification is ignored.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_server_request_is_answered_and_its_notification_ignored() {
        if bubblewrap().is_none() {
            eprintln!("skipping the server request test: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("normal") else {
            eprintln!("skipping the server request test: python3 is not installed");
            return;
        };
        let mut client = StdioClient::start(fixture.spec)
            .await
            .expect("the fixture server starts");
        client
            .open()
            .await
            .expect("the fixture answers the handshake");

        let answer = client
            .call("request", json!({}))
            .await
            .expect("the client answers the server's requests and takes the result");
        let replies = text_of(&answer);
        let mut replies = replies.lines();
        let ping: Value = serde_json::from_str(replies.next().expect("the ping answer is a line"))
            .expect("the ping answer is JSON");
        assert_eq!(ping["jsonrpc"], "2.0");
        assert_eq!(ping["id"], "server-request-ping");
        assert_eq!(
            ping["result"],
            json!({}),
            "MCP requires ping to succeed with an empty result"
        );
        assert!(
            ping.get("error").is_none(),
            "a successful ping carries no error: {ping}"
        );
        let unknown: Value =
            serde_json::from_str(replies.next().expect("the unknown-method answer is a line"))
                .expect("the unknown-method answer is JSON");
        assert_eq!(unknown["jsonrpc"], "2.0");
        assert_eq!(unknown["id"], "server-request-1");
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(unknown["error"]["message"], "method not found");
        assert!(
            unknown.get("result").is_none(),
            "a method-not-found answer carries no result: {unknown}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_handshake_with_another_protocol_version_is_refused() {
        if bubblewrap().is_none() {
            eprintln!("skipping the version refusal: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("wrong-version") else {
            eprintln!("skipping the version refusal: python3 is not installed");
            return;
        };
        let mut client = StdioClient::start(fixture.spec)
            .await
            .expect("the fixture server starts");
        let error = client
            .open()
            .await
            .expect_err("another protocol version is refused");
        assert!(matches!(error, StdioError::Protocol(_)));
        assert!(
            client
                .child
                .try_wait()
                .expect("the child is waitable")
                .is_some(),
            "a server that speaks another revision is killed"
        );
    }

    /// An answer that names another JSON-RPC version cannot be matched to the
    /// request, so the transport is torn down.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_answer_with_another_json_rpc_version_is_refused() {
        if bubblewrap().is_none() {
            eprintln!("skipping the JSON-RPC version refusal: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("wrong-jsonrpc") else {
            eprintln!("skipping the JSON-RPC version refusal: python3 is not installed");
            return;
        };
        let mut client = StdioClient::start(fixture.spec)
            .await
            .expect("the fixture server starts");
        let error = client
            .open()
            .await
            .expect_err("another JSON-RPC version is refused");
        assert!(matches!(error, StdioError::Protocol(_)));
        assert!(
            client
                .child
                .try_wait()
                .expect("the child is waitable")
                .is_some(),
            "a server whose answer names another version is killed"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_malformed_line_is_refused() {
        if bubblewrap().is_none() {
            eprintln!("skipping the malformed line refusal: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("malformed") else {
            eprintln!("skipping the malformed line refusal: python3 is not installed");
            return;
        };
        let mut client = StdioClient::start(fixture.spec)
            .await
            .expect("the fixture server starts");
        let error = client
            .open()
            .await
            .expect_err("a malformed line is a protocol error");
        assert!(matches!(error, StdioError::Protocol(_)));
        assert!(
            client
                .child
                .try_wait()
                .expect("the child is waitable")
                .is_some(),
            "a server that breaks the framing is killed"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_call_past_its_deadline_is_killed() {
        if bubblewrap().is_none() {
            eprintln!("skipping the deadline test: bubblewrap is not installed");
            return;
        }
        let Some(fixture) = fixture("slow") else {
            eprintln!("skipping the deadline test: python3 is not installed");
            return;
        };
        let mut client = StdioClient::start(fixture.spec)
            .await
            .expect("the fixture server starts");
        client
            .open()
            .await
            .expect("the fixture answers the handshake");
        client.call_timeout = Duration::from_millis(200);
        let error = client
            .call("echo", json!({"text": "hello"}))
            .await
            .expect_err("a call past its deadline fails");
        assert!(matches!(error, StdioError::Unreachable(_)));
        assert!(
            client
                .child
                .try_wait()
                .expect("the child is waitable")
                .is_some(),
            "the server is killed when its deadline passes"
        );
    }

    #[tokio::test]
    async fn a_line_past_the_cap_is_refused() {
        let mut reader = tokio::io::BufReader::new(tokio::io::repeat(b'x'));
        let error = read_line(&mut reader)
            .await
            .expect_err("an endless line is refused");
        assert!(matches!(error, StdioError::Protocol(_)));
    }

    #[tokio::test]
    async fn a_line_at_the_cap_is_read() {
        let mut bytes = vec![b'x'; crate::mcp::MAX_ANSWER_BYTES];
        bytes.push(b'\n');
        let mut reader = tokio::io::BufReader::new(bytes.as_slice());
        let line = read_line(&mut reader)
            .await
            .expect("a line of exactly the bound is read");
        assert_eq!(
            line.len(),
            crate::mcp::MAX_ANSWER_BYTES + 1,
            "the newline is framing"
        );
    }
}
