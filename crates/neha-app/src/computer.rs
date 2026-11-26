//! The machine side of a real Computer: pairing and the channel daemon.
//!
//! `neha computer pair` asks a Space to admit this machine, shows the User a
//! one-time code, and waits until the User enters it. The machine then keeps
//! its own token in `computer.json` and serves the Space over one outgoing
//! WebSocket. `neha computer run` serves a saved pairing again.
//!
//! The daemon listens on no port and runs each action with the machine user's
//! rights, fenced by bubblewrap: a Bot reads what that user reads and writes
//! only its own folder.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
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
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::client::{self, PairingState};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the machine asks its Space how the pairing is doing.
const PAIR_POLL: Duration = Duration::from_secs(2);
/// How long a machine waits for its pairing code to be entered.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the daemon waits before it reconnects to its Space.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

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
    // A machine that cannot fence a command never serves as a Computer.
    fence(&config.workspace)?;
    let mut runtimes = HashMap::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            served = connect_and_serve(&config, &mut runtimes) => {
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
fn fence(workspace: &Path) -> Result<()> {
    LocalRuntime::machine(workspace)?;

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
async fn connect_and_serve(
    config: &Config,
    runtimes: &mut HashMap<String, LocalRuntime>,
) -> Result<()> {
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

    loop {
        let Some(message) = socket.next().await else {
            bail!("the Space closed the channel");
        };
        match message.context("the channel failed")? {
            Message::Text(text) => {
                let frame: HostFrame = serde_json::from_str(text.as_str())
                    .context("the Space sent a frame this daemon does not understand")?;
                send(&mut socket, &answer(config, runtimes, frame).await).await?;
            }
            Message::Close(_) => bail!("the Space closed the channel"),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

/// Answers one frame from the Space.
async fn answer(
    config: &Config,
    runtimes: &mut HashMap<String, LocalRuntime>,
    frame: HostFrame,
) -> DeviceFrame {
    match frame {
        HostFrame::Action { id, bot, action } => match machine(config, runtimes, &bot) {
            Ok(runtime) => {
                let result = runtime.execute(action.into()).await;
                DeviceFrame::Result {
                    id,
                    status: result.status,
                    output: result.output,
                }
            }
            Err(message) => DeviceFrame::Failed { id, message },
        },
        HostFrame::Read { id, bot, path } => match machine(config, runtimes, &bot) {
            Ok(runtime) => match runtime.read_bytes(&path).await {
                Ok(bytes) => DeviceFrame::Bytes {
                    id,
                    bytes: encode_bytes(&bytes),
                },
                Err(message) => DeviceFrame::Failed { id, message },
            },
            Err(message) => DeviceFrame::Failed { id, message },
        },
        HostFrame::Write {
            id,
            bot,
            path,
            bytes,
        } => match machine(config, runtimes, &bot) {
            Ok(runtime) => match decode_bytes(&bytes) {
                Ok(bytes) => {
                    let length = bytes.len();
                    match runtime.write_bytes(&path, bytes).await {
                        Ok(()) => write_receipt(id, &path, length),
                        Err(message) => DeviceFrame::Failed { id, message },
                    }
                }
                Err(message) => DeviceFrame::Failed { id, message },
            },
            Err(message) => DeviceFrame::Failed { id, message },
        },
        HostFrame::Ping => DeviceFrame::Pong,
    }
}

/// Returns this Bot's runtime, opening its folder on first use.
fn machine<'a>(
    config: &Config,
    runtimes: &'a mut HashMap<String, LocalRuntime>,
    bot: &str,
) -> Result<&'a mut LocalRuntime, String> {
    let folder = folder_for(&config.workspace, bot)?;
    match runtimes.entry(bot.to_owned()) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let runtime = LocalRuntime::machine(folder).map_err(|error| error.to_string())?;
            Ok(entry.insert(runtime))
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

/// Returns the Space URL to pair with, refusing a missing or non-HTTP one.
fn space_url(explicit: Option<String>, saved: Option<String>) -> Result<String> {
    let Some(url) = explicit.or(saved) else {
        bail!("pass --space <url>, or set NEHA_SPACE_URL");
    };
    normalized_space_url(&url)
}

/// Returns a Space URL without a trailing slash, refusing a non-HTTP one.
fn normalized_space_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("the Space URL is empty");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("the Space URL must start with http:// or https://");
    }

    Ok(url.to_owned())
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
    Ok(home()?.join("workspace"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use neha::id::Id;
    use neha_computer::{ActionStatus, DeviceFrame, HostFrame, ToolOutput, WireAction};

    use super::{
        Config, answer, decode_bytes, encode_bytes, folder_for, home_from, normalized_space_url,
        secret, socket_url, space_url, write_receipt,
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
    fn a_pairing_needs_a_space_url_and_prefers_the_one_given() {
        let error = space_url(None, None).unwrap_err().to_string();
        assert!(error.contains("NEHA_SPACE_URL"), "{error}");
        assert_eq!(
            space_url(None, Some("http://127.0.0.1:7337/".to_owned())).unwrap(),
            "http://127.0.0.1:7337"
        );
        assert_eq!(
            space_url(
                Some("https://space.example".to_owned()),
                Some("http://127.0.0.1:7337".to_owned())
            )
            .unwrap(),
            "https://space.example"
        );
        let error = space_url(Some("space.example".to_owned()), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("http://"), "{error}");
    }

    #[test]
    fn a_space_url_is_trimmed_and_must_be_http() {
        assert_eq!(
            normalized_space_url(" https://space.example/ ").unwrap(),
            "https://space.example"
        );
        assert!(normalized_space_url("").is_err());
        assert!(normalized_space_url("ftp://space.example").is_err());
    }

    #[test]
    fn the_channel_speaks_ws_or_wss_to_the_space() {
        assert_eq!(
            socket_url("http://127.0.0.1:7337/").unwrap(),
            "ws://127.0.0.1:7337/api/computer/socket"
        );
        assert_eq!(
            socket_url("https://space.example").unwrap(),
            "wss://space.example/api/computer/socket"
        );
        assert!(socket_url("ftp://space.example").is_err());
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
        let mut runtimes = HashMap::new();

        assert_eq!(
            answer(&config, &mut runtimes, HostFrame::Ping).await,
            DeviceFrame::Pong
        );
        assert!(runtimes.is_empty());
    }

    #[tokio::test]
    async fn an_action_for_a_bot_that_is_not_an_identifier_is_refused() {
        let config = config_at(PathBuf::from("/home/ada/.neha/workspace"));
        let mut runtimes = HashMap::new();

        let answer = answer(
            &config,
            &mut runtimes,
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
        assert!(runtimes.is_empty());
    }
}
