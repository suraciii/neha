//! The CLI's connection to one Space.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use neha::file::MAX_FILE_BYTES;
use neha::id::Id;
use neha::surface::{ClientFrame, CommandCall, FileSummary, PROTOCOL_VERSION, ServerFrame};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the CLI asks a Space how a run is doing.
const RUN_POLL: Duration = Duration::from_millis(200);
/// How long the CLI waits for a run to end.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
/// The token a run's shell uses to reach its Space.
const BOT_TOKEN_ENV: &str = "NEHA_BOT_TOKEN";
/// The Space address a run's shell uses.
const BOT_SPACE_ENV: &str = "NEHA_SPACE_URL";
/// The Chat a run works in.
const BOT_CHAT_ENV: &str = "NEHA_CHAT";

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
        // One resolver for the CLI and the daemon's hidden roots: a relative
        // `XDG_CONFIG_HOME` must not put the session under the working
        // directory, where a Bot can read it. A legacy copy is moved or
        // removed first; if that cleanup fails, signing in is refused.
        crate::computer::migrate_legacy_credential()?;
        let path = crate::computer::credential_path()?;

        Self::load_from(&path)
    }

    /// Loads a credential from one resolved path.
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| {
            format!(
                "not signed in: {} is missing; run `neha join <invite link>`",
                path.display()
            )
        })?;
        serde_json::from_slice(&bytes).context("the saved credential is corrupt")
    }

    /// Saves the credential for later commands.
    pub fn save(&self) -> Result<()> {
        // Saving a new session also clears any old cwd-relative copy, so the
        // CLI never leaves a readable session behind.
        crate::computer::migrate_legacy_credential()?;
        write_private(
            &crate::computer::credential_path()?,
            &serde_json::to_vec(self)?,
        )
    }
}

/// Writes a secret file only this machine user can read, atomically.
///
/// The directory is made 0700 and the file 0600, and the file moves into place
/// whole, so a reader never sees a partly written or world-readable secret.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        restrict(parent, 0o700)?;
    }
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
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot move the file to {}", path.display()))?;
    restrict(path, 0o600)?;

    Ok(())
}

#[cfg(unix)]
pub(crate) fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot restrict {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn restrict(_path: &Path, _mode: u32) -> Result<()> {
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
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    let token = body["token"]
        .as_str()
        .context("the Space's answer has no token")?
        .to_owned();
    let account: SignedIn = serde_json::from_value(body["account"].clone())
        .context("the Space's account does not fit")?;

    Ok((token, account))
}

/// Returns the answer when the Space accepted the request, or names why it refused.
async fn accepted(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let code = body["code"].as_str().unwrap_or("error");
    let message = body["message"]
        .as_str()
        .or(status.canonical_reason())
        .unwrap_or("no message");
    bail!("the Space refused ({code}): {message}");
}

/// The pairing a Space gives a Local Computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedComputer {
    /// The Computer the Space created for this machine.
    pub computer: String,
    /// The credential this machine serves the Space with.
    pub credential: String,
}

/// Returns the HTTP client for local pairing exchanges.
///
/// Both exchanges carry secrets: the one-use ticket, and the machine's new
/// credential. A redirect would hand them to whatever origin the answer names,
/// so any redirect is a refusal. This milestone pairs only with a Space on this
/// machine, so a system proxy must not see the exchange either.
fn local_pairing_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("cannot build the pairing HTTP client")
}

/// Asks a signed-in Space for one local pairing ticket.
///
/// The ticket is normally in the file `neha serve` wrote. This path covers a
/// Space that started without an Admin: once a User has signed in, the CLI can
/// ask the Space for the ticket itself.
pub async fn request_ticket(space_url: &str, token: &str) -> Result<String> {
    let body: Value = accepted(
        local_pairing_client()?
            .post(ticket_url(space_url))
            .bearer_auth(token)
            .send()
            .await
            .context("the Space could not be reached")?,
    )
    .await?
    .json()
    .await
    .context("the Space did not answer with JSON")?;

    text_field(&body, "ticket")
}

