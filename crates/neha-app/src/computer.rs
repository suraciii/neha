//! The machine side of a real Computer: pairing and the channel daemon.
//!
//! `neha computer pair` asks a Space to admit this machine, shows the User a
//! one-time code, and waits until the User enters it. The machine then keeps
//! its own token in `computer.json` and serves the Space over one outgoing
//! WebSocket. `neha computer run` serves a saved pairing again.
//!
//! The daemon listens on no port and runs each action with the machine user's
//! rights, fenced by bubblewrap: a Bot reads what that user reads and writes
//! only its own folder. Reading and running are separate tasks, so a Space
//! that goes away is noticed at once and the shell it asked for is killed
//! instead of running on unattended.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt as _, StreamExt as _};
use neha::id::Id;
use neha_computer::{
    ActionStatus, CHANNEL_VERSION, ComputerRuntime as _, DeviceFrame, HostFrame, LocalRuntime,
    MAX_CHANNEL_FRAME_BYTES, ToolOutput,
};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::client::{self, PairingState};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The runtimes this machine opened, one per Bot, kept across reconnects.
type Runtimes = Arc<Mutex<HashMap<String, Arc<LocalRuntime>>>>;

/// How often the machine asks its Space how the pairing is doing.
const PAIR_POLL: Duration = Duration::from_secs(2);
/// How long a machine waits for its pairing code to be entered.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the daemon waits before it reconnects to its Space.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How long a cancelled action may take to end before its task is dropped.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// What a paired machine remembers between starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Config {
    /// The Space's base URL.
    space_url: String,
    /// The Computer this machine is.
    computer: String,
    /// The token the Space admits this machine with.
    token: String,
    /// The directory that holds one folder per Bot.
    workspace: PathBuf,
}

impl Config {
    /// Loads the saved pairing.
    fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    /// Loads the pairing saved at one path.
    fn load_from(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "not paired: {} is missing; run `neha computer pair`",
                path.display()
            )
        })?;
        serde_json::from_slice(&bytes).context("the saved computer configuration is corrupt")
    }

    /// Saves the pairing for later starts.
    fn save(&self) -> Result<()> {
        Self::save_to(self, &config_path()?)
    }

    /// Saves the pairing at one path, readable only by this machine user.
    fn save_to(&self, path: &Path) -> Result<()> {
        client::write_private(path, &serde_json::to_vec(self)?)
    }
}

/// Pairs this machine and then serves the Space until Ctrl-C.
pub async fn pair(
    space: Option<String>,
    workspace: Option<PathBuf>,
    name: Option<String>,
) -> Result<()> {
    let space_url = space_url(space, std::env::var("NEHA_SPACE_URL").ok())?;
    let (workspace, default) = match workspace {
        Some(path) => (path, false),
        None => (default_workspace()?, true),
    };
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("cannot create {}", workspace.display()))?;
    if default {
        client::restrict(&workspace, 0o700)?;
    }
    let workspace = std::fs::canonicalize(&workspace)
        .with_context(|| format!("cannot resolve {}", workspace.display()))?;
    // The channel credential lives in the configuration directory; a Bot must
    // never have it under its own writable root.
    refuse_workspace(&home()?, &workspace)?;

    let token = secret();
    let poll_secret = secret();
    let machine = name.unwrap_or_else(machine_name);
    let start = client::pair_start(
        &space_url,
        &machine,
        workspace.to_string_lossy().as_ref(),
        &token,
        &poll_secret,
    )
    .await?;
    println!("Pairing code: {}", start.code);
    let computer = wait_for_approval(&space_url, &start.device, &poll_secret).await?;

    let config = Config {
        space_url,
        computer,
        token,
        workspace,
    };
    config.save()?;

    serve(config).await
}

/// Starts the daemon with the saved pairing.
pub async fn run() -> Result<()> {
    serve(Config::load()?).await
}

