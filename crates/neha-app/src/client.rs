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
        // directory, where a Bot can read it.
        let path = crate::computer::credential_path();
        crate::computer::migrate_legacy_credential(&path)?;
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "not signed in: {} is missing; run `neha join <invite link>`",
                path.display()
            )
        })?;
        serde_json::from_slice(&bytes).context("the saved credential is corrupt")
    }

    /// Saves the credential for later commands.
    pub fn save(&self) -> Result<()> {
        write_private(
            &crate::computer::credential_path(),
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

/// Polls one run until it ends, reports each view, and returns the last one.
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
        if matches!(view["status"].as_str(), Some("succeeded" | "failed")) {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::file::MAX_FILE_BYTES;
    use neha::id::Id;
    use neha::surface::PROTOCOL_VERSION;
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        Credential, PairedComputer, SignedIn, disposition_name, pair_body, pair_url,
        pair_with_ticket, read_for_upload, request_ticket, ticket_url, wait_for_run_within,
    };

    /// The CLI's credential for a Space at this address.
    fn credential_at(address: std::net::SocketAddr) -> Credential {
        Credential {
            url: format!("http://{address}"),
            token: "token".to_owned(),
            account: SignedIn {
                id: Id::new(),
                display_name: "Ada".to_owned(),
                is_admin: false,
            },
        }
    }

    /// Waits for one run against a Space that never answers, and returns the
    /// error the wait produced.
    async fn timed_out_run_wait(credential: &Credential) -> anyhow::Error {
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_run_within(credential, Id::new(), Duration::from_millis(300), |_| {}),
        )
        .await
        .expect("the wait must end")
        .expect_err("a Space that never answers must end the wait")
    }

    /// Serves a Space that greets the CLI and then never answers a command.
    async fn silent_space(listener: tokio::net::TcpListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        loop {
            let Some(Ok(message)) = socket.next().await else {
                break;
            };
            let Message::Text(text) = message else {
                continue;
            };
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            if frame["type"] == "hello" {
                let greeting = serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION});
                socket
                    .send(Message::Text(greeting.to_string().into()))
                    .await
                    .unwrap();
            }
        }
    }

    #[test]
    fn a_file_larger_than_the_limit_is_refused_before_upload() {
        let directory = tempfile::tempdir().unwrap();
        let big = directory.path().join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        std::fs::write(directory.path().join("small.txt"), "hi").unwrap();

        let error = read_for_upload(&big).unwrap_err().to_string();
        assert!(error.contains("at most"), "{error}");
        assert_eq!(
            read_for_upload(&directory.path().join("small.txt")).unwrap(),
            b"hi"
        );
        assert!(read_for_upload(directory.path()).is_err());
    }

    #[test]
    fn the_local_pairing_paths_and_body_are_the_pinned_ones() {
        assert_eq!(
            ticket_url("http://127.0.0.1:7337/"),
            "http://127.0.0.1:7337/api/computer/ticket"
        );
        assert_eq!(
            pair_url("http://127.0.0.1:7337/"),
            "http://127.0.0.1:7337/api/computer/pair"
        );
        assert_eq!(
            pair_body("ticket-a"),
            serde_json::json!({"ticket": "ticket-a"}),
            "the pairing body carries the ticket and nothing else"
        );
    }

    #[test]
    fn a_disposition_names_the_file() {
        assert_eq!(
            disposition_name("attachment; filename*=UTF-8''report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            disposition_name("attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A%20v2.txt").as_deref(),
            Some("报告 v2.txt")
        );
        assert_eq!(disposition_name("attachment").as_deref(), None);
        assert_eq!(
            disposition_name("attachment; filename*=UTF-8''%zz").as_deref(),
            None
        );
    }

    #[tokio::test]
    async fn a_space_that_never_answers_ends_the_run_wait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let space = tokio::spawn(silent_space(listener));

        let started = std::time::Instant::now();
        let error = timed_out_run_wait(&credential_at(address)).await;

        assert!(error.to_string().contains("did not end"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait took {:?}",
            started.elapsed()
        );
        space.abort();
    }

    #[tokio::test]
    async fn a_space_that_never_opens_ends_the_run_wait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let space = tokio::spawn(async move {
            // Accept the connection and never answer the opening handshake.
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let started = std::time::Instant::now();
        let error = timed_out_run_wait(&credential_at(address)).await;

        assert!(error.to_string().contains("did not end"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait took {:?}",
            started.elapsed()
        );
        space.abort();
    }

    /// Serves a 302 to `location` for every request.
    async fn redirecting_space(
        location: String,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::http::header::LOCATION;
        use axum::response::IntoResponse;
        use axum::routing::any;

        let app = Router::new().fallback(any(move || {
            let location = location.clone();
            async move { (StatusCode::FOUND, [(LOCATION, location)]).into_response() }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (address, task)
    }

    #[tokio::test]
    async fn pairing_requests_do_not_follow_redirects() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A redirect target that counts every connection it receives.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let target_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = target.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        let (address, space) = redirecting_space(format!("http://{target_address}")).await;
        let space_url = format!("http://{address}");

        let ticket = request_ticket(&space_url, "token").await;
        assert!(
            ticket.is_err(),
            "a redirecting ticket request must not be accepted"
        );
        let paired = pair_with_ticket(&space_url, "ticket-a").await;
        assert!(
            paired.is_err(),
            "a redirecting pairing must not be accepted"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "pairing requests must not follow a redirect"
        );

        space.abort();
        target_task.abort();
    }

    /// Runs only as the child half of `pairing_requests_bypass_a_system_proxy`.
    ///
    /// The parent starts a fake Space and points the proxy variables at a
    /// listener it counts; this child then pairs through the real client. It
    /// returns at once when the normal suite runs it.
    #[tokio::test]
    async fn a_local_pairing_client_child_pairs_around_the_proxy() {
        let Ok(space) = std::env::var("NEHA_PAIRING_CHILD_SPACE") else {
            return;
        };

        let ticket = request_ticket(&space, "token")
            .await
            .expect("the direct ticket request must succeed");
        assert_eq!(ticket, "ticket-from-space");
        assert_eq!(
            pair_with_ticket(&space, "ticket-from-space").await.unwrap(),
            PairedComputer {
                computer: "computer-from-space".to_owned(),
                credential: "credential-from-space".to_owned(),
            }
        );
    }

    #[test]
    fn pairing_requests_bypass_a_system_proxy() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::Json;
        use axum::Router;
        use axum::routing::post;
        use serde_json::json;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // A proxy that counts every connection it receives.
            let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = proxy_listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&hits);
            let proxy = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = proxy_listener.accept().await else {
                        break;
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
            });

            // A fake Space on loopback that answers both local pairing routes.
            let app = Router::new()
                .route(
                    "/api/computer/ticket",
                    post(|| async { Json(json!({"ticket": "ticket-from-space"})) }),
                )
                .route(
                    "/api/computer/pair",
                    post(|| async {
                        Json(json!({
                            "computer": "computer-from-space",
                            "credential": "credential-from-space",
                        }))
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let space_address = listener.local_addr().unwrap();
            let space = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let proxy_url = format!("http://{proxy_address}");
            let space_url = format!("http://{space_address}");
            let child = tokio::task::spawn_blocking(move || {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("client::tests::a_local_pairing_client_child_pairs_around_the_proxy")
                    .arg("--nocapture")
                    .env("HTTP_PROXY", &proxy_url)
                    .env("HTTPS_PROXY", &proxy_url)
                    .env("ALL_PROXY", &proxy_url)
                    .env("NEHA_PAIRING_CHILD_SPACE", &space_url)
                    .output()
                    .expect("the child test process starts")
            })
            .await
            .unwrap();

            let stdout = String::from_utf8_lossy(&child.stdout);
            let stderr = String::from_utf8_lossy(&child.stderr);
            assert!(
                child.status.success() && stdout.contains("1 passed"),
                "the child pairing failed: {stdout}\n{stderr}"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                0,
                "the pairing client must not touch the system proxy"
            );
            space.abort();
            proxy.abort();
        });
    }
}
