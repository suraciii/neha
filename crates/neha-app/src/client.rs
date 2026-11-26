//! The CLI's connection to one Space.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use neha::id::Id;
use neha::surface::{ClientFrame, CommandCall, PROTOCOL_VERSION, ServerFrame};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The account the CLI acts as.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedIn {
    /// The account identifier.
    pub id: Id,
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// What the CLI remembers between runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credential {
    /// The Space's base URL.
    pub url: String,
    /// The device token.
    pub token: String,
    /// The account.
    pub account: SignedIn,
}

impl Credential {
    /// Loads the saved credential.
    pub fn load() -> Result<Self> {
        let path = credential_path();
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "not signed in: {} is missing; run `neha join <invite link>`",
                path.display()
            )
        })?;
        serde_json::from_slice(&bytes).context("the saved credential is corrupt")
    }

    /// Saves the credential for later commands.
    ///
    /// The file is created 0600 and replaces the old one atomically, so a
    /// reader never sees a partly written or world-readable credential.
    pub fn save(&self) -> Result<()> {
        let path = credential_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            restrict(parent, 0o700)?;
        }
        let bytes = serde_json::to_vec(self)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        {
            use std::io::Write as _;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        drop(file);
        fs::rename(&temporary, &path)
            .with_context(|| format!("cannot move the credential to {}", path.display()))?;
        restrict(&path, 0o600)?;

        Ok(())
    }
}

fn credential_path() -> PathBuf {
    if let Some(path) = std::env::var_os("NEHA_CREDENTIAL") {
        return PathBuf::from(path);
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config).join("neha").join("credential.json");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();

    PathBuf::from(home)
        .join(".config")
        .join("neha")
        .join("credential.json")
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot restrict {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Splits an invitation link into its Space URL and its secret.
pub fn split_invite(link: &str) -> Result<(String, String)> {
    let (base, secret) = link
        .split_once('#')
        .context("the invitation link has no secret; copy the whole link")?;
    let (scheme, rest) = base
        .split_once("://")
        .context("the invitation link must start with http://")?;
    if scheme != "http" && scheme != "https" {
        bail!("the invitation link must start with http://");
    }
    let authority = rest
        .split('/')
        .next()
        .context("the invitation link has no host")?;
    if secret.is_empty() {
        bail!("the invitation link has an empty secret");
    }

    Ok((format!("{scheme}://{authority}"), secret.to_owned()))
}

/// Joins a Space and returns its credential.
pub async fn join(link: &str, display_name: &str, password: &str) -> Result<Credential> {
    let (url, secret) = split_invite(link)?;
    let session = session(reqwest::Client::new().post(format!("{url}/api/join")).json(
        &serde_json::json!({
            "secret": secret,
            "display_name": display_name,
            "password": password,
        }),
    ))
    .await?;

    Ok(Credential {
        url,
        token: session.0,
        account: session.1,
    })
}

/// Signs in to a Space and returns its credential.
pub async fn login(url: &str, display_name: &str, password: &str) -> Result<Credential> {
    let url = url.trim_end_matches('/').to_owned();
    let session = session(
        reqwest::Client::new()
            .post(format!("{url}/api/login"))
            .json(&serde_json::json!({
                "display_name": display_name,
                "password": password,
            })),
    )
    .await?;

    Ok(Credential {
        url,
        token: session.0,
        account: session.1,
    })
}

async fn session(request: reqwest::RequestBuilder) -> Result<(String, SignedIn)> {
    let response = request
        .send()
        .await
        .context("the Space could not be reached")?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    if !status.is_success() {
        let code = body["code"].as_str().unwrap_or("error");
        let message = body["message"].as_str().unwrap_or("no message");
        bail!("the Space refused ({code}): {message}");
    }
    let token = body["token"]
        .as_str()
        .context("the Space's answer has no token")?
        .to_owned();
    let account: SignedIn = serde_json::from_value(body["account"].clone())
        .context("the Space's account does not fit")?;

    Ok((token, account))
}

/// Runs one command and returns its result, reporting live events.
pub async fn command_following(
    credential: &Credential,
    command: CommandCall,
    mut on_event: impl FnMut(&neha::record::Event),
) -> Result<Value> {
    let mut socket = connect(credential).await?;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Subscribe {
                id: Id::new(),
                cursor: None,
                limit: None,
            })?
            .into(),
        ))
        .await
        .context("cannot subscribe to the record")?;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Command {
                id: Id::new(),
                command,
            })?
            .into(),
        ))
        .await
        .context("cannot send the command")?;

    loop {
        let Some(message) = socket.next().await else {
            bail!("the Space closed the connection");
        };
        let message = message.context("the Space's stream failed")?;
        let Message::Text(text) = message else {
            continue;
        };
        match serde_json::from_str::<ServerFrame>(text.as_str()) {
            Ok(ServerFrame::Confirmed { result, .. }) => return Ok(result),
            Ok(ServerFrame::Error { code, message, .. }) => {
                bail!("the Space refused ({code}): {message}");
            }
            Ok(ServerFrame::ProtocolError { message }) => {
                bail!("the Space could not read the command: {message}");
            }
            Ok(ServerFrame::Event { event }) => on_event(&event),
            Ok(
                ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. }
                | ServerFrame::Snapshot { .. }
                | ServerFrame::Hello { .. }
                | ServerFrame::Heartbeat { .. },
            ) => {}
            Err(_) => bail!("the Space sent a frame this CLI does not understand"),
        }
    }
}

/// Runs one command and returns its result.
pub async fn command(credential: &Credential, command: CommandCall) -> Result<Value> {
    let mut socket = connect(credential).await?;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Command {
                id: Id::new(),
                command,
            })?
            .into(),
        ))
        .await
        .context("cannot send the command")?;

    loop {
        let Some(message) = socket.next().await else {
            bail!("the Space closed the connection");
        };
        let message = message.context("the Space's stream failed")?;
        let Message::Text(text) = message else {
            continue;
        };
        match serde_json::from_str::<ServerFrame>(text.as_str()) {
            Ok(ServerFrame::Confirmed { result, .. }) => return Ok(result),
            Ok(ServerFrame::Error { code, message, .. }) => {
                bail!("the Space refused ({code}): {message}");
            }
            Ok(ServerFrame::ProtocolError { message }) => {
                bail!("the Space could not read the command: {message}");
            }
            Ok(
                ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. }
                | ServerFrame::Hello { .. }
                | ServerFrame::Heartbeat { .. },
            ) => {}
            Err(_) => bail!("the Space sent a frame this CLI does not understand"),
        }
    }
}

async fn connect(credential: &Credential) -> Result<Socket> {
    let url = credential
        .url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let mut request = format!("{url}/api/socket")
        .into_client_request()
        .context("the Space URL is not usable")?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", credential.token)
            .parse()
            .context("the token is not usable")?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("the Space could not be reached")?;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capabilities: Vec::new(),
            })?
            .into(),
        ))
        .await
        .context("cannot greet the Space")?;
    let Some(message) = socket.next().await else {
        bail!("the Space closed the connection");
    };
    let message = message.context("the Space's stream failed")?;
    let Message::Text(text) = message else {
        bail!("the Space did not greet the CLI");
    };
    match serde_json::from_str::<ServerFrame>(text.as_str()) {
        Ok(ServerFrame::Hello { version, .. }) if version == PROTOCOL_VERSION => Ok(socket),
        Ok(ServerFrame::ProtocolError { message }) => {
            bail!("the Space disagreed on the protocol: {message}");
        }
        _ => bail!("the Space did not greet the CLI"),
    }
}
