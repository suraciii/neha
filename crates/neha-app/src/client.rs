//! The CLI's connection to one Space.

use std::fs;
use std::path::{Path, PathBuf};

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

/// Downloads one File and returns its name and bytes.
///
/// The name becomes a path on this machine, so it has to be one path
/// component; anything else from the Space is refused.
pub async fn download_file(credential: &Credential, file: Id) -> Result<(String, Vec<u8>)> {
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
        .and_then(disposition_name)
        .context("the Space did not name the File; pass --out")?;
    if !is_one_component(&name) {
        bail!("the Space named the File {name:?}, which is not a file name; pass --out");
    }
    let bytes = response
        .bytes()
        .await
        .context("the Space's answer was cut short")?;

    Ok((name, bytes.to_vec()))
}

/// Whether a name can be one path component, wherever this CLI runs.
fn is_one_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
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

/// Runs one command and returns its result, reporting its own run's events.
pub async fn command_following(
    credential: &Credential,
    command: CommandCall,
    mut on_event: impl FnMut(&neha::record::Event),
) -> Result<Value> {
    let command_id = Id::new();
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
                id: command_id,
                command,
            })?
            .into(),
        ))
        .await
        .context("cannot send the command")?;

    let mut filter = FollowFilter::new(command_id);
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
                bail!("the Space ended the subscription: {message}");
            }
            Ok(ServerFrame::Event { event }) => {
                for event in filter.push_event(event) {
                    on_event(&event);
                }
            }
            Ok(ServerFrame::Snapshot { events, .. }) => {
                for event in filter.push_snapshot(events) {
                    on_event(&event);
                }
            }
            Ok(
                ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. }
                | ServerFrame::Hello { .. }
                | ServerFrame::Heartbeat { .. },
            ) => {}
            Err(_) => bail!("the Space sent a frame this CLI does not understand"),
        }
    }
}

/// Keeps only the events of the run a submitted command started.
#[derive(Debug)]
struct FollowFilter {
    command: Id,
    target: Option<Id>,
    pending: Vec<neha::record::Event>,
}

impl FollowFilter {
    fn new(command: Id) -> Self {
        Self {
            command,
            target: None,
            pending: Vec::new(),
        }
    }

    fn push_event(&mut self, event: neha::record::Event) -> Vec<neha::record::Event> {
        if self.target.is_none() {
            self.target = self.started_run(&event);
            self.pending.push(event);
            if self.target.is_some() {
                return std::mem::take(&mut self.pending)
                    .into_iter()
                    .filter(|event| self.belongs(event))
                    .collect();
            }
            return Vec::new();
        }
        if self.belongs(&event) {
            vec![event]
        } else {
            Vec::new()
        }
    }

    fn push_snapshot(&mut self, events: Vec<neha::record::Event>) -> Vec<neha::record::Event> {
        if self.target.is_none() {
            self.target = events.iter().find_map(|event| self.started_run(event));
        }

        events
            .into_iter()
            .filter(|event| self.belongs(event))
            .collect()
    }

    fn started_run(&self, event: &neha::record::Event) -> Option<Id> {
        if event.kind != neha::record::EventKind::RunStarted {
            return None;
        }
        let started_for = event.data.get("command")?.as_str()?;
        if started_for != self.command.to_string() {
            return None;
        }
        serde_json::from_value(event.data.get("run")?.clone()).ok()
    }

    fn belongs(&self, event: &neha::record::Event) -> bool {
        let Some(run) = self.target else {
            return false;
        };
        event
            .data
            .get("run")
            .and_then(|value| serde_json::from_value::<Id>(value.clone()).ok())
            == Some(run)
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

#[cfg(test)]
mod tests {
    use neha::file::MAX_FILE_BYTES;
    use neha::id::Id;
    use neha::record::{Author, Event, EventKind, KIND_VERSION};
    use neha::seq::Seq;
    use neha::time::Timestamp;

    use super::{FollowFilter, disposition_name, is_one_component, read_for_upload};

    #[test]
    fn a_file_name_decides_its_media_type() {
        let media_type = |name: &str| neha::file::media_type_for(name);

        assert_eq!(media_type("notes.txt"), "text/plain");
        assert_eq!(media_type("main.rs"), "text/plain");
        assert_eq!(media_type("data.json"), "application/json");
        assert_eq!(media_type("photo.JPG"), "image/jpeg");
        assert_eq!(media_type("paper.pdf"), "application/pdf");
        assert_eq!(media_type("archive.bin"), "application/octet-stream");
        assert_eq!(media_type("Makefile"), "application/octet-stream");
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

    #[test]
    fn only_a_single_path_component_is_a_file_name() {
        assert!(is_one_component("报告 v2.txt"));
        for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(!is_one_component(name), "{name:?}");
        }
    }

    fn event(kind: EventKind, data: serde_json::Value, seq: i64) -> Event {
        Event {
            seq: Seq::from_i64(seq).unwrap(),
            kind,
            version: KIND_VERSION,
            at: Timestamp::from_microseconds(0).unwrap(),
            author: Author::Bot(Id::new()),
            acting: Some(Id::new()),
            command: None,
            data,
        }
    }

    fn started(run: Id, command: Id, seq: i64) -> Event {
        event(
            EventKind::RunStarted,
            serde_json::json!({
                "run": run,
                "chat": Id::new(),
                "bot": Id::new(),
                "command": command,
                "message": Id::new(),
            }),
            seq,
        )
    }

    fn action(run: Id, seq: i64) -> Event {
        event(
            EventKind::RunStep,
            serde_json::json!({"run": run, "step": 1, "tool_calls": 1}),
            seq,
        )
    }

    #[test]
    fn a_snapshot_keeps_the_submitted_runs_early_progress() {
        let command = Id::new();
        let run = Id::new();
        let mut filter = FollowFilter::new(command);
        let snapshot = vec![
            event(
                EventKind::MessageAdded,
                serde_json::json!({"run": run, "chat": Id::new(), "text": "hi"}),
                1,
            ),
            started(run, command, 2),
            action(run, 3),
        ];

        let kept = filter.push_snapshot(snapshot);

        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn another_runs_events_are_ignored() {
        let command = Id::new();
        let other = Id::new();
        let mut filter = FollowFilter::new(command);

        let kept = filter.push_event(started(other, Id::new(), 1));

        assert!(kept.is_empty());
    }

    #[test]
    fn events_before_the_run_start_are_replayed_once() {
        let command = Id::new();
        let run = Id::new();
        let mut filter = FollowFilter::new(command);
        let user_message = event(
            EventKind::MessageAdded,
            serde_json::json!({"run": run, "chat": Id::new(), "text": "hi"}),
            1,
        );

        assert!(filter.push_event(user_message).is_empty());
        let kept = filter.push_event(started(run, command, 2));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].kind, EventKind::MessageAdded);
        assert_eq!(kept[1].kind, EventKind::RunStarted);
        assert_eq!(filter.push_event(action(run, 3)).len(), 1);
    }
}
