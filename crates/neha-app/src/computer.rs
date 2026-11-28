//! The machine side of a Local Computer: pairing and the channel daemon.
//!
//! `neha computer pair` exchanges the one-use ticket `neha serve` wrote for a
//! credential, and saves it in the configuration directory. `neha computer run`
//! then serves the Space over one outgoing WebSocket; the Space never connects
//! to this machine.
//!
//! The daemon listens on no port and runs each action with the machine user's
//! rights, fenced by bubblewrap: a Bot reads what that user reads and writes
//! only its own folder. A Bot its owner gave full access instead runs without a
//! sandbox at all, and the daemon opens its folder without the fence's scan or
//! probe. Reading and running are separate tasks, so a Space that goes away is
//! noticed at once and the shell it asked for is killed instead of running on
//! unattended.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt as _, StreamExt as _};
use neha::id::Id;
use neha_computer::channel::AccessFrame;
use neha_computer::{
    ActionStatus, CHANNEL_VERSION, ComputerRuntime as _, DeviceFrame, HostFrame, LocalRuntime,
    MAX_CHANNEL_FRAME_BYTES, ToolOutput,
};
use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::client::{self, Credential};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What makes one cached runtime different from another: a base runtime holds
/// the Bot's opened folder and, for a granted Bot, the sandboxed mode, and it
/// is kept across reconnects.
///
/// It holds no grant and no run directory: those are opened for each request,
/// so a directory archived and rebuilt between two runs is never served through
/// an older descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RuntimeKey {
    bot: String,
    full_access: bool,
}

type Runtimes = Arc<Mutex<HashMap<RuntimeKey, Arc<LocalRuntime>>>>;

/// How long the daemon waits before it reconnects to its Space.
/// The first retry delay after a channel that never acknowledged.
const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// The longest retry delay; a Space that stays away is retried at this pace.
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// How long a daemon waits for the Space to acknowledge its greeting.
const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a cancelled action may take to end before its task is dropped.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// How many frames may wait in one direction of one channel.
const CHANNEL_QUEUE: usize = 64;
/// How long one socket write may take before the channel is dropped.
///
/// Ten seconds in a production build. Tests get a short limit, in the
/// crate's own unit tests and through its `test-support` feature, so a Space
/// that stopped reading is disconnected without slowing the suite.
const CHANNEL_WRITE_TIMEOUT: Duration = if cfg!(any(test, feature = "test-support")) {
    Duration::from_millis(250)
} else {
    Duration::from_secs(10)
};
/// How many unprocessed bytes may wait toward the worker (inbound).
///
/// Two File limits in production. A test build uses a smaller sum so the
/// budget path is exercised cheaply.
const INBOUND_QUEUE_BYTES: usize = if cfg!(any(test, feature = "test-support")) {
    1 << 20
} else {
    2 * (neha::file::MAX_FILE_BYTES as usize)
};
/// How many unprocessed bytes may wait toward the socket (outbound).
///
/// Two File limits in production. A test build keeps a cap above the socket
/// buffers a test can fill, so the write-deadline path stays reachable.
const OUTBOUND_QUEUE_BYTES: usize = if cfg!(any(test, feature = "test-support")) {
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

    /// Reads the identity, token, origin, and mount a worker container gets.
    fn from_worker_env() -> Result<Self> {
        let space_url = normalized_worker_url(
            &std::env::var("NEHA_WORKER_SPACE_URL").context("NEHA_WORKER_SPACE_URL is not set")?,
        )
        .context("NEHA_WORKER_SPACE_URL is not usable")?;
        let computer =
            std::env::var("NEHA_WORKER_COMPUTER").context("NEHA_WORKER_COMPUTER is not set")?;
        let token = std::env::var("NEHA_WORKER_TOKEN").context("NEHA_WORKER_TOKEN is not set")?;
        let workspace = std::env::var_os("NEHA_WORKER_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/workspace"));
        if token.trim().is_empty() {
            bail!("NEHA_WORKER_TOKEN is empty");
        }
        computer
            .parse::<Id>()
            .context("NEHA_WORKER_COMPUTER is not an identifier")?;
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("cannot create {}", workspace.display()))?;
        let workspace = std::fs::canonicalize(&workspace)
            .with_context(|| format!("cannot resolve {}", workspace.display()))?;

        Ok(Self {
            space_url,
            computer,
            token,
            workspace,
        })
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
    // A session credential left in the working directory by the old resolver
    // is a secret a Bot could read; refuse to pair when it cannot be cleaned.
    migrate_legacy_credential()?;

    // A Workspace the configuration directory would leak into is refused
    // before anything reaches a Space.
    let config_directory = config_directory()?;
    let (workspace, default_workspace) = match workspace {
        Some(path) => (path, false),
        None => (default_workspace()?, true),
    };
    refuse_workspace(&config_directory, &workspace)?;

    // The explicit Space decides the flow before any ticket is read or any
    // request is made: a Space on another machine pairs through a code, while
    // a loopback Space keeps the local ticket.
    let explicit_space = space.as_deref().map(normalized_any_space_url).transpose()?;
    let remote_space = explicit_space
        .clone()
        .filter(|origin| !is_loopback_space_url(origin));
    if let Some(remote_space) = remote_space {
        if ticket_file.is_some() {
            bail!(
                "--ticket-file pairs with a Space on this machine; leave it out when \
                 --space names another machine"
            );
        }
        return pair_remote(
            remote_space,
            workspace,
            &config_directory,
            default_workspace,
            yes,
        )
        .await;
    }

    // Everything up to the confirmation only reads local files. The User must
    // see the pairing target before any request leaves this machine, and a
    // ticket or session that names a different Space is refused without one.
    let credential = Credential::load().ok();
    let saved = saved_session_origin(credential.as_ref())?;
    let (ticket, used_ticket_file) = match ticket_file {
        Some(path) => {
            let ticket = read_ticket(&path)?;
            (ticket, Some(path))
        }
        None => {
            let standard_config = standard_config_from_env();
            let found = standard_config
                .as_deref()
                .map(found_tickets)
                .unwrap_or_default();
            // With several tickets waiting and no --space, a loopback saved
            // session names the Space to match. A remote session is not a
            // local ticket's target.
            let target = match explicit_space.as_deref() {
                Some(space) => Some(normalized_space_url(space)?),
                None if found.len() <= 1 => None,
                None => Some(
                    saved
                        .as_deref()
                        .filter(|url| is_loopback_space_url(url))
                        .map(normalized_space_url)
                        .transpose()?
                        .context(
                            "several Space tickets are waiting; pass --space or --ticket-file \
                             to choose one",
                        )?,
                ),
            };
            match choose_ticket(found, target.as_deref())? {
                Some(found) => (found.ticket, Some(found.file)),
                None => (
                    Ticket {
                        ticket: String::new(),
                        url: None,
                    },
                    None,
                ),
            }
        }
    };
    let (space_url, ticket, needs_ticket) = if used_ticket_file.is_some() {
        if let (Some(ticket_url), Some(explicit)) =
            (ticket.url.as_deref(), explicit_space.as_deref())
        {
            check_ticket_space(ticket_url, explicit)?;
        }
        let space_url = match (explicit_space.as_deref(), ticket.url.as_deref()) {
            (Some(space), _) => normalized_space_url(space)?,
            (None, Some(ticket_url)) => normalized_space_url(ticket_url)?,
            (None, None) => local_session_origin(saved.as_deref())
                .context("pass --space, or pair with a ticket that names its Space")?,
        };
        (space_url, ticket.ticket, false)
    } else {
        let saved = saved.context(
            "there is no local pairing ticket; run `neha serve` on this machine, or sign \
             in with `neha login`",
        )?;
        let space_url = match explicit_space.as_deref() {
            Some(space) => normalized_space_url(space)?,
            None if is_loopback_space_url(&saved) => saved.clone(),
            None => bail!(
                "there is no local pairing ticket; pass --space {saved} to pair this machine \
                 with that Space"
            ),
        };
        let saved = local_session_origin(Some(&saved)).context(
            "the saved session belongs to another Space; pass --space with this machine's Space",
        )?;
        // The fallback asks the Space this session belongs to for a ticket;
        // sending the session's token anywhere else is refused.
        check_saved_origin(&space_url, &saved)?;
        (space_url, String::new(), true)
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
    if let Some(path) = &used_ticket_file {
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

/// Pairs this machine with a Space reached over the network.
///
/// The machine starts a pairing and prints a code; a signed-in User approves
/// it on any client, and this machine polls until the Space records its
/// Computer. No local ticket, saved session, or loopback address is involved.
async fn pair_remote(
    space_url: String,
    workspace: PathBuf,
    config_directory: &Path,
    default_workspace: bool,
    yes: bool,
) -> Result<()> {
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
    refuse_workspace(config_directory, &workspace)?;
    let workspace_text = workspace
        .to_str()
        .context("the Workspace path is not valid UTF-8; pairing needs a text path")?;

    let machine = machine_name();
    let token = random_secret();
    let poll_secret = random_secret();
    let started = client::start_pairing(&space_url, &machine, workspace_text, &token, &poll_secret)
        .await
        .with_context(|| format!("cannot start pairing with the Space at {space_url}"))?;
    println!(
        "To pair this Computer, sign in to {space_url} and enter code {} (expires in {} minutes).",
        display_pairing_code(&started.code),
        started.expires_in.div_ceil(60)
    );
    println!("Waiting for approval...");

    let computer = wait_for_remote_approval(&space_url, &started, &poll_secret)
        .await
        .with_context(|| format!("cannot poll the pairing at {space_url}"))?;
    let config = Config {
        space_url: space_url.clone(),
        computer,
        token,
        workspace,
    };
    config.save_to(&config_path()?)?;
    println!(
        "Paired Computer {} with {}.",
        config.computer, config.space_url
    );

    Ok(())
}

/// Waits for a User to approve one remote pairing.
///
/// A poll that fails for a reason other than the Space forgetting the pairing
/// is retried: the machine has not saved its token yet, so giving up after one
/// connection reset could leave an approved Computer on the Space that this
/// machine can never serve. Every request and sleep shares one deadline.
async fn wait_for_remote_approval(
    space_url: &str,
    started: &client::StartedPairing,
    poll_secret: &str,
) -> Result<String> {
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_secs(started.expires_in))
        .context("the Space announced an unusable pairing lifetime")?;
    let interval = Duration::from_secs(started.interval);
    let mut retry = Duration::from_secs(1);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!(
                "the pairing code expired before a User approved it; run \
                 `neha computer pair --space {space_url}` again"
            );
        }
        let outcome = tokio::time::timeout(
            remaining.min(Duration::from_secs(30)),
            client::poll_pairing(space_url, &started.device, poll_secret),
        )
        .await;
        let wait = match outcome {
            Ok(Ok(client::PairingPoll::Pending)) => interval,
            Ok(Ok(client::PairingPoll::Approved { computer })) => return Ok(computer),
            Ok(Ok(client::PairingPoll::Gone)) => bail!(
                "the pairing code expired or was replaced before a User approved it; run \
                 `neha computer pair --space {space_url}` again"
            ),
            Ok(Err(error)) => {
                eprintln!("warning: a pairing poll failed; retrying: {error:#}");
                let wait = retry;
                retry = (retry * 2).min(Duration::from_secs(30));
                wait
            }
            Err(_) => {
                eprintln!("warning: a pairing poll timed out; retrying");
                let wait = retry;
                retry = (retry * 2).min(Duration::from_secs(30));
                wait
            }
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            continue;
        }
        tokio::time::sleep(wait.min(remaining)).await;
    }
}

