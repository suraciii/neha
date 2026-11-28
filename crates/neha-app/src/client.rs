//! The CLI's connection to one Space.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt as _, StreamExt as _};
use neha::file::MAX_FILE_BYTES;
use neha::id::Id;
use neha::surface::{ClientFrame, CommandCall, FileSummary, PROTOCOL_VERSION, ServerFrame};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
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
/// The public Host a Bot's shell sends when an internal origin is not the
/// authority the Space's request guard expects.
const BOT_HOST_ENV: &str = "NEHA_BOT_HOST";

/// The account the CLI acts as.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedIn {
    /// The account identifier.
    pub id: Id,
    /// The display name.
    pub display_name: String,
    /// Whether the account administers the Space.
    pub is_admin: bool,
    /// The role the account holds; a credential saved before roles existed
    /// reads `None`, and its Admin flag decides.
    #[serde(default)]
    pub role: Option<neha::role::Role>,
}

impl SignedIn {
    /// Returns the role the account holds in the Space.
    #[must_use]
    pub fn role(&self) -> neha::role::Role {
        self.role.unwrap_or({
            if self.is_admin {
                neha::role::Role::Owner
            } else {
                neha::role::Role::Member
            }
        })
    }
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

/// Returns the account the Space currently signs this credential in as.
///
/// A role can change while the CLI keeps its credential, so whoami asks the
/// Space instead of trusting the account saved at sign-in.
pub async fn me(credential: &Credential) -> Result<SignedIn> {
    let url = credential.url.trim_end_matches('/');
    let response = reqwest::Client::new()
        .get(format!("{url}/api/me"))
        .bearer_auth(&credential.token)
        .send()
        .await
        .context("the Space could not be reached")?;
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;

    serde_json::from_value(body).context("the Space's account does not fit")
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

/// The device login a Space gives a CLI without a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedDeviceLogin {
    /// The device identifier the CLI polls with.
    pub device: String,
    /// The code a signed-in User enters.
    pub code: String,
    /// How long the code stays valid.
    pub expires_in: u64,
    /// How often the CLI should poll.
    pub interval: u64,
}

/// What a device-login poll answers.
#[derive(Clone, Debug)]
pub enum DeviceLoginPoll {
    /// No User approved the CLI yet.
    Pending,
    /// A User approved the CLI; its own token is now a session.
    Approved {
        /// The account that approved the terminal.
        account: SignedIn,
    },
    /// The Space no longer knows the login: it expired or was replaced.
    Gone,
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

/// A pairing a Space started for a machine that printed a code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedPairing {
    /// The identifier the machine polls with.
    pub device: String,
    /// The code a signed-in User enters.
    pub code: String,
    /// How long the Space keeps the code alive.
    pub expires_in: u64,
    /// How often the machine should poll, in seconds.
    pub interval: u64,
}

/// What a machine learns when it polls a remote pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingPoll {
    /// No User has approved the machine yet.
    Pending,
    /// A User approved the machine and the Space recorded its Computer.
    Approved {
        /// The Computer the machine now controls.
        computer: String,
    },
    /// The Space no longer knows this pairing: it expired or was replaced.
    Gone,
}

/// The HTTP client for a Space reached over the network.
///
/// The start and poll bodies carry secrets; a redirect would hand them to
/// whatever origin the answer names. The system's certificate store is the
/// only trust anchor, so a self-signed Space is refused.
fn remote_pairing_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("cannot build the pairing HTTP client")
}

/// Starts a remote pairing and returns the code to show the User.
pub async fn start_pairing(
    space_url: &str,
    machine: &str,
    workspace: &str,
    token: &str,
    poll_secret: &str,
) -> Result<StartedPairing> {
    let response = remote_pairing_client()?
        .post(format!(
            "{}/api/computers/pair",
            space_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "machine": machine,
            "workspace": workspace,
            "token": token,
            "poll_secret": poll_secret,
        }))
        .send()
        .await
        .context("the Space could not be reached")?;
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    // A Space is not trusted to send a device id, a short code, or timing
    // values the machine then prints and waits on; each field has the shape
    // this Space issues, or the answer is refused.
    let expires_in = body["expires_in"].as_u64().unwrap_or(900).clamp(1, 3600);
    let interval = body["interval"].as_u64().unwrap_or(2).clamp(1, 60);

    Ok(StartedPairing {
        device: pairing_device(&text_field(&body, "device")?)?,
        code: pairing_code(&text_field(&body, "code")?)?,
        expires_in,
        interval,
    })
}