/// Serves one Space until Ctrl-C, reconnecting after every failure.
async fn serve(config: Config) -> Result<()> {
    // Refuse a saved Space URL this milestone cannot serve before opening
    // anything: the Space listens on loopback only.
    normalized_space_url(&config.space_url).context("the saved Space URL is not usable")?;
    // A saved pairing from before the default moved may still name a workspace
    // inside the configuration directory; serving it would hand that directory
    // to the Bot through its own write root.
    refuse_workspace(&home()?, &config.workspace)?;
    // The configuration directory holds this Computer's channel credential.
    // Every Bot runtime hides it from the file tools and the sandbox.
    let hidden = hidden_roots()?;
    // A machine that cannot fence a command never serves as a Computer.
    fence(&config.workspace, &hidden)?;
    let runtimes: Runtimes = Arc::new(Mutex::new(HashMap::new()));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            served = connect_and_serve(&config, &hidden, Arc::clone(&runtimes)) => {
                if let Err(error) = served {
                    eprintln!("{error:#}");
                }
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

/// Refuses a machine whose commands bubblewrap cannot fence.
fn fence(workspace: &Path, hidden: &[PathBuf]) -> Result<()> {
    LocalRuntime::machine_with_hidden(workspace, hidden.to_vec())?;

    Ok(())
}

/// Polls the Space until a User enters the code, and returns the Computer.
async fn wait_for_approval(space_url: &str, device: &str, poll_secret: &str) -> Result<String> {
    let deadline = tokio::time::Instant::now() + PAIR_TIMEOUT;
    loop {
        match client::pair_poll(space_url, device, poll_secret).await? {
            PairingState::Approved { computer } => return Ok(computer),
            PairingState::Pending => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the pairing code expired; run `neha computer pair` again");
        }
        tokio::time::sleep(PAIR_POLL).await;
    }
}

/// Holds the channel to the Space until it closes, then returns the failure.
async fn connect_and_serve(config: &Config, hidden: &[PathBuf], runtimes: Runtimes) -> Result<()> {
    let mut request = socket_url(&config.space_url)?
        .into_client_request()
        .context("the Space URL is not usable")?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", config.token)
            .parse()
            .context("the saved token is not usable")?,
    );
    // A frame carries one File's bytes as base64, so both ends accept a frame
    // wider than the 16 MiB WebSocket default.
    let frame_limit = WebSocketConfig::default()
        .max_message_size(Some(MAX_CHANNEL_FRAME_BYTES))
        .max_frame_size(Some(MAX_CHANNEL_FRAME_BYTES));
    let (mut socket, _) =
        tokio_tungstenite::connect_async_with_config(request, Some(frame_limit), true)
            .await
            .context("the Space could not be reached")?;

    send(
        &mut socket,
        &DeviceFrame::Hello {
            version: CHANNEL_VERSION,
            computer: config.computer.clone(),
            workspace: config.workspace.to_string_lossy().into_owned(),
            sandbox: true,
        },
    )
    .await?;
    println!("Connected to {}.", config.space_url);

    serve_channel(config, hidden, socket, runtimes).await
}

/// Serves one open channel: reads frames, runs them one at a time, and kills
/// the running action as soon as the Space goes away.
async fn serve_channel(
    config: &Config,
    hidden: &[PathBuf],
    socket: Socket,
    runtimes: Runtimes,
) -> Result<()> {
    let (sink, stream) = socket.split();
    let (answers, mut outgoing) = mpsc::unbounded_channel::<DeviceFrame>();
    let mut writer = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(frame) = outgoing.recv().await {
            let Ok(text) = serde_json::to_string(&frame) else {
                break;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let (frames, mut incoming) = mpsc::unbounded_channel::<HostFrame>();
    // The runtime whose action is running, so the reader can kill it without
    // waiting for `execute` to return.
    let current: Arc<Mutex<Option<Arc<LocalRuntime>>>> = Arc::new(Mutex::new(None));
    let (stop, mut stopped) = watch::channel(false);

    let reader = tokio::spawn({
        let current = Arc::clone(&current);
        let stop = stop.clone();
        let answers = answers.clone();
        async move {
            let mut stream = stream;
            let result = loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<HostFrame>(text.as_str()) {
                            // A heartbeat needs no Computer work and must not
                            // queue behind a running action.
                            Ok(HostFrame::Ping) => {
                                if answers.send(DeviceFrame::Pong).is_err() {
                                    break Err(anyhow::anyhow!("the channel writer stopped"));
                                }
                            }
                            Ok(frame) => {
                                if frames.send(frame).is_err() {
                                    break Err(anyhow::anyhow!("the channel worker stopped"));
                                }
                            }
                            Err(_) => {
                                break Err(anyhow::anyhow!(
                                    "the Space sent a frame this daemon does not understand"
                                ));
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break Err(anyhow::anyhow!("the Space closed the channel"));
                    }
                    Some(Ok(Message::Binary(_))) => {
                        break Err(anyhow::anyhow!("the Space sent a binary frame"));
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Err(error)) => {
                        // A reset without a close frame means the Space left;
                        // either way this channel is over.
                        break Err(anyhow::anyhow!("the Space closed the channel: {error}"));
                    }
                }
            };
            // Tell the worker to stop, then kill the action it may be running:
            // a Space that left must not leave a command running on this
            // machine. The worker sweeps once more in case `execute` spawned
            // its process group just after this cancel.
            let _ = stop.send(true);
            let running = current.lock().ok().and_then(|mut current| current.take());
            if let Some(runtime) = running {
                runtime.cancel_in_flight();
            }

            result
        }
    });

    let mut worker = tokio::spawn({
        let config = config.clone();
        let hidden = hidden.to_vec();
        let runtimes = Arc::clone(&runtimes);
        let current = Arc::clone(&current);
        let answers = answers.clone();
        async move {
            loop {
                if *stopped.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    Some(frame) = incoming.recv() => {
                        let answer = tokio::select! {
                            biased;
                            _ = stopped.changed() => None,
                            answer = execute_frame(&config, &hidden, &runtimes, &current, frame) => {
                                Some(answer)
                            }
                        };
                        let Some(answer) = answer else { break; };
                        if answers.send(answer).is_err() {
                            break;
                        }
                    }
                }
            }
            // The second half of the cancellation: a frame that was starting
            // while the reader cancelled may have spawned a shell after it.
            if let Ok(runtimes) = runtimes.lock() {
                for runtime in runtimes.values() {
                    runtime.cancel_in_flight();
                }
            }
            if let Ok(mut current) = current.lock() {
                *current = None;
            }
        }
    });

    let reader_result = match reader.await {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("the channel reader failed: {error}")),
    };
    // The reader already told the worker to stop; wait for it to end and for
    // its shells to be killed before the next connection starts.
    let _ = stop.send(true);
    if tokio::time::timeout(CANCEL_GRACE, &mut worker)
        .await
        .is_err()
    {
        worker.abort();
        let _ = worker.await;
    }
    drop(answers);
    if tokio::time::timeout(CANCEL_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }

    reader_result
}