/// Starts the daemon with the saved pairing.
pub async fn run(workspace: Option<PathBuf>) -> Result<()> {
    // A session credential left in the working directory by the old resolver
    // is a secret a Bot could read; refuse to serve when it cannot be cleaned.
    migrate_legacy_credential()?;
    let mut config = Config::load()?;
    // The saved origin is printed and sent; canonicalize and validate it
    // before either, so a stale file cannot put credentials or a query in a
    // terminal or in the Host header.
    config.space_url = normalized_any_space_url(&config.space_url)
        .context("the saved Space URL is not usable; run `neha computer pair` again")?;
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
    serve(config, false).await
}

/// Runs this container as one Hosted Computer's worker.
///
/// The container is the sandbox. The worker takes its identity, token, Space
/// origin, and Workspace mount from the container environment, serves the
/// Computer channel, and starts no nested bubblewrap.
pub async fn worker() -> Result<()> {
    let config = Config::from_worker_env()?;
    println!("Hosted Computer worker {} is starting.", config.computer);
    println!("Workspace root: {}", config.workspace.display());
    serve(config, true).await
}

/// Serves one Space until Ctrl-C, reconnecting after every failure.
async fn serve(config: Config, worker: bool) -> Result<()> {
    // Refuse a saved Space URL this build cannot serve before opening
    // anything: a Local Computer keeps the origin it paired with (loopback or
    // a remote https Space); a worker must reach the public origin its
    // Supervisor injected.
    if worker {
        normalized_worker_url(&config.space_url).context("the worker Space URL is not usable")?;
    } else {
        normalized_any_space_url(&config.space_url).context("the saved Space URL is not usable")?;
    }
    // A saved pairing from before the default moved may still name a workspace
    // inside the configuration directory; serving it would hand that directory
    // to the Bot through its own write root.
    if !worker {
        refuse_workspace(&config_directory()?, &config.workspace)?;
    }
    // A Local Computer hides its configuration directory from the file tools
    // and the sandbox. A worker container holds no Space or host secrets: its
    // whole persistent mount is the Computer's Workspace.
    let hidden = if worker { Vec::new() } else { hidden_roots()? };
    let denied = if worker { Vec::new() } else { denied_paths()? };
    // A machine that cannot fence a command never serves as a Computer. The
    // worker container is the fence, so it has nothing to probe.
    if !worker {
        fence()?;
    }
    let runtimes: Runtimes = Arc::new(Mutex::new(HashMap::new()));
    // One signal future spans the whole reconnect loop: a stop delivered while
    // a channel is closing must not be lost in the gap between two selects.
    let mut shutdown = std::pin::pin!(shutdown_signal());
    let mut retry = RECONNECT_MIN;
    loop {
        let acknowledged = Arc::new(AtomicBool::new(false));
        let served = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            served = connect_and_serve(
                &config,
                &hidden,
                &denied,
                worker,
                Arc::clone(&runtimes),
                &acknowledged,
            ) => served,
        };
        if let Err(error) = served {
            eprintln!("{error:#}");
            // A Space that rejected this machine's credential will keep
            // rejecting it until the machine pairs again; retrying forever
            // would hide that from the User.
            if refused_credential(&error) {
                if refused_by_proxy(&error) {
                    eprintln!(
                        "a proxy refused the channel's Host (403); check that the Space's public origin and the proxy's Host header agree, or run `neha computer pair --space {}` again",
                        config.space_url
                    );
                } else {
                    eprintln!(
                        "this Computer's saved pairing was refused; run `neha computer pair --space {}` again",
                        config.space_url
                    );
                }
                return Err(error);
            }
        }
        if acknowledged.load(Ordering::Relaxed) {
            retry = RECONNECT_MIN;
        }
        let delay = retry + Duration::from_millis(rand::rng().random_range(0..250));
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            () = tokio::time::sleep(delay) => {}
        }
        retry = (retry * 2).min(RECONNECT_MAX);
    }
}