/// Polls one remote pairing. A 404 means the Space forgot the pairing.
pub async fn poll_pairing(space_url: &str, device: &str, poll_secret: &str) -> Result<PairingPoll> {
    let response = remote_pairing_client()?
        .post(format!(
            "{}/api/computers/pair/{device}",
            space_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({"secret": poll_secret}))
        .send()
        .await
        .context("the Space could not be reached")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(PairingPoll::Gone);
    }
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    match body["state"].as_str() {
        Some("pending") => Ok(PairingPoll::Pending),
        Some("approved") => Ok(PairingPoll::Approved {
            computer: pairing_computer(&text_field(&body, "computer")?)?,
        }),
        other => bail!("the Space answered a pairing poll with {other:?}"),
    }
}

/// Returns the base64url SHA-256 digest of a secret sent at device-login start.
fn secret_hash(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

/// Returns a fresh 32-byte URL-safe base64 secret.
pub(crate) fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Starts a CLI device login and returns the code to show the User.
pub async fn start_device_login(
    space_url: &str,
    client: &str,
    token: &str,
    poll_secret: &str,
) -> Result<StartedDeviceLogin> {
    let response = remote_pairing_client()?
        .post(format!(
            "{}/api/devices/login",
            space_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "client": client,
            "token_hash": secret_hash(token),
            "poll_hash": secret_hash(poll_secret),
        }))
        .send()
        .await
        .context("the Space could not be reached")?;
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    let expires_in = body["expires_in"].as_u64().unwrap_or(900).clamp(1, 3600);
    let interval = body["interval"].as_u64().unwrap_or(2).clamp(1, 60);

    Ok(StartedDeviceLogin {
        device: pairing_device(&text_field(&body, "device")?)?,
        code: pairing_code(&text_field(&body, "code")?)?,
        expires_in,
        interval,
    })
}