/// Answers one frame from the Space.
async fn execute_frame(
    config: &Config,
    hidden: &[PathBuf],
    runtimes: &Runtimes,
    current: &Mutex<Option<Arc<LocalRuntime>>>,
    frame: HostFrame,
) -> DeviceFrame {
    match frame {
        HostFrame::Ping => DeviceFrame::Pong,
        HostFrame::Action { id, bot, action } => {
            let runtime = match machine(config, hidden, runtimes, &bot) {
                Ok(runtime) => runtime,
                Err(message) => return DeviceFrame::Failed { id, message },
            };
            // The reader cancels whatever sits in this slot when the channel
            // goes away, even while `execute` still runs.
            *current.lock().expect("current action lock") = Some(Arc::clone(&runtime));
            let result = runtime.execute(action.into()).await;
            *current.lock().expect("current action lock") = None;
            DeviceFrame::Result {
                id,
                status: result.status,
                output: result.output,
            }
        }
        HostFrame::Read { id, bot, path } => match machine(config, hidden, runtimes, &bot) {
            Ok(runtime) => match runtime.read_bytes(&path).await {
                Ok(bytes) => DeviceFrame::Bytes {
                    id,
                    bytes: encode_bytes(&bytes),
                },
                Err(error) => DeviceFrame::Failed {
                    id,
                    message: error.to_string(),
                },
            },
            Err(message) => DeviceFrame::Failed { id, message },
        },
        HostFrame::Write {
            id,
            bot,
            path,
            bytes,
        } => match machine(config, hidden, runtimes, &bot) {
            Ok(runtime) => match decode_bytes(&bytes) {
                Ok(bytes) => {
                    let length = bytes.len();
                    match runtime.write_bytes(&path, bytes).await {
                        Ok(()) => write_receipt(id, &path, length),
                        Err(error) => DeviceFrame::Failed {
                            id,
                            message: error.to_string(),
                        },
                    }
                }
                Err(message) => DeviceFrame::Failed { id, message },
            },
            Err(message) => DeviceFrame::Failed { id, message },
        },
    }
}