/// Returns whether the Space refused the Computer's credential.
///
/// The WebSocket upgrade carries the Computer token, so the refusal arrives as
/// an HTTP status: retrying it cannot succeed, while a proxy or Host problem
/// may be fixed without pairing again.
fn refused_credential(error: &anyhow::Error) -> bool {
    matches!(
        http_status(error),
        Some(
            tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
                | tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN
        )
    )
}

/// Returns whether a proxy refused the upgrade before it reached the Space.
///
/// A 403 is a policy answer: the Host may disagree with the public origin, or
/// a proxy may deny the path. It is still fatal, because retrying a policy
/// refusal indefinitely hides the misconfiguration, but its diagnostic names
/// the Host/proxy check rather than a dead credential.
fn refused_by_proxy(error: &anyhow::Error) -> bool {
    http_status(error) == Some(tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN)
}

/// Returns the HTTP status a WebSocket upgrade answered with, if any.
fn http_status(error: &anyhow::Error) -> Option<tokio_tungstenite::tungstenite::http::StatusCode> {
    error.chain().find_map(|cause| {
        match cause.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
            Some(tokio_tungstenite::tungstenite::Error::Http(response)) => Some(response.status()),
            _ => None,
        }
    })
}

/// Refuses a machine whose commands bubblewrap cannot fence.
///
/// A Windows Computer is a managed workspace runtime. It has no bubblewrap;
/// its shell is limited to the Bot's folder by the runtime's working directory
/// and file tools, and approvals remain the command authority.
#[cfg(windows)]
fn fence() -> Result<()> {
    eprintln!(
        "Windows Local Computer: commands run with the machine user's rights in the Bot's folder; this build has no sandbox"
    );
    Ok(())
}

/// Refuses a machine whose commands bubblewrap cannot fence.
#[cfg(not(windows))]
fn fence() -> Result<()> {
    LocalRuntime::probe_sandbox().context("this machine cannot fence Bot commands")?;

    Ok(())
}

/// Resolves on Ctrl-C, or on the termination signal a service manager sends.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("the termination signal cannot be watched");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Holds the channel to the Space until it closes, then returns the failure.
async fn connect_and_serve(
    config: &Config,
    hidden: &[PathBuf],
    denied: &[PathBuf],
    worker: bool,
    runtimes: Runtimes,
    acknowledged: &AtomicBool,
) -> Result<()> {
    // A reconnect starts with fresh runtimes: descriptors opened for an older
    // channel must not outlive it or serve a different access choice.
    if let Ok(mut runtimes) = runtimes.lock() {
        runtimes.clear();
    }
    let mut request = if worker {
        socket_url_worker(&config.space_url)?
    } else {
        socket_url(&config.space_url)?
    }
    .into_client_request()
    .context("the Space URL is not usable")?;
    // A worker reaches the Space on its private network but must present the
    // public browser authority the Space's Host guard expects.
    if let Ok(host) = std::env::var("NEHA_WORKER_HOST")
        && !host.trim().is_empty()
    {
        let value = host
            .trim()
            .parse()
            .context("NEHA_WORKER_HOST is not a valid Host header")?;
        request
            .headers_mut()
            .insert(tokio_tungstenite::tungstenite::http::header::HOST, value);
    }
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
            // Windows runs a managed folder without an OS fence. The Space
            // accepts a truthful false only when its operator opted in.
            sandbox: worker || !cfg!(windows),
            // A worker's container is the fence; a Hosted Computer serves no
            // granted folders or full access either way.
            supports_grants: Some(!worker && !cfg!(windows)),
            supports_full_access: Some(!worker && !cfg!(windows)),
            // A worker reaches the Space on a private deployment address and
            // keeps the deployment variable for its Bot environment; a Local
            // Computer reports the origin it actually serves.
            origin: (!worker).then(|| config.space_url.clone()),
        },
    )
    .await?;
    // The greeting is not a connection until the Space says it accepted it:
    // an old Space, a proxy answering for it, or a Space that registered a
    // different Computer must not reset the retry pace.
    wait_for_welcome(&mut socket).await?;
    acknowledged.store(true, Ordering::Relaxed);
    println!("Connected to {}.", config.space_url);

    serve_channel(config, hidden, denied, worker, socket, runtimes).await
}