/// Exchanges one local ticket for this machine's Computer and credential.
pub async fn pair_with_ticket(space_url: &str, ticket: &str) -> Result<PairedComputer> {
    let body: Value = accepted(
        local_pairing_client()?
            .post(pair_url(space_url))
            .json(&pair_body(ticket))
            .send()
            .await
            .context("the Space could not be reached")?,
    )
    .await?
    .json()
    .await
    .context("the Space did not answer with JSON")?;

    Ok(PairedComputer {
        computer: text_field(&body, "computer")?,
        credential: text_field(&body, "credential")?,
    })
}

/// The URL a local ticket is issued at.
fn ticket_url(space_url: &str) -> String {
    format!("{}/api/computer/ticket", space_url.trim_end_matches('/'))
}

/// The URL a local ticket is exchanged at.
fn pair_url(space_url: &str) -> String {
    format!("{}/api/computer/pair", space_url.trim_end_matches('/'))
}

/// The body a machine pairs with: the one-use ticket and nothing else.
fn pair_body(ticket: &str) -> Value {
    serde_json::json!({"ticket": ticket})
}

/// Reads a string field out of a Space's answer.
fn text_field(value: &Value, field: &str) -> Result<String> {
    Ok(value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("the Space's answer has no {field}"))?
        .to_owned())
}

/// The short-lived session a Bot's shell has instead of a User credential.
#[derive(Clone, Debug)]
pub struct BotSession {
    url: String,
    token: String,
    chat: Option<Id>,
}

/// What a Bot's shell posted to its Chat.
pub struct BotPost {
    /// The Message the Space recorded.
    pub message: Id,
    /// The File the Message carries, when it carries one.
    pub file: Option<FileSummary>,
}

impl BotSession {
    /// Reads the environment a Bot's run injects, when this is such a shell.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(token) = std::env::var_os(BOT_TOKEN_ENV).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let token = token
            .to_str()
            .context("NEHA_BOT_TOKEN is not valid UTF-8")?
            .to_owned();
        let url = std::env::var(BOT_SPACE_ENV)
            .context("NEHA_SPACE_URL is not set; a Bot's shell needs its Space address")?
            .trim_end_matches('/')
            .to_owned();
        let chat = match std::env::var(BOT_CHAT_ENV) {
            Ok(text) if !text.trim().is_empty() => Some(
                text.trim()
                    .parse()
                    .map_err(|error| anyhow::anyhow!("NEHA_CHAT is not a Chat id: {error}"))?,
            ),
            _ => None,
        };

        Ok(Some(Self { url, token, chat }))
    }

    /// Returns the Chat to act in, which must be the token's Chat.
    pub fn chat(&self, chat: Option<Id>) -> Result<Id> {
        match (chat, self.chat) {
            (Some(chat), Some(bound)) if chat != bound => {
                bail!("this Bot's token is bound to Chat {bound}; it cannot act in another Chat")
            }
            (Some(chat), _) => Ok(chat),
            (None, Some(chat)) => Ok(chat),
            (None, None) => bail!("pass --chat; a Bot's shell does not name its Chat"),
        }
    }

    /// Uploads one File into the token's Chat.
    pub async fn upload(&self, path: &Path) -> Result<FileSummary> {
        let bytes = read_for_upload(path)?;
        let name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .with_context(|| format!("{} has no usable file name", path.display()))?;
        let response = reqwest::Client::new()
            .post(format!("{}/api/bot/files", self.url))
            .query(&[
                ("name", name),
                (
                    "media_type",
                    neha::file::media_type_for(&path.to_string_lossy()),
                ),
            ])
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .bearer_auth(&self.token)
            .body(bytes)
            .send()
            .await
            .context("the Space could not be reached")?;

        accepted(response)
            .await?
            .json()
            .await
            .context("the Space's answer does not describe a File")
    }

    /// Sends a Message, with an optional File, as the Bot.
    pub async fn send(
        &self,
        chat: Id,
        text: String,
        file: Option<&FileSummary>,
    ) -> Result<BotPost> {
        let mut body = serde_json::json!({"chat": chat, "text": text});
        if let Some(file) = file {
            body["file"] = serde_json::json!(file.id);
        }
        let response = accepted(
            reqwest::Client::new()
                .post(format!("{}/api/bot/messages", self.url))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .context("the Space could not be reached")?,
        )
        .await?;
        let value = response
            .json::<Value>()
            .await
            .context("the Space's answer does not describe a Message")?;
        let message = value["message"]
            .as_str()
            .and_then(|text| text.parse().ok())
            .context("the Space's answer has no Message id")?;

        Ok(BotPost {
            message,
            file: file.cloned(),
        })
    }

    /// Downloads one File of the token's Chat.
    pub async fn download(&self, file: Id) -> Result<(Option<String>, Vec<u8>)> {
        let response = accepted(
            reqwest::Client::new()
                .get(format!("{}/api/bot/files/{file}", self.url))
                .bearer_auth(&self.token)
                .send()
                .await
                .context("the Space could not be reached")?,
        )
        .await?;
        let name = response
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(disposition_name);
        let bytes = response
            .bytes()
            .await
            .context("the Space's answer was cut short")?;

        Ok((name, bytes.to_vec()))
    }
}

