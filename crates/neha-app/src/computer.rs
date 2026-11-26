//! The machine side of a Local Computer: pairing and the channel daemon.
//!
//! `neha computer pair` exchanges the one-use ticket `neha serve` wrote for a
//! credential, and saves it in the configuration directory. `neha computer run`
//! then serves the Space over one outgoing WebSocket; the Space never connects
//! to this machine.
//!
//! The daemon listens on no port and runs each action with the machine user's
//! rights, fenced by bubblewrap: a Bot reads what that user reads and writes
//! only its own folder. Reading and running are separate tasks, so a Space
//! that goes away is noticed at once and the shell it asked for is killed
//! instead of running on unattended.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::client::{self, Credential};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The runtimes this machine opened, one per Bot, kept across reconnects.
type Runtimes = Arc<Mutex<HashMap<String, Arc<LocalRuntime>>>>;

/// How long the daemon waits before it reconnects to its Space.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How long a cancelled action may take to end before its task is dropped.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// How many frames may wait in one direction of one channel.
const CHANNEL_QUEUE: usize = 64;
/// How long one socket write may take before the channel is dropped.
///
/// Ten seconds in a release build; the test build uses a short limit so a
/// Space that stopped reading is disconnected without slowing the suite.
const CHANNEL_WRITE_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(250)
} else {
    Duration::from_secs(10)
};
/// How many unprocessed bytes may wait toward the worker (inbound).
///
/// Two File limits in release. The test build uses a smaller sum so the budget
/// path is exercised cheaply.
const INBOUND_QUEUE_BYTES: usize = if cfg!(test) {
    1 << 20
} else {
    2 * (neha::file::MAX_FILE_BYTES as usize)
};
/// How many unprocessed bytes may wait toward the socket (outbound).
///
/// Two File limits in release. The test build keeps a cap above the socket
/// buffers a test can fill, so the write-deadline path stays reachable.
const OUTBOUND_QUEUE_BYTES: usize = if cfg!(test) {
    32 << 20
} else {
    2 * (neha::file::MAX_FILE_BYTES as usize)
};

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
        refuse_older_pairing()?;
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

    /// Saves the pairing at one path, readable only by this machine user.
    fn save_to(&self, path: &Path) -> Result<()> {
        client::write_private(path, &serde_json::to_vec(self)?)
    }
}

/// Pairs this machine with the Space on this machine, without a code.
///
/// The ticket normally comes from the file `neha serve` wrote. A Space that
/// started without an Admin writes none; a signed-in CLI asks the Space for one
/// instead. The pairing is saved in the configuration directory, and
/// `neha computer run` then serves it.
pub async fn pair(
    space: Option<String>,
    ticket_file: Option<PathBuf>,
    workspace: Option<PathBuf>,
    yes: bool,
) -> Result<()> {
    refuse_older_pairing()?;

    // A Workspace the configuration directory would leak into is refused
    // before anything reaches a Space.
    let config_directory = config_directory()?;
    let (workspace, default_workspace) = match workspace {
        Some(path) => (path, false),
        None => (default_workspace()?, true),
    };
    refuse_workspace(&config_directory, &workspace)?;

    // Everything up to the confirmation only reads local files. The User must
    // see the pairing target before any request leaves this machine, and a
    // ticket or session that names a different Space is refused without one.
    let credential = Credential::load().ok();
    let ticket_file = ticket_file.or_else(find_ticket_file);
    let ticket = match &ticket_file {
        Some(path) => Some(read_ticket(path)?),
        None => None,
    };
    let saved_url = credential.as_ref().map(saved_session_url);
    let (space_url, ticket, needs_ticket) = match ticket {
        Some(ticket) => {
            if let (Some(ticket_url), Some(explicit)) = (ticket.url.as_deref(), space.as_deref()) {
                check_ticket_space(ticket_url, explicit)?;
            }
            let space_url = target_url(
                space.as_deref(),
                ticket.url.as_deref(),
                saved_url.as_deref(),
            )?;
            (space_url, ticket.ticket, false)
        }
        None => {
            let saved = saved_url.as_deref().context(
                "there is no local pairing ticket; run `neha serve` on this machine, or sign \
                 in with `neha login`",
            )?;
            let space_url = match space.as_deref() {
                Some(space) => normalized_space_url(space)?,
                None => saved.to_owned(),
            };
            // The fallback asks the Space this session belongs to for a
            // ticket; sending the session's token anywhere else is refused.
            check_saved_origin(&space_url, saved)?;
            (space_url, String::new(), true)
        }
    };

    if !confirm_pairing(&space_url, yes)? {
        println!("Pairing cancelled.");
        return Ok(());
    }

    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("cannot create {}", workspace.display()))?;
    if default_workspace {
        client::restrict(&workspace, 0o700)?;
    }
    let workspace = std::fs::canonicalize(&workspace)
        .with_context(|| format!("cannot resolve {}", workspace.display()))?;
    refuse_workspace(&config_directory, &workspace)?;

    let ticket = if needs_ticket {
        let credential = credential.as_ref().context(
            "there is no local pairing ticket; run `neha serve` on this machine, or sign in \
             with `neha login`",
        )?;
        client::request_ticket(&space_url, &credential.token)
            .await
            .with_context(|| format!("cannot ask the Space at {space_url} for a pairing ticket"))?
    } else {
        ticket
    };

    let paired = client::pair_with_ticket(&space_url, &ticket)
        .await
        .with_context(|| format!("this Computer could not pair with the Space at {space_url}"))?;
    let config = Config {
        space_url,
        computer: paired.computer,
        token: paired.credential,
        workspace,
    };
    config.save_to(&config_path()?)?;
    println!(
        "Paired Computer {} with {}.",
        config.computer, config.space_url
    );

    // A used ticket is dead; remove the file so a rerun cannot try it again.
    // The Space removes it too when it claims the ticket, so a missing file is
    // success rather than a warning.
    if let Some(path) = &ticket_file {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "warning: cannot delete the used ticket {}: {error}",
                path.display()
            ),
        }
    }

    Ok(())
}