/// Waits for the Space's acknowledgement, ignoring heartbeats.
///
/// One deadline covers the whole greeting: a Space that only sends heartbeats
/// cannot keep a daemon waiting forever.
async fn wait_for_welcome(socket: &mut Socket) -> Result<()> {
    let deadline = std::time::Instant::now() + HELLO_ACK_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("the Space did not acknowledge the greeting");
        }
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .context("the Space did not acknowledge the greeting")?
            .context("the Space closed the channel during the greeting")?
            .context("the channel failed during the greeting")?;
        match message {
            Message::Text(text) => match serde_json::from_str::<HostFrame>(text.as_str()) {
                Ok(HostFrame::Welcome) => return Ok(()),
                Ok(HostFrame::Ping) => continue,
                Ok(other) => bail!("the Space sent {other:?} before acknowledging the greeting"),
                Err(error) => bail!("the Space's greeting answer is not a frame: {error}"),
            },
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                continue;
            }
            Message::Close(_) => bail!("the Space closed the channel during the greeting"),
        }
    }
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
    denied: &[PathBuf],
    worker: bool,
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
    // The runtime of the action the worker is running, so the worker can stop
    // it without waiting for `execute` to return. The worker is the one task
    // that owns the child process; the reader only signals it.
    let current: Arc<Mutex<Option<Arc<LocalRuntime>>>> = Arc::new(Mutex::new(None));

    let reader = tokio::spawn({
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
            // Tell the worker to stop. The worker owns the running child, so
            // it stops the action and finishes the whole process tree before
            // the execute future and its pid are released; the reader must not
            // reach into another task's child.
            let _ = stop.send(true);

            result
        }
    });

    let mut worker = tokio::spawn({
        let config = config.clone();
        let hidden = hidden.to_vec();
        let denied = denied.to_vec();
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
                        let executing = execute_frame(
                            &config,
                            &hidden,
                            &denied,
                            &runtimes,
                            &current,
                            worker,
                            frame,
                        );
                        tokio::pin!(executing);
                        // This task owns the child process the frame may start.
                        // When the channel goes away it stops that child and
                        // ends its whole process tree before the execute future
                        // and its pid are released; no other task touches the
                        // child, so the cleanup happens exactly once. A future
                        // that never started owns no child and is just dropped.
                        let answer = tokio::select! {
                            biased;
                            _ = stopped.changed() => {
                                if let Ok(mut current) = current.lock()
                                    && let Some(runtime) = current.take()
                                {
                                    runtime.cancel_in_flight();
                                }
                                None
                            }
                            answer = &mut executing => Some(answer),
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

/// Opens the managed runtime a Hosted worker uses for one Bot.
fn worker_base(config: &Config, bot: &str) -> Result<LocalRuntime, String> {
    let folder = folder_for(&config.workspace, bot)?;
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    LocalRuntime::worker(&config.workspace).map_err(|error| error.to_string())
}

/// Opens a Hosted worker run in the Bot's directory.
fn worker_runtime(config: &Config, bot: &str) -> Result<Arc<LocalRuntime>, String> {
    let folder = folder_for(&config.workspace, bot)?;
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let runtime = LocalRuntime::worker(&config.workspace).map_err(|error| error.to_string())?;
    runtime
        .with_working_dir(&folder.to_string_lossy())
        .map(Arc::new)
        .map_err(|error| error.to_string())
}

/// Returns the runtime for one action, in either a worker or a Local daemon.
#[allow(clippy::too_many_arguments)]
fn bot_runtime(
    config: &Config,
    hidden: &[PathBuf],
    denied: &[PathBuf],
    runtimes: &Runtimes,
    bot: &str,
    access: &AccessFrame,
    working_dir: Option<&str>,
    worker: bool,
) -> Result<Arc<LocalRuntime>, String> {
    if worker {
        return worker_runtime(config, bot);
    }
    machine(config, hidden, denied, runtimes, bot, access, working_dir)
}

/// Answers one frame from the Space.
async fn execute_frame(
    config: &Config,
    hidden: &[PathBuf],
    denied: &[PathBuf],
    runtimes: &Runtimes,
    current: &Mutex<Option<Arc<LocalRuntime>>>,
    worker: bool,
    frame: HostFrame,
) -> DeviceFrame {
    match frame {
        HostFrame::Ping => DeviceFrame::Pong,
        // The greeting was acknowledged before this channel served any
        // frame; a repeated acknowledgement needs no work.
        HostFrame::Welcome => DeviceFrame::Pong,
        HostFrame::Action {
            id,
            bot,
            action,
            working_dir,
            access,
            env,
        } => {
            let runtime = match bot_runtime(
                config,
                hidden,
                denied,
                runtimes,
                &bot,
                &access,
                working_dir.as_deref(),
                worker,
            ) {
                Ok(runtime) => runtime,
                Err(message) => return DeviceFrame::Failed { id, message },
            };
            runtime.set_env(env);
            // The worker finds the running action here when the channel goes
            // away, so it can stop the child it owns.
            *current.lock().expect("current action lock") = Some(Arc::clone(&runtime));
            let result = runtime.execute(action.into()).await;
            *current.lock().expect("current action lock") = None;
            DeviceFrame::Result {
                id,
                status: result.status,
                output: result.output,
            }
        }
        HostFrame::Read {
            id,
            bot,
            path,
            working_dir,
            access,
        } => {
            // The read uses the run's own access: a verification of a write an
            // interrupted full-access run may have made outside the Bot's
            // folder has to reach the same paths that run could.
            match bot_runtime(
                config,
                hidden,
                denied,
                runtimes,
                &bot,
                &access,
                working_dir.as_deref(),
                worker,
            ) {
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
            }
        }
        HostFrame::Write {
            id,
            bot,
            path,
            bytes,
            access,
        } => {
            // A File delivery is chosen by the Space, so it stays inside the
            // Bot's folder whatever the run's access: full access only picks
            // the base runtime that opens no sandbox, and the delivery then
            // goes through the folder descriptor with no write roots.
            let opened: Result<LocalRuntime, String> = if worker {
                worker_base(config, &bot).and_then(|base| {
                    let folder = folder_for(&config.workspace, &bot)?;
                    base.with_working_dir(&folder.to_string_lossy())
                        .map_err(|error| error.to_string())
                })
            } else {
                match machine_base(config, hidden, denied, runtimes, &bot, access.full_access) {
                    Ok(base) => base
                        .with_machine_access(false, &[], None)
                        .map_err(|error| error.to_string()),
                    Err(message) => Err(message),
                }
            };
            match opened {
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
            }
        }
    }
}

/// Returns this Bot's runtime with one request's access and run directory.
///
/// `working_dir` is the directory the run works in. Without full access it is
/// also that run's one-off write root, so the shell may write there and a
/// relative file path resolves against it. The access-specific directories are
/// opened here for this request only; the cached base holds none of them.
fn machine(
    config: &Config,
    hidden: &[PathBuf],
    denied: &[PathBuf],
    runtimes: &Runtimes,
    bot: &str,
    access: &AccessFrame,
    working_dir: Option<&str>,
) -> Result<Arc<LocalRuntime>, String> {
    let base = machine_base(config, hidden, denied, runtimes, bot, access.full_access)?;
    let runtime = base
        .with_machine_access(access.full_access, &access.write_roots, working_dir)
        .map_err(|error| error.to_string())?;

    Ok(Arc::new(runtime))
}

/// Returns this Bot's opened runtime base, opening it on first use.
///
/// The base holds the Bot's folder descriptor and the sandbox decision for its
/// access model. A full-access base is opened without a scan or a bubblewrap
/// probe; a granted base is scanned and fenced once per connection.
#[allow(clippy::too_many_arguments)]
fn machine_base(
    config: &Config,
    hidden: &[PathBuf],
    denied: &[PathBuf],
    runtimes: &Runtimes,
    bot: &str,
    full_access: bool,
) -> Result<Arc<LocalRuntime>, String> {
    // The Bot must be one identifier component; reject anything else before
    // touching the filesystem.
    folder_for(&config.workspace, bot)?;

    let key = RuntimeKey {
        bot: bot.to_owned(),
        full_access,
    };
    let mut runtimes = runtimes
        .lock()
        .map_err(|_| "the runtime table is locked".to_owned())?;
    match runtimes.entry(key) {
        Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
        Entry::Vacant(entry) => {
            // Windows has no directory descriptors or bubblewrap. The managed
            // runtime creates the Bot folder, keeps its file tools inside that
            // folder, and supports the same actions.
            #[cfg(windows)]
            let runtime = {
                let folder = folder_for(&config.workspace, bot)?;
                std::fs::create_dir_all(&folder)
                    .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
                LocalRuntime::machine_with_hidden(&folder, hidden.to_vec())
                    .map_err(|error| error.to_string())?
                    .with_denied(denied.to_vec())
            };
            // Open the workspace root without following a symlink, then walk
            // the Bot's folder through descriptors: a swapped path component
            // can never redirect the runtime outside its folder.
            #[cfg(not(windows))]
            let runtime = {
                let root = LocalRuntime::open_workspace(&config.workspace)
                    .map_err(|error| error.to_string())?;
                if full_access {
                    // The owner gave the Bot the machine user's own rights: no
                    // sandbox is probed and the folder is not scanned, because the
                    // Bot's own links are its business and this must work on a
                    // machine whose bubblewrap cannot start a sandbox.
                    LocalRuntime::full_access(&root, bot).map_err(|error| error.to_string())?
                } else {
                    LocalRuntime::machine_at(&root, bot, hidden.to_vec())
                        .map_err(|error| error.to_string())?
                        .with_denied(denied.to_vec())
                }
            };
            Ok(Arc::clone(entry.insert(Arc::new(runtime))))
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
    let base = normalized_any_space_url(space_url)?;
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("the Space URL must start with http:// or https://");
    };

    Ok(format!("{socket}/api/computer/socket"))
}

/// Returns the WebSocket address a worker reaches its public Space through.
fn socket_url_worker(space_url: &str) -> Result<String> {
    let base = normalized_worker_url(space_url)?;
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("the Space URL must start with http:// or https://");
    };

    Ok(format!("{socket}/api/computer/socket"))
}

/// Returns a worker Space URL without a trailing slash.
///
/// Unlike a Local Computer's loopback-only pairing, a worker container reaches
/// the Space over the deployment's public origin; credentials in the URL are
/// still refused because the origin is logged.
fn normalized_worker_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("the Space URL is empty");
    }
    let parsed =
        reqwest::Url::parse(url).context("the Space URL must be an absolute http(s) URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("the Space URL must start with http:// or https://");
    }
    if parsed.host_str().is_none() {
        bail!("the Space URL has no host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("the Space URL must not carry credentials");
    }

    Ok(url.to_owned())
}

/// Returns a Space origin without a trailing slash, refusing a non-loopback one.
///
/// The local ticket and the saved CLI session both describe a Space on this
/// machine. Sending either to anything else would hand a secret to another
/// origin, so this path names the allowed hosts.
fn normalized_space_url(url: &str) -> Result<String> {
    let parsed = parsed_space_origin(url)?;
    let host = parsed
        .host_str()
        .map(str::to_owned)
        .context("the Space URL has no host")?;
    if !is_loopback_host(&host) {
        bail!(
            "this pairing path uses a Space on this machine: \
             the Space URL must use 127.0.0.1, ::1, or localhost, not {host}"
        );
    }

    Ok(parsed.origin().ascii_serialization())
}

/// Returns a Space origin, allowing one reached over the network.
///
/// A remote Space must use https: the machine sends its pairing token and its
/// daemon credential there, and only the system's certificate store is trusted.
pub(crate) fn normalized_any_space_url(url: &str) -> Result<String> {
    let parsed = parsed_space_origin(url)?;
    let host = parsed
        .host_str()
        .map(str::to_owned)
        .context("the Space URL has no host")?;
    if parsed.scheme() == "http" && !is_loopback_host(&host) {
        bail!("a Space reached over the network must use https, not http");
    }

    Ok(parsed.origin().ascii_serialization())
}

/// Parses one Space origin, leaving the scheme's trust rule to the caller.
fn parsed_space_origin(url: &str) -> Result<reqwest::Url> {
    let value = url.trim().trim_end_matches('/');
    if value.is_empty() {
        bail!("the Space URL is empty");
    }
    let parsed =
        reqwest::Url::parse(value).context("the Space URL must be an absolute http(s) URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("the Space URL must start with http:// or https://");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("the Space URL must not carry credentials");
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("the Space URL must be an origin with no path, query, or fragment");
    }

    Ok(parsed)
}

/// Returns whether a Space origin is the Space on this machine.
fn is_loopback_space_url(origin: &str) -> bool {
    reqwest::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| is_loopback_host(&host))
}

/// Returns whether a host names this machine's loopback.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Returns the name this machine reports to a remote Space.
///
/// The pairing names the Computer in the record, so the User sees the machine
/// name rather than a generic one. The value is bounded and control-free
/// because it reaches a terminal and the record.
pub(crate) fn machine_name() -> String {
    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let name = name.trim();
    if name.is_empty() {
        return "Computer".to_owned();
    }

    name.chars().take(64).collect()
}

/// Returns a fresh 32-byte URL-safe base64 secret.
fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Shows a pairing code the way a User types it.
pub(crate) fn display_pairing_code(code: &str) -> String {
    let mut letters = code.chars();
    let first: String = letters.by_ref().take(4).collect();
    let rest: String = letters.collect();
    if first.chars().count() == 4 && rest.chars().count() == 4 {
        format!("{first}-{rest}")
    } else {
        code.to_owned()
    }
}

/// The ticket file a Space writes for this machine.
const TICKET_NAME: &str = "local-computer.ticket";

/// The ticket a Space wrote for this machine's first pairing.
#[derive(Debug)]
struct Ticket {
    /// The one-use pairing secret.
    ticket: String,
    /// The Space URL the ticket file names, when it names one.
    url: Option<String>,
}

/// Returns the configuration directory that holds this machine's Neha files.
#[cfg(windows)]
fn config_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NEHA_COMPUTER_HOME").map(PathBuf::from) {
        return Ok(path);
    }
    let Some(base) = windows_config_base() else {
        bail!(
            "LOCALAPPDATA is not set; set NEHA_COMPUTER_HOME to the directory that holds this machine's Neha files"
        );
    };

    Ok(base.join("Neha").join("computer"))
}