/// Uploads one File to a Chat and returns what the Space recorded.
///
/// The size is checked before anything leaves this machine, so a File the
/// Space cannot hold never starts an upload.
pub async fn upload_file(credential: &Credential, chat: Id, path: &Path) -> Result<FileSummary> {
    let bytes = read_for_upload(path)?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .with_context(|| format!("{} has no usable file name", path.display()))?;
    let response = reqwest::Client::new()
        .post(format!("{}/api/chats/{chat}/files", credential.url))
        .query(&[
            ("name", name),
            (
                "media_type",
                neha::file::media_type_for(&path.to_string_lossy()),
            ),
        ])
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .bearer_auth(&credential.token)
        .body(bytes)
        .send()
        .await
        .context("the Space could not be reached")?;
    accepted(response)
        .await?
        .json()
        .await
        .context("the Space's answer does not describe a File")
}

/// Downloads one File and returns the name the Space gave it and its bytes.
///
/// The name is only a suggestion for a destination: the caller decides
/// whether it needs one, and an explicit path ignores it.
pub async fn download_file(credential: &Credential, file: Id) -> Result<(Option<String>, Vec<u8>)> {
    let response = accepted(
        reqwest::Client::new()
            .get(format!("{}/api/files/{file}", credential.url))
            .bearer_auth(&credential.token)
            .send()
            .await
            .context("the Space could not be reached")?,
    )
    .await?;
    let name = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(disposition_name);
    let bytes = response
        .bytes()
        .await
        .context("the Space's answer was cut short")?;

    Ok((name, bytes.to_vec()))
}

/// Reads a File for upload, refusing one larger than the Space holds.
fn read_for_upload(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a file", path.display());
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!(
            "{} is {} bytes; a File can be at most {MAX_FILE_BYTES} bytes",
            path.display(),
            metadata.len()
        );
    }
    fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

/// Reads the File name out of a `filename*=UTF-8''…` disposition parameter.
fn disposition_name(disposition: &str) -> Option<String> {
    let parameter = disposition.split(';').find(|part| {
        part.trim_start()
            .get(..9)
            .is_some_and(|name| name.eq_ignore_ascii_case("filename*"))
    })?;
    let encoded = parameter.split_once('=')?.1.trim();
    let (_charset, encoded) = encoded.split_once('\'')?;
    let (_language, encoded) = encoded.split_once('\'')?;
    percent_decode(encoded)
}

/// Decodes RFC 3986 percent-escapes into UTF-8 text.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let digits = bytes.get(index + 1..index + 3)?;
            let high = (digits[0] as char).to_digit(16)?;
            let low = (digits[1] as char).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Polls one run until it ends or waits for an approval, reporting each view,