/// Starts the daemon with the saved pairing.
pub async fn run(workspace: Option<PathBuf>) -> Result<()> {
    let mut config = Config::load()?;
    if let Some(workspace) = workspace {
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("cannot create {}", workspace.display()))?;
        config.workspace = std::fs::canonicalize(&workspace)
            .with_context(|| format!("cannot resolve {}", workspace.display()))?;
    }
    refuse_workspace(&config_directory()?, &config.workspace)?;
    println!(
        "Computer {} is running for {}.",
        config.computer, config.space_url
    );
    println!("Workspace root: {}", config.workspace.display());
    println!("Press Ctrl-C to stop.");
    serve(config).await
}

/// Serves one Space until Ctrl-C, reconnecting after every failure.
async fn serve(config: Config) -> Result<()> {
    // Refuse a saved Space URL this milestone cannot serve before opening
    // anything: the Space listens on loopback only.
    normalized_space_url(&config.space_url).context("the saved Space URL is not usable")?;
    // A saved pairing from before the default moved may still name a workspace
    // inside the configuration directory; serving it would hand that directory
    // to the Bot through its own write root.
    refuse_workspace(&config_directory()?, &config.workspace)?;
    // The configuration directory holds this Computer's channel credential.
    // Every Bot runtime hides it from the file tools and the sandbox.
    let hidden = hidden_roots()?;
    // A machine that cannot fence a command never serves as a Computer. This
    // only probes bubblewrap: Bot folders are opened lazily, each through the
    // workspace root's descriptor.
    fence()?;
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
fn fence() -> Result<()> {
    LocalRuntime::probe_sandbox().context("this machine cannot fence Bot commands")?;

    Ok(())
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
///
/// Both directions are bounded and byte-budgeted. A Space that stops reading,
/// or sends faster than this machine can run, can only fill a queue; when a
/// queue or a budget fills, the channel is treated as broken and dropped, and
/// the worker's action dies with it. Socket writes have a deadline for the
/// same reason.
async fn serve_channel(
    config: &Config,
    hidden: &[PathBuf],
    socket: Socket,
    runtimes: Runtimes,
) -> Result<()> {
    let (sink, stream) = socket.split();
    let (stop, _) = watch::channel(false);
    let outbound_bytes = Arc::new(AtomicUsize::new(0));
    let (answers, mut outgoing) = mpsc::channel::<(String, usize)>(CHANNEL_QUEUE);
    let mut writer = tokio::spawn({
        let outbound_bytes = Arc::clone(&outbound_bytes);
        let stop = stop.clone();
        async move {
            let mut sink = sink;
            while let Some((text, size)) = outgoing.recv().await {
                // A Space that never drains its socket must not pin this task
                // (or the answers behind it) forever.
                let sent = tokio::time::timeout(
                    CHANNEL_WRITE_TIMEOUT,
                    sink.send(Message::Text(text.into())),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    let _ = stop.send(true);
                    break;
                }
                release_bytes(&outbound_bytes, size);
            }
        }
    });

    let (frames, mut incoming) = mpsc::channel::<(HostFrame, usize)>(CHANNEL_QUEUE);
    let inbound_bytes = Arc::new(AtomicUsize::new(0));
    // The runtime whose action is running, so the reader can kill it without
    // waiting for `execute` to return.
    let current: Arc<Mutex<Option<Arc<LocalRuntime>>>> = Arc::new(Mutex::new(None));

    let reader = tokio::spawn({
        let current = Arc::clone(&current);
        let answers = answers.clone();
        let frames = frames.clone();
        let inbound_bytes = Arc::clone(&inbound_bytes);
        let outbound_bytes = Arc::clone(&outbound_bytes);
        let stop = stop.clone();
        let mut stopped = stop.subscribe();
        async move {
            let mut stream = stream;
            let result = loop {
                let message = tokio::select! {
                    biased;
                    _ = stopped.changed() => break Err(anyhow::anyhow!(
                        "the channel writer stopped"
                    )),
                    message = stream.next() => message,
                };
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<HostFrame>(text.as_str()) {
                            // A heartbeat needs no Computer work and must not
                            // wait on a full queue.
                            Ok(HostFrame::Ping) => {
                                let Ok(text) = serde_json::to_string(&DeviceFrame::Pong) else {
                                    break Err(anyhow::anyhow!("cannot write a heartbeat"));
                                };
                                let size = text.len();
                                if within_byte_budget(&outbound_bytes, OUTBOUND_QUEUE_BYTES, size)
                                    && answers.try_send((text, size)).is_err()
                                {
                                    release_bytes(&outbound_bytes, size);
                                }
                            }
                            Ok(frame) => {
                                let size = text.len();
                                if !within_byte_budget(&inbound_bytes, INBOUND_QUEUE_BYTES, size) {
                                    break Err(anyhow::anyhow!(
                                        "the Space is sending more than this Computer can run"
                                    ));
                                }
                                if frames.try_send((frame, size)).is_err() {
                                    release_bytes(&inbound_bytes, size);
                                    break Err(anyhow::anyhow!(
                                        "the Space is sending frames faster than this Computer runs them"
                                    ));
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
        let inbound_bytes = Arc::clone(&inbound_bytes);
        let outbound_bytes = Arc::clone(&outbound_bytes);
        let stop = stop.clone();
        let mut stopped = stop.subscribe();
        async move {
            loop {
                if *stopped.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    frame = incoming.recv() => {
                        let Some((frame, size)) = frame else { break; };
                        let answer = tokio::select! {
                            biased;
                            _ = stopped.changed() => None,
                            answer = execute_frame(&config, &hidden, &runtimes, &current, frame) => {
                                Some(answer)
                            }
                        };
                        release_bytes(&inbound_bytes, size);
                        let Some(answer) = answer else { break; };
                        let Ok(text) = serde_json::to_string(&answer) else { break; };
                        let size = text.len();
                        // The answer queue is bounded too: a Space that stopped
                        // reading cannot make the worker buffer output forever.
                        if !within_byte_budget(&outbound_bytes, OUTBOUND_QUEUE_BYTES, size) {
                            let _ = stop.send(true);
                            break;
                        }
                        if answers.send((text, size)).await.is_err() {
                            let _ = stop.send(true);
                            break;
                        }
                    }
                }
            }
            let _ = stop.send(true);
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

/// Counts `size` unprocessed bytes in one direction, returning false when the
/// queue's byte budget would be exceeded.
fn within_byte_budget(counted: &AtomicUsize, budget: usize, size: usize) -> bool {
    let held = counted.fetch_add(size, Ordering::SeqCst) + size;
    if held > budget {
        counted.fetch_sub(size, Ordering::SeqCst);
        return false;
    }

    true
}

/// Removes processed bytes from a direction's backlog counter.
fn release_bytes(counted: &AtomicUsize, size: usize) {
    let mut held = counted.load(Ordering::SeqCst);
    loop {
        let next = held.saturating_sub(size);
        match counted.compare_exchange(held, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(actual) => held = actual,
        }
    }
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
    // The Bot must be one identifier component; reject anything else before
    // touching the filesystem.
    folder_for(&config.workspace, bot)?;
    let mut runtimes = runtimes
        .lock()
        .map_err(|_| "the runtime table is locked".to_owned())?;
    match runtimes.entry(bot.to_owned()) {
        Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
        Entry::Vacant(entry) => {
            // Open the workspace root without following a symlink, then walk
            // the Bot's folder through descriptors: a swapped path component
            // can never redirect the runtime outside its folder.
            let root = LocalRuntime::open_workspace(&config.workspace)
                .map_err(|error| error.to_string())?;
            let runtime = Arc::new(
                LocalRuntime::machine_at(&root, bot, hidden.to_vec())
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

/// The ticket file a Space writes for this machine.
const TICKET_NAME: &str = "local-computer.ticket";

/// The ticket a Space wrote for this machine's first pairing.
struct Ticket {
    /// The one-use pairing secret.
    ticket: String,
    /// The Space URL the ticket file names, when it names one.
    url: Option<String>,
}

/// Returns the configuration directory that holds this machine's Neha files.
fn config_directory() -> Result<PathBuf> {
    config_directory_from(
        std::env::var_os("NEHA_COMPUTER_HOME").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Returns the configuration directory from the variables that may name it.
///
/// `NEHA_COMPUTER_HOME` wins outright; otherwise the platform's per-user
/// configuration directory holds `neha`. A relative `XDG_CONFIG_HOME` is
/// ignored, as the XDG base directory specification requires.
fn config_directory_from(
    computer_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = computer_home {
        return Ok(path);
    }
    let Some(directory) = standard_config_directory(xdg_config_home, user_home) else {
        bail!(
            "HOME is not set; set NEHA_COMPUTER_HOME to the directory that holds this machine's Neha files"
        );
    };

    Ok(directory)
}

/// Returns the per-user configuration directory the CLI and the Space share.
///
/// `NEHA_COMPUTER_HOME` does not move it: the Space writes its ticket there and
/// the CLI keeps its session credential there.
fn standard_config_directory(
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| path.is_absolute())
        .or_else(|| user_home.map(|home| home.join(".config")))
        .map(|base| base.join("neha"))
}

/// Returns the path of the saved pairing.
fn config_path() -> Result<PathBuf> {
    Ok(config_directory()?.join("computer.json"))
}

/// Returns the path a pairing was saved at before the configuration moved.
fn older_pairing_path(user_home: &Path) -> PathBuf {
    user_home.join(".neha").join("computer.json")
}

/// Refuses to pair or serve while a pairing from the old default exists.
///
/// The old file is never migrated: it may name a stale Workspace or Computer,
/// and its credential is authorization material a Bot could reach. The User
/// removes it and pairs again.
fn refuse_older_pairing() -> Result<()> {
    refuse_older_pairing_from(std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

/// Refuses at a known home, for tests and for the check above.
fn refuse_older_pairing_from(user_home: Option<&Path>) -> Result<()> {
    if let Some(user_home) = user_home
        && older_pairing_path(user_home).exists()
    {
        bail!(
            "an older pairing is at ~/.neha/computer.json; remove it and run `neha computer pair` again"
        );
    }

    Ok(())
}

/// Returns the ticket file to read, in the pinned order.
///
/// An explicit `--ticket-file` wins. The Space writes its ticket where the CLI
/// keeps its own state first, and under its home second; the first file that
/// exists is used.
fn find_ticket_file() -> Option<PathBuf> {
    let directory = config_directory().ok()?;
    let neha_home = std::env::var_os("NEHA_HOME").map(PathBuf::from);

    first_existing(ticket_candidates_from(&directory, neha_home))
}

/// Returns the ticket files to try, in the pinned order.
fn ticket_candidates_from(config_directory: &Path, neha_home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = vec![config_directory.join(TICKET_NAME)];
    if let Some(home) = neha_home {
        candidates.push(home.join("state").join(TICKET_NAME));
    }

    candidates
}

/// Returns the first candidate that exists.
fn first_existing(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.exists())
}

/// Reads one ticket file.
fn read_ticket(path: &Path) -> Result<Ticket> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the ticket file {}", path.display()))?;
    parse_ticket(&text).with_context(|| format!("the ticket file {} is not usable", path.display()))
}

/// Parses a ticket file: one line with the ticket, or JSON with a URL too.
fn parse_ticket(text: &str) -> Result<Ticket> {
    let text = text.trim();
    if text.is_empty() {
        bail!("the ticket is empty");
    }
    if text.starts_with('{') {
        #[derive(Deserialize)]
        struct JsonTicket {
            ticket: String,
            #[serde(default)]
            url: Option<String>,
        }
        let file: JsonTicket =
            serde_json::from_str(text).context("the ticket is not a JSON ticket")?;
        let ticket = file.ticket.trim().to_owned();
        if ticket.is_empty() {
            bail!("the JSON ticket is empty");
        }
        let url = file
            .url
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());

        return Ok(Ticket { ticket, url });
    }

    Ok(Ticket {
        ticket: text.lines().next().unwrap_or_default().trim().to_owned(),
        url: None,
    })
}

/// Returns the Space this pairing targets.
///
/// An explicit `--space` wins over the URL a ticket file names, which wins over
/// the Space a signed-in CLI already knows. A ticket from a Space with no URL
/// in it is enough only when one of the other two is known.
fn target_url(
    space: Option<&str>,
    ticket: Option<&str>,
    credential: Option<&str>,
) -> Result<String> {
    let url = space.or(ticket).or(credential).context(
        "pass --space, sign in with `neha login`, or run `neha serve` on this machine to \
         write a ticket",
    )?;
    normalized_space_url(url)
}

/// The URL the saved CLI session belongs to, normalized for messages.
fn saved_session_url(credential: &Credential) -> String {
    normalized_space_url(&credential.url)
        .unwrap_or_else(|_| credential.url.trim_end_matches('/').to_owned())
}

/// Refuses a ticket that names a different Space than `--space` does.
fn check_ticket_space(ticket_url: &str, space: &str) -> Result<()> {
    let ticket = normalized_space_url(ticket_url)?;
    let space = normalized_space_url(space)?;
    if ticket != space {
        bail!("the ticket belongs to {ticket}; pair with that Space or leave out --space");
    }

    Ok(())
}

/// Refuses to send the saved session to a Space it does not belong to.
fn check_saved_origin(space: &str, saved: &str) -> Result<()> {
    if !same_origin(space, saved)? {
        bail!("the saved session belongs to {saved}");
    }

    Ok(())
}

/// Returns whether two URLs share a scheme, host, and port.
fn same_origin(left: &str, right: &str) -> Result<bool> {
    let left =
        reqwest::Url::parse(left).with_context(|| format!("{left:?} is not an absolute URL"))?;
    let right =
        reqwest::Url::parse(right).with_context(|| format!("{right:?} is not an absolute URL"))?;

    Ok(left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default())
}

/// Prints the pairing target and returns whether the User confirmed it.
///
/// A non-interactive run must say `--yes`: silently pairing this machine with
/// whatever URL a script passed would skip the one decision pairing asks for.
fn confirm_pairing(space: &str, yes: bool) -> Result<bool> {
    use std::io::IsTerminal as _;

    if !confirmation_needed(yes, std::io::stdin().is_terminal())? {
        return Ok(true);
    }
    print!("Pair this Computer with the Space at {space}? [y/N] ");
    std::io::stdout()
        .flush()
        .context("cannot write the prompt")?;
    let mut answer = String::new();
    let read = std::io::stdin()
        .read_line(&mut answer)
        .context("cannot read the answer")?;
    if read == 0 {
        return Ok(false);
    }

    Ok(answer_is_yes(&answer))
}

/// Returns whether the prompt must be shown, refusing an unattended run.
///
/// `--yes` answers the prompt; a non-interactive run without it is an error,
/// because silently pairing this machine with whatever URL a script passed
/// would skip the one decision pairing asks for.
fn confirmation_needed(yes: bool, interactive: bool) -> Result<bool> {
    if yes {
        return Ok(false);
    }
    if !interactive {
        bail!(
            "pairing is not confirmed: stdin is not a terminal; pass --yes to pair without a prompt"
        );
    }

    Ok(true)
}

/// Returns whether a prompt answer means yes.
fn answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Returns the directory that holds one folder per Bot by default.
fn default_workspace() -> Result<PathBuf> {
    default_workspace_from(std::env::var_os("HOME").map(PathBuf::from))
}

/// Returns the default Workspace root beside the machine user's home.
fn default_workspace_from(user_home: Option<PathBuf>) -> Result<PathBuf> {
    let Some(user_home) = user_home else {
        bail!("HOME is not set; pass --workspace to choose where the Bot Workspaces live");
    };

    Ok(user_home.join(".neha").join("workspaces"))
}

/// Returns the configuration directory as the runtimes must hide it.
///
/// The daemon passes this root to every Bot runtime: it holds the channel
/// credential in `computer.json`, so neither a Bot's file tools nor its sandbox
/// may reach it.
fn hidden_roots() -> Result<Vec<PathBuf>> {
    Ok(hidden_roots_from(
        config_directory()?,
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("NEHA_CREDENTIAL").map(PathBuf::from),
        std::env::var_os("NEHA_HOME")
            .map(|home| PathBuf::from(home).join("state").join(TICKET_NAME)),
    ))
}

/// Returns the paths a Bot must not reach, resolved and de-duplicated.
///
/// The daemon's configuration directory holds the channel credential, and the
/// CLI keeps its session credential in the per-user configuration directory;
/// `NEHA_CREDENTIAL` may move that file anywhere. A path that does not exist
/// is dropped: there is nothing there to hide yet.
fn hidden_roots_from(
    config_directory: PathBuf,
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
    credential: Option<PathBuf>,
    ticket: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = vec![config_directory];
    if let Some(directory) = standard_config_directory(xdg_config_home, user_home) {
        candidates.push(directory);
    }
    candidates.extend(credential);
    candidates.extend(ticket);

    let mut seen = std::collections::HashSet::new();
    let mut roots = Vec::new();
    for candidate in candidates {
        let Ok(root) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }

    roots
}

/// Refuses a Workspace and the configuration directory that contains each
/// other.
///
/// The configuration directory is a hidden root, and a Bot's write root may
/// neither hold it nor sit inside it. Both paths are resolved, so a symlink
/// between them cannot make the check miss; the parts that do not exist yet
/// are compared lexically.
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
    if directory.starts_with(&workspace) {
        bail!(
            "the Computer's configuration directory {} is inside the workspace {}; \
             choose a workspace that does not contain it",
            directory.display(),
            workspace.display()
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
    use std::time::Duration;

    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::id::Id;
    use neha_computer::{
        ActionStatus, CHANNEL_VERSION, DeviceFrame, HostFrame, LocalRuntime, ToolOutput, WireAction,
    };
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        Config, Runtimes, Socket, answer_is_yes, check_saved_origin, check_ticket_space,
        config_directory_from, confirmation_needed, decode_bytes, default_workspace_from,
        encode_bytes, execute_frame, first_existing, folder_for, hidden_roots_from,
        normalized_space_url, parse_ticket, refuse_older_pairing_from, refuse_workspace,
        same_origin, serve_channel, socket_url, target_url, ticket_candidates_from, write_receipt,
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
    fn the_configuration_directory_comes_from_the_environment_in_order() {
        // NEHA_COMPUTER_HOME wins outright.
        assert_eq!(
            config_directory_from(
                Some(PathBuf::from("/srv/neha")),
                Some(PathBuf::from("/home/ada/.config")),
                Some(PathBuf::from("/home/ada"))
            )
            .unwrap(),
            PathBuf::from("/srv/neha")
        );
        // Then XDG_CONFIG_HOME/neha.
        assert_eq!(
            config_directory_from(
                None,
                Some(PathBuf::from("/x/config")),
                Some(PathBuf::from("/home/ada"))
            )
            .unwrap(),
            PathBuf::from("/x/config/neha")
        );
        // Then the platform default under $HOME.
        assert_eq!(
            config_directory_from(None, None, Some(PathBuf::from("/home/ada"))).unwrap(),
            PathBuf::from("/home/ada/.config/neha")
        );
        // A relative XDG_CONFIG_HOME is ignored, as the XDG spec says.
        assert_eq!(
            config_directory_from(
                None,
                Some(PathBuf::from("relative/config")),
                Some(PathBuf::from("/home/ada"))
            )
            .unwrap(),
            PathBuf::from("/home/ada/.config/neha")
        );
        let error = config_directory_from(None, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("NEHA_COMPUTER_HOME"), "{error}");
    }

    #[test]
    fn the_default_workspace_root_sits_under_the_user_home() {
        assert_eq!(
            default_workspace_from(Some(PathBuf::from("/home/ada"))).unwrap(),
            PathBuf::from("/home/ada/.neha/workspaces")
        );
        let error = default_workspace_from(None).unwrap_err().to_string();
        assert!(error.contains("--workspace"), "{error}");
    }

    #[test]
    fn an_older_pairing_is_refused_until_it_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let user_home = directory.path();
        let older = user_home.join(".neha").join("computer.json");
        std::fs::create_dir_all(older.parent().unwrap()).unwrap();
        std::fs::write(&older, "{}").unwrap();
        // Even a new pairing saved at the new place does not make the old
        // authorization material safe to leave around.
        let configuration = user_home.join(".config/neha");
        std::fs::create_dir_all(&configuration).unwrap();
        std::fs::write(configuration.join("computer.json"), "{}").unwrap();

        let error = refuse_older_pairing_from(Some(user_home))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "an older pairing is at ~/.neha/computer.json; remove it and run `neha computer pair` again"
        );

        std::fs::remove_file(&older).unwrap();
        refuse_older_pairing_from(Some(user_home)).unwrap();
    }

    #[test]
    fn a_ticket_file_is_one_line_or_json_with_a_url() {
        let plain = parse_ticket("ticket-a\n").unwrap();
        assert_eq!(plain.ticket, "ticket-a");
        assert_eq!(plain.url, None);
        let json =
            parse_ticket(r#"{"ticket": "ticket-b", "url": "http://127.0.0.1:7337"}"#).unwrap();
        assert_eq!(json.ticket, "ticket-b");
        assert_eq!(json.url.as_deref(), Some("http://127.0.0.1:7337"));
        let without_url = parse_ticket(r#"{"ticket": "ticket-c"}"#).unwrap();
        assert_eq!(without_url.url, None);
        assert!(parse_ticket("").is_err());
        assert!(parse_ticket("   \n  ").is_err());
        assert!(parse_ticket(r#"{"ticket": ""}"#).is_err());
        assert!(parse_ticket("{not json").is_err());
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

        // The reverse overlap is refused too: a Workspace may not contain the
        // configuration directory it is supposed to hide.
        let error = refuse_workspace(&configuration, directory.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("configuration directory") && error.contains("inside the workspace"),
            "{error}"
        );
    }

    #[test]
    fn a_pairing_prefers_the_space_then_the_ticket_then_the_credential() {
        let error = target_url(None, None, None).unwrap_err().to_string();
        assert!(error.contains("--space"), "{error}");
        assert_eq!(
            target_url(None, Some("http://127.0.0.1:7337/"), None).unwrap(),
            "http://127.0.0.1:7337"
        );
        assert_eq!(
            target_url(None, None, Some("http://127.0.0.1:7337")).unwrap(),
            "http://127.0.0.1:7337"
        );
        assert_eq!(
            target_url(
                Some("https://localhost:7337"),
                Some("http://127.0.0.1:7337"),
                Some("http://127.0.0.1:7338")
            )
            .unwrap(),
            "https://localhost:7337"
        );
        // A ticket URL wins over the saved credential's Space.
        assert_eq!(
            target_url(
                None,
                Some("http://127.0.0.1:7337"),
                Some("http://127.0.0.1:7338")
            )
            .unwrap(),
            "http://127.0.0.1:7337"
        );
        let error = target_url(Some("space.example"), None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("URL"), "{error}");
    }

    #[test]
    fn the_saved_session_only_pairs_with_its_own_origin() {
        assert!(same_origin("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap());
        assert!(same_origin("https://localhost:7337", "https://localhost:7337/").unwrap());
        // A different port, host, or scheme is a different origin.
        for (left, right) in [
            ("http://127.0.0.1:7337", "http://127.0.0.1:7338"),
            ("http://localhost:7337", "http://127.0.0.1:7337"),
            ("https://127.0.0.1:7337", "http://127.0.0.1:7337"),
        ] {
            assert!(!same_origin(left, right).unwrap(), "{left} vs {right}");
        }

        check_saved_origin("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap();
        let error = check_saved_origin("http://127.0.0.1:7338", "http://127.0.0.1:7337")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "the saved session belongs to http://127.0.0.1:7337");
    }

    #[test]
    fn a_ticket_naming_another_space_is_refused() {
        check_ticket_space("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap();
        let error = check_ticket_space("http://127.0.0.1:7338", "http://127.0.0.1:7337")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the ticket belongs to http://127.0.0.1:7338; pair with that Space or leave out --space"
        );
    }

    #[test]
    fn ticket_files_are_tried_in_the_pinned_order() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = directory.path().join(".config/neha");
        let neha_home = directory.path().join("space");
        std::fs::create_dir_all(&configuration).unwrap();
        std::fs::create_dir_all(neha_home.join("state")).unwrap();
        let candidates = ticket_candidates_from(&configuration, Some(neha_home.clone()));
        assert_eq!(
            candidates,
            vec![
                configuration.join("local-computer.ticket"),
                neha_home.join("state/local-computer.ticket"),
            ]
        );

        // The first existing file wins; the Space home file is the fallback.
        assert_eq!(first_existing(candidates.clone()), None);
        std::fs::write(&candidates[1], "ticket-b").unwrap();
        assert_eq!(
            first_existing(candidates.clone()),
            Some(candidates[1].clone())
        );
        std::fs::write(&candidates[0], "ticket-a").unwrap();
        assert_eq!(
            first_existing(candidates),
            Some(configuration.join("local-computer.ticket"))
        );
    }

    #[test]
    fn hidden_roots_cover_the_configuration_and_credential_directories() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = directory.path().join("computer-home");
        let standard = directory.path().join(".config/neha");
        let credential = directory.path().join("credential.json");
        for path in [&configuration, &standard] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(&credential, "{}").unwrap();

        let roots = hidden_roots_from(
            configuration.clone(),
            Some(directory.path().join(".config")),
            None,
            Some(credential.clone()),
            None,
        );
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();
        assert!(roots.contains(&canonical(&configuration)));
        assert!(roots.contains(&canonical(&standard)));
        assert!(roots.contains(&canonical(&credential)));

        // A path that does not exist has nothing to hide and is dropped; the
        // configuration directory is not duplicated when it is also the
        // platform configuration directory.
        let missing = directory.path().join("missing.json");
        let roots = hidden_roots_from(
            configuration.clone(),
            Some(directory.path().join(".config")),
            None,
            Some(missing),
            None,
        );
        assert_eq!(roots.len(), 2);
        let roots = hidden_roots_from(
            standard.clone(),
            Some(directory.path().join(".config")),
            None,
            None,
            None,
        );
        assert_eq!(roots, vec![canonical(&standard)]);
    }

    #[test]
    fn pairing_needs_a_terminal_or_an_explicit_yes() {
        // --yes always pairs without asking, terminal or not.
        assert!(!confirmation_needed(true, false).unwrap());
        assert!(!confirmation_needed(true, true).unwrap());
        // A terminal asks, and the answer decides.
        assert!(confirmation_needed(false, true).unwrap());
        // A non-interactive run must not silently confirm.
        let error = confirmation_needed(false, false).unwrap_err().to_string();
        assert!(error.contains("--yes"), "{error}");
        assert!(error.contains("not a terminal"), "{error}");
    }

    #[test]
    fn a_confirmation_answer_is_yes_only_when_it_says_so() {
        for answer in ["y", "Y", "yes", " YES \n"] {
            assert!(answer_is_yes(answer), "{answer:?}");
        }
        for answer in ["", "n", "no", "later", "y n"] {
            assert!(!answer_is_yes(answer), "{answer:?}");
        }
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

    /// The accepting side of a fake Space's channel.
    type SpaceSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    /// Starts a fake Space's listener on loopback.
    async fn space_listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        (listener, address)
    }

    /// Accepts the daemon's channel and reads its greeting.
    async fn accept_space(listener: tokio::net::TcpListener) -> SpaceSocket {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let greeting = socket.next().await.unwrap().unwrap();
        assert!(matches!(greeting, Message::Text(_)));

        socket
    }

    /// Connects the daemon end of the channel and announces this Computer.
    async fn connect_daemon(address: std::net::SocketAddr, workspace: &Path) -> Socket {
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/api/computer/socket"))
                .await
                .unwrap();
        let hello = DeviceFrame::Hello {
            version: CHANNEL_VERSION,
            computer: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
            workspace: workspace.to_string_lossy().into_owned(),
            sandbox: true,
        };
        socket
            .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        socket
    }

    /// Sends one frame as the Space would.
    async fn send_space_frame(socket: &mut SpaceSocket, frame: HostFrame) {
        socket
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .unwrap();
    }

    /// A workspace holding one managed Bot runtime, ready for the daemon.
    fn managed_bot(workspace: &Path, bot: Id) -> (PathBuf, Runtimes) {
        let folder = workspace.join(bot.to_string());
        std::fs::create_dir_all(&folder).unwrap();
        let runtime = LocalRuntime::new(&folder).unwrap();
        let runtimes: Runtimes = Arc::new(Mutex::new(HashMap::from([(
            bot.to_string(),
            Arc::new(runtime),
        )])));

        (folder, runtimes)
    }

    fn channel_config(workspace: &Path) -> Config {
        Config {
            workspace: workspace.to_path_buf(),
            ..config_at(workspace.to_path_buf())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_space_that_sends_faster_than_the_worker_is_disconnected() {
        let workspace = tempfile::tempdir().unwrap();
        let bot = Id::new();
        let (folder, runtimes) = managed_bot(workspace.path(), bot);
        let config = channel_config(workspace.path());
        let (listener, address) = space_listener().await;
        let space = tokio::spawn({
            let folder = folder.clone();
            async move {
                let mut socket = accept_space(listener).await;
                // One action keeps the worker busy while the Space floods the
                // channel with frames it cannot run yet.
                send_space_frame(
                    &mut socket,
                    HostFrame::Action {
                        id: 1,
                        bot: bot.to_string(),
                        action: WireAction::Shell {
                            command: format!(
                                "sleep 0.5; echo done > {}",
                                folder.join("done").display()
                            ),
                        },
                    },
                )
                .await;
                for id in 2..=200 {
                    let frame = HostFrame::Action {
                        id,
                        bot: "not a bot".to_owned(),
                        action: WireAction::Shell {
                            command: "true".to_owned(),
                        },
                    };
                    if socket
                        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let daemon = connect_daemon(address, workspace.path()).await;

        let error = tokio::time::timeout(
            Duration::from_secs(10),
            serve_channel(&config, &[], daemon, runtimes),
        )
        .await
        .expect("a flooding Space must not hang the daemon")
        .expect_err("a flooding Space is disconnected");
        assert!(error.to_string().contains("faster"), "{error}");

        space.abort();
        // The action the abandoned channel was running is killed.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !folder.join("done").exists(),
            "an abandoned action is killed with its channel"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_space_that_sends_huge_frames_is_disconnected() {
        let workspace = tempfile::tempdir().unwrap();
        let bot = Id::new();
        let (folder, runtimes) = managed_bot(workspace.path(), bot);
        let config = channel_config(workspace.path());
        let (listener, address) = space_listener().await;
        let space = tokio::spawn({
            let folder = folder.clone();
            async move {
                let mut socket = accept_space(listener).await;
                send_space_frame(
                    &mut socket,
                    HostFrame::Action {
                        id: 1,
                        bot: bot.to_string(),
                        action: WireAction::Shell {
                            command: format!(
                                "sleep 0.5; echo done > {}",
                                folder.join("done").display()
                            ),
                        },
                    },
                )
                .await;
                // Each frame alone fits; together they exceed the byte budget
                // long before the worker can run them.
                let huge = "x".repeat(600 * 1024);
                for id in 2..=4 {
                    let frame = HostFrame::Action {
                        id,
                        bot: "not a bot".to_owned(),
                        action: WireAction::Shell {
                            command: huge.clone(),
                        },
                    };
                    send_space_frame(&mut socket, frame).await;
                }
            }
        });
        let daemon = connect_daemon(address, workspace.path()).await;

        let error = tokio::time::timeout(
            Duration::from_secs(10),
            serve_channel(&config, &[], daemon, runtimes),
        )
        .await
        .expect("a Space stuffing huge frames must not hang the daemon")
        .expect_err("a Space stuffing huge frames is disconnected");
        assert!(error.to_string().contains("more than"), "{error}");

        space.abort();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !folder.join("done").exists(),
            "an abandoned action is killed with its channel"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_space_that_stops_reading_is_disconnected() {
        let workspace = tempfile::tempdir().unwrap();
        let bot = Id::new();
        let (folder, runtimes) = managed_bot(workspace.path(), bot);
        std::fs::write(folder.join("big.bin"), vec![7_u8; 4 * 1024 * 1024]).unwrap();
        let config = channel_config(workspace.path());
        let (listener, address) = space_listener().await;
        let space = tokio::spawn({
            async move {
                let mut socket = accept_space(listener).await;
                for id in 1..=5 {
                    send_space_frame(
                        &mut socket,
                        HostFrame::Read {
                            id,
                            bot: bot.to_string(),
                            path: "big.bin".to_owned(),
                        },
                    )
                    .await;
                }
                // This Space never reads an answer.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        let daemon = connect_daemon(address, workspace.path()).await;

        let error = tokio::time::timeout(
            Duration::from_secs(10),
            serve_channel(&config, &[], daemon, runtimes),
        )
        .await
        .expect("a Space that stops reading must not hang the daemon")
        .expect_err("a Space that stops reading is disconnected");
        assert!(error.to_string().contains("writer"), "{error}");

        space.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_space_that_stops_sending_ends_the_channel() {
        let workspace = tempfile::tempdir().unwrap();
        let config = channel_config(workspace.path());
        let (listener, address) = space_listener().await;
        let space = tokio::spawn(async move {
            let socket = accept_space(listener).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(socket);
        });
        let daemon = connect_daemon(address, workspace.path()).await;

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            serve_channel(&config, &[], daemon, empty_runtimes()),
        )
        .await
        .expect("a quiet Space must not hang the daemon")
        .expect_err("a closed channel ends the session");
        assert!(error.to_string().contains("closed"), "{error}");

        let _ = space.await;
    }
}