/// Returns the configuration directory that holds this machine's Neha files.
#[cfg(not(windows))]
fn config_directory() -> Result<PathBuf> {
    config_directory_from(
        std::env::var_os("NEHA_COMPUTER_HOME").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Returns the per-user base directory Windows uses for Neha state.
#[cfg(windows)]
fn windows_config_base() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
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
#[cfg(windows)]
fn standard_config_directory(
    _xdg_config_home: Option<PathBuf>,
    _user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    windows_config_base().map(|base| base.join("Neha"))
}

/// Returns the per-user configuration directory the CLI and the Space share.
///
/// `NEHA_COMPUTER_HOME` does not move it: the Space writes its ticket there and
/// the CLI keeps its session credential there.
#[cfg(not(windows))]
fn standard_config_directory(
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| path.is_absolute())
        .or_else(|| user_home.map(|home| home.join(".config")))
        .map(|base| base.join("neha"))
}

/// Returns the resolved path of the CLI session credential.
///
/// The resolver is shared by the CLI and the daemon's hidden roots, and it
/// never returns a relative path:
/// - `NEHA_CREDENTIAL` wins; a relative value is resolved against the working
///   directory so the file it names is the file the daemon hides;
/// - otherwise the standard configuration directory (`$XDG_CONFIG_HOME/neha`,
///   with a relative XDG ignored per the XDG base directory specification, or
///   `$HOME/.config/neha`);
/// - with none of them there is nowhere safe to keep a session, so this is an
///   error instead of a cwd-relative fallback a Bot could read.
pub(crate) fn credential_path() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot read the working directory")?;

    credential_path_from(
        std::env::var_os("NEHA_CREDENTIAL").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        &cwd,
    )
}

/// Returns the credential path for known environment values.
fn credential_path_from(
    neha_credential: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
    cwd: &Path,
) -> Result<PathBuf> {
    if let Some(path) = neha_credential {
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };

        return Ok(resolved_path(&path));
    }
    let Some(directory) = standard_config_directory(xdg_config_home, user_home) else {
        bail!(
            "cannot find a place for the session credential: set HOME, XDG_CONFIG_HOME, or NEHA_CREDENTIAL"
        );
    };
    let directory = if directory.is_absolute() {
        directory
    } else {
        cwd.join(directory)
    };

    Ok(resolved_path(&directory.join("credential.json")))
}