/// and returns the last one.
///
/// The Space confirms the command that starts a run when it queues the run,
/// so the run's result comes from the record, not from that command.
pub async fn wait_for_run(
    credential: &Credential,
    run: Id,
    on_view: impl FnMut(&Value),
) -> Result<Value> {
    wait_for_run_within(credential, run, RUN_TIMEOUT, on_view).await
}

/// Polls one run for at most `timeout`, and reports each view on the way.
///
/// The whole wait has one deadline: the connection, every frame, and every
/// pause between polls share the time that is left.
async fn wait_for_run_within(
    credential: &Credential,
    run: Id,
    timeout: Duration,
    mut on_view: impl FnMut(&Value),
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut socket = tokio::time::timeout(left(deadline, timeout)?, connect(credential))
        .await
        .map_err(|_| run_timeout(timeout))??;
    loop {
        let view = tokio::time::timeout(
            left(deadline, timeout)?,
            command_on(&mut socket, CommandCall::RunShow { run }),
        )
        .await
        .map_err(|_| run_timeout(timeout))??;
        on_view(&view);
        // A waiting run has not ended; the caller answers the approval and
        // asks again. It is reported like a terminal view so the wait does
        // not turn a decision into a timeout.
        if matches!(
            view["status"].as_str(),
            Some("succeeded" | "failed" | "waiting")
        ) {
            return Ok(view);
        }
        tokio::time::sleep(RUN_POLL.min(left(deadline, timeout)?)).await;
    }
}

/// Returns the time left before the deadline, or the error when it passed.
fn left(deadline: tokio::time::Instant, timeout: Duration) -> Result<Duration> {
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    if left.is_zero() {
        return Err(run_timeout(timeout));
    }

    Ok(left)
}

/// The error a run that outlasts the wait produces.
fn run_timeout(timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!("the run did not end within {timeout:?}")
}

/// Runs one command on an open connection and returns its result.
async fn command_on(socket: &mut Socket, command: CommandCall) -> Result<Value> {
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

/// Runs one command and returns its result.
pub async fn command(credential: &Credential, command: CommandCall) -> Result<Value> {
    let mut socket = connect(credential).await?;
    command_on(&mut socket, command).await
}

/// Maps the Space's HTTP origin to the command socket's scheme.
fn socket_url(url: &str) -> String {
    url.replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1)
}

async fn connect(credential: &Credential) -> Result<Socket> {
    let url = socket_url(&credential.url);
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

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    //! Fixtures and accessors the integration tests need from inside the client.

    use std::path::Path;
    use std::time::Duration;

    use neha::id::Id;
    use serde_json::Value;

    use super::Credential;

    /// Maps a Space's HTTP origin to the command socket's scheme.
    pub fn socket_url(url: &str) -> String {
        super::socket_url(url)
    }

    /// The URL a local ticket is issued at.
    pub fn ticket_url(space_url: &str) -> String {
        super::ticket_url(space_url)
    }

    /// The URL a local ticket is exchanged at.
    pub fn pair_url(space_url: &str) -> String {
        super::pair_url(space_url)
    }

    /// The body a machine pairs with.
    pub fn pair_body(ticket: &str) -> Value {
        super::pair_body(ticket)
    }

    /// Reads the File name out of a `filename*=UTF-8''…` disposition parameter.
    pub fn disposition_name(disposition: &str) -> Option<String> {
        super::disposition_name(disposition)
    }

    /// Reads a File for upload, refusing one larger than the Space holds.
    pub fn read_for_upload(path: &Path) -> anyhow::Result<Vec<u8>> {
        super::read_for_upload(path)
    }

    /// Polls one run for at most `timeout`, reporting each view on the way.
    pub async fn wait_for_run_within(
        credential: &Credential,
        run: Id,
        timeout: Duration,
        on_view: impl FnMut(&Value),
    ) -> anyhow::Result<Value> {
        super::wait_for_run_within(credential, run, timeout, on_view).await
    }

    /// Loads the credential saved at one path.
    pub fn load_credential_from(path: &Path) -> anyhow::Result<Credential> {
        Credential::load_from(path)
    }

    /// Writes a secret this machine user alone may read.
    pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        super::write_private(path, bytes)
    }
}