/// Returns this Bot's runtime, opening its folder on first use.
fn machine(
    config: &Config,
    hidden: &[PathBuf],
    runtimes: &Runtimes,
    bot: &str,
) -> Result<Arc<LocalRuntime>, String> {
    let folder = folder_for(&config.workspace, bot)?;
    let mut runtimes = runtimes
        .lock()
        .map_err(|_| "the runtime table is locked".to_owned())?;
    match runtimes.entry(bot.to_owned()) {
        Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
        Entry::Vacant(entry) => {
            let runtime = Arc::new(
                LocalRuntime::machine_with_hidden(folder, hidden.to_vec())
                    .map_err(|error| error.to_string())?,
            );
            Ok(Arc::clone(entry.insert(runtime)))
        }
    }
}

/// Returns the folder a Bot works in, refusing a name that is not an identifier.
fn folder_for(workspace: &Path, bot: &str) -> Result<PathBuf, String> {
    let id: Id = bot
        .parse()
        .map_err(|_| format!("the Space named a Bot {bot:?}, which is not an identifier"))?;

    Ok(workspace.join(id.to_string()))
}

/// Answers a Write that succeeded: the protocol has no dedicated receipt frame.
fn write_receipt(id: u64, path: &str, bytes: usize) -> DeviceFrame {
    DeviceFrame::Result {
        id,
        status: ActionStatus::Succeeded,
        output: ToolOutput::full(format!("wrote {bytes} bytes to {path}")),
    }
}

/// Encodes File bytes for a frame.
fn encode_bytes(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes File bytes from a frame.
fn decode_bytes(text: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|error| format!("the File bytes are not URL-safe base64: {error}"))
}

/// Sends one frame to the Space.
async fn send(socket: &mut Socket, frame: &DeviceFrame) -> Result<()> {
    let text = serde_json::to_string(frame).context("cannot write the frame")?;
    socket
        .send(Message::Text(text.into()))
        .await
        .context("cannot send the frame")
}

/// Returns the WebSocket address of one Space's Computer channel.
fn socket_url(space_url: &str) -> Result<String> {
    let base = normalized_space_url(space_url)?;
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("the Space URL must start with http:// or https://");
    };

    Ok(format!("{socket}/api/computer/socket"))
}

/// Returns the Space URL to pair with, refusing a missing or non-loopback one.
fn space_url(explicit: Option<String>, saved: Option<String>) -> Result<String> {
    let Some(url) = explicit.or(saved) else {
        bail!("pass --space <url>, or set NEHA_SPACE_URL");
    };
    normalized_space_url(&url)
}

/// Returns a Space URL without a trailing slash, refusing a non-loopback one.
///
/// This milestone's Space listens on loopback only. Pairing with anything else
/// would send this machine's token to a Space this milestone cannot guard, so
/// the daemon refuses it with a message that names the allowed hosts.
fn normalized_space_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("the Space URL is empty");
    }
    let parsed =
        reqwest::Url::parse(url).context("the Space URL must be an absolute http(s) URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("the Space URL must start with http:// or https://");
    }
    let host = parsed
        .host_str()
        .context("the Space URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        bail!(
            "this milestone pairs only with a Space on this machine: \
             the Space URL must use 127.0.0.1, ::1, or localhost, not {host}"
        );
    }

    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// Returns 32 random bytes as URL-safe base64 without padding.
fn secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Returns the name this machine is known by, for the Space to show.
fn machine_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "computer".to_owned())
}