/// Removes a session credential written by the old cwd-relative resolver.
///
/// Before this resolver existed, a relative `XDG_CONFIG_HOME` made the CLI
/// save the session under that relative path — inside whatever directory the
/// User ran the command in, which is exactly where a Bot's shell may read it.
/// The resolved file wins; when it is missing the old credential is moved
/// there, and the old file is always removed. A cleanup failure is an error:
/// leaving a plaintext session in a Bot-readable directory is not a warning.
pub(crate) fn migrate_legacy_credential() -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the working directory")?;

    migrate_legacy_credential_for(
        &cwd,
        std::env::var_os("NEHA_CREDENTIAL").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Migrates the legacy credential for known environment values.
fn migrate_legacy_credential_for(
    cwd: &Path,
    neha_credential: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Result<()> {
    let Some(legacy) = legacy_relative_credential_path(
        cwd,
        neha_credential.as_deref(),
        xdg_config_home.as_deref(),
    ) else {
        return Ok(());
    };
    if !legacy.exists() {
        return Ok(());
    }
    let target = credential_path_from(neha_credential, xdg_config_home, user_home, cwd)?;

    migrate_legacy_credential_between(&target, &legacy)
}

/// Moves `legacy` to `target` unless they are the same directory entry.
///
/// The copy reads through the old name (a symbolic link's target is read), but
/// the cleanup removes the name itself: for a symbolic link that is the link,
/// never the file it points at.
fn migrate_legacy_credential_between(target: &Path, legacy: &Path) -> Result<()> {
    if same_directory_entry(target, legacy) {
        // cwd == HOME and XDG_CONFIG_HOME == ".config" resolve to one entry;
        // it already sits in the hidden configuration directory, so it stays
        // exactly where it is.
        return Ok(());
    }
    if !target.exists() {
        let bytes = std::fs::read(legacy)
            .with_context(|| format!("cannot read the older credential at {}", legacy.display()))?;
        client::write_private(target, &bytes)?;
    }
    std::fs::remove_file(legacy).with_context(|| {
        format!(
            "cannot remove the older session credential {}",
            legacy.display()
        )
    })?;

    Ok(())
}

/// Returns whether two paths name the same directory entry.
///
/// Only the parent directory is resolved; the final name is kept as written.
/// Resolving the last component would make a hard link under another name look
/// like the same entry (it is not — the old name still exists and must go) and
/// would turn a symbolic link into its target (whose name must never be
/// removed).
fn same_directory_entry(left: &Path, right: &Path) -> bool {
    match (
        resolved_directory_entry(left),
        resolved_directory_entry(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// Returns the resolved parent directory plus the path's final name.
fn resolved_directory_entry(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let name = path.file_name()?;

    Some(resolved_path(parent).join(name))
}

/// Returns the cwd-relative credential path the old resolver used, if any.
///
/// The final name is preserved even when it is a symbolic link: migration must
/// remove the link, not the file it points at.
fn legacy_relative_credential_path(
    cwd: &Path,
    neha_credential: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Option<PathBuf> {
    // A relative NEHA_CREDENTIAL now resolves against the cwd to the very file
    // the old resolver wrote, so there is nothing to migrate.
    if neha_credential.is_some() {
        return None;
    }
    let xdg = xdg_config_home?;
    if xdg.is_absolute() {
        return None;
    }

    resolved_directory_entry(&cwd.join(xdg).join("neha").join("credential.json"))
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

/// A Space ticket found on disk, with the file it came from.
#[derive(Debug)]
struct FoundTicket {
    /// The file the ticket was read from.
    file: PathBuf,
    /// The ticket itself.
    ticket: Ticket,
}

/// Returns the standard per-user configuration directory from the environment.
fn standard_config_from_env() -> Option<PathBuf> {
    standard_config_directory(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Returns every Space ticket under `<config>/spaces/<home-key>/`.
///
/// Each Space writes its own ticket; only a ticket that names its Space can be
/// matched, so a file without a URL is skipped. Unreadable or corrupt files
/// are skipped too: another Space's in-progress file must not break pairing.
fn found_tickets(standard_config: &Path) -> Vec<FoundTicket> {
    let Ok(entries) = std::fs::read_dir(standard_config.join("spaces")) else {
        return Vec::new();
    };
    let mut tickets = Vec::new();
    for entry in entries.flatten() {
        let file = entry.path().join(TICKET_NAME);
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(ticket) = parse_ticket(&text) else {
            continue;
        };
        if ticket.url.is_none() {
            continue;
        }
        tickets.push(FoundTicket { file, ticket });
    }
    tickets.sort_by(|left, right| left.file.cmp(&right.file));

    tickets
}

/// Chooses the one ticket for `target`, or `None` to ask the Space instead.
///
/// With no known target, exactly one waiting ticket is unambiguous. Anything
/// ambiguous is an error: pairing this machine with the wrong Space because a
/// directory happened to hold two tickets is not a choice to guess.
fn choose_ticket(found: Vec<FoundTicket>, target: Option<&str>) -> Result<Option<FoundTicket>> {
    let Some(target) = target else {
        return match found.len() {
            0 => Ok(None),
            1 => Ok(found.into_iter().next()),
            _ => bail!(
                "several Space tickets are waiting; pass --space or --ticket-file to choose one"
            ),
        };
    };
    let target = normalized_space_url(target)?;
    let mut matching = found.into_iter().filter(|found| {
        found
            .ticket
            .url
            .as_deref()
            .and_then(|url| normalized_space_url(url).ok())
            .is_some_and(|url| url == target)
    });
    let ticket = matching.next();
    if matching.next().is_some() {
        bail!("several Space tickets match {target}; pass --ticket-file with the one to use");
    }

    Ok(ticket)
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

/// Returns the saved CLI session's Space origin, when there is one.
///
/// The saved session may name a remote Space; the caller decides whether the
/// local-ticket path may use it. A malformed saved URL is an error, never a
/// fallback: the HTTP ticket route would otherwise send the session's bearer
/// token to whatever host the stale or edited file names.
fn saved_session_origin(credential: Option<&Credential>) -> Result<Option<String>> {
    credential
        .map(|credential| normalized_any_space_url(&credential.url))
        .transpose()
}

/// Returns the saved session's origin only when it is this machine's Space.
fn local_session_origin(saved: Option<&str>) -> Option<String> {
    saved
        .filter(|origin| is_loopback_space_url(origin))
        .map(str::to_owned)
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
#[cfg(windows)]
fn default_workspace() -> Result<PathBuf> {
    let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
        bail!("USERPROFILE is not set; pass --workspace to choose where the Bot Workspaces live");
    };

    Ok(home.join("Neha").join("workspaces"))
}

/// Returns the directory that holds one folder per Bot by default.
#[cfg(not(windows))]
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
    hidden_roots_for(
        config_directory()?,
        standard_config_from_env(),
        &credential_path()?,
    )
}

/// Returns the directories a Bot must not reach, resolved and de-duplicated.
///
/// The daemon's configuration directory holds the channel credential, the
/// per-user configuration directory holds the CLI's session credential and the
/// Space tickets, and `NEHA_CREDENTIAL` may move that session file into a
/// directory of its own. The roots are directories even when the files inside
/// them do not exist yet: the directories are created here, so a file created
/// later inside one is still covered by the sandbox's mount and by the file
/// tools' prefix check.
fn hidden_roots_for(
    config_directory: PathBuf,
    standard_config: Option<PathBuf>,
    credential: &Path,
) -> Result<Vec<PathBuf>> {
    let mut candidates = vec![config_directory];
    if let Some(directory) = standard_config {
        candidates.push(directory);
    }
    // The credential's own directory is hidden even when NEHA_CREDENTIAL is
    // relative or the standard configuration directory is unavailable: the
    // resolver points at the file the CLI actually uses.
    let parent = credential
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "the session credential {} has no directory to hide",
                credential.display()
            )
        })?;
    candidates.push(parent);
    // The roots must exist before the daemon starts; creating a hidden root
    // later would leave the files already there unprotected.
    for candidate in &candidates {
        std::fs::create_dir_all(candidate)
            .with_context(|| format!("cannot create {}", candidate.display()))?;
    }
    let mut seen = std::collections::HashSet::new();
    let mut roots = Vec::new();
    for candidate in candidates {
        let root = std::fs::canonicalize(&candidate)
            .with_context(|| format!("cannot resolve {}", candidate.display()))?;
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }

    Ok(roots)
}

/// Returns the files the file tools must refuse even when they appear later.
///
/// These are the daemon configuration, the CLI session credential, and the
/// per-Space ticket directory. The sandbox hides their directories; the file
/// tools run with the machine user's rights, so they get the paths themselves,
/// existing or not.
fn denied_paths() -> Result<Vec<PathBuf>> {
    Ok(denied_paths_from(
        config_path().ok(),
        credential_path()?,
        standard_config_from_env(),
    ))
}

/// Returns the deny list for known paths.
///
/// The credential path comes from [`credential_path_from`], the same resolver
/// the daemon uses to choose hidden directories: a relative `XDG_CONFIG_HOME`
/// cannot put the credential outside the hidden configuration directory.
fn denied_paths_from(
    config_path: Option<PathBuf>,
    credential_path: PathBuf,
    standard_config: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = config_path {
        paths.push(resolved_path(&path));
    }
    paths.push(resolved_path(&credential_path));
    if let Some(directory) = standard_config {
        paths.push(resolved_path(&directory.join("spaces")));
    }

    paths
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

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    //! Fixtures and accessors the integration tests need from inside the daemon.

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use neha_computer::{DeviceFrame, HostFrame, LocalRuntime};

    use crate::client::Credential;

    use super::{Config, FoundTicket, RuntimeKey, Ticket};

    /// The daemon's end of one channel, so a fixture can name it.
    pub type TestSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// One saved pairing, built by hand for a fixture.
    ///
    /// The fields stay private: a test builds one with [`config`] and reads it
    /// through the accessors, exactly as the daemon does.
    #[derive(Debug, PartialEq, Eq)]
    pub struct TestConfig(Config);

    impl TestConfig {
        /// The Space's base URL.
        pub fn space_url(&self) -> &str {
            &self.0.space_url
        }

        /// The Computer this machine is.
        pub fn computer(&self) -> &str {
            &self.0.computer
        }

        /// The token the Space admits this machine with.
        pub fn token(&self) -> &str {
            &self.0.token
        }

        /// The directory that holds one folder per Bot.
        pub fn workspace(&self) -> &Path {
            &self.0.workspace
        }

        /// Saves this pairing at one path, readable only by this machine user.
        pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
            self.0.save_to(path)
        }
    }

    /// Builds the saved pairing the daemon fixtures pair with.
    pub fn config(workspace: PathBuf) -> TestConfig {
        TestConfig(Config {
            space_url: "http://127.0.0.1:7337".to_owned(),
            computer: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
            token: "machine-token".to_owned(),
            workspace,
        })
    }

    /// Loads the pairing saved at one path.
    pub fn load_config(path: &Path) -> anyhow::Result<TestConfig> {
        Config::load_from(path).map(TestConfig)
    }

    /// The runtime cache, wrapped so a fixture can name it.
    pub struct TestRuntimes(super::Runtimes);

    /// Builds an empty runtime cache.
    pub fn empty_runtimes() -> TestRuntimes {
        TestRuntimes(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Builds a runtime cache holding one opened runtime for one Bot.
    pub fn runtimes_with(bot: &str, full_access: bool, runtime: LocalRuntime) -> TestRuntimes {
        let key = RuntimeKey {
            bot: bot.to_owned(),
            full_access,
        };
        TestRuntimes(Arc::new(Mutex::new(HashMap::from([(
            key,
            Arc::new(runtime),
        )]))))
    }

    /// Whether the cache holds no runtime.
    pub fn runtimes_is_empty(runtimes: &TestRuntimes) -> bool {
        runtimes.0.lock().unwrap().is_empty()
    }

    /// How many runtimes the cache holds.
    pub fn runtimes_len(runtimes: &TestRuntimes) -> usize {
        runtimes.0.lock().unwrap().len()
    }

    /// Whether every cached runtime is a full-access base.
    pub fn runtimes_all_full_access(runtimes: &TestRuntimes) -> bool {
        runtimes.0.lock().unwrap().keys().all(|key| key.full_access)
    }

    /// Answers one frame from the Space.
    pub async fn execute_frame(
        config: &TestConfig,
        hidden: &[PathBuf],
        denied: &[PathBuf],
        runtimes: &TestRuntimes,
        current: &Mutex<Option<Arc<LocalRuntime>>>,
        frame: HostFrame,
    ) -> DeviceFrame {
        super::execute_frame(
            &config.0,
            hidden,
            denied,
            &runtimes.0,
            current,
            false,
            frame,
        )
        .await
    }

    /// Serves one connected channel until it ends or fails.
    pub async fn serve_channel(
        config: &TestConfig,
        hidden: &[PathBuf],
        denied: &[PathBuf],
        socket: TestSocket,
        runtimes: TestRuntimes,
    ) -> anyhow::Result<()> {
        super::serve_channel(&config.0, hidden, denied, false, socket, runtimes.0).await
    }

    /// Returns a Bot's folder, refusing a name that is not an identifier.
    pub fn folder_for(workspace: &Path, bot: &str) -> Result<PathBuf, String> {
        super::folder_for(workspace, bot)
    }

    /// Answers a Write that succeeded.
    pub fn write_receipt(id: u64, path: &str, bytes: usize) -> DeviceFrame {
        super::write_receipt(id, path, bytes)
    }

    /// Encodes File bytes for a frame.
    pub fn encode_bytes(bytes: &[u8]) -> String {
        super::encode_bytes(bytes)
    }

    /// Decodes File bytes from a frame.
    pub fn decode_bytes(text: &str) -> Result<Vec<u8>, String> {
        super::decode_bytes(text)
    }

    /// Returns the configuration directory for known environment values.
    pub fn config_directory_from(
        neha_computer_home: Option<PathBuf>,
        xdg_config_home: Option<PathBuf>,
        user_home: Option<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        super::config_directory_from(neha_computer_home, xdg_config_home, user_home)
    }

    /// Returns the standard per-user configuration directory.
    pub fn standard_config_directory(
        xdg_config_home: Option<PathBuf>,
        user_home: Option<PathBuf>,
    ) -> Option<PathBuf> {
        super::standard_config_directory(xdg_config_home, user_home)
    }

    /// Returns the default workspace root under the user home.
    pub fn default_workspace_from(user_home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
        super::default_workspace_from(user_home)
    }

    /// Refuses to work while a pairing from the old default exists.
    pub fn refuse_older_pairing_from(user_home: Option<&Path>) -> anyhow::Result<()> {
        super::refuse_older_pairing_from(user_home)
    }

    /// Refuses a Workspace and the configuration directory that contains each
    /// other.
    pub fn refuse_workspace(config_directory: &Path, workspace: &Path) -> anyhow::Result<()> {
        super::refuse_workspace(config_directory, workspace)
    }

    /// The parts of a ticket file a test inspects.
    #[derive(Debug)]
    pub struct TestTicket(Ticket);

    impl TestTicket {
        /// The one-use pairing secret.
        pub fn ticket(&self) -> &str {
            &self.0.ticket
        }

        /// The Space URL the ticket file names, when it names one.
        pub fn url(&self) -> Option<&str> {
            self.0.url.as_deref()
        }
    }

    /// Reads one ticket file.
    pub fn parse_ticket(text: &str) -> anyhow::Result<TestTicket> {
        super::parse_ticket(text).map(TestTicket)
    }

    /// A Space ticket found on disk, with the file it came from.
    #[derive(Debug)]
    pub struct TestFoundTicket(FoundTicket);

    impl TestFoundTicket {
        /// The file the ticket was read from.
        pub fn file(&self) -> &Path {
            &self.0.file
        }

        /// The ticket itself.
        pub fn ticket(&self) -> &str {
            &self.0.ticket.ticket
        }

        /// The Space URL the ticket file names, when it names one.
        pub fn url(&self) -> Option<&str> {
            self.0.ticket.url.as_deref()
        }
    }

    /// Returns every Space ticket under `<config>/spaces/<home-key>/`.
    pub fn found_tickets(standard_config: &Path) -> Vec<TestFoundTicket> {
        super::found_tickets(standard_config)
            .into_iter()
            .map(TestFoundTicket)
            .collect()
    }

    /// Chooses the one ticket to pair with, or refuses an ambiguous choice.
    pub fn choose_ticket(
        found: Vec<TestFoundTicket>,
        target: Option<&str>,
    ) -> anyhow::Result<Option<TestFoundTicket>> {
        super::choose_ticket(found.into_iter().map(|found| found.0).collect(), target)
            .map(|chosen| chosen.map(TestFoundTicket))
    }

    /// Returns the credential path for known environment values.
    pub fn credential_path_from(
        neha_credential: Option<PathBuf>,
        xdg_config_home: Option<PathBuf>,
        user_home: Option<PathBuf>,
        cwd: &Path,
    ) -> anyhow::Result<PathBuf> {
        super::credential_path_from(neha_credential, xdg_config_home, user_home, cwd)
    }

    /// Removes a session credential written by the old cwd-relative resolver.
    pub fn migrate_legacy_credential_for(
        cwd: &Path,
        neha_credential: Option<PathBuf>,
        xdg_config_home: Option<PathBuf>,
        user_home: Option<PathBuf>,
    ) -> anyhow::Result<()> {
        super::migrate_legacy_credential_for(cwd, neha_credential, xdg_config_home, user_home)
    }

    /// Returns the path the old cwd-relative resolver wrote the credential at.
    pub fn legacy_relative_credential_path(
        cwd: &Path,
        neha_credential: Option<&Path>,
        xdg_config_home: Option<&Path>,
    ) -> Option<PathBuf> {
        super::legacy_relative_credential_path(cwd, neha_credential, xdg_config_home)
    }

    /// Returns the directories the sandbox hides from a Bot.
    pub fn hidden_roots_for(
        config_directory: PathBuf,
        standard_config: Option<PathBuf>,
        credential: &Path,
    ) -> anyhow::Result<Vec<PathBuf>> {
        super::hidden_roots_for(config_directory, standard_config, credential)
    }

    /// Returns the credential and pairing files a Bot must not read.
    pub fn denied_paths_from(
        config_path: Option<PathBuf>,
        credential_path: PathBuf,
        standard_config: Option<PathBuf>,
    ) -> Vec<PathBuf> {
        super::denied_paths_from(config_path, credential_path, standard_config)
    }

    /// The normalized Space URL a machine may pair with, or why it may not.
    pub fn normalized_space_url(url: &str) -> anyhow::Result<String> {
        super::normalized_space_url(url)
    }

    /// Returns a Space origin that may be reached over the network.
    pub fn normalized_any_space_url(url: &str) -> anyhow::Result<String> {
        super::normalized_any_space_url(url)
    }

    /// Returns whether an origin names the Space on this machine.
    pub fn is_loopback_space_url(origin: &str) -> bool {
        super::is_loopback_space_url(origin)
    }

    /// Returns the name this machine reports to a Space.
    pub fn machine_name() -> String {
        super::machine_name()
    }

    /// Shows a pairing code the way a User types it.
    pub fn display_pairing_code(code: &str) -> String {
        super::display_pairing_code(code)
    }

    /// Returns whether a channel error means the saved pairing was refused.
    pub fn refused_credential(error: &anyhow::Error) -> bool {
        super::refused_credential(error)
    }

    /// Returns whether a proxy refused the channel's Host.
    pub fn refused_by_proxy(error: &anyhow::Error) -> bool {
        super::refused_by_proxy(error)
    }

    /// Waits for approval of one remote pairing, retrying transient polls.
    pub async fn wait_for_remote_approval(
        space_url: &str,
        started: &crate::client::StartedPairing,
        poll_secret: &str,
    ) -> anyhow::Result<String> {
        super::wait_for_remote_approval(space_url, started, poll_secret).await
    }

    /// The saved session's Space URL, when the session is usable for pairing.
    pub fn saved_session_origin(credential: Option<&Credential>) -> anyhow::Result<Option<String>> {
        super::saved_session_origin(credential)
    }

    /// Returns a saved session origin only when it is this machine's Space.
    pub fn local_session_origin(saved: Option<&str>) -> Option<String> {
        super::local_session_origin(saved)
    }

    /// Refuses a ticket that names another Space.
    pub fn check_ticket_space(ticket_url: &str, space: &str) -> anyhow::Result<()> {
        super::check_ticket_space(ticket_url, space)
    }

    /// Refuses a saved session that names another Space.
    pub fn check_saved_origin(space: &str, saved: &str) -> anyhow::Result<()> {
        super::check_saved_origin(space, saved)
    }

    /// Whether two Space URLs name the same origin.
    pub fn same_origin(left: &str, right: &str) -> anyhow::Result<bool> {
        super::same_origin(left, right)
    }

    /// Whether pairing may go ahead: a terminal or an explicit yes.
    pub fn confirmation_needed(yes: bool, interactive: bool) -> anyhow::Result<bool> {
        super::confirmation_needed(yes, interactive)
    }

    /// Whether an answer means yes.
    pub fn answer_is_yes(answer: &str) -> bool {
        super::answer_is_yes(answer)
    }

    /// The command-socket URL the channel talks to.
    pub fn socket_url(space_url: &str) -> anyhow::Result<String> {
        super::socket_url(space_url)
    }
}