/// Polls one CLI device login. A 404 means the login expired or was replaced.
pub async fn poll_device_login(
    space_url: &str,
    device: &str,
    poll_secret: &str,
) -> Result<DeviceLoginPoll> {
    let response = remote_pairing_client()?
        .post(format!(
            "{}/api/devices/login/{device}",
            space_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({"secret": poll_secret}))
        .send()
        .await
        .context("the Space could not be reached")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(DeviceLoginPoll::Gone);
    }
    let body: Value = accepted(response)
        .await?
        .json()
        .await
        .context("the Space did not answer with JSON")?;
    match body["state"].as_str() {
        Some("pending") => Ok(DeviceLoginPoll::Pending),
        Some("approved") => Ok(DeviceLoginPoll::Approved {
            account: serde_json::from_value(body["account"].clone())
                .context("the approved account does not fit")?,
        }),
        other => bail!("the Space answered a device-login poll with {other:?}"),
    }
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

/// Returns a device identifier this Space could have issued.
fn pairing_device(value: &str) -> Result<String> {
    if value.len() == 22
        && URL_SAFE_NO_PAD
            .decode(value)
            .is_ok_and(|bytes| bytes.len() == 16)
    {
        return Ok(value.to_owned());
    }

    bail!("the Space answered with an unusable pairing device")
}

/// The characters a pairing code may contain.
///
/// This is the Space's alphabet: base32 without `I`, `O`, `0`, and `1`; it
/// keeps `8` and `9`. Validation must accept every code the Space issues, or
/// a valid pairing would be discarded before it is shown.
const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Returns an eight-letter pairing code this Space could have issued.
fn pairing_code(value: &str) -> Result<String> {
    if value.len() == 8
        && value
            .bytes()
            .all(|byte| PAIRING_CODE_ALPHABET.contains(&byte))
    {
        return Ok(value.to_owned());
    }

    bail!("the Space answered with an unusable pairing code")
}

/// Returns the Computer identifier the Space recorded for this machine.
fn pairing_computer(value: &str) -> Result<String> {
    value
        .parse::<Id>()
        .map(|_| value.to_owned())
        .context("the Space answered with an unusable Computer identifier")
}

/// The short-lived session a Bot's shell has instead of a User credential.
#[derive(Clone, Debug)]
pub struct BotSession {
    url: String,
    token: String,
    chat: Option<Id>,
    /// The public Host an internal Space origin needs.
    host: Option<String>,
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
        let host = std::env::var(BOT_HOST_ENV)
            .ok()
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty());

        Ok(Some(Self {
            url,
            token,
            chat,
            host,
        }))
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

    /// Builds one Bot request, naming the public Host when the deployment
    /// gives an internal origin.
    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let mut request = reqwest::Client::new().request(method, url);
        if let Some(host) = &self.host {
            request = request.header(reqwest::header::HOST, host);
        }
        request
    }

    /// Uploads one File into the token's Chat.
    pub async fn upload(&self, path: &Path) -> Result<FileSummary> {
        let bytes = read_for_upload(path)?;
        let name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .with_context(|| format!("{} has no usable file name", path.display()))?;
        let response = self
            .request(reqwest::Method::POST, format!("{}/api/bot/files", self.url))
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
            self.request(
                reqwest::Method::POST,
                format!("{}/api/bot/messages", self.url),
            )
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
            self.request(
                reqwest::Method::GET,
                format!("{}/api/bot/files/{file}", self.url),
            )
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

    /// Checks one start-response pairing code against the Space's alphabet.
    pub fn pairing_code(value: &str) -> anyhow::Result<String> {
        super::pairing_code(value)
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

#[cfg(test)]
mod tests {
    use super::BotSession;

    #[tokio::test]
    async fn a_device_login_start_sends_digests_not_the_secrets() {
        use std::sync::{Arc, Mutex};

        use axum::extract::Json as AxumJson;
        use axum::routing::post;
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let captured = Arc::new(Mutex::new(None::<serde_json::Value>));
        let handler = {
            let captured = Arc::clone(&captured);
            move |AxumJson(body): AxumJson<serde_json::Value>| {
                *captured.lock().expect("the captured body") = Some(body);
                async {
                    AxumJson(serde_json::json!({
                        "device": URL_SAFE_NO_PAD.encode([1_u8; 16]),
                        "code": "ABCD2345",
                        "expires_in": 900,
                        "interval": 2,
                    }))
                }
            }
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/api/devices/login", post(handler)),
            )
            .await
            .unwrap();
        });

        let started = super::start_device_login(
            &format!("http://{address}"),
            "laptop",
            "cli-token",
            "poll-secret",
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(started.code, "ABCD2345");
        let body = captured
            .lock()
            .expect("the captured body")
            .clone()
            .expect("the start body");
        assert_eq!(body["token_hash"], super::secret_hash("cli-token"));
        assert_eq!(body["poll_hash"], super::secret_hash("poll-secret"));
        assert!(
            body.get("token").is_none(),
            "the bearer never leaves the terminal"
        );
        assert!(body.get("poll_secret").is_none());
        assert_ne!(body["token_hash"], "cli-token");
    }

    #[test]
    fn a_bot_request_names_the_public_host_when_the_deployment_gives_one() {
        let session = BotSession {
            url: "http://space:7337".to_owned(),
            token: "bot-token".to_owned(),
            chat: None,
            host: Some("neha.example.com".to_owned()),
        };
        let request = session
            .request(
                reqwest::Method::GET,
                format!(
                    "{}/api/bot/files/00000000-0000-0000-0000-000000000000",
                    session.url
                ),
            )
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::HOST)
                .and_then(|value| value.to_str().ok()),
            Some("neha.example.com")
        );
    }

    #[test]
    fn a_bot_request_keeps_the_origin_host_without_an_override() {
        let session = BotSession {
            url: "http://127.0.0.1:7337".to_owned(),
            token: "bot-token".to_owned(),
            chat: None,
            host: None,
        };
        let request = session
            .request(
                reqwest::Method::GET,
                format!(
                    "{}/api/bot/files/00000000-0000-0000-0000-000000000000",
                    session.url
                ),
            )
            .build()
            .unwrap();

        assert!(request.headers().get(reqwest::header::HOST).is_none());
    }
}