/// Returns the directory that holds this machine's Neha files.
fn home() -> Result<PathBuf> {
    home_from(
        std::env::var_os("NEHA_COMPUTER_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Returns the machine's Neha directory from the two variables it may name.
fn home_from(computer_home: Option<PathBuf>, user_home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = computer_home {
        return Ok(path);
    }
    let Some(user_home) = user_home else {
        bail!(
            "HOME is not set; set NEHA_COMPUTER_HOME to the directory that holds this machine's Neha files"
        );
    };

    Ok(user_home.join(".neha"))
}

/// Returns the path of the saved pairing.
fn config_path() -> Result<PathBuf> {
    Ok(home()?.join("computer.json"))
}

/// Returns the directory that holds one folder per Bot by default.
fn default_workspace() -> Result<PathBuf> {
    default_workspace_from(std::env::var_os("HOME").map(PathBuf::from))
}

/// Returns the default workspace beside the machine user's home.
fn default_workspace_from(user_home: Option<PathBuf>) -> Result<PathBuf> {
    let Some(user_home) = user_home else {
        bail!("HOME is not set; pass --workspace to choose where the Bot Workspaces live");
    };

    Ok(user_home.join("Neha"))
}

/// Returns the configuration directory as the runtimes must hide it.
///
/// The daemon passes this root to every Bot runtime: it holds the channel
/// credential in `computer.json`, so neither a Bot's file tools nor its sandbox
/// may reach it.
fn hidden_roots() -> Result<Vec<PathBuf>> {
    let directory = home()?;
    let directory = std::fs::canonicalize(&directory)
        .with_context(|| format!("cannot resolve {}", directory.display()))?;

    Ok(vec![directory])
}

/// Refuses a workspace inside the directory that holds the saved pairing.
///
/// Both paths are resolved, so a symlink between them cannot make the check
/// miss; the parts that do not exist yet are compared lexically.
fn refuse_workspace(config_directory: &Path, workspace: &Path) -> Result<()> {
    let directory = resolved_path(config_directory);
    let workspace = resolved_path(workspace);
    if workspace.starts_with(&directory) {
        bail!(
            "the workspace {} is inside the Computer's configuration directory {}; \
             choose a workspace outside it",
            workspace.display(),
            directory.display()
        );
    }

    Ok(())
}

/// Resolves a path through the symlinks of the longest existing prefix.
///
/// A path that does not exist yet compares as if its missing tail stayed where
/// it was named, which is enough for a containment check before creation.
fn resolved_path(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |dir| dir.join(path))
    };
    let mut existing = path.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let mut resolved = resolved;
            for part in missing.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let Some(name) = existing.file_name() else {
            return path;
        };
        missing.push(name.to_owned());
        match existing.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => existing = parent,
            _ => return path,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::id::Id;
    use neha_computer::{
        ActionStatus, CHANNEL_VERSION, DeviceFrame, HostFrame, LocalRuntime, ToolOutput, WireAction,
    };
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        Config, Runtimes, decode_bytes, default_workspace_from, encode_bytes, execute_frame,
        folder_for, home_from, normalized_space_url, refuse_workspace, secret, serve_channel,
        socket_url, space_url, write_receipt,
    };

    /// One saved pairing that names a fixed Computer.
    fn config_at(workspace: PathBuf) -> Config {
        Config {
            space_url: "http://127.0.0.1:7337".to_owned(),
            computer: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
            token: "machine-token".to_owned(),
            workspace,
        }
    }

    fn empty_runtimes() -> Runtimes {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn the_saved_configuration_round_trips_and_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("computer.json");
        let config = config_at(directory.path().join("workspace"));

        config.save_to(&path).unwrap();

        assert_eq!(Config::load_from(&path).unwrap(), config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_machine_that_is_not_paired_is_told_to_pair() {
        let directory = tempfile::tempdir().unwrap();
        let error = Config::load_from(&directory.path().join("computer.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("neha computer pair"), "{error}");
    }

    #[test]
    fn the_machine_directory_comes_from_its_own_variable_first() {
        assert_eq!(
            home_from(
                Some(PathBuf::from("/srv/neha")),
                Some(PathBuf::from("/home/ada"))
            )
            .unwrap(),
            PathBuf::from("/srv/neha")
        );
        assert_eq!(
            home_from(None, Some(PathBuf::from("/home/ada"))).unwrap(),
            PathBuf::from("/home/ada/.neha")
        );
        let error = home_from(None, None).unwrap_err().to_string();
        assert!(error.contains("NEHA_COMPUTER_HOME"), "{error}");
    }

    #[test]
    fn the_default_workspace_sits_beside_the_user_home() {
        assert_eq!(
            default_workspace_from(Some(PathBuf::from("/home/ada"))).unwrap(),
            PathBuf::from("/home/ada/Neha")
        );
        let error = default_workspace_from(None).unwrap_err().to_string();
        assert!(error.contains("--workspace"), "{error}");
    }

    #[test]
    fn a_workspace_inside_the_configuration_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = directory.path().join(".neha");
        std::fs::create_dir(&configuration).unwrap();
        let inside = configuration.join("workspace");
        std::fs::create_dir(&inside).unwrap();

        for workspace in [
            &inside,
            &configuration,
            &configuration.join("new/workspace"),
        ] {
            let error = refuse_workspace(&configuration, workspace)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("is inside the Computer's configuration directory"),
                "{error}"
            );
        }

        let outside = directory.path().join("Neha");
        std::fs::create_dir(&outside).unwrap();
        refuse_workspace(&configuration, &outside).unwrap();

        // A symlink is resolved before the two paths are compared.
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&configuration, &alias).unwrap();
            let error = refuse_workspace(&configuration, &alias.join("workspace"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("is inside the Computer's configuration directory"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_pairing_needs_a_space_url_and_prefers_the_one_given() {
        let error = space_url(None, None).unwrap_err().to_string();
        assert!(error.contains("NEHA_SPACE_URL"), "{error}");
        assert_eq!(
            space_url(None, Some("http://127.0.0.1:7337/".to_owned())).unwrap(),
            "http://127.0.0.1:7337"
        );
        assert_eq!(
            space_url(
                Some("https://localhost:7337".to_owned()),
                Some("http://127.0.0.1:7337".to_owned())
            )
            .unwrap(),
            "https://localhost:7337"
        );
        let error = space_url(Some("space.example".to_owned()), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("URL"), "{error}");
    }

    #[test]
    fn a_space_url_is_trimmed_and_must_be_http() {
        assert_eq!(
            normalized_space_url(" http://localhost:7337/ ").unwrap(),
            "http://localhost:7337"
        );
        assert!(normalized_space_url("").is_err());
        assert!(normalized_space_url("ftp://127.0.0.1:7337").is_err());
    }

    #[test]
    fn a_space_url_must_stay_on_this_machine() {
        for url in [
            "http://127.0.0.1:7337",
            "http://localhost:7337",
            "https://[::1]:7337",
        ] {
            assert!(normalized_space_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://10.0.0.5:7337",
            "http://127.0.0.2:7337",
            "https://space.example",
        ] {
            let error = normalized_space_url(url).unwrap_err().to_string();
            assert!(
                error.contains("127.0.0.1"),
                "a remote Space must be refused clearly: {error}"
            );
        }
        assert_eq!(
            socket_url("http://[::1]:7337").unwrap(),
            "ws://[::1]:7337/api/computer/socket"
        );
    }

    #[test]
    fn the_channel_speaks_ws_or_wss_to_the_space() {
        assert_eq!(
            socket_url("http://127.0.0.1:7337/").unwrap(),
            "ws://127.0.0.1:7337/api/computer/socket"
        );
        assert_eq!(
            socket_url("https://localhost:7337").unwrap(),
            "wss://localhost:7337/api/computer/socket"
        );
        assert!(socket_url("ftp://127.0.0.1:7337").is_err());
    }

    #[test]
    fn a_secret_is_32_random_bytes_of_url_safe_base64() {
        let first = secret();
        let second = secret();

        assert_eq!(first.len(), 43, "32 bytes are 43 base64 characters");
        assert_ne!(first, second);
        assert!(!first.contains(['+', '/', '=']), "{first}");
        assert_eq!(decode_bytes(&first).unwrap().len(), 32);
    }

    #[test]
    fn file_bytes_travel_as_url_safe_base64_without_padding() {
        assert_eq!(encode_bytes(b"hello"), "aGVsbG8");
        assert_eq!(encode_bytes(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode_bytes("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_bytes("-_8").unwrap(), vec![0xfb, 0xff]);
        assert!(
            decode_bytes("aGk=").is_err(),
            "the pinned alphabet has no padding"
        );
        assert!(decode_bytes("not base64!").is_err());
    }

    #[test]
    fn a_bot_folder_is_named_by_its_identifier() {
        let workspace = Path::new("/home/ada/.neha/workspace");
        let bot = Id::new();

        assert_eq!(
            folder_for(workspace, &bot.to_string()).unwrap(),
            workspace.join(bot.to_string())
        );
        for bot in ["", ".", "..", "a/b", "../escape", "not-an-identifier"] {
            assert!(folder_for(workspace, bot).is_err(), "{bot:?}");
        }
    }

    #[test]
    fn a_write_is_acknowledged_with_a_succeeded_result() {
        assert_eq!(
            write_receipt(7, "out/report.bin", 5),
            DeviceFrame::Result {
                id: 7,
                status: ActionStatus::Succeeded,
                output: ToolOutput::full("wrote 5 bytes to out/report.bin"),
            }
        );
    }

    #[tokio::test]
    async fn a_ping_is_answered_with_a_pong() {
        let config = config_at(PathBuf::from("/home/ada/.neha/workspace"));
        let runtimes = empty_runtimes();
        let current = Mutex::new(None);

        assert_eq!(
            execute_frame(&config, &[], &runtimes, &current, HostFrame::Ping).await,
            DeviceFrame::Pong
        );
        assert!(runtimes.lock().unwrap().is_empty());
        assert!(current.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn an_action_for_a_bot_that_is_not_an_identifier_is_refused() {
        let config = config_at(PathBuf::from("/home/ada/.neha/workspace"));
        let runtimes = empty_runtimes();
        let current = Mutex::new(None);

        let answer = execute_frame(
            &config,
            &[],
            &runtimes,
            &current,
            HostFrame::Action {
                id: 3,
                bot: "../escape".to_owned(),
                action: WireAction::Shell {
                    command: "true".to_owned(),
                },
            },
        )
        .await;

        match answer {
            DeviceFrame::Failed { id, message } => {
                assert_eq!(id, 3);
                assert!(message.contains("not an identifier"), "{message}");
            }
            other => panic!("a Bot that is not an identifier must be refused: {other:?}"),
        }
        assert!(runtimes.lock().unwrap().is_empty());
    }

    /// Serves a fake Space on the other end of the daemon's channel.
    ///
    /// It greets nothing, sends a shell that starts and then sleeps, queues a
    /// second action behind it, and leaves once the first shell has started.
    /// A shell that survives the closed channel writes its later markers.
    async fn fake_space(listener: tokio::net::TcpListener, folder: PathBuf, bot: Id) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let greeting = socket.next().await.unwrap().unwrap();
        assert!(matches!(greeting, Message::Text(_)));
        let started = folder.join("started");
        let first = HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::Shell {
                command: format!(
                    "echo started > {}; sleep 0.6; echo done > {}",
                    started.display(),
                    folder.join("done").display()
                ),
            },
        };
        let second = HostFrame::Action {
            id: 2,
            bot: bot.to_string(),
            action: WireAction::Shell {
                command: format!("echo second > {}", folder.join("second").display()),
            },
        };
        for frame in [first, second] {
            socket
                .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                .await
                .unwrap();
        }
        for _ in 0..300 {
            if started.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(socket);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_channel_cancels_the_running_action() {
        let workspace = tempfile::tempdir().unwrap();
        let bot = Id::new();
        let folder = workspace.path().join(bot.to_string());
        std::fs::create_dir_all(&folder).unwrap();
        let runtime = LocalRuntime::new(&folder).unwrap();
        let runtimes: Runtimes = Arc::new(Mutex::new(HashMap::from([(
            bot.to_string(),
            Arc::new(runtime),
        )])));
        let config = Config {
            workspace: workspace.path().to_path_buf(),
            ..config_at(workspace.path().to_path_buf())
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let space = tokio::spawn(fake_space(listener, folder.clone(), bot));
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/api/computer/socket"))
                .await
                .unwrap();
        // The greeting is the handshake `connect_and_serve` does before it
        // serves; this test starts at the serving step.
        let hello = DeviceFrame::Hello {
            version: CHANNEL_VERSION,
            computer: config.computer.clone(),
            workspace: config.workspace.to_string_lossy().into_owned(),
            sandbox: true,
        };
        socket
            .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        let error = serve_channel(&config, &[], socket, runtimes)
            .await
            .expect_err("a closed channel ends the session");
        assert!(error.to_string().contains("closed"), "{error}");

        // The shell sleeps 0.6s before its second marker; wait past that.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        assert!(
            folder.join("started").is_file(),
            "the action ran before its Space left"
        );
        assert!(
            !folder.join("done").exists(),
            "a lost channel kills the action it was running"
        );
        assert!(
            !folder.join("second").exists(),
            "a frame queued behind the killed action never starts"
        );
        let _ = space.await;
    }
}
