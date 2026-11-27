//! The Space's HTTP and WebSocket server.
//!
//! A client joins with the Space's invitation and password, then reaches
//! every capability through one WebSocket and sends and receives Files over
//! HTTP. The server listens on loopback until a remote Space is configured.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::rand_core::RngCore as _;
use axum::body::Bytes;
use axum::extract::Path as PathParams;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody, QueryRejection};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Query, Request, State};
use axum::http::header::{
    AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use neha::error::ErrorCode;
use neha::file::MAX_FILE_BYTES;
use neha::id::Id;
use neha::record::{Author, EventDraft, EventKind};
use neha::surface::{ClientFrame, CommandCall, MAX_FRAME_BYTES, PROTOCOL_VERSION, ServerFrame};
use neha_computer::channel::{CHANNEL_VERSION, DeviceFrame, HostFrame, MAX_CHANNEL_FRAME_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::blobs::{Blobs, BlobsError};
use crate::commands;
use crate::computer_link::{self, ComputerLinks};
use crate::config::{Config, ConfigError, Storage};
use crate::home::Home;
use crate::identity::{Account, Identity, IdentityError, PairingState};
use crate::model::{ModelClient, ModelError};
use crate::runs::RunQueue;
use crate::secrets::{Secrets, SecretsError};
use crate::state::State as SpaceState;
use crate::store::{AppendRequest, Projection, Store, StoreError, StoreReader};
use crate::ticket::{self, PairLimiter, TicketError};

const LOGIN_ATTEMPTS: usize = 5;
const SOURCE_ATTEMPTS: usize = 20;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LOGIN_NAMES: usize = 1024;
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How many pairing attempts one User may have in a window.
const PAIRING_ATTEMPTS: usize = 10;
/// The window pairing attempts are counted over.
const PAIRING_WINDOW: Duration = Duration::from_secs(60);
/// The longest machine name or workspace a pairing start accepts, in bytes.
const MAX_PAIRING_FIELD_BYTES: usize = 256;
/// How long a Computer may take to send its Hello.
///
/// Ten seconds in a release build; the test build uses a short limit so the
/// timeout is asserted without slowing the suite.
const COMPUTER_HELLO: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(10)
};
/// How often the Space pings a Computer's channel.
const COMPUTER_PING: Duration = Duration::from_secs(15);
/// How long a Computer may stay silent before the channel is dropped.
const COMPUTER_IDLE: Duration = Duration::from_secs(60);

/// A Space ready to answer clients.
pub struct SpaceServer {
    pub(crate) home: Home,
    pub(crate) store: Store,
    identity: Identity,
    pub(crate) blobs: Blobs,
    storage: Storage,
    invite: Mutex<Option<Invite>>,
    login: Mutex<LoginLimiter>,
    pub(crate) state: Mutex<SpaceState>,
    pub(crate) secrets: Secrets,
    pub(crate) model: ModelClient,
    pub(crate) events: broadcast::Sender<neha::record::Event>,
    pub(crate) tool_timeout: Duration,
    /// The live channel of each connected real Computer.
    pub(crate) links: ComputerLinks,
    /// Pairing attempts that are running or recently failed, per account.
    pub(crate) pairings: tokio::sync::Mutex<PairingAttempts>,
    /// Serializes approval answers and rule changes.
    pub(crate) approvals: tokio::sync::Mutex<()>,
    /// Local-ticket pair attempts, counted per source address.
    pub(crate) pairs: Mutex<PairLimiter>,
    /// The ticket file this start wrote for its Admin, with its digest.
    start_ticket: Mutex<Option<crate::ticket::StartTicket>>,
    /// Set once the Space stops: no command may then start new work.
    closed: AtomicBool,
    pub(crate) runs: Arc<RunQueue>,
    uploads: Mutex<std::collections::HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
    ui: Option<PathBuf>,
    session_cookie: String,
    browser_host: String,
}

impl SpaceServer {
    /// Opens a home and prepares its invitation when it has no account.
    pub async fn open(home: Home) -> Result<Self, ServerError> {
        Self::open_with(home, neha_computer::local::DEFAULT_TIMEOUT).await
    }

    async fn open_with(home: Home, tool_timeout: Duration) -> Result<Self, ServerError> {
        Self::open_with_ui(home, None, tool_timeout).await
    }

    /// Opens a home and serves the browser client from `ui`.
    async fn open_with_ui(
        home: Home,
        ui: Option<PathBuf>,
        tool_timeout: Duration,
    ) -> Result<Self, ServerError> {
        let session_cookie = session_cookie_name(home.root());
        let browser_host = browser_host(home.root());
        let storage = Config::load(&home)?.storage;
        let store = Store::open(&home).await?;
        let identity = Identity::open(&home).await?;
        let blobs = Blobs::open(&home, &storage)?;
        match blobs.check().await {
            Ok(()) => {}
            // A store that is temporarily unreachable does not refuse the
            // start: File operations answer 503 until it returns.
            Err(error) if error.is_unavailable() => {}
            Err(source) => {
                return Err(ServerError::FileStore {
                    storage: Box::new(storage.clone()),
                    source: Box::new(source),
                });
            }
        }
        let invite = if identity.has_accounts().await? {
            None
        } else {
            Some(Invite::generate())
        };
        let mut state = SpaceState::default();
        store.replay(&mut state).await?;
        let secrets = Secrets::load(&home)?;
        let model = ModelClient::new()?;
        let (events, _) = broadcast::channel(256);

        Ok(Self {
            home,
            store,
            identity,
            blobs,
            storage,
            invite: Mutex::new(invite),
            login: Mutex::new(LoginLimiter::default()),
            state: Mutex::new(state),
            secrets,
            model,
            events,
            tool_timeout,
            links: ComputerLinks::new(),
            pairings: tokio::sync::Mutex::new(PairingAttempts::default()),
            approvals: tokio::sync::Mutex::new(()),
            pairs: Mutex::new(PairLimiter::default()),
            start_ticket: Mutex::new(None),
            closed: AtomicBool::new(false),
            runs: Arc::new(RunQueue::new()),
            uploads: Mutex::new(std::collections::HashMap::new()),
            ui,
            session_cookie,
            browser_host,
        })
    }

    /// Returns the lock that serializes uploads in one Chat.
    ///
    /// Two uploads of the same bytes must not both record a File; a Bot's run
    /// holds its own lock and never waits on this one.
    fn upload_lock(&self, chat: Id) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.uploads.lock().expect("upload lock");
        Arc::clone(
            locks
                .entry(chat)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Returns whether the Space has stopped accepting work.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Refuses new work, then lets the queue drain.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Returns the home.
    #[must_use]
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// Returns the router a listener serves.
    pub fn router(self: &Arc<Self>) -> Router {
        let router = Router::new()
            .route("/healthz", get(health))
            .route("/api/join", post(join))
            .route("/api/login", post(login))
            .route("/api/me", get(me))
            .route("/api/socket", get(socket))
            .route("/api/computer/socket", get(computer_socket))
            .route("/api/computer/ticket", post(computer_ticket))
            .route("/api/computer/pair", post(computer_pair))
            .route("/api/computers/pair", post(computer_pair_start))
            .route("/api/computers/pair/{device}", post(computer_pair_poll))
            .route(
                "/api/chats/{chat}/files",
                post(file_upload).layer(DefaultBodyLimit::max(MAX_FILE_BYTES as usize)),
            )
            .route("/api/files/{id}", get(file_download))
            .route("/api/bot/messages", post(bot_message))
            .route(
                "/api/bot/files",
                post(bot_file_upload).layer(DefaultBodyLimit::max(MAX_FILE_BYTES as usize)),
            )
            .route("/api/bot/files/{id}", get(bot_file_download))
            .route("/api", any(api_not_found))
            .route("/api/", any(api_not_found))
            .route("/api/{*rest}", any(api_not_found));
        let router = if self.ui.is_some() {
            router.fallback(get(client_file))
        } else {
            router.route("/invite", get(invite_page))
        };
        router
            .layer(DefaultBodyLimit::max(64 * 1024))
            .layer(middleware::from_fn_with_state(Arc::clone(self), guard))
            .with_state(Arc::clone(self))
    }

    /// Returns the invitation secret while the Space has no account.
    pub fn invite_secret(&self) -> Option<String> {
        self.invite
            .lock()
            .expect("invite lock")
            .as_ref()
            .map(|invite| invite.secret.clone())
    }

    fn consume_invite(&self) {
        *self.invite.lock().expect("invite lock") = None;
    }
}

/// A server listening on a bound address.
pub struct Running {
    /// The address the server listens on.
    pub addr: std::net::SocketAddr,
    /// The invitation URL, when the Space has no Admin yet.
    pub invite: Option<String>,
    /// The local ticket `neha computer pair` reads, when the Space has an Admin.
    pub local_ticket: Option<PathBuf>,
    /// The per-Space hostname the browser client uses.
    pub browser_host: String,
    home: Home,
    storage: Storage,
    server: Arc<SpaceServer>,
    task: JoinHandle<()>,
}

impl Running {
    /// Returns the base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Returns the URL the browser client uses.
    #[must_use]
    pub fn browser_url(&self) -> String {
        format!("http://{}:{}", self.browser_host, self.addr.port())
    }

    /// Returns the browser invitation, when the Space has no Admin yet.
    #[must_use]
    pub fn browser_invite(&self) -> Option<String> {
        self.invite
            .as_ref()
            .map(|_| format!("{}/invite#{}", self.browser_url(), self.invite_secret()))
    }

    fn invite_secret(&self) -> String {
        self.invite
            .as_deref()
            .and_then(|invite| invite.split_once('#').map(|(_, secret)| secret.to_owned()))
            .unwrap_or_default()
    }

    /// Returns the Space's home.
    #[must_use]
    pub fn server_home(&self) -> std::path::PathBuf {
        self.home.root().to_path_buf()
    }

    /// Returns where the bytes of Files are stored.
    #[must_use]
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Stops the server.
    ///
    /// It refuses new work first, then stops the queues, so a command that
    /// arrives during the stop cannot start a run. A run still working is
    /// abandoned the way it was before a stop.
    pub async fn shutdown(self) {
        self.server.close();
        self.server.runs.shutdown().await;
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Binds a home on loopback and starts serving it.
pub async fn start(home: Home, port: u16) -> Result<Running, ServerError> {
    start_with_tool_timeout(home, port, neha_computer::local::DEFAULT_TIMEOUT).await
}

/// Binds a home on loopback and starts serving it with a browser client.
pub async fn start_with_ui(
    home: Home,
    port: u16,
    ui: Option<PathBuf>,
) -> Result<Running, ServerError> {
    start_with(home, port, ui, neha_computer::local::DEFAULT_TIMEOUT).await
}

/// Binds a home on loopback and starts serving it with a tool time limit.
pub async fn start_with_tool_timeout(
    home: Home,
    port: u16,
    tool_timeout: Duration,
) -> Result<Running, ServerError> {
    start_with(home, port, None, tool_timeout).await
}

async fn start_with(
    home: Home,
    port: u16,
    ui: Option<PathBuf>,
    tool_timeout: Duration,
) -> Result<Running, ServerError> {
    if let Some(dir) = &ui
        && !dir.join("index.html").is_file()
    {
        return Err(ServerError::Ui(dir.clone()));
    }
    let server = Arc::new(SpaceServer::open_with_ui(home, ui, tool_timeout).await?);
    validate_client_directory(&server).await?;
    // The port is bound before anything is written or woken: a start that
    // cannot listen must not make a run, reach a Provider, or record a failure.
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    if let Ok(mut state) = server.state.lock() {
        state.url = Some(format!("http://{addr}"));
    }
    recover_interrupted(&server.store, &server.state).await?;
    reconcile_pairings(&server).await?;
    // The record is the truth about Computers: a token whose Computer the
    // replay does not hold is an orphan from a crash between a pairing's token
    // write and its append, and must not authenticate anything.
    let recorded: Vec<Id> = server
        .state
        .lock()
        .map_err(|_| {
            ServerError::Store(StoreError::Corrupt("the Space state is locked".to_owned()))
        })?
        .computers
        .keys()
        .copied()
        .collect();
    server
        .identity
        .forget_unrecorded_computer_tokens(&recorded)
        .await?;
    // A local ticket binds one start of the Space: the previous start's
    // tickets are void before this one issues its own for the Admin.
    ticket::clear(&server.identity).await?;
    // The standard configuration directory is the only place the file lives.
    // A Space clears and writes only its own `spaces/<home-key>` folder, so
    // starting one Space never removes another Space's waiting ticket. When
    // neither XDG_CONFIG_HOME nor HOME names a directory no file ticket is
    // issued; a pairing takes its ticket from the signed-in CLI instead.
    let ticket_path = ticket::path(&server.home);
    if let Some(path) = &ticket_path {
        ticket::remove_file(path).await?;
    }
    let start_ticket = match (server.identity.admin_account().await?, ticket_path) {
        (Some(admin), Some(path)) => {
            let url = format!("http://{addr}");
            let start = ticket::issue_for_start(&server.identity, &path, admin.id, &url).await?;
            *server.start_ticket.lock().expect("start ticket lock") = Some(start.clone());
            Some(start)
        }
        (Some(_), None) | (None, _) => None,
    };
    server.runs.wake_all(&server);
    let invite = server
        .invite_secret()
        .map(|secret| format!("http://{addr}/invite#{secret}"));
    let router = server.router();
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Ok(Running {
        addr,
        invite,
        local_ticket: start_ticket.map(|start| start.path),
        browser_host: server.browser_host.clone(),
        home: server.home().clone(),
        storage: server.storage.clone(),
        server,
        task,
    })
}

async fn guard(State(server): State<Arc<SpaceServer>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let browser_host = host
        .and_then(split_authority)
        .is_some_and(|(host, _)| host.eq_ignore_ascii_case(&server.browser_host));
    if !host.is_some_and(loopback_host) && !browser_host {
        return ApiError::forbidden("the Host header must name loopback").into_response();
    }
    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok());
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer "));
    if let Some(origin) = origin {
        let same_site = host.is_some_and(|host| same_authority(origin, host));
        if (!same_site || !browser_host) && !bearer {
            return ApiError::forbidden("a browser Origin must name this Space").into_response();
        }
    }

    next.run(request).await
}

fn same_authority(origin: &str, host: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    // This Space listens on plain HTTP on loopback. A page served by it has
    // an http Origin; accepting another scheme would claim transport security
    // the listener does not provide.
    if scheme != "http" {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.eq_ignore_ascii_case(host)
}

fn loopback_host(value: &str) -> bool {
    let Some((host, port)) = split_authority(value) else {
        return false;
    };
    if let Some(port) = port
        && !port.parse::<u16>().is_ok_and(|port| port != 0)
    {
        return false;
    }

    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn split_authority(value: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':')?)
        };
        return Some((host, port));
    }
    match value.split_once(':') {
        Some((host, port)) if !port.contains(':') => Some((host, Some(port))),
        Some(_) => None,
        None => Some((value, None)),
    }
}

fn path_identity(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

/// Returns the stable browser hostname for a Space.
///
/// A cookie is scoped to a host but not a port. A dedicated `*.localhost`
/// hostname keeps the browser session off shared loopback aliases and other
/// local services.
fn browser_host(home: &Path) -> String {
    let digest = Sha256::digest(path_identity(home));
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("neha-{suffix}.localhost")
}

async fn validate_client_directory(server: &SpaceServer) -> Result<(), ServerError> {
    let Some(dir) = &server.ui else {
        return Ok(());
    };
    let root = tokio::fs::canonicalize(dir)
        .await
        .map_err(|_| ServerError::Ui(dir.clone()))?;
    let index = tokio::fs::canonicalize(root.join("index.html"))
        .await
        .map_err(|_| ServerError::Ui(dir.clone()))?;
    if !index.starts_with(&root) || !index.is_file() {
        return Err(ServerError::Ui(dir.clone()));
    }
    let home = tokio::fs::canonicalize(server.home.root())
        .await
        .map_err(|_| ServerError::Ui(dir.clone()))?;
    if root.starts_with(&home) || home.starts_with(&root) {
        return Err(ServerError::UiOverlapsHome(dir.clone()));
    }
    // The standard configuration directory holds the plaintext local ticket
    // (and the saved Computer credential). A static root that can reach it
    // would serve the secret as a File, and the file's 0600 mode would not
    // stop the Space itself from reading it. The check is by path, so it also
    // covers a ticket folder the current start has not created yet.
    if let Some(config) = crate::ticket::configuration_directory(&server.home) {
        let config = resolve_path(&config)
            .await
            .map_err(|_| ServerError::Ui(dir.clone()))?;
        if root.starts_with(&config) || config.starts_with(&root) {
            return Err(ServerError::UiOverlapsConfiguration(dir.clone()));
        }
    }
    // A credential file named in the environment is hidden from Bots like the
    // saved pairing; a static root that can reach it (or its folder) would
    // hand the secret to a caller that has not signed in.
    if let Some(credential) = std::env::var_os("NEHA_CREDENTIAL").map(PathBuf::from) {
        let credential = resolve_path(&credential)
            .await
            .map_err(|_| ServerError::Ui(dir.clone()))?;
        let parent = credential
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| credential.clone());
        if root.starts_with(&parent)
            || parent.starts_with(&root)
            || credential.starts_with(&root)
            || root == credential
        {
            return Err(ServerError::UiOverlapsConfiguration(dir.clone()));
        }
    }
    // The daemon's machine configuration directory holds `computer.json`,
    // which carries a Computer's credential; a static root that reaches it
    // would serve that bearer to a caller that has not signed in.
    if let Some(computer_home) = std::env::var_os("NEHA_COMPUTER_HOME").map(PathBuf::from) {
        let directory = resolve_path(&computer_home)
            .await
            .map_err(|_| ServerError::Ui(dir.clone()))?;
        if root.starts_with(&directory) || directory.starts_with(&root) {
            return Err(ServerError::UiOverlapsConfiguration(dir.clone()));
        }
    }

    Ok(())
}

/// Resolves `path` to an absolute path even when it does not exist yet.
///
/// The deepest existing ancestor is canonicalized and the missing names are
/// appended, so the result follows symlinks of the part that exists while the
/// rest is compared by name.
async fn resolve_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match tokio::fs::canonicalize(&path).await {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }

                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = path.file_name().map(|name| name.to_os_string()) else {
                    return Err(error);
                };
                path.pop();
                missing.push(name);
                if path.as_os_str().is_empty() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn api_not_found() -> Response {
    ApiError::not_found("no such API route").into_response()
}

async fn client_file(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let host = headers.get(HOST).and_then(|value| value.to_str().ok());
    let host_name = host
        .and_then(split_authority)
        .map(|(host, _)| host)
        .unwrap_or_default();
    if !host_name.eq_ignore_ascii_case(&server.browser_host) {
        let port = host
            .and_then(split_authority)
            .and_then(|(_, port)| port)
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        let location = format!("http://{}{}{}", server.browser_host, port, uri.path());
        return (StatusCode::TEMPORARY_REDIRECT, [("location", location)]).into_response();
    }
    let Some(configured) = &server.ui else {
        return ApiError::not_found("no browser client is configured").into_response();
    };
    let Ok(root) = tokio::fs::canonicalize(configured).await else {
        return ApiError::not_found("the browser client is missing").into_response();
    };
    let requested = uri.path().trim_start_matches('/');
    let requested = if requested.is_empty() {
        PathBuf::from("index.html")
    } else {
        PathBuf::from(requested)
    };
    if requested.is_absolute()
        || requested
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return ApiError::not_found("no such file").into_response();
    }

    let target = root.join(&requested);
    let target = match tokio::fs::canonicalize(&target).await {
        Ok(target) if target.starts_with(&root) && target.is_file() => target,
        _ if requested.extension().is_some() => {
            return ApiError::not_found("no such file").into_response();
        }
        _ => match tokio::fs::canonicalize(root.join("index.html")).await {
            Ok(index) if index.starts_with(&root) && index.is_file() => index,
            _ => return ApiError::not_found("the browser client is missing").into_response(),
        },
    };

    let Ok(bytes) = tokio::fs::read(&target).await else {
        return ApiError::not_found("no such file").into_response();
    };
    let content_type = content_type(&target);
    let mut response = ([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response();
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") | Some("map") => "application/json",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn invite_page() -> Html<&'static str> {
    Html(
        "<!doctype html><title>Neha invitation</title>\
         <p>Run <code>neha join &lt;this URL&gt;</code> in a terminal to become the Admin.",
    )
}

#[derive(Deserialize)]
struct JoinRequest {
    secret: String,
    display_name: String,
    password: String,
}

#[derive(Deserialize)]
struct LoginRequest {
    display_name: String,
    password: String,
}

#[derive(Serialize)]
struct SessionResponse {
    token: String,
    account: AccountBody,
}

#[derive(Serialize)]
struct AccountBody {
    id: neha::id::Id,
    display_name: String,
    is_admin: bool,
}

impl From<Account> for AccountBody {
    fn from(account: Account) -> Self {
        Self {
            id: account.id,
            display_name: account.display_name,
            is_admin: account.is_admin,
        }
    }
}

async fn join(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    Json(request): Json<JoinRequest>,
) -> Result<Response, ApiError> {
    let accepted = server
        .invite
        .lock()
        .expect("invite lock")
        .as_ref()
        .is_some_and(|invite| invite.accepts(&request.secret));
    if !accepted {
        return Err(ApiError::not_found("this invitation is not usable"));
    }

    let (account, token) = server
        .identity
        .create_admin(&request.display_name, &request.password)
        .await
        .map_err(identity_error)?;
    let joined = EventDraft::new(
        EventKind::UserJoined,
        Author::User(account.id),
        serde_json::json!({
            "display_name": account.display_name,
            "is_admin": account.is_admin,
        }),
    );
    server
        .store
        .append(AppendRequest::new(
            Id::new(),
            format!("join:{}", account.id),
            vec![joined],
        ))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    server.consume_invite();

    let account = AccountBody::from(account);
    let browser = browser_request(&server, &headers);
    let cookie = browser.then(|| server.session_headers(&token));
    let response = if browser {
        Json(account).into_response()
    } else {
        Json(SessionResponse { token, account }).into_response()
    };
    Ok(match cookie {
        Some(cookie) => (cookie, response).into_response(),
        None => response,
    })
}

async fn login(
    State(server): State<Arc<SpaceServer>>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    if !server
        .login
        .lock()
        .expect("login lock")
        .allows(&request.display_name, source.ip())
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            "too many sign-in attempts",
        ));
    }

    match server
        .identity
        .login(&request.display_name, &request.password)
        .await
    {
        Ok((account, token)) => {
            let account = AccountBody::from(account);
            let browser = browser_request(&server, &headers);
            let cookie = browser.then(|| server.session_headers(&token));
            let response = if browser {
                Json(account).into_response()
            } else {
                Json(SessionResponse { token, account }).into_response()
            };
            Ok(match cookie {
                Some(cookie) => (cookie, response).into_response(),
                None => response,
            })
        }
        Err(IdentityError::InvalidCredentials) => {
            server
                .login
                .lock()
                .expect("login lock")
                .record(&request.display_name, source.ip());
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "invalid credentials",
            ))
        }
        Err(error) => Err(identity_error(error)),
    }
}

impl SpaceServer {
    fn session_headers(&self, token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let cookie = format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict",
            self.session_cookie
        );
        if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
            headers.insert(SET_COOKIE, value);
        }
        headers
    }
}

fn session_cookie_name(home: &Path) -> String {
    let digest = Sha256::digest(path_identity(home));
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("neha_session_{suffix}")
}

fn browser_request(server: &SpaceServer, headers: &HeaderMap) -> bool {
    let origin = headers.get(ORIGIN).and_then(|value| value.to_str().ok());
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(split_authority)
        .map(|(host, _)| host);
    origin.is_some_and(|origin| {
        host.is_some_and(|host| host.eq_ignore_ascii_case(&server.browser_host))
            && headers
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|host| same_authority(origin, host))
    })
}

async fn me(Authed { account }: Authed) -> Json<AccountBody> {
    Json(account.into())
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    media_type: String,
}

/// Stores the request body as a File of a Chat the account owns.
async fn file_upload(
    State(server): State<Arc<SpaceServer>>,
    Authed { account }: Authed,
    PathParams(chat): PathParams<String>,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let chat = chat
        .parse()
        .map_err(|_| ApiError::not_found("no such Chat"))?;
    let Query(query) = query
        .map_err(|_| ApiError::invalid_argument("the upload needs a name and a media_type"))?;
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))) => {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::InvalidArgument,
                "a File must be at most 32 MiB",
            ));
        }
        Err(_) => {
            return Err(ApiError::invalid_argument(
                "the request body could not be read",
            ));
        }
    };
    let upload_lock = server.upload_lock(chat);
    let _upload = upload_lock.lock().await;
    let outcome = commands::file_upload(
        &server.store,
        &server.state,
        &server.blobs,
        &account,
        chat,
        commands::FileUpload {
            name: query.name,
            media_type: query.media_type,
            bytes: bytes.to_vec(),
        },
    )
    .await?;
    broadcast(&server, &outcome.events);

    Ok(Json(outcome.result).into_response())
}

/// Sends the bytes of a File in a Chat the account owns.
async fn file_download(
    State(server): State<Arc<SpaceServer>>,
    Authed { account }: Authed,
    PathParams(id): PathParams<String>,
) -> Result<Response, ApiError> {
    let file = {
        let state = server
            .state
            .lock()
            .map_err(|_| ApiError::internal("state lock"))?;
        id.parse()
            .ok()
            .and_then(|id| state.owned_file(id, account.id).cloned())
    }
    .ok_or_else(|| ApiError::not_found("no such File"))?;
    let bytes = server.blobs.get(&file.sha256).await?;
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&file.media_type)
            .map_err(|_| ApiError::internal("the File has no usable media type"))?,
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            percent_encode(&file.name)
        ))
        .map_err(|_| ApiError::internal("the File has no usable name"))?,
    );

    Ok(response)
}

/// Returns the binding of the request's Bot run token.
fn bot_bearer(
    server: &Arc<SpaceServer>,
    headers: &HeaderMap,
) -> Result<crate::state::BotToken, ApiError> {
    let token = bearer(headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "a Bot run token is required",
        )
    })?;
    let mut state = server
        .state
        .lock()
        .map_err(|_| ApiError::internal("state lock"))?;
    state.bot_token(token).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "the Bot run token is not valid",
        )
    })
}

#[derive(Deserialize)]
struct BotMessageRequest {
    chat: Option<Id>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    file: Option<Id>,
}

/// Posts a Message as the Bot whose run token authorized the request.
async fn bot_message(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    Json(request): Json<BotMessageRequest>,
) -> Result<Json<Value>, ApiError> {
    let token = bot_bearer(&server, &headers)?;
    let chat = request.chat.unwrap_or(token.chat);
    let outcome = commands::bot_message(
        &server.store,
        &server.state,
        &token,
        chat,
        request.text,
        request.file,
    )
    .await?;
    broadcast(&server, &outcome.events);

    Ok(Json(outcome.result))
}

/// Stores the request body as a File of the Chat a Bot's run token names.
async fn bot_file_upload(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let token = bot_bearer(&server, &headers)?;
    let Query(query) = query
        .map_err(|_| ApiError::invalid_argument("the upload needs a name and a media_type"))?;
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))) => {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::InvalidArgument,
                "a File must be at most 32 MiB",
            ));
        }
        Err(_) => {
            return Err(ApiError::invalid_argument(
                "the request body could not be read",
            ));
        }
    };
    let upload_lock = server.upload_lock(token.chat);
    let _upload = upload_lock.lock().await;
    let outcome = commands::bot_file_upload(
        &server.store,
        &server.state,
        &server.blobs,
        &token,
        token.chat,
        commands::FileUpload {
            name: query.name,
            media_type: query.media_type,
            bytes: bytes.to_vec(),
        },
    )
    .await?;
    broadcast(&server, &outcome.events);

    Ok(Json(outcome.result))
}

/// Sends the bytes of a File in the Chat a Bot's run token names.
async fn bot_file_download(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    PathParams(id): PathParams<String>,
) -> Result<Response, ApiError> {
    let token = bot_bearer(&server, &headers)?;
    let file = {
        let state = server
            .state
            .lock()
            .map_err(|_| ApiError::internal("state lock"))?;
        id.parse()
            .ok()
            .and_then(|id| state.file_of(token.chat, id).cloned())
    }
    .ok_or_else(|| ApiError::not_found("no such File"))?;
    let bytes = server.blobs.get(&file.sha256).await?;
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&file.media_type)
            .map_err(|_| ApiError::internal("the File has no usable media type"))?,
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            percent_encode(&file.name)
        ))
        .map_err(|_| ApiError::internal("the File has no usable name"))?,
    );

    Ok(response)
}

#[derive(Deserialize)]
struct PairStartRequest {
    machine: String,
    workspace: String,
    token: String,
    poll_secret: String,
}

/// Starts pairing a real Computer.
///
/// The machine generated its token and polling secret and keeps them; the
/// Space stores only their digests and answers with the device identifier it
/// polls by and the code a User enters.
async fn computer_pair_start(
    State(server): State<Arc<SpaceServer>>,
    Json(request): Json<PairStartRequest>,
) -> Result<Json<Value>, ApiError> {
    let machine = request.machine.trim();
    if !is_pairing_text(machine) {
        return Err(ApiError::invalid_argument(
            "the machine name is not usable: it must be at most 256 bytes and carry no control characters",
        ));
    }
    let workspace = request.workspace.trim();
    if !Path::new(workspace).is_absolute() || !is_pairing_text(workspace) {
        return Err(ApiError::invalid_argument(
            "the workspace is not usable: it must be an absolute path of at most 256 bytes with no control characters",
        ));
    }
    if !is_pairing_secret(&request.token) || !is_pairing_secret(&request.poll_secret) {
        return Err(ApiError::invalid_argument(
            "the token and poll_secret must be 32 bytes of URL-safe base64",
        ));
    }
    let start = server
        .identity
        .begin_pairing(machine, workspace, &request.token, &request.poll_secret)
        .await
        .map_err(identity_error)?;

    Ok(Json(
        serde_json::json!({"device": start.device, "code": start.code}),
    ))
}

/// Returns whether a pairing field is usable: bounded, non-empty, no controls.
///
/// Pairing is the only unauthenticated write, and both values reach a terminal
/// and the record: a newline or an ANSI escape could forge either. The refusal
/// never echoes the value back.
fn is_pairing_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PAIRING_FIELD_BYTES
        && !value.chars().any(char::is_control)
}

/// Returns whether a value is a 32-byte URL-safe base64 secret.
fn is_pairing_secret(value: &str) -> bool {
    URL_SAFE_NO_PAD
        .decode(value)
        .is_ok_and(|bytes| bytes.len() == 32)
}

#[derive(Deserialize)]
struct PairPollRequest {
    secret: String,
}

/// What a machine learns when it polls its pairing.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PairingReply {
    /// No User approved the machine yet.
    Pending,
    /// A User approved the machine and the Space made its Computer.
    Approved {
        /// The Computer the machine now controls.
        computer: Id,
    },
}

/// Answers a machine's pairing poll.
///
/// The poll is a POST: the polling secret is a secret, and a request line ends
/// up in proxy logs and browser history. A poll never changes the pairing.
async fn computer_pair_poll(
    State(server): State<Arc<SpaceServer>>,
    PathParams(device): PathParams<String>,
    Json(request): Json<PairPollRequest>,
) -> Result<Json<PairingReply>, ApiError> {
    match server
        .identity
        .pairing(&device, &request.secret)
        .await
        .map_err(identity_error)?
    {
        Some(PairingState::Pending) => Ok(Json(PairingReply::Pending)),
        Some(PairingState::Approved { computer }) => {
            // The token may be final while the record is not: the command that
            // claims the code finishes the token first and appends the Computer
            // second, so a crash in between rolls the token back. The machine
            // must keep polling until the record holds its Computer, or it
            // would save a pairing that the next start revokes.
            if !computer_is_recorded(&server, computer)? {
                return Ok(Json(PairingReply::Pending));
            }
            Ok(Json(PairingReply::Approved { computer }))
        }
        None => Err(ApiError::not_found("no such pairing")),
    }
}

#[derive(Serialize)]
struct LocalTicketBody {
    ticket: String,
}

/// Issues a local ticket to a signed-in User.
///
/// This is the CLI-credential fallback for `neha computer pair`: the ticket
/// stays in the response and is never written to the Space's home.
async fn computer_ticket(
    State(server): State<Arc<SpaceServer>>,
    Authed { account }: Authed,
) -> Result<Json<LocalTicketBody>, ApiError> {
    let ticket = ticket::issue(&server.identity, account.id)
        .await
        .map_err(ticket_error)?;

    Ok(Json(LocalTicketBody { ticket }))
}

#[derive(Deserialize)]
struct PairTicketRequest {
    ticket: String,
}

#[derive(Serialize)]
struct PairedComputerBody {
    computer: Id,
    credential: String,
}

/// Exchanges a local ticket for a Computer and its credential.
///
/// The endpoint needs no login: the ticket is the credential, it works once,
/// and it belongs to one start of the Space. Attempts are counted per source
/// address so an unauthenticated caller cannot churn tickets or Computers.
async fn computer_pair(
    State(server): State<Arc<SpaceServer>>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    Json(request): Json<PairTicketRequest>,
) -> Result<Json<PairedComputerBody>, ApiError> {
    if !server.pairs.lock().expect("pair lock").allows(source.ip()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            "too many pairing attempts",
        ));
    }
    let start_ticket = server
        .start_ticket
        .lock()
        .expect("start ticket lock")
        .clone();
    let owner = ticket::claim(&server.identity, start_ticket.as_ref(), &request.ticket)
        .await
        .map_err(ticket_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "the ticket is not valid",
            )
        })?;

    // The credential is recorded before the Computer, so a machine never gets
    // a working token for a Computer the record refuses. A failed append rolls
    // the token back before the answer.
    let computer = Id::new();
    let credential = ticket::secret();
    server
        .identity
        .record_computer_token(computer, &credential)
        .await
        .map_err(identity_error)?;
    let draft = EventDraft::new(
        EventKind::ComputerCreated,
        Author::User(owner),
        serde_json::json!({
            "computer": computer,
            "owner": owner,
            "kind": "local",
            "workspace": "",
        }),
    );
    let appended = match server
        .store
        .append(AppendRequest::new(
            Id::new(),
            format!("computer.pair:{computer}"),
            vec![draft],
        ))
        .await
    {
        Ok(appended) => appended,
        Err(error) => {
            if let Err(cleanup) = server.identity.forget_computer_token(&credential).await {
                // The start-up reconciliation removes the orphan; report it
                // now so a failure to roll back is visible.
                eprintln!("neha: cannot roll back a pairing token: {cleanup}");
            }
            return Err(ApiError::internal(error.to_string()));
        }
    };
    {
        let mut state = server
            .state
            .lock()
            .map_err(|_| ApiError::internal("the Space state is locked"))?;
        for event in &appended.events {
            state
                .apply(event)
                .map_err(|error| ApiError::internal(error.to_string()))?;
        }
    }
    broadcast(&server, &appended.events);

    Ok(Json(PairedComputerBody {
        computer,
        credential,
    }))
}

/// Maps a ticket failure to an API error.
fn ticket_error(error: TicketError) -> ApiError {
    match error {
        TicketError::Identity(error) => identity_error(error),
        TicketError::Io(_) | TicketError::Corrupt(_) => ApiError::internal(error.to_string()),
    }
}

/// Whether the record holds this Computer.
///
/// The pairing poll and the channel handshake both need the answer: the token
/// is finished before `ComputerCreated` is appended, so anything that trusts
/// the token alone could hand a machine a pairing the next start's
/// reconciliation rolls back.
fn computer_is_recorded(server: &SpaceServer, computer: Id) -> Result<bool, ApiError> {
    let state = server
        .state
        .lock()
        .map_err(|_| ApiError::internal("the Space state is locked"))?;

    Ok(state.computers.contains_key(&computer))
}

/// Upgrades a paired Computer's control connection.
async fn computer_socket(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let token = bearer(&headers).ok_or_else(unauthorized_computer)?;
    let computer = server
        .identity
        .authenticate_computer(token)
        .await
        .map_err(identity_error)?
        .ok_or_else(unauthorized_computer)?;
    // A token only opens a channel for a Computer the record holds: until the
    // append commits, the token may still be rolled back by the start-up
    // reconciliation, and the hung-up handshake is refused exactly like an
    // unknown Computer's.
    if !computer_is_recorded(&server, computer)? {
        return Err(unauthorized_computer());
    }

    Ok(ws
        .max_message_size(MAX_CHANNEL_FRAME_BYTES)
        .max_frame_size(MAX_CHANNEL_FRAME_BYTES)
        .on_upgrade(move |socket| handle_computer_socket(socket, server, computer)))
}

fn unauthorized_computer() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "a Computer token is required",
    )
}

/// Serves one Computer's channel until it goes away.
///
/// The first frame must be a `Hello` naming the Computer the token belongs to,
/// speaking this Space's version, and stating that the machine fences its
/// Bots. The channel then registers, the run queue wakes, and frames flow both
/// ways until the connection drops, the Computer stays silent for too long, or
/// the Computer breaks the protocol.
async fn handle_computer_socket(mut socket: WebSocket, server: Arc<SpaceServer>, computer: Id) {
    // A connection that never greets is not the Computer we paired: it must
    // not hold a task open while waiting for a frame that may never come.
    let hello = match tokio::time::timeout(COMPUTER_HELLO, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<DeviceFrame>(text.as_str()) {
                Ok(DeviceFrame::Hello {
                    version,
                    computer: named,
                    workspace: _,
                    sandbox,
                }) => Some((version, named, sandbox)),
                _ => None,
            }
        }
        _ => None,
    };
    // A daemon that cannot fence its machine never runs, and a connection that
    // names another Computer would act as that Computer: both are refused.
    let Some((version, named, sandbox)) = hello else {
        return;
    };
    if version != CHANNEL_VERSION || named != computer.to_string() || !sandbox {
        return;
    }

    let (sink, mut stream) = socket.split();
    let (frames, mut outgoing) = mpsc::channel::<HostFrame>(computer_link::FRAME_QUEUE);
    let link = server.links.connect(computer, frames.clone());
    // When a Computer reconnects, it replaces its old link and this handler
    // must stop: the Space already failed the old link's requests, and two
    // readers acting as one Computer would race each other.
    let mut closing = link.closed_signal();
    server.runs.wake_all(&server);
    // The writer owns the sending half, so this reader never blocks on a
    // Computer that stopped reading. Every frame has a deadline; one that
    // misses it closes the link, which fails its request and wakes this loop.
    let mut writer = tokio::spawn({
        let link = Arc::clone(&link);
        let mut closing = link.closed_signal();
        let mut sink = sink;
        async move {
            loop {
                tokio::select! {
                    biased;
                    _ = closing.changed() => break,
                    frame = outgoing.recv() => {
                        let Some(frame) = frame else { break; };
                        let Ok(text) = serde_json::to_string(&frame) else { break; };
                        let sent = tokio::time::timeout(
                            computer_link::FRAME_TIMEOUT,
                            sink.send(Message::Text(text.into())),
                        )
                        .await;
                        if !matches!(sent, Ok(Ok(()))) {
                            link.close();
                            break;
                        }
                    }
                }
            }
            // Close from our side so a replaced Computer learns its channel
            // moved instead of discovering it on its next write.
            let _ = tokio::time::timeout(
                computer_link::FRAME_TIMEOUT,
                sink.send(Message::Close(None)),
            )
            .await;
        }
    });
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + COMPUTER_PING, COMPUTER_PING);
    let mut last_pong = Instant::now();
    loop {
        if *closing.borrow() {
            break;
        }
        tokio::select! {
            changed = closing.changed() => {
                let _ = changed;
                break;
            }
            message = stream.next() => {
                let Some(Ok(message)) = message else { break; };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    // The channel speaks JSON text, nothing else.
                    Message::Binary(_) => break,
                };
                if text.len() > MAX_CHANNEL_FRAME_BYTES {
                    break;
                }
                match serde_json::from_str::<DeviceFrame>(text.as_str()) {
                    Ok(DeviceFrame::Pong) => last_pong = Instant::now(),
                    // A second Hello is a protocol error, not a heartbeat.
                    Ok(DeviceFrame::Hello { .. }) => break,
                    Ok(frame) => {
                        if let Some((id, answer)) = computer_link::answer(frame) {
                            link.deliver(id, answer);
                        }
                    }
                    Err(_) => break,
                }
            }
            _ = ping.tick() => {
                if last_pong.elapsed() >= COMPUTER_IDLE {
                    break;
                }
                // A heartbeat must never block the reader: when the queue is
                // full the Computer is already wedged, and its requests will
                // fail on their own deadline.
                let _ = frames.try_send(HostFrame::Ping);
            }
        }
    }

    server.links.disconnect(&link);
    server.runs.wake_all(&server);
    // The writer stops on the link's close signal; give it its deadline, then
    // drop it if the socket refuses to finish.
    if tokio::time::timeout(computer_link::FRAME_TIMEOUT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
}

/// Returns the value of an RFC 8187 `filename*`: every byte that is not an
/// `attr-char` as the uppercase hex of its UTF-8.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    encoded
}

async fn socket(
    Authed { account }: Authed,
    ws: WebSocketUpgrade,
    State(server): State<Arc<SpaceServer>>,
) -> Response {
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, server, account))
}

async fn handle_socket(mut socket: WebSocket, server: Arc<SpaceServer>, account: Account) {
    let Some(Ok(Message::Text(text))) = socket.recv().await else {
        return;
    };
    let hello = match serde_json::from_str::<ClientFrame>(text.as_str()) {
        Ok(ClientFrame::Hello { version, .. }) => version,
        _ => {
            let _ = send_raw(
                &mut socket,
                ServerFrame::ProtocolError {
                    message: "the first frame must be hello".to_owned(),
                },
            )
            .await;
            return;
        }
    };
    if hello != PROTOCOL_VERSION {
        let _ = send_raw(
            &mut socket,
            ServerFrame::ProtocolError {
                message: format!("this Space speaks protocol version {PROTOCOL_VERSION}"),
            },
        )
        .await;
        return;
    }
    if send_raw(
        &mut socket,
        ServerFrame::Hello {
            version: PROTOCOL_VERSION,
            capabilities: Vec::new(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let (mut sink, mut stream) = socket.split();
    let (out, mut pending) = mpsc::channel::<ServerFrame>(128);
    let writer = tokio::spawn(async move {
        while let Some(frame) = pending.recv().await {
            let Ok(text) = serde_json::to_string(&frame) else {
                break;
            };
            if text.len() > MAX_FRAME_BYTES {
                let notice = ServerFrame::ProtocolError {
                    message: "a frame exceeded the 1 MiB limit".to_owned(),
                };
                if let Ok(text) = serde_json::to_string(&notice) {
                    let _ = sink.send(Message::Text(text.into())).await;
                }
                break;
            }
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut forwarder: Option<JoinHandle<()>> = None;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(Ok(message)) = message else { break; };
                let Message::Text(text) = message else { continue; };
                match serde_json::from_str::<ClientFrame>(text.as_str()) {
                    Ok(ClientFrame::Hello { .. }) => {
                        if send(&out, ServerFrame::ProtocolError { message: "hello was already sent".to_owned() }).await.is_err() {
                            break;
                        }
                    }
                    Ok(ClientFrame::Subscribe { id: _, cursor, limit }) => {
                        if let Some(task) = forwarder.take() {
                            task.abort();
                        }
                        let receiver = server.events.subscribe();
                        forwarder = Some(spawn_forwarder(
                            receiver,
                            server.store.reader(),
                            out.clone(),
                            cursor.unwrap_or(neha::seq::Seq::ZERO),
                            limit,
                            account.id,
                        ));
                    }
                    Ok(ClientFrame::Command { id, command }) => {
                        if server.is_closed() {
                            let _ = send(&out, ServerFrame::Error { id: Some(id), code: ErrorCode::Unavailable, message: "the Space is stopping".to_owned() }).await;
                            break;
                        }
                        if send(&out, ServerFrame::Accepted { id }).await.is_err() {
                            break;
                        }
                        // A recorded command answers from the record; it must not
                        // hold the Bot's place while it waits for the run's result.
                        let replayed = match &command {
                            CommandCall::MessageSend(_) | CommandCall::BotMove { .. } => {
                                matches!(server.store.command(id).await, Ok(Some(_)))
                            }
                            _ => false,
                        };
                        // Taking the next place and recording it happen under the
                        // Bot's admit lock, so concurrent Messages confirm places
                        // that are unique and in the record's order. Two first
                        // submissions of one id may both miss the record and take
                        // the lock; the second then finds the record and waits for
                        // its result. That tiny race settles on the same answer.
                        //
                        // A move takes the same lock for its whole command: its
                        // check of the Bot's Computer and the event it appends must
                        // not interleave with another move, or the record would
                        // hold an event the projection refuses. The Computer the
                        // command saw before waiting is compared under the lock: a
                        // move that waited behind another move is refused instead
                        // of silently becoming a second, different move.
                        let admit = match &command {
                            CommandCall::MessageSend(message) if !replayed => {
                                let bot = server.state.lock().ok().and_then(|state| {
                                    state.chats.get(&message.chat).map(|chat| chat.bot)
                                });
                                match bot {
                                    Some(bot) => Some(server.runs.admit(bot).await),
                                    None => None,
                                }
                            }
                            CommandCall::BotMove { bot, .. } if !replayed => {
                                let observed = server.state.lock().ok().and_then(|state| {
                                    state.bots.get(bot).map(|bot| bot.computer)
                                });
                                let guard = server.runs.admit(*bot).await;
                                let current = server.state.lock().ok().and_then(|state| {
                                    state.bots.get(bot).map(|bot| bot.computer)
                                });
                                if current != observed {
                                    drop(guard);
                                    let moved = ApiError::invalid_argument(
                                        "the Bot moved while the command was waiting; show it and try again",
                                    );
                                    let _ = send(
                                        &out,
                                        ServerFrame::Error {
                                            id: Some(id),
                                            code: moved.code(),
                                            message: moved.to_string(),
                                        },
                                    )
                                    .await;
                                    continue;
                                }
                                Some(guard)
                            }
                            _ => None,
                        };
                        let context = commands::CommandContext {
                            store: &server.store,
                            state: &server.state,
                            home: &server.home,
                            secrets: &server.secrets,
                            identity: &server.identity,
                            links: &server.links,
                            pairings: &server.pairings,
                            approvals: &server.approvals,
                            runs: &server.runs,
                            events: &server.events,
                        };
                        let reset = matches!(command, CommandCall::ComputerReset { .. });
                        let outcome = commands::execute(&context, &account, id, command).await;
                        // A Message that arrived during a reset queued a run
                        // whose pump saw the reset mark and stopped. Wake the
                        // pumps whether the reset succeeded or failed: the mark
                        // is gone either way.
                        if reset {
                            server.runs.wake_all(&server);
                        }
                        let result = match outcome {
                            Ok(outcome) => {
                                broadcast(&server, &outcome.events);
                                // The pump is woken before any frame whose failure
                                // ends this loop: a lost answer must not lose the run.
                                if let Some(bot) = outcome.queued {
                                    server.runs.enqueue(&server, bot);
                                }
                                drop(admit);
                                if let Some(seq) = outcome.committed
                                    && send(&out, ServerFrame::Committed { id, seq }).await.is_err()
                                {
                                    break;
                                }
                                outcome.result
                            }
                            Err(error) => {
                                let _ = send(&out, ServerFrame::Error { id: Some(id), code: error.code(), message: error.to_string() }).await;
                                continue;
                            }
                        };
                        if send(&out, ServerFrame::Confirmed { id, result }).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        if send(&out, ServerFrame::ProtocolError { message: "the frame is not valid JSON".to_owned() }).await.is_err() {
                            break;
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                let cursor = server.store.last_seq().await.unwrap_or(neha::seq::Seq::ZERO);
                if send(&out, ServerFrame::Heartbeat { cursor }).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(task) = forwarder.take() {
        task.abort();
    }
    drop(out);
    let _ = writer.await;
}

async fn send_raw(socket: &mut WebSocket, frame: ServerFrame) -> Result<(), ()> {
    let text = serde_json::to_string(&frame).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn send(out: &mpsc::Sender<ServerFrame>, frame: ServerFrame) -> Result<(), ()> {
    out.send(frame).await.map_err(|_| ())
}

fn broadcast(server: &SpaceServer, events: &[neha::record::Event]) {
    for event in events {
        let _ = server.events.send(event.clone());
    }
}

fn spawn_forwarder(
    mut receiver: broadcast::Receiver<neha::record::Event>,
    reader: StoreReader,
    out: mpsc::Sender<ServerFrame>,
    cursor: neha::seq::Seq,
    limit: Option<usize>,
    account: Id,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut cursor = match catch_up(&reader, &out, cursor, limit, account).await {
            Ok(cursor) => cursor,
            Err(StreamError::Oversized(event)) => {
                let _ = out
                    .send(ServerFrame::ProtocolError {
                        message: format!("event {event} is larger than the frame limit"),
                    })
                    .await;
                return;
            }
            Err(StreamError::Store | StreamError::Closed) => {
                let _ = out
                    .send(ServerFrame::Error {
                        id: None,
                        code: ErrorCode::Internal,
                        message: "the record could not be read".to_owned(),
                    })
                    .await;
                return;
            }
        };
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if event.seq <= cursor {
                        continue;
                    }
                    let seq = event.seq;
                    // A client sees its own events and the Space-wide ones;
                    // the rest only advance the cursor so a later reconnect
                    // neither repeats nor misses them.
                    if !visible_to(account, &event) {
                        cursor = seq;
                        continue;
                    }
                    let frame = ServerFrame::Event { event };
                    if !frame_fits(&frame) {
                        let _ = out
                            .send(ServerFrame::ProtocolError {
                                message: format!("event {seq} is larger than the frame limit"),
                            })
                            .await;
                        break;
                    }
                    if out.send(frame).await.is_err() {
                        break;
                    }
                    cursor = seq;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    match catch_up(&reader, &out, cursor, limit, account).await {
                        Ok(next) => cursor = next,
                        Err(StreamError::Oversized(event)) => {
                            let _ = out
                                .send(ServerFrame::ProtocolError {
                                    message: format!(
                                        "event {event} is larger than the frame limit"
                                    ),
                                })
                                .await;
                            break;
                        }
                        Err(StreamError::Store | StreamError::Closed) => break,
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[derive(Debug)]
enum StreamError {
    Store,
    Oversized(neha::seq::Seq),
    Closed,
}

fn frame_fits(frame: &ServerFrame) -> bool {
    serde_json::to_vec(frame).is_ok_and(|bytes| bytes.len() + 4096 <= MAX_FRAME_BYTES)
}

/// Sends every recorded page that fits a frame, then returns the cursor.
async fn catch_up(
    reader: &StoreReader,
    out: &mpsc::Sender<ServerFrame>,
    cursor: neha::seq::Seq,
    limit: Option<usize>,
    account: Id,
) -> Result<neha::seq::Seq, StreamError> {
    let page_size = limit.unwrap_or(256).clamp(1, 256);
    let mut cursor = cursor;
    let mut sent = false;
    loop {
        let page = reader
            .events_from(cursor, page_size)
            .await
            .map_err(|error| {
                eprintln!("neha: subscription read failed: {error}");
                StreamError::Store
            })?;
        if page.events.is_empty() {
            if page.next > cursor {
                cursor = page.next;
                continue;
            }
            if !sent {
                out.send(ServerFrame::Snapshot {
                    cursor,
                    events: Vec::new(),
                })
                .await
                .map_err(|_| StreamError::Closed)?;
            }
            return Ok(cursor);
        }
        let full_page = page.events.len() == page_size;
        let first = page.events[0].seq;
        let page_next = page.next;
        // Only the account's own events and the Space-wide ones go into a
        // snapshot; a filtered event still counts as scanned, so the cursor
        // never repeats it.
        let mut events: Vec<neha::record::Event> = page
            .events
            .into_iter()
            .filter(|event| visible_to(account, event))
            .collect();
        if events.is_empty() {
            cursor = page_next;
            if full_page {
                continue;
            }
            if !sent {
                out.send(ServerFrame::Snapshot {
                    cursor,
                    events: Vec::new(),
                })
                .await
                .map_err(|_| StreamError::Closed)?;
            }
            return Ok(cursor);
        }
        let complete = events.len();
        while !events.is_empty() {
            let candidate = if events.len() == complete {
                page_next
            } else {
                events.last().map(|event| event.seq).unwrap_or(cursor)
            };
            let frame = ServerFrame::Snapshot {
                cursor: candidate,
                events: events.clone(),
            };
            if frame_fits(&frame) {
                break;
            }
            events.pop();
        }
        if events.is_empty() {
            return Err(StreamError::Oversized(first));
        }
        let next = events.last().map(|event| event.seq).unwrap_or(page_next);
        // The chunk's cursor is where scanning resumes: the whole page when
        // its events fit, otherwise the last event the chunk holds.
        let next = if complete == events.len() {
            page_next
        } else {
            next
        };
        out.send(ServerFrame::Snapshot {
            cursor: next,
            events,
        })
        .await
        .map_err(|_| StreamError::Closed)?;
        sent = true;
        // A full page or a trimmed page leaves more to read.
        if !full_page && next >= page_next {
            return Ok(next);
        }
        cursor = next;
    }
}

/// Returns whether one account may see an event.
///
/// This milestone has no Groups: an event belongs to the User who wrote it,
/// to the User a Bot wrote it for, or to the Space itself.
fn visible_to(account: Id, event: &neha::record::Event) -> bool {
    if matches!(event.author, neha::record::Author::User(author) if author == account) {
        return true;
    }
    if event.acting == Some(account) {
        return true;
    }

    matches!(
        event.kind,
        neha::record::EventKind::UserJoined | neha::record::EventKind::ProviderAdded
    )
}

/// An authenticated account.
pub struct Authed {
    /// The account the request's token belongs to.
    pub account: Account,
}

impl FromRequestParts<Arc<SpaceServer>> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        server: &Arc<SpaceServer>,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers)
            .or_else(|| {
                let browser_host = parts
                    .headers
                    .get(HOST)
                    .and_then(|value| value.to_str().ok())
                    .and_then(split_authority)
                    .is_some_and(|(host, _)| host.eq_ignore_ascii_case(&server.browser_host));
                browser_host
                    .then(|| cookie_token(&parts.headers, &server.session_cookie))
                    .flatten()
            })
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    ErrorCode::Unauthorized,
                    "a session token is required",
                )
            })?;
        let account = server
            .identity
            .authenticate(token)
            .await
            .map_err(identity_error)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    ErrorCode::Unauthorized,
                    "the token is not valid",
                )
            })?;

        Ok(Self { account })
    }
}

/// Heals a pairing a crash left between its claim and its Computer's record.
///
/// `computer_add` claims the code, appends the Computer and its command result,
/// then finishes the claim by writing the machine's token. A crash between the
/// steps leaves one of two states, and this settles both before the Space
/// accepts connections:
///
/// - the claim has no recorded Computer: roll it back, so the code works again
///   and the machine keeps polling pending;
/// - the record has the Computer but the claim is not finished: write the
///   token, so the machine becomes approved and can authenticate.
async fn reconcile_pairings(server: &Arc<SpaceServer>) -> Result<(), ServerError> {
    for claim in server.identity.pairing_claims().await? {
        let recorded = {
            let state = server.state.lock().map_err(|_| {
                ServerError::Store(StoreError::Corrupt("the Space state is locked".to_owned()))
            })?;
            state
                .computers
                .get(&claim.computer)
                .is_some_and(|computer| computer.owner == claim.owner)
        };
        if recorded {
            if !claim.finalized
                && !server
                    .identity
                    .finalize_pairing(claim.owner, claim.computer)
                    .await?
            {
                eprintln!("neha: a recorded Computer's pairing claim vanished while reconciling");
            }
        } else {
            server
                .identity
                .release_pairing(claim.owner, claim.computer)
                .await?;
        }
    }

    Ok(())
}

/// Finishes runs the previous Space run left working.
///
/// A one-shot run is answered or failed; it is never silently dropped. The
/// Bot says the work was interrupted and the command that started the run
/// receives that failure as its result. This runs after the Space owns its
/// port and before it serves, so a start that cannot listen writes nothing.
async fn recover_interrupted(store: &Store, state: &Mutex<SpaceState>) -> Result<(), StoreError> {
    // A run whose action waited keeps waiting when its approval is undecided;
    // an allowed one is left for the pump to resume. Anything else that was
    // still running when the Space stopped is recovered from its record: the
    // pump replays the completed steps and verifies or refuses an action that
    // started and never returned. A record that cannot be replayed fails the
    // run here, with its own message, instead of being dropped or rerun.
    let interrupted: Vec<crate::state::RunView> = {
        let state = state
            .lock()
            .map_err(|_| StoreError::Corrupt("the Space state is locked".to_owned()))?;
        state
            .runs
            .values()
            .filter(|run| run.status == crate::state::RunStatus::Running)
            .filter(|run| !state.is_resumable(run))
            .cloned()
            .collect()
    };
    for run in interrupted {
        match commands::prepare_recovery(state, &run) {
            Ok(plan) => {
                let mut state = state
                    .lock()
                    .map_err(|_| StoreError::Corrupt("the Space state is locked".to_owned()))?;
                if matches!(plan, commands::RecoveryPlan::Final { .. }) {
                    state.mark_final_recovered(run.id);
                } else {
                    state.mark_recovered(run.id);
                }
            }
            Err(reason) => {
                fail_interrupted(store, state, &run, &reason).await?;
            }
        }
    }

    Ok(())
}

/// Records an interrupted run the record cannot replay as failed.
async fn fail_interrupted(
    store: &Store,
    state: &Mutex<SpaceState>,
    run: &crate::state::RunView,
    reason: &str,
) -> Result<(), StoreError> {
    let message = Id::new();
    let text = format!("I could not continue after the interruption: {reason}.");
    let error = format!("the Space stopped before the run finished: {reason}");
    let usage = commands::known_usage(state, run.id);
    let drafts = vec![
        EventDraft::new(
            EventKind::MessageAdded,
            Author::Bot(run.bot),
            serde_json::json!({"message": message, "chat": run.chat, "text": text, "run": run.id}),
        )
        .acting(run.acting),
        EventDraft::new(
            EventKind::RunFinished,
            Author::Bot(run.bot),
            serde_json::json!({
                "run": run.id,
                "status": crate::state::RunStatus::Failed,
                "reply": message,
                "usage": usage,
                "error": error,
            }),
        )
        .acting(run.acting),
    ];
    let appended = store
        .append(
            AppendRequest::new(Id::new(), format!("recover:{}", run.id), drafts).with_confirmation(
                run.command,
                serde_json::json!({
                    "run": run.id,
                    "message": run.message,
                    "status": "failed",
                    "reply": message,
                    "text": text,
                    "error": error,
                }),
            ),
        )
        .await?;
    let mut state = state
        .lock()
        .map_err(|_| StoreError::Corrupt("the Space state is locked".to_owned()))?;
    for event in &appended.events {
        state.apply(event).map_err(StoreError::Corrupt)?;
    }

    Ok(())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;

    value.strip_prefix("Bearer ").map(str::trim)
}

fn cookie_token<'a>(headers: &'a HeaderMap, cookie_name: &str) -> Option<&'a str> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|cookie| {
        let (name, value) = cookie.trim().split_once('=')?;
        (name == cookie_name).then_some(value)
    })
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::Forbidden, message)
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidArgument, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, message)
    }

    pub(crate) fn idempotency_conflict(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            message,
        )
    }

    pub(crate) fn unsupported_version(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::UnsupportedVersion, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        eprintln!("neha: internal error: {}", message.into());
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "the Space hit an internal error",
        )
    }

    pub(crate) fn code(&self) -> ErrorCode {
        self.code
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "code": self.code,
            "message": self.message,
        });

        (self.status, Json(body)).into_response()
    }
}

impl From<BlobsError> for ApiError {
    fn from(error: BlobsError) -> Self {
        if error.is_unavailable() {
            // The store is down, not broken: a caller may retry.
            eprintln!("neha: unavailable: {error}");
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "the File store is unavailable",
            );
        }

        ApiError::internal(error.to_string())
    }
}

pub(crate) fn identity_error(error: IdentityError) -> ApiError {
    match error {
        IdentityError::InvalidName | IdentityError::WeakPassword => ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidArgument,
            error.to_string(),
        ),
        IdentityError::NameTaken | IdentityError::AlreadyInitialized => {
            ApiError::new(StatusCode::CONFLICT, ErrorCode::Conflict, error.to_string())
        }
        IdentityError::InvalidCredentials => ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "invalid credentials",
        ),
        other => ApiError::internal(other.to_string()),
    }
}

struct Invite {
    secret: String,
    digest: [u8; 32],
}

impl Invite {
    fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let secret = URL_SAFE_NO_PAD.encode(bytes);
        let digest = digest(&secret);

        Self { secret, digest }
    }

    fn accepts(&self, secret: &str) -> bool {
        digest(secret).ct_eq(&self.digest).into()
    }
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

#[derive(Default)]
struct LoginLimiter {
    failures: HashMap<String, Vec<Instant>>,
    sources: HashMap<IpAddr, Vec<Instant>>,
}

impl LoginLimiter {
    fn allows(&mut self, display_name: &str, source: IpAddr) -> bool {
        self.prune();
        let account_ok = self
            .failures
            .get(display_name)
            .is_none_or(|failures| failures.len() < LOGIN_ATTEMPTS);
        let source_ok = self
            .sources
            .get(&source)
            .is_none_or(|failures| failures.len() < SOURCE_ATTEMPTS);
        let room_for_name =
            self.failures.contains_key(display_name) || self.failures.len() < LOGIN_NAMES;

        account_ok && source_ok && room_for_name
    }

    fn record(&mut self, display_name: &str, source: IpAddr) {
        let now = Instant::now();
        self.failures
            .entry(display_name.to_owned())
            .or_default()
            .push(now);
        self.sources.entry(source).or_default().push(now);
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.failures.retain(|_, failures| {
            failures.retain(|at| now.duration_since(*at) < LOGIN_WINDOW);
            !failures.is_empty()
        });
        self.sources.retain(|_, failures| {
            failures.retain(|at| now.duration_since(*at) < LOGIN_WINDOW);
            !failures.is_empty()
        });
    }
}

/// The Space's memory of pairing attempts that are running or failed.
///
/// A pairing code is cheap to guess at if a caller may fire attempts in
/// parallel: two connections could both read a count below the limit and both
/// proceed. An attempt therefore takes its place before any asynchronous work
/// starts, and releases it once the attempt ends. Only failed attempts stay
/// counted for the window; a success frees its place at once.
///
/// The book lives in memory, so it resets when the Space restarts. Each
/// account has its own count; unrelated accounts never block one another.
#[derive(Default)]
pub struct PairingAttempts {
    /// Attempts that started and have not settle()d yet, by account.
    running: HashMap<Id, Vec<Instant>>,
    /// Failed attempts by account, inside the window.
    failures: HashMap<Id, Vec<Instant>>,
}

impl PairingAttempts {
    /// Takes one attempt place for an account before its lookup runs.
    ///
    /// Returns `false` when the account already has a full window of attempts
    /// or failed ones. The caller must call [`settle`](Self::settle) whatever
    /// the attempt's outcome, or its place stays taken until the window moves.
    pub(crate) fn reserve(&mut self, account: Id) -> bool {
        self.prune();
        let attempts = self.failures.get(&account).map_or(0, Vec::len)
            + self.running.get(&account).map_or(0, Vec::len);
        if attempts >= PAIRING_ATTEMPTS {
            return false;
        }
        self.running
            .entry(account)
            .or_default()
            .push(Instant::now());

        true
    }

    /// Releases the place [`reserve`](Self::reserve) took.
    ///
    /// A failed attempt stays counted for the window; a successful one or one
    /// whose lookup failed for a reason the User did not cause frees the place.
    pub(crate) fn settle(&mut self, account: Id, failed: bool) {
        if let Some(running) = self.running.get_mut(&account) {
            running.pop();
            if running.is_empty() {
                self.running.remove(&account);
            }
        }
        if failed {
            self.failures
                .entry(account)
                .or_default()
                .push(Instant::now());
        }
    }

    /// Drops attempts and failures that aged out of their window.
    fn prune(&mut self) {
        let now = Instant::now();
        self.running.retain(|_, attempts| {
            attempts.retain(|at| now.duration_since(*at) < PAIRING_WINDOW);
            !attempts.is_empty()
        });
        self.failures.retain(|_, attempts| {
            attempts.retain(|at| now.duration_since(*at) < PAIRING_WINDOW);
            !attempts.is_empty()
        });
    }
}

/// The server cannot start.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The record failed.
    #[error("record error: {0}")]
    Store(#[from] StoreError),
    /// Identity failed.
    #[error("identity error: {0}")]
    Identity(#[from] IdentityError),
    /// The listener failed.
    #[error("listener error: {0}")]
    Io(#[from] std::io::Error),
    /// The secrets cannot be loaded.
    #[error("secrets error: {0}")]
    Secrets(#[from] SecretsError),
    /// The Space's configuration cannot be used.
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
    /// The File bytes cannot be stored.
    #[error("blobs error: {0}")]
    Blobs(#[from] BlobsError),
    /// The configured File store cannot be used.
    #[error("the File store {storage} cannot be used: {source}")]
    FileStore {
        /// Where the Space was told to store the bytes.
        storage: Box<Storage>,
        /// Why the store cannot be used.
        #[source]
        source: Box<BlobsError>,
    },
    /// The model client cannot be built.
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    /// The browser client has no entry point.
    #[error("the browser client at {} has no index.html", .0.display())]
    Ui(PathBuf),
    /// The browser client directory overlaps the Space home.
    #[error("the browser client at {} overlaps the Space home", .0.display())]
    UiOverlapsHome(PathBuf),
    /// The browser client directory overlaps the machine's configuration and
    /// credential directory, where the local pairing ticket is written.
    #[error(
        "the browser client at {} overlaps the Neha configuration and credential directories, \
         including the machine configuration directory",
        .0.display()
    )]
    UiOverlapsConfiguration(PathBuf),
    /// The local pairing ticket cannot be issued or cleared.
    #[error("local ticket error: {0}")]
    Ticket(#[from] TicketError),
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{Running, ServerError, SpaceServer, StreamError, catch_up, start, start_with_ui};
    use crate::blobs::test_s3;
    use crate::config::S3Storage;
    use crate::home::Home;
    use crate::store::{AppendRequest, Store};

    async fn running() -> (tempfile::TempDir, Running, String) {
        let dir = tempfile::tempdir().unwrap();
        let home = test_home(dir.path());
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");

        (dir, running, invite)
    }

    /// Starts a fresh Space whose `config.toml` is written first.
    async fn running_with_config(config: &str) -> (tempfile::TempDir, Running, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let running = start(test_home(dir.path()), 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");

        (dir, running, invite)
    }

    /// Returns the `config.toml` that points a Space at one S3 test prefix.
    fn s3_config(storage: &S3Storage) -> String {
        format!(
            "[storage]\nbackend = \"s3\"\nbucket = \"{}\"\nregion = \"{}\"\n\
             prefix = \"{}\"\nendpoint = \"{}\"\n",
            storage.bucket, storage.region, storage.prefix, storage.endpoint
        )
    }

    async fn join(
        running: &Running,
        invite: &str,
        name: &str,
        password: &str,
    ) -> serde_json::Value {
        let secret = invite.split_once('#').unwrap().1;
        reqwest::Client::new()
            .post(format!("{}/api/join", running.url()))
            .json(&json!({"secret": secret, "display_name": name, "password": password}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn joins_the_first_admin_and_signs_in() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();

        let me = reqwest::Client::new()
            .get(format!("{}/api/me", running.url()))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let account: serde_json::Value = me.json().await.unwrap();
        assert_eq!(account["display_name"], "Ada");
        assert_eq!(account["is_admin"], true);

        let wrong = reqwest::Client::new()
            .post(format!("{}/api/login", running.url()))
            .json(&json!({"display_name": "Ada", "password": "wrong password"}))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        let right = reqwest::Client::new()
            .post(format!("{}/api/login", running.url()))
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();
        assert_eq!(right.status(), reqwest::StatusCode::OK);

        let again = join(&running, &invite, "Grace", "correct horse battery").await;
        assert!(again.get("token").is_none());

        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_requires_a_token_and_answers_hello() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let socket = format!("ws://{}/api/socket", running.addr);

        let anonymous = tokio_tungstenite::connect_async(socket.clone()).await;
        assert!(anonymous.is_err());

        let mut request = socket.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&ClientFrame::Hello {
                    version: PROTOCOL_VERSION,
                    capabilities: Vec::new(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap();
        let frame: ServerFrame = serde_json::from_str(reply.to_text().unwrap()).unwrap();

        assert!(matches!(frame, ServerFrame::Hello { version, .. } if version == PROTOCOL_VERSION));
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscription_returns_the_record() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut request = format!("ws://{}/api/socket", running.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let frames = [
            ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capabilities: Vec::new(),
            },
            ClientFrame::Subscribe {
                id: neha::id::Id::new(),
                cursor: None,
                limit: Some(10),
            },
        ];
        for frame in frames {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&frame).unwrap().into(),
                ))
                .await
                .unwrap();
        }
        let _hello = socket.next().await.unwrap().unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        let frame: ServerFrame = serde_json::from_str(frame.to_text().unwrap()).unwrap();

        match frame {
            ServerFrame::Snapshot { events, .. } => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].kind, neha::record::EventKind::UserJoined);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    use neha::error::ErrorCode;
    use neha::file::MAX_FILE_BYTES;
    use neha::id::Id;
    use neha::record::{Author, EventDraft, EventKind};
    use neha::seq::Seq;
    use neha::surface::{
        BotComputer, BotCreate, CommandCall, MessageSend, ModelSpec, ProviderAdd, ProviderProtocol,
    };
    use serde_json::Value;
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

    async fn fake_provider() -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task)
    }

    /// Holds a scripted reply until the test opens its gate.
    #[derive(Debug)]
    struct Gate(std::sync::Arc<tokio::sync::watch::Sender<bool>>);

    impl Gate {
        fn open(&self) {
            let _ = self.0.send(true);
        }
    }

    async fn wait_for_gate(mut gate: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *gate.borrow_and_update() {
                break;
            }
            if gate.changed().await.is_err() {
                break;
            }
        }
    }

    #[derive(Clone)]
    enum ScriptedReply {
        Text(String),
        Hold {
            text: String,
        },
        Tool {
            name: String,
            arguments: Value,
        },
        RawTool {
            name: String,
            arguments: String,
        },
        TruncatedTool {
            name: String,
            arguments: Value,
        },
        TruncatedToolWithUsage {
            name: String,
            arguments: Value,
            prompt: u64,
            completion: u64,
        },
        StreamEndedUsage {
            text: String,
        },
        TextWithUsage {
            text: String,
            prompt: u64,
            completion: u64,
        },
    }

    /// A scripted Provider whose replies run in order; a `Hold` reply waits
    /// for the test to open the returned gate, so a test can keep a run in
    /// flight without sleeping.
    async fn gated_provider(
        script: Vec<ScriptedReply>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        Vec<Gate>,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::Json;
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let mut gates = Vec::new();
        let mut waits: Vec<Option<tokio::sync::watch::Receiver<bool>>> = Vec::new();
        for reply in &script {
            if matches!(reply, ScriptedReply::Hold { .. }) {
                let (sender, receiver) = tokio::sync::watch::channel(false);
                gates.push(Gate(Arc::new(sender)));
                waits.push(Some(receiver));
            } else {
                waits.push(None);
            }
        }
        let waits = Arc::new(waits);
        let script = Arc::new(script);
        let requests = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let script = Arc::clone(&script);
                let requests = Arc::clone(&requests);
                let waits = Arc::clone(&waits);
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(body);
                    let index = requests.fetch_add(1, Ordering::SeqCst);
                    let chosen = index.min(script.len().saturating_sub(1));
                    let reply = script.get(chosen).cloned().expect("a scripted reply");
                    let body = match reply {
                        ScriptedReply::Text(text) => {
                            let chunk = json!({"choices":[{"delta":{"content":text}}]});
                            format!("data: {chunk}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::Hold { text } => {
                            let gate = waits[chosen].clone().expect("a Hold reply has a gate");
                            wait_for_gate(gate).await;
                            let chunk = json!({"choices":[{"delta":{"content":text}}]});
                            format!("data: {chunk}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::Tool { name, arguments } => {
                            let chunk = json!({"choices":[{"delta":{"tool_calls":[{
                                "index": 0,
                                "id": format!("call-{index}"),
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(&arguments).unwrap(),
                                }
                            }]}}]});
                            format!("data: {chunk}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::RawTool { name, arguments } => {
                            let chunk = json!({"choices":[{"delta":{"tool_calls":[{
                                "index": 0,
                                "id": format!("call-{index}"),
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }]}}]});
                            format!("data: {chunk}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::TruncatedToolWithUsage {
                            name,
                            arguments,
                            prompt,
                            completion,
                        } => {
                            let chunk = json!({"choices":[{
                                "delta": {"tool_calls":[{
                                    "index": 0,
                                    "id": format!("call-{index}"),
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(&arguments).unwrap(),
                                    },
                                }]},
                                "finish_reason": "length",
                            }]});
                            let usage = json!({"choices":[],"usage":{
                                "prompt_tokens": prompt,
                                "completion_tokens": completion,
                            }});
                            format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::StreamEndedUsage { text } => {
                            let chunk = json!({"choices":[{"delta":{"content":text}}]});
                            let usage = json!({"choices":[],"usage":{
                                "prompt_tokens": 40,
                                "completion_tokens": 12,
                            }});
                            format!("data: {chunk}\n\ndata: {usage}\n\n")
                        }
                        ScriptedReply::TextWithUsage {
                            text,
                            prompt,
                            completion,
                        } => {
                            let chunk = json!({"choices":[{"delta":{"content":text}}]});
                            let usage = json!({"choices":[],"usage":{
                                "prompt_tokens": prompt,
                                "completion_tokens": completion,
                            }});
                            format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
                        }
                        ScriptedReply::TruncatedTool { name, arguments } => {
                            let chunk = json!({"choices":[{
                                "delta": {"tool_calls":[{
                                    "index": 0,
                                    "id": format!("call-{index}"),
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(&arguments).unwrap(),
                                    },
                                }]},
                                "finish_reason": "length",
                            }]});
                            let usage = json!({"choices":[],"usage":{
                                "prompt_tokens": 40,
                                "completion_tokens": 12,
                            }});
                            format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
                        }
                    };
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task, gates, recorded)
    }

    async fn scripted_provider(
        script: Vec<ScriptedReply>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let (base, task, _gates, _recorded) = gated_provider(script).await;

        (base, task)
    }

    fn hold(text: &str) -> ScriptedReply {
        ScriptedReply::Hold {
            text: text.to_owned(),
        }
    }

    fn tool(name: &str, arguments: Value) -> ScriptedReply {
        ScriptedReply::Tool {
            name: name.to_owned(),
            arguments,
        }
    }

    fn text(text: &str) -> ScriptedReply {
        ScriptedReply::Text(text.to_owned())
    }

    fn raw_tool(name: &str, arguments: String) -> ScriptedReply {
        ScriptedReply::RawTool {
            name: name.to_owned(),
            arguments,
        }
    }

    fn truncated_tool(name: &str, arguments: Value) -> ScriptedReply {
        ScriptedReply::TruncatedTool {
            name: name.to_owned(),
            arguments,
        }
    }

    async fn open_socket(running: &Running, token: &str) -> Socket {
        let mut request = format!("ws://{}/api/socket", running.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Hello {
                    version: PROTOCOL_VERSION,
                    capabilities: Vec::new(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let _hello = next_frame(&mut socket).await;
        socket
    }

    async fn next_frame(socket: &mut Socket) -> ServerFrame {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let Message::Text(text) = message else {
                continue;
            };
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }

    async fn try_command(
        socket: &mut Socket,
        id: Id,
        command: CommandCall,
    ) -> Result<(Option<Seq>, Value), (ErrorCode, String)> {
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command { id, command })
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
        let mut committed = None;
        loop {
            match next_frame(socket).await {
                ServerFrame::Accepted { .. } => {}
                ServerFrame::Committed { seq, .. } => committed = Some(seq),
                ServerFrame::Confirmed { result, .. } => return Ok((committed, result)),
                ServerFrame::Error { code, message, .. } => return Err((code, message)),
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn command(socket: &mut Socket, id: Id, command: CommandCall) -> Value {
        try_command(socket, id, command).await.unwrap().1
    }

    fn provider_add(base_url: String) -> CommandCall {
        CommandCall::ProviderAdd(ProviderAdd {
            protocol: ProviderProtocol::ChatCompletions,
            base_url,
            api_key: "sk-test".to_owned(),
            models: vec![ModelSpec {
                name: "test-model".to_owned(),
                context_limit: 128_000,
                max_output: 8_192,
                reads_images: false,
            }],
            default_model: Some("test-model".to_owned()),
        })
    }

    async fn admin(running: &Running, invite: &str) -> (Socket, String) {
        let session = join(running, invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let socket = open_socket(running, &token).await;

        (socket, token)
    }

    async fn admin_socket(running: &Running, invite: &str) -> Socket {
        admin(running, invite).await.0
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_round_trip_writes_messages_and_a_run() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;

        let provider = command(&mut socket, Id::new(), provider_add(base)).await;
        let provider_id: Id = serde_json::from_value(provider["provider"].clone()).unwrap();
        assert!(
            provider["models"]
                .as_array()
                .is_some_and(|models| models.len() == 1)
        );

        let models = command(&mut socket, Id::new(), CommandCall::ModelList).await;
        assert_eq!(models.as_array().unwrap().len(), 1);
        assert_eq!(models[0]["is_default"], true);
        assert_eq!(models[0]["provider"], serde_json::json!(provider_id));

        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let bot_id: Id = serde_json::from_value(bot["bot"]["id"].clone()).unwrap();
        assert_eq!(bot["computer"]["kind"], "hosted");

        let sent = send(&mut socket, chat, "hello").await;
        assert_eq!(sent["status"], "succeeded");
        assert_eq!(sent["text"], "Hello from fake");
        let run = run_of(&sent);

        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        assert_eq!(messages[1]["text"], "Hello from fake");

        let shown = command(&mut socket, Id::new(), CommandCall::RunShow { run }).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["bot"], serde_json::json!(bot_id));

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_without_a_provider_cannot_work() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        let result = try_command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;

        assert!(matches!(result, Err((ErrorCode::InvalidArgument, _))));
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_command_id_is_not_run_twice() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let command_id = Id::new();
        let create = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: None,
        });

        let (first_seq, first) = try_command(&mut socket, command_id, create.clone())
            .await
            .unwrap();
        let (second_seq, second) = try_command(&mut socket, command_id, create).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first_seq, second_seq);
        let bots = command(&mut socket, Id::new(), CommandCall::BotList).await;
        assert_eq!(bots.as_array().unwrap().len(), 1);
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_keeps_the_record_and_the_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let first = start(home.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let mut socket = admin_socket(&first, &invite).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        socket.close(None).await.unwrap();
        first.shutdown().await;

        let second = start(home, 0).await.unwrap();
        let login: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/api/login", second.url()))
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut socket = open_socket(&second, login["token"].as_str().unwrap()).await;
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;

        assert!(second.invite.is_none());
        assert_eq!(messages.as_array().unwrap().len(), 0);
        let bots = command(&mut socket, Id::new(), CommandCall::BotList).await;
        assert_eq!(bots.as_array().unwrap().len(), 1);
        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_wrong_passwords_are_rate_limited() {
        let (_dir, running, invite) = running().await;
        let _session = join(&running, &invite, "Ada", "correct horse battery").await;
        let client = reqwest::Client::new();
        for _ in 0..5 {
            let wrong = client
                .post(format!("{}/api/login", running.url()))
                .json(&json!({"display_name": "Ada", "password": "wrong password"}))
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
        }

        let limited = client
            .post(format!("{}/api/login", running.url()))
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();

        assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let body: Value = limited.json().await.unwrap();
        assert_eq!(body["code"], "rate_limited");
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_is_rate_limited_across_names() {
        let (_dir, running, invite) = running().await;
        let _session = join(&running, &invite, "Ada", "correct horse battery").await;
        let client = reqwest::Client::new();
        for index in 0..20 {
            let wrong = client
                .post(format!("{}/api/login", running.url()))
                .json(
                    &json!({"display_name": format!("name-{index}"), "password": "wrong password"}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
        }

        let limited = client
            .post(format!("{}/api/login", running.url()))
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();

        assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        running.shutdown().await;
    }

    async fn raw_socket(running: &Running, token: &str) -> Socket {
        let mut request = format!("ws://{}/api/socket", running.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_rejects_a_bad_greeting() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut malformed = raw_socket(&running, token).await;
        malformed
            .send(Message::Text("not a frame".into()))
            .await
            .unwrap();
        assert!(matches!(
            next_frame(&mut malformed).await,
            ServerFrame::ProtocolError { .. }
        ));

        let mut outdated = raw_socket(&running, token).await;
        outdated
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Hello {
                    version: PROTOCOL_VERSION + 1,
                    capabilities: Vec::new(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            next_frame(&mut outdated).await,
            ServerFrame::ProtocolError { .. }
        ));
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_refuses_an_oversized_frame() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let oversized = "x".repeat(neha::surface::MAX_FRAME_BYTES + 1);

        let _ = socket.send(Message::Text(oversized.into())).await;
        let next = socket.next().await;

        assert!(!matches!(next, Some(Ok(Message::Text(_)))));
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cursor_skips_the_events_it_already_holds() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;

        let first = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let _chat: Id = serde_json::from_value(first["chat"]["id"].clone()).unwrap();

        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ServerFrame::Snapshot { cursor, events } = next_frame(&mut socket).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(events.len(), 4);

        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: Some(cursor),
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ServerFrame::Snapshot { cursor, events } = next_frame(&mut socket).await else {
            panic!("expected a snapshot");
        };
        assert!(events.is_empty());
        assert_eq!(cursor, neha::seq::Seq::from_i64(4).unwrap());
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_answers_an_interrupted_run() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let store = Store::open(&home).await.unwrap();
        let user = Id::new();
        let bot = Id::new();
        let computer = Id::new();
        let chat = Id::new();
        let message = Id::new();
        let run = Id::new();
        let command = Id::new();
        let drafts = vec![
            EventDraft::new(
                EventKind::UserJoined,
                Author::User(user),
                json!({"display_name": "Ada", "is_admin": true}),
            ),
            EventDraft::new(
                EventKind::ComputerCreated,
                Author::User(user),
                json!({"computer": computer, "owner": user, "kind": "managed", "workspace": "/tmp/workspace"}),
            ),
            EventDraft::new(
                EventKind::BotCreated,
                Author::User(user),
                json!({"bot": bot, "owner": user, "name": "Scout", "computer": computer, "chat": chat}),
            ),
            EventDraft::new(
                EventKind::ChatCreated,
                Author::User(user),
                json!({"chat": chat, "owner": user, "bot": bot}),
            ),
            EventDraft::new(
                EventKind::MessageAdded,
                Author::User(user),
                json!({"message": message, "chat": chat, "text": "hello", "run": run}),
            ),
            EventDraft::new(
                EventKind::RunStarted,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "chat": chat,
                    "bot": bot,
                    "message": message,
                    "command": command,
                }),
            )
            .acting(user),
            EventDraft::new(
                EventKind::RunStep,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "step": 1,
                    "usage": {"uncached_input": 5, "cache_read": 0, "cache_write": 0, "output": 2, "reasoning": 0},
                    "tool_calls": 0,
                }),
            )
            .acting(user),
        ];
        store
            .append(AppendRequest::new(command, "seed", drafts))
            .await
            .unwrap();
        store.close().await.unwrap();

        let running = start(home, 0).await.unwrap();
        {
            let state = running.server.state.lock().unwrap();
            let recovered = &state.runs[&run];

            assert_eq!(recovered.status, crate::state::RunStatus::Failed);
            assert_eq!(recovered.usage.unwrap().budgeted(), 7);
            assert_eq!(recovered.reply, Some(state.messages.last().unwrap().id));
            assert_eq!(
                state.messages.last().unwrap().text,
                "I could not continue after the interruption: step 1 has no recorded reply."
            );
        }
        let recorded = running
            .server
            .store
            .command(command)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorded.result.unwrap()["status"], "failed");
        running.shutdown().await;
    }

    async fn raw_http(addr: std::net::SocketAddr, request: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_checks_host_and_origin() {
        let (_dir, running, invite) = running().await;
        assert!(running.addr.ip().is_loopback());

        let evil_host = raw_http(
            running.addr,
            "GET /healthz HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(evil_host.contains("403"), "{evil_host}");
        let loopback = raw_http(
            running.addr,
            "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(loopback.contains("200"), "{loopback}");
        for bad in ["[::1]junk", "127.0.0.1:", "evil.example:7337"] {
            let response = raw_http(
                running.addr,
                &format!("GET /healthz HTTP/1.1\r\nHost: {bad}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(response.contains("403"), "{bad}: {response}");
        }

        let browser = reqwest::Client::new()
            .get(format!("{}/healthz", running.url()))
            .header("origin", "http://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(browser.status(), reqwest::StatusCode::FORBIDDEN);

        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let native = reqwest::Client::new()
            .get(format!("{}/api/me", running.url()))
            .bearer_auth(token)
            .header("origin", "http://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(native.status(), reqwest::StatusCode::OK);
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_command_gets_accepted_committed_and_confirmed() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: Id::new(),
                    command: CommandCall::BotCreate(BotCreate {
                        name: "Scout".to_owned(),
                        computer: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();

        let mut seen = Vec::new();
        loop {
            match next_frame(&mut socket).await {
                ServerFrame::Heartbeat { .. } => {}
                frame => {
                    let last = matches!(frame, ServerFrame::Confirmed { .. });
                    seen.push(frame);
                    if last {
                        break;
                    }
                }
            }
        }

        assert!(matches!(seen[0], ServerFrame::Accepted { .. }));
        assert!(matches!(seen[1], ServerFrame::Committed { .. }));
        assert!(matches!(seen.last(), Some(ServerFrame::Confirmed { .. })));
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_command_with_new_parameters_conflicts() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let command_id = Id::new();

        command(
            &mut socket,
            command_id,
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let conflict = try_command(
            &mut socket,
            command_id,
            CommandCall::BotCreate(BotCreate {
                name: "Other".to_owned(),
                computer: None,
            }),
        )
        .await;

        assert!(matches!(conflict, Err((ErrorCode::IdempotencyConflict, _))));
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    async fn recording_provider() -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        use std::sync::Arc;
        use std::sync::Mutex;

        use axum::Json;
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(request): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded.lock().unwrap().push(request);
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task, seen)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_next_message_sees_the_previous_one() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, seen) = recording_provider().await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        send(&mut socket, chat, "hello").await;
        send(&mut socket, chat, "again").await;

        {
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let messages = requests[1]["messages"].as_array().unwrap();
            let texts: Vec<_> = messages
                .iter()
                .map(|message| message["content"].as_str().unwrap_or_default())
                .collect();
            assert!(texts.contains(&"hello"));
            assert!(texts.contains(&"Hello from fake"));
            assert!(texts.contains(&"again"));
        }
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    fn provider_base(base_url: String) -> CommandCall {
        provider_add(base_url)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replaying_create_commands_has_no_side_effects() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (provider, provider_task) = fake_provider().await;
        let provider_command = Id::new();
        let add = provider_base(provider);

        command(&mut socket, provider_command, add.clone()).await;
        command(&mut socket, provider_command, add).await;

        let models = command(&mut socket, Id::new(), CommandCall::ModelList).await;
        assert_eq!(models.as_array().unwrap().len(), 1);
        assert_eq!(models[0]["is_default"], true);

        let create = Id::new();
        let bot_command = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: None,
        });
        let first = command(&mut socket, create, bot_command.clone()).await;
        let second = command(&mut socket, create, bot_command).await;
        assert_eq!(first, second);
        let bots = command(&mut socket, Id::new(), CommandCall::BotList).await;
        assert_eq!(bots.as_array().unwrap().len(), 1);

        let home = running.server_home();
        let keys = std::fs::read_dir(home.join("secrets"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("provider-"))
            .count();
        assert_eq!(keys, 1, "a replay must not leave an orphan Provider key");
        let computers = std::fs::read_dir(home.join("computers")).unwrap().count();
        assert_eq!(computers, 1, "a replay must not leave an orphan workspace");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_send_replay_changes_nothing() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;
        command(&mut socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let send = Id::new();
        let message = CommandCall::MessageSend(MessageSend {
            chat,
            text: "hello".to_owned(),
            file: None,

            cwd: None,
            cwd_writable: None,
        });

        let first = command(&mut socket, send, message.clone()).await;
        assert_eq!(first["status"], "queued");
        let run = run_of(&first);
        let shown = wait_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");

        // A replay after the run ended answers with the run's final result and
        // never queues a second run.
        let second = command(&mut socket, send, message).await;
        assert_eq!(second["run"], first["run"]);
        assert_eq!(second["status"], "succeeded");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_while_the_run_works_waits_for_the_first_result() {
        let (_dir, running, invite) = running().await;
        let (mut first_socket, token) = admin(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("Hello from fake")]).await;
        command(&mut first_socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut first_socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let command_id = Id::new();
        let queued = command(
            &mut first_socket,
            command_id,
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;
        assert_eq!(queued["status"], "queued");
        let run = run_of(&queued);
        wait_status(&mut first_socket, run, "running").await;

        // The run still works; a replay of its command waits for the result.
        let mut second_socket = open_socket(&running, &token).await;
        second_socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: command_id,
                    command: CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "hello".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            match next_frame(&mut second_socket).await {
                ServerFrame::Accepted { id } => {
                    assert_eq!(id, command_id);
                    break;
                }
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
        gates[0].open();
        let result = loop {
            match next_frame(&mut second_socket).await {
                ServerFrame::Confirmed { id, result } => {
                    assert_eq!(id, command_id);
                    break result;
                }
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. }
                | ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        };
        assert_eq!(result["status"], "succeeded");

        let messages = command(
            &mut first_socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        first_socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_provider_leaves_a_failure_message_and_run() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = failing_provider().await;
        command(&mut socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        let result = send(&mut socket, chat, "hello").await;

        assert_eq!(result["status"], "failed");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        assert!(
            messages[1]["text"]
                .as_str()
                .unwrap()
                .contains("could not answer")
        );
        let run = run_of(&result);
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "failed");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_provider_key_is_rejected_before_the_record_changes() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;
        command(&mut socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let home = running.server_home();
        for entry in std::fs::read_dir(home.join("secrets")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with("provider-") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }

        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;

        assert!(matches!(error, Err((ErrorCode::InvalidArgument, _))));
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert!(messages.as_array().unwrap().is_empty());
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    async fn failing_provider() -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::routing::post;

        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_key_never_reaches_disk_in_plaintext() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;
        let secret = "sk-unique-review-secret";

        let mut add = provider_base(base);
        if let CommandCall::ProviderAdd(arguments) = &mut add {
            arguments.api_key = secret.to_owned();
        }
        command(&mut socket, Id::new(), add).await;

        let mut found = Vec::new();
        let mut pending = vec![running.server_home()];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(&path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(bytes) = std::fs::read(&path)
                    && bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes())
                {
                    found.push(path);
                }
            }
        }
        assert!(found.is_empty(), "plaintext key found in {found:?}");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscription_streams_live_events_and_resumes() {
        use neha::record::EventKind;

        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;
        command(&mut socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ServerFrame::Snapshot { cursor, events } = next_frame(&mut socket).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(events.len(), 5);
        assert_eq!(cursor, events.last().unwrap().seq);

        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: Id::new(),
                    command: CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "hello".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let mut live = Vec::new();
        loop {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(5), next_frame(&mut socket))
                    .await
                    .expect("the subscription keeps sending");
            if let ServerFrame::Event { event } = frame {
                assert!(event.seq > cursor);
                let last = matches!(event.kind, EventKind::RunFinished);
                live.push(event);
                if last {
                    break;
                }
            }
        }
        assert!(
            live.iter()
                .any(|event| event.kind == EventKind::MessageAdded)
        );
        assert_eq!(live.last().unwrap().kind, EventKind::RunFinished);
        let resume = live.last().unwrap().seq;

        let mut second = open_socket(&running, &token).await;
        second
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: Some(resume),
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ServerFrame::Snapshot { events, cursor } = next_frame(&mut second).await else {
            panic!("expected a snapshot");
        };
        assert!(events.is_empty());
        assert_eq!(cursor, resume);

        command(
            &mut second,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "again".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;
        loop {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(5), next_frame(&mut second))
                    .await
                    .expect("the subscription keeps sending");
            if let ServerFrame::Event { event } = frame
                && event.kind == EventKind::RunFinished
            {
                assert!(event.seq > resume);
                break;
            }
        }
        socket.close(None).await.unwrap();
        second.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_history_streams_in_pages_without_gaps() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let store = Store::open(&home).await.unwrap();
        let author = Id::new();
        let chat = Id::new();
        let drafts = (0..259)
            .map(|index| {
                EventDraft::new(
                    EventKind::MessageAdded,
                    Author::User(author),
                    json!({
                        "message": Id::new(),
                        "chat": chat,
                        "text": format!("old {index}"),
                        "run": null,
                    }),
                )
            })
            .collect();
        store
            .append(AppendRequest::new(Id::new(), "seed", drafts))
            .await
            .unwrap();
        store.close().await.unwrap();

        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let (mut socket, _token) = admin(&running, &invite).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let history = 1 + 259 + 3;

        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: Id::new(),
                    command: CommandCall::BotCreate(BotCreate {
                        name: "Live".to_owned(),
                        computer: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();

        let mut seen = std::collections::BTreeSet::new();
        let mut snapshots = 0;
        let last = history + 3;
        while seen.last().copied().unwrap_or(0) < last {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(10), next_frame(&mut socket))
                    .await
                    .expect("the stream keeps sending");
            match frame {
                ServerFrame::Snapshot { events, .. } => {
                    snapshots += 1;
                    for event in events {
                        seen.insert(event.seq.as_i64());
                    }
                }
                ServerFrame::Event { event } => {
                    seen.insert(event.seq.as_i64());
                }
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. }
                | ServerFrame::Confirmed { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }

        // The seeded messages belong to nobody in this Space, so the stream
        // skips them; the admin's own events arrive in order and without gaps.
        let recorded = running
            .server
            .store
            .events_from(neha::seq::Seq::ZERO, 10_000)
            .await
            .unwrap()
            .events;
        let admin = recorded
            .iter()
            .find(|event| event.kind == EventKind::UserJoined)
            .and_then(|event| match event.author {
                Author::User(id) => Some(id),
                Author::Bot(_) => None,
            })
            .expect("the admin joined");
        let expected: Vec<i64> = recorded
            .iter()
            .filter(|event| {
                matches!(event.author, Author::User(author) if author == admin)
                    || event.acting == Some(admin)
                    || matches!(event.kind, EventKind::UserJoined | EventKind::ProviderAdded)
            })
            .map(|event| event.seq.as_i64())
            .collect();
        assert!(snapshots >= 1, "the history streams in a snapshot");
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            expected,
            "the stream skipped a visible event or showed a hidden one"
        );
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_event_larger_than_one_frame_is_reported() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let store = Store::open(&home).await.unwrap();
        let user = Id::new();
        store
            .append(AppendRequest::new(
                Id::new(),
                "seed",
                vec![
                    EventDraft::new(
                        EventKind::UserJoined,
                        Author::User(user),
                        json!({"display_name": "Ada", "is_admin": true}),
                    ),
                    EventDraft::new(
                        EventKind::MessageAdded,
                        Author::User(user),
                        json!({
                            "message": Id::new(),
                            "chat": Id::new(),
                            "text": "x".repeat(neha::surface::MAX_FRAME_BYTES + 1024),
                            "run": null,
                        }),
                    ),
                ],
            ))
            .await
            .unwrap();
        let reader = store.reader();
        let (out, mut frames) = tokio::sync::mpsc::channel(8);

        let result = catch_up(&reader, &out, neha::seq::Seq::ZERO, None, user).await;

        assert!(matches!(result, Err(StreamError::Oversized(_))));
        assert!(matches!(
            frames.try_recv(),
            Ok(ServerFrame::Snapshot { .. })
        ));
        assert!(frames.try_recv().is_err());
        store.close().await.unwrap();
    }

    async fn huge_provider() -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let content = "x".repeat(neha::surface::MAX_FRAME_BYTES + 1024);
        let body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n\
             data: [DONE]\n\n"
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let body = body.clone();
                async move {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_oversized_live_event_is_reported_to_the_subscriber() {
        let (_dir, running, invite) = running().await;
        let (mut sender, token) = admin(&running, &invite).await;
        let (base, provider_task) = huge_provider().await;
        command(&mut sender, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut sender,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        let mut subscriber = open_socket(&running, &token).await;
        subscriber
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let _ = next_frame(&mut subscriber).await;
        sender
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: Id::new(),
                    command: CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "answer with a huge reply".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();

        let mut reported = false;
        for _ in 0..50 {
            let frame = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                next_frame(&mut subscriber),
            )
            .await
            .expect("the subscriber is never left waiting silently");
            if let ServerFrame::ProtocolError { message } = frame {
                assert!(message.contains("larger than the frame limit"), "{message}");
                reported = true;
                break;
            }
        }

        assert!(reported, "an oversized live event must be reported");
        sender.close(None).await.unwrap();
        subscriber.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    async fn tool_bot(socket: &mut Socket, base: String) -> (Id, Id, Id) {
        command(socket, Id::new(), provider_add(base)).await;
        let bot = command(
            socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let bot_id: Id = serde_json::from_value(bot["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let computer: Id = serde_json::from_value(bot["computer"]["id"].clone()).unwrap();

        (bot_id, chat, computer)
    }

    /// Sends a Message and returns the command's queued run.
    async fn submit(socket: &mut Socket, chat: Id, text: &str, file: Option<Id>) -> Value {
        command(
            socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: text.to_owned(),
                file,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await
    }

    /// Sends a Message that names the directory the run works in.
    async fn submit_in(socket: &mut Socket, chat: Id, text: &str, cwd: &str) -> Value {
        command(
            socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: text.to_owned(),
                file: None,

                cwd: Some(cwd.to_owned()),
                cwd_writable: Some(true),
            }),
        )
        .await
    }

    fn run_of(result: &Value) -> Id {
        serde_json::from_value(result["run"].clone()).expect("a run identifier")
    }

    /// Waits until a run is in a status and returns its view.
    async fn wait_status(socket: &mut Socket, run: Id, status: &str) -> Value {
        for _ in 0..500 {
            let shown = show_run(socket, run).await;
            if shown["status"] == status {
                return shown;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("run {run} never became {status}");
    }

    /// Waits until a run ends and returns its view.
    async fn wait_run(socket: &mut Socket, run: Id) -> Value {
        for _ in 0..500 {
            let shown = show_run(socket, run).await;
            if matches!(shown["status"].as_str(), Some("succeeded" | "failed")) {
                return shown;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("run {run} did not finish");
    }

    /// Waits for the run a command queued and returns the result a client used
    /// to receive when a command ran its run before confirming.
    async fn finish(socket: &mut Socket, chat: Id, queued: Value) -> Value {
        let run = run_of(&queued);
        let shown = wait_run(socket, run).await;
        let messages = command(
            socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let reply = shown["reply"].clone();
        let text = messages
            .as_array()
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|message| message["id"] == reply)
                    .map(|message| message["text"].clone())
            })
            .unwrap_or(Value::Null);
        json!({
            "run": run,
            "message": shown["message"],
            "status": shown["status"],
            "reply": shown["reply"],
            "text": text,
            "usage": shown["usage"],
        })
    }

    /// Allows the first Local use of a run that waits in a Chat.
    ///
    /// A Local Computer asks before its first action for a Bot and acting
    /// User; tests that exercise the action itself answer that ask first.
    async fn allow_first_use(socket: &mut Socket, chat: Id) -> Id {
        for _ in 0..500 {
            let approvals = command(
                socket,
                Id::new(),
                CommandCall::ApprovalList { chat: Some(chat) },
            )
            .await;
            if let Some(first) = approvals.as_array().and_then(|list| list.first()) {
                let approval: Id = serde_json::from_value(first["approval"].clone()).unwrap();
                command(
                    socket,
                    Id::new(),
                    CommandCall::ApprovalDecide {
                        approval,
                        allow: true,
                        lasting: false,
                    },
                )
                .await;

                return approval;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        panic!("the run never asked for its first Local use");
    }

    async fn send(socket: &mut Socket, chat: Id, text: &str) -> Value {
        let queued = submit(socket, chat, text, None).await;
        finish(socket, chat, queued).await
    }

    async fn send_file(socket: &mut Socket, chat: Id, file: Id) -> Value {
        let queued = submit(socket, chat, "", Some(file)).await;
        finish(socket, chat, queued).await
    }

    async fn show_run(socket: &mut Socket, run: Id) -> Value {
        command(socket, Id::new(), CommandCall::RunShow { run }).await
    }

    /// Reads the record of a stopped Space.
    async fn record(root: &std::path::Path) -> Vec<neha::record::Event> {
        let store = Store::open(&Home::open(root).unwrap()).await.unwrap();
        let events = store.events_from(Seq::ZERO, 10_000).await.unwrap().events;
        store.close().await.unwrap();

        events
    }

    /// Returns the cursor of the one event of a kind that names a run.
    fn event_seq(events: &[neha::record::Event], kind: EventKind, run: Id) -> Seq {
        events
            .iter()
            .find(|event| event.kind == kind && event.data["run"] == json!(run))
            .unwrap_or_else(|| panic!("no {kind:?} event of run {run}"))
            .seq
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_writes_and_reads_a_file_with_tools() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "write_file",
                json!({"path": "hello.txt", "content": "hello neha"}),
            ),
            tool("read_file", json!({"path": "hello.txt"})),
            text("The file says hello neha."),
        ])
        .await;
        let (bot, chat, computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "write hello.txt and read it back").await;

        assert_eq!(result["status"], "succeeded");
        assert!(result["text"].as_str().unwrap().contains("hello neha"));
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
            "hello neha"
        );
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["steps"].as_array().unwrap().len(), 3);
        let actions = shown["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["tool"], "write_file");
        assert_eq!(actions[0]["status"], "succeeded");
        assert_eq!(actions[1]["tool"], "read_file");
        assert!(
            actions[1]["output"]
                .as_str()
                .unwrap()
                .contains("hello neha"),
            "{}",
            actions[1]["output"]
        );
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let messages = messages.as_array().unwrap();
        let user = messages
            .iter()
            .find(|message| message["author"]["kind"] == "user")
            .unwrap();
        let bot_message = messages
            .iter()
            .find(|message| message["author"]["kind"] == "bot")
            .unwrap();
        assert_eq!(bot_message["acting"], user["author"]["id"]);

        // A restart rebuilds the same run from the record.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let restarted = start(test_home(&root), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        let shown_again = show_run(&mut socket, run).await;
        assert_eq!(shown_again["steps"], shown["steps"]);
        assert_eq!(shown_again["actions"], shown["actions"]);
        let _ = bot;
        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_runs_a_shell_command() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "printf 42"})),
            text("The command printed 42."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "run printf 42").await;

        assert_eq!(result["status"], "succeeded");
        assert!(result["text"].as_str().unwrap().contains("42"));
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let actions = shown["actions"].as_array().unwrap();
        assert_eq!(actions[0]["tool"], "shell");
        assert_eq!(actions[0]["status"], "succeeded");
        assert!(
            actions[0]["output"].as_str().unwrap().contains("42"),
            "{}",
            actions[0]["output"]
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tool_cannot_leave_the_workspace() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("read_file", json!({"path": "../../outside.txt"})),
            text("The read was blocked."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;
        std::fs::write(running.server_home().join("outside.txt"), "secret").unwrap();

        let result = send(&mut socket, chat, "read outside.txt").await;

        assert_eq!(result["status"], "succeeded");
        assert!(!result["text"].as_str().unwrap().contains("secret"));
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let action = &shown["actions"][0];
        assert_eq!(action["tool"], "read_file");
        assert_eq!(action["status"], "failed");
        assert!(
            action["output"].as_str().unwrap().contains("escapes"),
            "{}",
            action["output"]
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_that_keeps_calling_tools_stops_at_the_step_limit() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let mut script = vec![tool("list_files", json!({"path": "."})); 8];
        script.push(text("I am still here."));
        let (base, provider_task) = scripted_provider(script).await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let failed = send(&mut socket, chat, "keep listing files").await;
        assert_eq!(failed["status"], "failed");
        assert!(failed["text"].as_str().unwrap().contains("8 steps"));
        let after = send(&mut socket, chat, "are you still there?").await;
        assert_eq!(after["status"], "succeeded");
        assert_eq!(after["text"], "I am still here.");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_tool_is_marked_timed_out() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let running =
            super::start_with_tool_timeout(home, 0, std::time::Duration::from_millis(300))
                .await
                .unwrap();
        let invite = running.invite.clone().unwrap();
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "sleep 30"})),
            text("The command timed out."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let started = std::time::Instant::now();
        let result = send(&mut socket, chat, "run sleep 30").await;

        assert_eq!(result["status"], "succeeded");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let action = &shown["actions"][0];
        assert_eq!(action["tool"], "shell");
        assert_eq!(action["status"], "timed_out");
        assert!(
            action["output"].as_str().unwrap().contains("time limit"),
            "{}",
            action["output"]
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_clients_on_one_chat_take_turns() {
        let (_dir, running, invite) = running().await;
        let (mut first, token) = admin(&running, &invite).await;
        let mut second = open_socket(&running, &token).await;
        let (base, provider_task) =
            scripted_provider(vec![text("first answer"), text("second answer")]).await;
        command(&mut first, Id::new(), provider_add(base)).await;
        let bot = command(
            &mut first,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        let (left, right) = tokio::join!(
            send(&mut first, chat, "from the first client"),
            send(&mut second, chat, "from the second client"),
        );
        assert_eq!(left["status"], "succeeded");
        assert_eq!(right["status"], "succeeded");
        assert_ne!(run_of(&left), run_of(&right));

        let messages = command(
            &mut first,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let messages = messages.as_array().unwrap().clone();
        assert_eq!(messages.len(), 4, "{messages:?}");
        let user = messages
            .iter()
            .find(|message| message["author"]["kind"] == "user")
            .unwrap();
        let bot = messages
            .iter()
            .find(|message| message["author"]["kind"] == "bot")
            .unwrap();
        assert_eq!(bot["acting"], user["author"]["id"]);
        let mut replies: Vec<String> = messages
            .iter()
            .filter(|message| message["author"]["kind"] == "bot")
            .map(|message| message["text"].as_str().unwrap().to_owned())
            .collect();
        replies.sort();
        assert_eq!(replies, ["first answer", "second answer"]);

        // One Bot runs one run at a time: the second run starts after the first ends.
        let root = running.server_home();
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let store = Store::open(&Home::open(&root).unwrap()).await.unwrap();
        let events = store.events_from(Seq::ZERO, 1000).await.unwrap().events;
        store.close().await.unwrap();
        let seq_of = |kind: EventKind, run: Id| {
            events
                .iter()
                .find(|event| event.kind == kind && event.data["run"] == json!(run))
                .unwrap_or_else(|| panic!("{kind:?} of run {run}"))
                .seq
        };
        let (first_run, second_run) = if seq_of(EventKind::RunStarted, run_of(&left))
            < seq_of(EventKind::RunStarted, run_of(&right))
        {
            (run_of(&left), run_of(&right))
        } else {
            (run_of(&right), run_of(&left))
        };
        assert!(
            seq_of(EventKind::RunStarted, second_run) > seq_of(EventKind::RunFinished, first_run)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_request_waits_for_the_first() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("first answer"), text("second answer")]).await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        assert_eq!(first["status"], "queued");
        assert_eq!(first["position"], 1);
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;

        // The first run still works, so the second waits behind it.
        let second = submit(&mut socket, chat, "second", None).await;
        assert_eq!(second["status"], "queued");
        assert_eq!(second["position"], 2);
        let second_run = run_of(&second);
        let shown = show_run(&mut socket, second_run).await;
        assert_eq!(shown["status"], "queued");
        assert_eq!(shown["position"], 2);
        assert_eq!(show_run(&mut socket, first_run).await["status"], "running");

        gates[0].open();
        assert_eq!(
            wait_run(&mut socket, first_run).await["status"],
            "succeeded"
        );
        assert_eq!(
            wait_run(&mut socket, second_run).await["status"],
            "succeeded"
        );

        let root = running.server_home();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let events = record(&root).await;
        assert!(
            event_seq(&events, EventKind::RunStarted, second_run)
                > event_seq(&events, EventKind::RunFinished, first_run),
            "the second run must start after the first one ends"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_run_starts_after_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");
        let (mut socket, token) = admin(&running, &invite).await;
        let (base, provider_task, _gates, _recorded) = gated_provider(vec![
            hold("first answer"),
            text("recovered first"),
            text("second answer"),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;
        let second = submit(&mut socket, chat, "second", None).await;
        let second_run = run_of(&second);
        assert_eq!(second["status"], "queued");
        assert_eq!(second["position"], 2);

        // The Space stops with one run working and one waiting.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;

        let restarted = start(test_home(&root), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        // The working run recovers from its record; the waiting one runs after it.
        assert_eq!(
            wait_run(&mut socket, first_run).await["status"],
            "succeeded"
        );
        let shown = wait_run(&mut socket, second_run).await;
        assert_eq!(shown["status"], "succeeded");
        assert!(
            shown.get("position").is_none(),
            "a run that ended has no place in the queue"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_woken_worker_drains_the_whole_backlog() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");
        let (mut socket, token) = admin(&running, &invite).await;
        let (base, provider_task, _gates, _recorded) = gated_provider(vec![
            hold("one"),
            text("recovered one"),
            text("two"),
            text("three"),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;
        let second = submit(&mut socket, chat, "second", None).await;
        let third = submit(&mut socket, chat, "third", None).await;
        assert_eq!(second["position"], 2);
        assert_eq!(third["position"], 3);
        let (second_run, third_run) = (run_of(&second), run_of(&third));

        // The restart wakes each Bot once; one wake must work the whole backlog.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let restarted = start(test_home(&root), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        assert_eq!(
            wait_run(&mut socket, first_run).await["status"],
            "succeeded"
        );
        assert_eq!(
            wait_run(&mut socket, second_run).await["status"],
            "succeeded"
        );
        assert_eq!(
            wait_run(&mut socket, third_run).await["status"],
            "succeeded"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_of_different_bots_run_in_parallel() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("first"), hold("second")]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let mut chats = Vec::new();
        for name in ["First", "Second"] {
            let bot = command(
                &mut socket,
                Id::new(),
                CommandCall::BotCreate(BotCreate {
                    name: name.to_owned(),
                    computer: None,
                }),
            )
            .await;
            chats.push(serde_json::from_value::<Id>(bot["chat"]["id"].clone()).unwrap());
        }

        let first = submit(&mut socket, chats[0], "one", None).await;
        let second = submit(&mut socket, chats[1], "two", None).await;
        let (first_run, second_run) = (run_of(&first), run_of(&second));

        let mut overlapped = false;
        for _ in 0..200 {
            let left = show_run(&mut socket, first_run).await;
            let right = show_run(&mut socket, second_run).await;
            if left["status"] == "running" && right["status"] == "running" {
                overlapped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            overlapped,
            "runs of different Bots must work at the same time"
        );
        gates[0].open();
        gates[1].open();
        assert_eq!(
            wait_run(&mut socket, first_run).await["status"],
            "succeeded"
        );
        assert_eq!(
            wait_run(&mut socket, second_run).await["status"],
            "succeeded"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_runs_start_in_the_order_they_were_recorded() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("one"), text("two"), text("three")]).await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;
        let second = submit(&mut socket, chat, "second", None).await;
        let third = submit(&mut socket, chat, "third", None).await;
        assert_eq!(second["position"], 2);
        assert_eq!(third["position"], 3);
        let runs = [first_run, run_of(&second), run_of(&third)];
        gates[0].open();
        for run in runs {
            assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        }

        let root = running.server_home();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let events = record(&root).await;
        let queued = runs.map(|run| event_seq(&events, EventKind::RunQueued, run));
        let started = runs.map(|run| event_seq(&events, EventKind::RunStarted, run));
        assert!(
            queued.windows(2).all(|pair| pair[0] < pair[1]),
            "the record holds the runs in arrival order"
        );
        assert!(
            started.is_sorted(),
            "the runs start in the order the record queued them"
        );
        for pair in runs.windows(2) {
            assert!(
                event_seq(&events, EventKind::RunStarted, pair[1])
                    > event_seq(&events, EventKind::RunFinished, pair[0]),
                "a run starts only after the run before it ends"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_run_moves_up_as_the_queue_moves() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("one"), hold("two"), text("three")]).await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;
        let second = submit(&mut socket, chat, "second", None).await;
        let third = submit(&mut socket, chat, "third", None).await;
        let (second_run, third_run) = (run_of(&second), run_of(&third));
        assert_eq!(show_run(&mut socket, second_run).await["position"], 2);
        assert_eq!(show_run(&mut socket, third_run).await["position"], 3);

        // The run at the head ends; the next one works and the last one moves up.
        gates[0].open();
        assert_eq!(
            wait_run(&mut socket, first_run).await["status"],
            "succeeded"
        );
        wait_status(&mut socket, second_run, "running").await;
        let shown = show_run(&mut socket, third_run).await;
        assert_eq!(shown["status"], "queued");
        assert_eq!(shown["position"], 2);

        gates[1].open();
        for run in [second_run, third_run] {
            assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        }
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_requests_keep_their_recorded_order() {
        let (_dir, running, invite) = running().await;
        let (mut admin, token) = admin(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("first answer"), text("answer")]).await;
        command(&mut admin, Id::new(), provider_add(base)).await;
        let bot = command(
            &mut admin,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        // The first run holds the Bot's work while the burst takes its places.
        let first = submit(&mut admin, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut admin, first_run, "running").await;

        let mut sockets = Vec::new();
        for _ in 0..8 {
            sockets.push(open_socket(&running, &token).await);
        }
        let mut requests = Vec::new();
        for mut socket in sockets {
            requests.push(tokio::spawn(async move {
                command(
                    &mut socket,
                    Id::new(),
                    CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "burst".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                )
                .await
            }));
        }
        let mut placed: Vec<(u64, Id)> = Vec::new();
        for request in requests {
            let queued = request.await.unwrap();
            assert_eq!(queued["status"], "queued");
            placed.push((
                queued["position"].as_u64().expect("a place in the queue"),
                run_of(&queued),
            ));
        }
        let mut places: Vec<u64> = placed.iter().map(|(place, _)| *place).collect();
        places.sort_unstable();
        assert_eq!(
            places,
            (2..=9).collect::<Vec<_>>(),
            "each confirmation must name its own place"
        );

        gates[0].open();
        assert_eq!(wait_run(&mut admin, first_run).await["status"], "succeeded");
        for (_, run) in &placed {
            assert_eq!(wait_run(&mut admin, *run).await["status"], "succeeded");
        }

        let root = running.server_home();
        admin.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let events = record(&root).await;
        // The record orders the burst by the place each command confirmed.
        let mut by_queue: Vec<(Seq, u64, Id)> = placed
            .iter()
            .map(|(place, run)| (event_seq(&events, EventKind::RunQueued, *run), *place, *run))
            .collect();
        by_queue.sort_unstable();
        assert_eq!(
            by_queue
                .iter()
                .map(|(_, place, _)| *place)
                .collect::<Vec<_>>(),
            (2..=9).collect::<Vec<_>>(),
            "the record's order must be the places the commands confirmed"
        );
        let started: Vec<Seq> = by_queue
            .iter()
            .map(|(_, _, run)| event_seq(&events, EventKind::RunStarted, *run))
            .collect();
        assert!(started.is_sorted(), "the runs start in the record's order");
        assert!(
            started[0] > event_seq(&events, EventKind::RunFinished, first_run),
            "the burst starts after the run that held the Bot"
        );
        for window in by_queue.windows(2) {
            assert!(
                event_seq(&events, EventKind::RunFinished, window[0].2)
                    < event_seq(&events, EventKind::RunStarted, window[1].2),
                "one Bot runs one run at a time"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_command_does_not_hold_the_queue() {
        let (_dir, running, invite) = running().await;
        let (mut admin, token) = admin(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("first answer"), text("answer")]).await;
        command(&mut admin, Id::new(), provider_add(base)).await;
        let bot = command(
            &mut admin,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let command_id = Id::new();
        let first = command(
            &mut admin,
            command_id,
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "first".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;
        let first_run = run_of(&first);
        wait_status(&mut admin, first_run, "running").await;

        // A replay of the queued command waits for the run's result.
        let mut replay_socket = open_socket(&running, &token).await;
        replay_socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: command_id,
                    command: CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "first".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            match next_frame(&mut replay_socket).await {
                ServerFrame::Accepted { id } => {
                    assert_eq!(id, command_id);
                    break;
                }
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
        let replay = tokio::spawn(async move {
            loop {
                match next_frame(&mut replay_socket).await {
                    ServerFrame::Confirmed { id, result } => {
                        assert_eq!(id, command_id);

                        return result;
                    }
                    ServerFrame::Heartbeat { .. }
                    | ServerFrame::Event { .. }
                    | ServerFrame::Snapshot { .. }
                    | ServerFrame::Accepted { .. }
                    | ServerFrame::Committed { .. } => {}
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        });

        // The Bot still takes a new request: the replay holds no place.
        let mut next_socket = open_socket(&running, &token).await;
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            command(
                &mut next_socket,
                Id::new(),
                CommandCall::MessageSend(MessageSend {
                    chat,
                    text: "next".to_owned(),
                    file: None,

                    cwd: None,
                    cwd_writable: None,
                }),
            ),
        )
        .await
        .expect("a replay must not hold the Bot's queue");
        assert_eq!(next["status"], "queued");
        assert_eq!(next["position"], 2);

        gates[0].open();
        let replayed = replay.await.unwrap();
        assert_eq!(replayed["status"], "succeeded");
        assert_eq!(replayed["run"], first["run"]);
        assert_eq!(wait_run(&mut admin, first_run).await["status"], "succeeded");
        assert_eq!(
            wait_run(&mut admin, run_of(&next)).await["status"],
            "succeeded"
        );
        admin.close(None).await.unwrap();
        next_socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_run_that_cannot_resume_is_failed() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");
        let (mut socket, token) = admin(&running, &invite).await;
        let (base, provider_task, _gates, _recorded) =
            gated_provider(vec![hold("first answer"), text("second answer")]).await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let first = submit(&mut socket, chat, "first", None).await;
        let first_run = run_of(&first);
        wait_status(&mut socket, first_run, "running").await;
        let command_id = Id::new();
        let second = command(
            &mut socket,
            command_id,
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "second".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;
        assert_eq!(second["status"], "queued");
        let second_run = run_of(&second);

        // Take away the key the waiting run needs after the restart.
        let home = Home::open(directory.path()).unwrap();
        for entry in std::fs::read_dir(home.root().join("secrets")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with("provider-") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;

        let restarted = start(test_home(&root), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        assert_eq!(wait_run(&mut socket, first_run).await["status"], "failed");
        let failed = wait_run(&mut socket, second_run).await;
        assert_eq!(failed["status"], "failed");

        // A retry of the command answers with the same failure at once and
        // records nothing new.
        let before = restarted.server.store.last_seq().await.unwrap();
        let retried = command(
            &mut socket,
            command_id,
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "second".to_owned(),
                file: None,

                cwd: None,
                cwd_writable: None,
            }),
        )
        .await;
        assert_eq!(retried["run"], second["run"]);
        assert_eq!(retried["status"], "failed");
        assert_eq!(
            restarted.server.store.last_seq().await.unwrap(),
            before,
            "a retried command must not write again"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_shutdown_rejects_a_late_command() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![text("unused")]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let root = running.server_home();
        let before = running.server.store.last_seq().await.unwrap();

        // The client keeps its socket while the Space stops.
        running.shutdown().await;
        let _ = socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command {
                    id: Id::new(),
                    command: CommandCall::MessageSend(MessageSend {
                        chat,
                        text: "late".to_owned(),
                        file: None,

                        cwd: None,
                        cwd_writable: None,
                    }),
                })
                .unwrap()
                .into(),
            ))
            .await;
        let refused = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("the Space answers or closes");
        match refused {
            Some(Ok(Message::Text(text))) => {
                let frame: ServerFrame = serde_json::from_str(text.as_str()).unwrap();
                assert!(
                    matches!(
                        frame,
                        ServerFrame::Error {
                            code: ErrorCode::Unavailable,
                            ..
                        }
                    ),
                    "a stopped Space accepted a command: {frame:?}"
                );
            }
            Some(Ok(_)) => panic!("the Space sent a non-text frame"),
            Some(Err(error)) => panic!("the socket failed: {error}"),
            None => panic!("the Space neither answered nor closed"),
        }

        provider_task.abort();
        let store = Store::open(&Home::open(&root).unwrap()).await.unwrap();
        let page = store.events_from(before, 10).await.unwrap();
        assert!(
            page.events.is_empty(),
            "the stopped Space wrote {:?}",
            page.events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>()
        );
        store.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_start_leaves_the_queue_untouched() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let provider = Id::new();
        let model = Id::new();
        let user = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let (message, run) = (Id::new(), Id::new());
        let command_id = Id::new();
        crate::secrets::Secrets::load(&home)
            .unwrap()
            .store_provider_key(provider, "sk-test")
            .unwrap();
        std::fs::create_dir_all(home.workspace_dir(computer)).unwrap();
        let store = Store::open(&home).await.unwrap();
        let drafts = vec![
            EventDraft::new(
                EventKind::ProviderAdded,
                Author::User(user),
                json!({
                    "provider": provider,
                    "protocol": "chat_completions",
                    "base_url": "http://127.0.0.1:9/v1",
                    "models": [{
                        "id": model,
                        "name": "test-model",
                        "context_limit": 128_000,
                        "max_output": 8_192,
                        "reads_images": false,
                    }],
                    "default_model": model,
                }),
            ),
            EventDraft::new(
                EventKind::ComputerCreated,
                Author::User(user),
                json!({"computer": computer, "owner": user, "kind": "managed", "workspace": home.workspace_dir(computer).to_string_lossy()}),
            ),
            EventDraft::new(
                EventKind::BotCreated,
                Author::User(user),
                json!({"bot": bot, "owner": user, "name": "Scout", "computer": computer, "chat": chat}),
            ),
            EventDraft::new(
                EventKind::ChatCreated,
                Author::User(user),
                json!({"chat": chat, "owner": user, "bot": bot}),
            ),
            EventDraft::new(
                EventKind::MessageAdded,
                Author::User(user),
                json!({"message": message, "chat": chat, "text": "hello", "run": run}),
            ),
            EventDraft::new(
                EventKind::RunQueued,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "chat": chat,
                    "bot": bot,
                    "message": message,
                    "command": command_id,
                }),
            )
            .acting(user),
        ];
        let seeded = store
            .append(AppendRequest::new(command_id, "seed", drafts))
            .await
            .unwrap();
        let seed_end = seeded.events.last().unwrap().seq;
        store.close().await.unwrap();

        // A start that cannot listen must not waken the record.
        let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = occupied.local_addr().unwrap().port();
        let result = start(test_home(directory.path()), port).await;
        assert!(result.is_err(), "a taken port must refuse the start");
        drop(occupied);

        let store = Store::open(&home).await.unwrap();
        for _ in 0..30 {
            let page = store.events_from(seed_end, 100).await.unwrap();
            assert!(
                page.events.is_empty(),
                "a failed start wrote {:?}",
                page.events
                    .iter()
                    .map(|event| event.kind)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        store.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_record_without_a_queued_event_replays() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let identity = crate::identity::Identity::open(&home).await.unwrap();
        let account = identity
            .add_account_for_test("Ada", "correct horse battery")
            .await
            .unwrap();
        drop(identity);
        let user = account.id;
        let (bot, computer, chat) = (Id::new(), Id::new(), Id::new());
        let (message, run, command) = (Id::new(), Id::new(), Id::new());
        let store = Store::open(&home).await.unwrap();
        let drafts = vec![
            EventDraft::new(
                EventKind::UserJoined,
                Author::User(user),
                json!({"display_name": "Ada", "is_admin": true}),
            ),
            EventDraft::new(
                EventKind::ComputerCreated,
                Author::User(user),
                json!({"computer": computer, "owner": user, "kind": "managed", "workspace": "/tmp/workspace"}),
            ),
            EventDraft::new(
                EventKind::BotCreated,
                Author::User(user),
                json!({"bot": bot, "owner": user, "name": "Scout", "computer": computer, "chat": chat}),
            ),
            EventDraft::new(
                EventKind::ChatCreated,
                Author::User(user),
                json!({"chat": chat, "owner": user, "bot": bot}),
            ),
            EventDraft::new(
                EventKind::MessageAdded,
                Author::User(user),
                json!({"message": message, "chat": chat, "text": "hello", "run": run}),
            ),
            // Before runs were queued, a start was all the record held for a run.
            EventDraft::new(
                EventKind::RunStarted,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "chat": chat,
                    "bot": bot,
                    "message": message,
                    "command": command,
                }),
            )
            .acting(user),
            EventDraft::new(
                EventKind::RunFinished,
                Author::Bot(bot),
                json!({"run": run, "status": "succeeded", "reply": message}),
            )
            .acting(user),
        ];
        let appended = store
            .append(AppendRequest::new(command, "old record", drafts))
            .await
            .unwrap();
        let started = appended
            .events
            .iter()
            .find(|event| event.kind == EventKind::RunStarted)
            .expect("the start event")
            .seq;
        store.close().await.unwrap();

        let running = start(home, 0).await.unwrap();
        let token = sign_in(&running, "Ada").await;
        let mut socket = open_socket(&running, &token).await;
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["seq"], json!(started));
        assert!(
            shown.get("position").is_none(),
            "only a waiting run has a place in a queue"
        );
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_write_is_still_inspectable() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let content = "x".repeat(60 * 1024);
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "write_file",
                json!({"path": "large.txt", "content": content}),
            ),
            text("I wrote the large file."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "write a large file").await;

        assert_eq!(result["status"], "succeeded");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let action = &shown["actions"][0];
        assert_eq!(action["tool"], "write_file");
        assert_eq!(action["arguments"]["truncated"], true);
        assert!(
            action["arguments"]["bytes"].as_u64().unwrap() >= 60 * 1024,
            "{}",
            action["arguments"]
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_computer_that_cannot_start_fails_the_run() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![text("unused")]).await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        // A Hosted workspace already has a `.tmp` directory, so replace it with
        // a file: the runtime cannot create its working folder and must fail
        // the run instead of starting on a half-prepared Computer.
        std::fs::remove_dir_all(workspace.join(".tmp")).unwrap();
        std::fs::write(workspace.join(".tmp"), "not a directory").unwrap();

        let result = send(&mut socket, chat, "hello").await;

        assert_eq!(result["status"], "failed");
        assert!(
            result["text"]
                .as_str()
                .unwrap()
                .contains("could not start my Computer"),
            "{}",
            result["text"]
        );
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "failed");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let last = messages.as_array().unwrap().last().unwrap();
        assert_eq!(last["author"]["kind"], "bot");
        assert!(
            last["text"]
                .as_str()
                .unwrap()
                .contains("could not start my Computer")
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_truncated_tool_call_does_not_run() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            truncated_tool(
                "write_file",
                json!({"path": "truncated.txt", "content": "never written"}),
            ),
            text("unused"),
        ])
        .await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "write a file").await;

        assert_eq!(result["status"], "failed");
        assert!(
            result["text"].as_str().unwrap().contains("output limit"),
            "{}",
            result["text"]
        );
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "failed");
        assert_eq!(shown["usage"]["uncached_input"], 40);
        assert_eq!(shown["usage"]["output"], 12);
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert!(!workspace.join("truncated.txt").exists());
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unicode_write_arguments_are_recorded_in_full() {
        use neha::record::EventKind;

        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let content = "漢".repeat(21_800);
        let padding = " ".repeat(500);
        let raw = format!(
            "{{{padding}\"path\":\"unicode.txt\",\"content\":\"{}\"}}",
            "\\u6f22".repeat(21_800)
        );
        assert!(
            raw.len() > 131_072,
            "the test must exceed the old summary threshold: {}",
            raw.len()
        );
        let (base, provider_task) = scripted_provider(vec![
            raw_tool("write_file", raw),
            text("I wrote the unicode file."),
        ])
        .await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "write unicode text").await;

        assert_eq!(result["status"], "succeeded");
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read(workspace.join("unicode.txt")).unwrap().len(),
            65_400
        );
        let root = running.server_home();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let home = Home::open(&root).unwrap();
        let store = Store::open(&home).await.unwrap();
        let page = store.events_from(Seq::ZERO, 1000).await.unwrap();
        let started = page
            .events
            .iter()
            .find(|event| event.kind == EventKind::ActionStarted)
            .expect("an action event");
        assert_eq!(
            started.data["arguments"]["content"].as_str(),
            Some(content.as_str()),
            "a running action must keep its real arguments"
        );
        store.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_record_of_large_events_catches_up_across_frames() {
        use neha::record::EventKind;

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let store = Store::open(&home).await.unwrap();
        let user = Id::new();
        let bot = Id::new();
        let chat = Id::new();
        let run = Id::new();
        let command = Id::new();
        let message = Id::new();
        let mut drafts = vec![
            EventDraft::new(
                EventKind::RunStarted,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "chat": chat,
                    "bot": bot,
                    "command": command,
                    "message": message,
                }),
            )
            .acting(user),
            EventDraft::new(
                EventKind::RunStep,
                Author::Bot(bot),
                json!({"run": run, "step": 1, "tool_calls": 18}),
            )
            .acting(user),
        ];
        for index in 0..18 {
            let action = Id::new();
            drafts.push(
                EventDraft::new(
                    EventKind::ActionStarted,
                    Author::Bot(bot),
                    json!({
                        "run": run,
                        "step": 1,
                        "action": action,
                        "tool": "write_file",
                        "arguments": {
                            "path": format!("big-{index}.txt"),
                            "content": "x".repeat(60 * 1024),
                        },
                    }),
                )
                .acting(user),
            );
            drafts.push(
                EventDraft::new(
                    EventKind::ActionFinished,
                    Author::Bot(bot),
                    json!({
                        "run": run,
                        "step": 1,
                        "action": action,
                        "tool": "write_file",
                        "status": "succeeded",
                        "output": "ok",
                    }),
                )
                .acting(user),
            );
        }
        drafts.push(
            EventDraft::new(
                EventKind::RunFinished,
                Author::Bot(bot),
                json!({"run": run, "status": "succeeded", "reply": null, "usage": null}),
            )
            .acting(user),
        );
        store
            .append(AppendRequest::new(command, "seed", drafts))
            .await
            .unwrap();
        let last = store.last_seq().await.unwrap();
        let reader = store.reader();
        let (out, mut frames) = tokio::sync::mpsc::channel(64);
        // The account is the one the record's events act for; the whole
        // record is visible to it.
        let result = catch_up(&reader, &out, neha::seq::Seq::ZERO, None, user).await;
        assert!(result.is_ok());
        drop(out);
        let mut seen = std::collections::BTreeSet::new();
        let mut snapshots = 0;
        while let Ok(frame) = frames.try_recv() {
            match frame {
                ServerFrame::Snapshot { cursor, events } => {
                    snapshots += 1;
                    assert!(cursor >= events.last().map(|event| event.seq).unwrap_or(cursor));
                    for event in events {
                        seen.insert(event.seq.as_i64());
                    }
                }
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert!(snapshots >= 2, "the record must be split across frames");
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            (1..=last.as_i64()).collect::<Vec<_>>()
        );
        store.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_huge_path_is_recorded_bounded() {
        use neha::record::EventKind;

        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let raw = format!("{{\"path\":\"{}\"}}", "x".repeat(1_100_000));
        let (base, provider_task) = scripted_provider(vec![
            raw_tool("read_file", raw),
            text("The read was blocked."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "read a huge path").await;

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["text"], "The read was blocked.");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let action = &shown["actions"][0];
        assert_eq!(action["tool"], "read_file");
        assert_eq!(action["status"], "failed");
        assert_eq!(action["arguments"]["truncated"], true);

        let root = running.server_home();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        let home = Home::open(&root).unwrap();
        let store = Store::open(&home).await.unwrap();
        let page = store.events_from(Seq::ZERO, 1000).await.unwrap();
        let started = page
            .events
            .iter()
            .find(|event| event.kind == EventKind::ActionStarted)
            .expect("an action event");
        let arguments = serde_json::to_string(&started.data["arguments"]).unwrap();
        assert!(
            arguments.len() < 4096,
            "the refused call must be summarized, got {} bytes",
            arguments.len()
        );
        store.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retried_model_call_keeps_every_attempt_usage() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            ScriptedReply::StreamEndedUsage {
                text: "partial".to_owned(),
            },
            ScriptedReply::TextWithUsage {
                text: "second complete".to_owned(),
                prompt: 8,
                completion: 2,
            },
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "answer me").await;

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["text"], "second complete");
        assert_eq!(result["usage"]["uncached_input"], 48);
        assert_eq!(result["usage"]["output"], 14);
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["usage"]["uncached_input"], 48);
        assert_eq!(shown["usage"]["output"], 14);
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let bot_messages: Vec<_> = messages
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["author"]["kind"] == "bot")
            .collect();
        assert_eq!(
            bot_messages.len(),
            1,
            "the partial attempt never becomes a Message"
        );
        assert_eq!(bot_messages[0]["text"], "second complete");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_retry_keeps_every_attempt_usage() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            ScriptedReply::StreamEndedUsage {
                text: "partial".to_owned(),
            },
            ScriptedReply::TruncatedToolWithUsage {
                name: "write_file".to_owned(),
                arguments: json!({"path": "never.txt", "content": "never"}),
                prompt: 8,
                completion: 2,
            },
        ])
        .await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "answer me").await;

        assert_eq!(result["status"], "failed");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "failed");
        assert_eq!(shown["usage"]["uncached_input"], 48);
        assert_eq!(shown["usage"]["output"], 14);
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert!(!workspace.join("never.txt").exists());
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    // The SHA-256 of `abc`, from FIPS 180-2.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    async fn create_bot(socket: &mut Socket) -> (Id, Id, Id) {
        let bot = command(
            socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        )
        .await;

        (
            serde_json::from_value(bot["bot"]["id"].clone()).unwrap(),
            serde_json::from_value(bot["chat"]["id"].clone()).unwrap(),
            serde_json::from_value(bot["computer"]["id"].clone()).unwrap(),
        )
    }

    async fn upload(
        running: &Running,
        token: &str,
        chat: Id,
        name: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/api/chats/{chat}/files", running.url()))
            .query(&[("name", name), ("media_type", media_type)])
            .bearer_auth(token)
            .body(bytes)
            .send()
            .await
            .unwrap()
    }

    async fn download(running: &Running, token: &str, file: Id) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}/api/files/{file}", running.url()))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn file_list(socket: &mut Socket, chat: Id) -> Vec<Value> {
        command(socket, Id::new(), CommandCall::FileList { chat })
            .await
            .as_array()
            .cloned()
            .unwrap()
    }

    async fn sign_in(running: &Running, name: &str) -> String {
        let session: Value = reqwest::Client::new()
            .post(format!("{}/api/login", running.url()))
            .json(&json!({"display_name": name, "password": "correct horse battery"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        session["token"].as_str().unwrap().to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_identical_uploads_record_one_file() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        let uploads: Vec<_> = (0..8)
            .map(|_| {
                upload(
                    &running,
                    &token,
                    chat,
                    "notes.txt",
                    "text/plain",
                    b"read me".to_vec(),
                )
            })
            .collect();
        let responses = futures_util::future::join_all(uploads).await;
        let mut ids = Vec::new();
        for response in responses {
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let file: Value = response.json().await.unwrap();
            ids.push(serde_json::from_value::<Id>(file["id"].clone()).unwrap());
        }

        assert!(
            ids.windows(2).all(|pair| pair[0] == pair[1]),
            "every upload answers with the same File: {ids:?}"
        );
        let files = file_list(&mut socket, chat).await;
        assert_eq!(files.len(), 1, "{files:?}");
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_uploads_with_its_session_cookie() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        // Sign in the way the browser does, on the host its page is served
        // from: the token arrives as a cookie and the request carries the
        // page's own Origin.
        let page = running.browser_url();
        let login = reqwest::Client::new()
            .post(format!("{page}/api/login"))
            .header("origin", page.clone())
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), reqwest::StatusCode::OK);
        let cookie = login
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .expect("a sign-in sets the session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();

        let upload = |origin: String| {
            let cookie = cookie.clone();
            let url = running.browser_url();
            async move {
                reqwest::Client::new()
                    .post(format!("{url}/api/chats/{chat}/files"))
                    .query(&[("name", "notes.txt"), ("media_type", "text/plain")])
                    .header("cookie", cookie)
                    .header("origin", origin)
                    .body("read me")
                    .send()
                    .await
                    .unwrap()
            }
        };

        let accepted = upload(page.clone()).await;
        assert_eq!(accepted.status(), reqwest::StatusCode::OK);
        let file: Value = accepted.json().await.unwrap();
        let id: Id = serde_json::from_value(file["id"].clone()).unwrap();

        let download = reqwest::Client::new()
            .get(format!("{page}/api/files/{id}"))
            .header("cookie", cookie.clone())
            .header("origin", page.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(download.status(), reqwest::StatusCode::OK);
        assert_eq!(download.bytes().await.unwrap().as_ref(), b"read me");

        let foreign = upload("http://evil.example".to_owned()).await;
        assert_eq!(foreign.status(), reqwest::StatusCode::FORBIDDEN);
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_is_sent_with_a_message_and_downloaded() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        // A subscriber receives the record so far, then every new File live.
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: Some(100),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ServerFrame::Snapshot { events, .. } = next_frame(&mut socket).await else {
            panic!("expected a snapshot");
        };
        assert!(
            events
                .iter()
                .all(|event| event.kind != EventKind::FileAdded)
        );

        let response = upload(
            &running,
            &token,
            chat,
            "hello world.txt",
            "text/plain",
            b"abc".to_vec(),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let first: Value = response.json().await.unwrap();
        let file: Id = serde_json::from_value(first["id"].clone()).unwrap();
        assert_eq!(first["chat"], json!(chat));
        assert_eq!(first["name"], "hello world.txt");
        assert_eq!(first["media_type"], "text/plain");
        assert_eq!(first["size"], 3);
        assert_eq!(first["sha256"], ABC);
        assert_eq!(first["author"]["kind"], "user");
        assert!(first.get("acting").is_none());

        // The same bytes are stored once, however many Files carry them.
        let again: Value = upload(
            &running,
            &token,
            chat,
            "copy.txt",
            "text/plain",
            b"abc".to_vec(),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_ne!(again["id"], first["id"]);
        assert_eq!(again["sha256"], ABC);
        let objects: Vec<_> =
            std::fs::read_dir(running.server_home().join("blobs").join(&ABC[..2]))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
        assert_eq!(objects, [std::ffi::OsStr::new(ABC)]);

        // Each upload wrote one event, which the subscriber received live.
        let mut added = Vec::new();
        for summary in [&first, &again] {
            match next_frame(&mut socket).await {
                ServerFrame::Event { event } => {
                    assert_eq!(json!(event.seq), summary["seq"]);
                    added.push(event);
                }
                other => panic!("expected the File's event, got {other:?}"),
            }
        }
        assert!(
            added
                .iter()
                .all(|event| event.kind == EventKind::FileAdded && event.data["sha256"] == ABC)
        );
        assert_eq!(added[0].data["file"], json!(file));
        // The Chat lists the Files in the order they were sent.
        assert_eq!(file_list(&mut socket, chat).await, vec![first, again]);

        // A Message carries the File with no text.
        let (base, provider_task) = scripted_provider(vec![text("Hello from fake")]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let sent = send_file(&mut socket, chat, file).await;
        assert_eq!(sent["status"], "succeeded");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages[0]["file"], json!(file));
        assert_eq!(messages[0]["text"], "");
        assert!(messages[1]["file"].is_null());

        let response = download(&running, &token, file).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(
            response.headers()["content-disposition"],
            "attachment; filename*=UTF-8''hello%20world.txt"
        );
        assert_eq!(response.bytes().await.unwrap(), b"abc".as_slice());

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_stays_with_the_account_of_its_chat() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;
        let mine: Value = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            b"abc".to_vec(),
        )
        .await
        .json()
        .await
        .unwrap();
        let file: Id = serde_json::from_value(mine["id"].clone()).unwrap();

        // Without a session neither route answers.
        let anonymous = reqwest::Client::new();
        let route = format!("{}/api/chats/{chat}/files", running.url());
        let upload_anon = anonymous
            .post(&route)
            .query(&[("name", "a.txt"), ("media_type", "text/plain")])
            .body("abc")
            .send()
            .await
            .unwrap();
        assert_eq!(upload_anon.status(), reqwest::StatusCode::UNAUTHORIZED);
        let download_anon = anonymous
            .get(format!("{}/api/files/{file}", running.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(download_anon.status(), reqwest::StatusCode::UNAUTHORIZED);

        // An unknown Chat is not found.
        let unknown = upload(
            &running,
            &token,
            Id::new(),
            "a.txt",
            "text/plain",
            b"abc".to_vec(),
        )
        .await;
        assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

        // Another account may neither write to the Chat nor read its Files.
        let identity = crate::identity::Identity::open(&Home::open(running.server_home()).unwrap())
            .await
            .unwrap();
        identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let grace = sign_in(&running, "Grace").await;
        let foreign = upload(
            &running,
            &grace,
            chat,
            "a.txt",
            "text/plain",
            b"abc".to_vec(),
        )
        .await;
        assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
        let foreign = download(&running, &grace, file).await;
        assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
        let mut grace_socket = open_socket(&running, &grace).await;
        assert!(matches!(
            try_command(&mut grace_socket, Id::new(), CommandCall::FileList { chat }).await,
            Err((ErrorCode::NotFound, _))
        ));

        // A Message may only carry a File of its own Chat.
        let helper = command(
            &mut grace_socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Helper".to_owned(),
                computer: None,
            }),
        )
        .await;
        let helper_chat: Id = serde_json::from_value(helper["chat"]["id"].clone()).unwrap();
        assert!(matches!(
            try_command(
                &mut grace_socket,
                Id::new(),
                CommandCall::MessageSend(MessageSend {
                    chat: helper_chat,
                    text: String::new(),
                    file: Some(file),

                    cwd: None,
                    cwd_writable: None,
                }),
            )
            .await,
            Err((ErrorCode::InvalidArgument, _))
        ));

        // Nothing of Grace's attempts reached either Chat.
        assert!(file_list(&mut grace_socket, helper_chat).await.is_empty());
        assert_eq!(file_list(&mut socket, chat).await.len(), 1);
        socket.close(None).await.unwrap();
        grace_socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retried_upload_records_one_file() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        let first: Value = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            b"read me".to_vec(),
        )
        .await
        .json()
        .await
        .unwrap();
        let retried: Value = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            b"read me".to_vec(),
        )
        .await
        .json()
        .await
        .unwrap();

        // A retry answers with the File it recorded the first time.
        assert_eq!(retried, first);
        assert_eq!(file_list(&mut socket, chat).await, vec![first]);
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_upload_the_blob_store_cannot_hold_records_no_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        let blobs = running.server_home().join("blobs");
        let writable = std::fs::metadata(&blobs).unwrap().permissions();
        std::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(0o555)).unwrap();
        let refused = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            b"read me".to_vec(),
        )
        .await;
        std::fs::set_permissions(&blobs, writable).unwrap();

        // The bytes could not be written, so no File is recorded.
        assert_eq!(refused.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(file_list(&mut socket, chat).await.is_empty());

        // Once the store can write again the same upload succeeds.
        let stored = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            b"read me".to_vec(),
        )
        .await;
        assert_eq!(stored.status(), reqwest::StatusCode::OK);
        assert_eq!(file_list(&mut socket, chat).await.len(), 1);
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_at_the_size_limit_is_kept_and_one_byte_more_is_refused() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;

        // Exactly the limit is a File, even though it is far larger than the
        // server's default body limit.
        let at_limit: Value = upload(
            &running,
            &token,
            chat,
            "notes.txt",
            "text/plain",
            vec![b'x'; MAX_FILE_BYTES as usize],
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(at_limit["size"], MAX_FILE_BYTES);

        let over = upload(
            &running,
            &token,
            chat,
            "huge.bin",
            "application/octet-stream",
            vec![0; MAX_FILE_BYTES as usize + 1],
        )
        .await;
        assert_eq!(over.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
        let body: Value = over.json().await.unwrap();
        assert_eq!(body["code"], "invalid_argument");

        for (name, media_type) in [
            ("", "text/plain"),
            (".", "text/plain"),
            ("a/b.txt", "text/plain"),
            ("notes.txt", ""),
            ("notes.txt", "text/plain\n"),
        ] {
            let refused = upload(&running, &token, chat, name, media_type, b"abc".to_vec()).await;
            assert_eq!(
                refused.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{name:?} with {media_type:?}"
            );
            let body: Value = refused.json().await.unwrap();
            assert_eq!(body["code"], "invalid_argument");
        }
        let missing_type = reqwest::Client::new()
            .post(format!("{}/api/chats/{chat}/files", running.url()))
            .query(&[("name", "notes.txt")])
            .bearer_auth(&token)
            .body("abc")
            .send()
            .await
            .unwrap();
        assert_eq!(missing_type.status(), reqwest::StatusCode::BAD_REQUEST);

        // Only the File at the limit was recorded or stored.
        assert_eq!(file_list(&mut socket, chat).await, vec![at_limit]);
        let stored = std::fs::read_dir(running.server_home().join("blobs"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_name() != "tmp")
            .count();
        assert_eq!(stored, 1, "the refused upload left no bytes behind");
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_run_token_only_reaches_its_own_chat() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (bot, chat, _computer) = create_bot(&mut socket).await;
        let (_, other_chat, _) = create_bot(&mut socket).await;
        let acting = {
            let state = running.server.state.lock().unwrap();
            state.bots[&bot].owner
        };
        let issued = |run, chat, expires| {
            let mut state = running.server.state.lock().unwrap();
            state.issue_bot_token(crate::state::BotToken {
                bot,
                chat,
                run,
                acting,
                expires,
            })
        };
        let bot_token = issued(
            Id::new(),
            chat,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );
        let client = reqwest::Client::new();
        let uploaded: Value = client
            .post(format!("{}/api/bot/files", running.url()))
            .query(&[("name", "bot.txt"), ("media_type", "text/plain")])
            .bearer_auth(&bot_token)
            .body("from the bot".to_owned())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let file: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
        assert_eq!(uploaded["name"], "bot.txt");

        // The token posts as the Bot, only into its own Chat.
        let posted: Value = client
            .post(format!("{}/api/bot/messages", running.url()))
            .bearer_auth(&bot_token)
            .json(&serde_json::json!({"chat": chat, "file": file}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let message: Id = serde_json::from_value(posted["message"].clone()).unwrap();
        {
            let state = running.server.state.lock().unwrap();
            let message = state
                .messages
                .iter()
                .find(|view| view.id == message)
                .unwrap();
            assert_eq!(message.author, Author::Bot(bot));
        }
        let forbidden = client
            .post(format!("{}/api/bot/messages", running.url()))
            .bearer_auth(&bot_token)
            .json(&serde_json::json!({"chat": other_chat, "file": file}))
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
        // The token sends File Messages only; a text-only Message is refused
        // and records nothing.
        let before = {
            let state = running.server.state.lock().unwrap();
            state.messages.len()
        };
        let text_only = client
            .post(format!("{}/api/bot/messages", running.url()))
            .bearer_auth(&bot_token)
            .json(&serde_json::json!({"chat": chat, "text": "just words"}))
            .send()
            .await
            .unwrap();
        assert_eq!(text_only.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            running.server.state.lock().unwrap().messages.len(),
            before,
            "a refused Bot Message records nothing"
        );

        // The bytes come back, and a bad or expired token is refused.
        let response = client
            .get(format!("{}/api/bot/files/{file}", running.url()))
            .bearer_auth(&bot_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"from the bot");
        let expired = issued(
            Id::new(),
            chat,
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        for bad in ["not-a-token".to_owned(), expired] {
            let response = client
                .post(format!("{}/api/bot/messages", running.url()))
                .bearer_auth(bad)
                .json(&serde_json::json!({"chat": chat, "text": "hello"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = start(test_home(dir.path()), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let (mut socket, token) = admin(&first, &invite).await;
        let (_bot, chat, _computer) = create_bot(&mut socket).await;
        let uploaded: Value = upload(
            &first,
            &token,
            chat,
            "report.csv",
            "text/csv",
            b"a,b\n1,2\n".to_vec(),
        )
        .await
        .json()
        .await
        .unwrap();
        let file: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
        socket.close(None).await.unwrap();
        first.shutdown().await;

        let second = start(test_home(dir.path()), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        assert_eq!(file_list(&mut socket, chat).await, vec![uploaded]);
        let response = download(&second, &token, file).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/csv");
        assert_eq!(response.bytes().await.unwrap(), b"a,b\n1,2\n".as_slice());
        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[test]
    fn a_space_keeps_files_in_s3() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            let (dir, running, invite) = running_with_config(&s3_config(&storage)).await;
            assert_eq!(
                running.storage().to_string(),
                format!(
                    "s3://{}/{} at {}",
                    storage.bucket, storage.prefix, storage.endpoint
                )
            );
            let (mut socket, token) = admin(&running, &invite).await;
            let (_bot, chat, _computer) = create_bot(&mut socket).await;

            let uploaded: Value = upload(
                &running,
                &token,
                chat,
                "notes.txt",
                "text/plain",
                b"read me".to_vec(),
            )
            .await
            .json()
            .await
            .unwrap();
            let file: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
            let sha = uploaded["sha256"].as_str().unwrap();
            let response = download(&running, &token, file).await;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"read me");

            // The bytes went under the configured prefix, and the home kept none.
            assert_eq!(
                test_s3::objects(&storage).await,
                [(format!("{}/{}", &sha[..2], sha), 7)]
            );
            assert!(
                std::fs::read_dir(running.server_home().join("blobs"))
                    .unwrap()
                    .next()
                    .is_none(),
                "the local blob directory stays empty"
            );

            // A restart reads the same bytes back from S3.
            socket.close(None).await.unwrap();
            running.shutdown().await;
            let restarted = start(test_home(dir.path()), 0).await.unwrap();
            let token = sign_in(&restarted, "Ada").await;
            let mut socket = open_socket(&restarted, &token).await;
            let response = download(&restarted, &token, file).await;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"read me");
            socket.close(None).await.unwrap();
            restarted.shutdown().await;
        });
    }

    #[test]
    fn a_space_refuses_a_store_it_cannot_use() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            let bucket = format!("neha-missing-{}", Id::new());
            let config = format!(
                "[storage]\nbackend = \"s3\"\nbucket = \"{bucket}\"\nregion = \"{}\"\n\
                 prefix = \"{}\"\nendpoint = \"{}\"\n",
                storage.region, storage.prefix, storage.endpoint
            );
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("config.toml"), config).unwrap();

            let Err(error) = SpaceServer::open(Home::open(dir.path()).unwrap()).await else {
                panic!("a store the Space cannot use must refuse the start");
            };

            assert!(matches!(error, ServerError::FileStore { .. }), "{error}");
            let message = error.to_string();
            assert!(message.contains("File store"), "{message}");
            assert!(message.contains(&bucket), "{message}");
        });
    }

    #[test]
    fn a_file_whose_bytes_are_gone_answers_internal() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            let (_dir, running, invite) = running_with_config(&s3_config(&storage)).await;
            let (mut socket, token) = admin(&running, &invite).await;
            let (_bot, chat, _computer) = create_bot(&mut socket).await;
            let uploaded: Value = upload(
                &running,
                &token,
                chat,
                "notes.txt",
                "text/plain",
                b"read me".to_vec(),
            )
            .await
            .json()
            .await
            .unwrap();
            let file: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
            test_s3::delete(&storage, uploaded["sha256"].as_str().unwrap()).await;

            // The record still names the File but its bytes are gone: the store
            // answered, it just has nothing to return. That is not an outage.
            let refused = download(&running, &token, file).await;

            assert_eq!(refused.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
            let body: Value = refused.json().await.unwrap();
            assert_eq!(body["code"], "internal");
            assert_eq!(
                file_list(&mut socket, chat).await,
                vec![uploaded],
                "the File is still recorded"
            );
            socket.close(None).await.unwrap();
            running.shutdown().await;
        });
    }

    #[test]
    fn an_upload_to_a_store_that_is_down_answers_unavailable() {
        test_s3::block_on(async {
            let Some(_storage) = test_s3::storage() else {
                return;
            };
            // A Space whose store cannot be reached still starts and serves: the
            // bytes are the only thing the store holds, so uploads fail alone. A
            // signed request needs the credentials the S3 gate requires; the
            // environment-free check of the same classification lives in `blobs`.
            let config = "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\n\
                          region = \"us-east-1\"\nendpoint = \"http://127.0.0.1:1\"\n";
            let (_dir, running, invite) = running_with_config(config).await;
            let (mut socket, token) = admin(&running, &invite).await;
            let (_bot, chat, _computer) = create_bot(&mut socket).await;

            let refused = upload(
                &running,
                &token,
                chat,
                "notes.txt",
                "text/plain",
                b"read me".to_vec(),
            )
            .await;

            assert_eq!(refused.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            let body: Value = refused.json().await.unwrap();
            assert_eq!(body["code"], "unavailable");
            assert!(
                file_list(&mut socket, chat).await.is_empty(),
                "an unreachable store records no File"
            );
            socket.close(None).await.unwrap();
            running.shutdown().await;
        });
    }

    #[tokio::test]
    async fn a_space_refuses_an_invalid_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[storage]\nbackend = \"s3\"\nregion = \"us-east-1\"\n",
        )
        .unwrap();

        let Err(error) = start(test_home(dir.path()), 0).await else {
            panic!("an invalid configuration must refuse the start");
        };

        assert!(matches!(error, ServerError::Config(_)), "{error}");
        assert!(error.to_string().contains("storage.bucket"), "{error}");
    }

    // ---- Real Computers -------------------------------------------------

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use neha_computer::channel::{CHANNEL_VERSION, DeviceFrame, HostFrame, WireAction};
    use neha_computer::{ActionStatus, ToolOutput};

    use crate::computer_link::Answer;
    use crate::identity::MAX_PENDING_PAIRINGS;

    use super::MAX_PAIRING_FIELD_BYTES;

    /// The workspace a fake real Computer reports.
    const MACHINE_WORKSPACE: &str = "/home/ada/.neha/workspace";

    fn machine_token() -> String {
        URL_SAFE_NO_PAD.encode([7_u8; 32])
    }

    fn machine_poll() -> String {
        URL_SAFE_NO_PAD.encode([9_u8; 32])
    }

    /// The body a fake machine sends to start pairing.
    fn pair_body(machine: &str, workspace: &str, token: &str, poll_secret: &str) -> Value {
        json!({
            "machine": machine,
            "workspace": workspace,
            "token": token,
            "poll_secret": poll_secret,
        })
    }

    async fn pair_start(running: &Running, body: &Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/api/computers/pair", running.url()))
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// Starts pairing a fake machine over HTTP and returns its device and code.
    async fn pair_machine(running: &Running) -> (String, String) {
        pair_machine_as(running, "laptop", MACHINE_WORKSPACE).await
    }

    /// Starts pairing a fake machine with the given name and workspace.
    async fn pair_machine_as(
        running: &Running,
        machine: &str,
        workspace: &str,
    ) -> (String, String) {
        let response = pair_start(
            running,
            &pair_body(machine, workspace, &machine_token(), &machine_poll()),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        let device = body["device"].as_str().unwrap().to_owned();
        let code = body["code"].as_str().unwrap().to_owned();
        assert!(!device.is_empty());
        assert_eq!(code.len(), 8);

        (device, code)
    }

    async fn poll_pairing(running: &Running, device: &str, secret: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/api/computers/pair/{device}", running.url()))
            .json(&json!({"secret": secret}))
            .send()
            .await
            .unwrap()
    }

    fn add_computer(code: &str) -> CommandCall {
        CommandCall::ComputerAdd {
            code: code.to_owned(),
        }
    }

    /// Sends one command without waiting for its answer.
    async fn send_command(socket: &mut Socket, id: Id, command: CommandCall) {
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Command { id, command })
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
    }

    /// Waits until the server accepts a command it was sent.
    async fn expect_accepted(socket: &mut Socket, id: Id) {
        loop {
            match next_frame(socket).await {
                ServerFrame::Accepted { id: answered } => {
                    assert_eq!(answered, id);
                    return;
                }
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. } => {}
                other => panic!("expected Accepted for {id}, got {other:?}"),
            }
        }
    }

    /// Waits for a sent command's answer.
    async fn expect_answer(socket: &mut Socket, id: Id) -> Result<Value, (ErrorCode, String)> {
        loop {
            match next_frame(socket).await {
                ServerFrame::Committed { .. } => {}
                ServerFrame::Confirmed {
                    id: answered,
                    result,
                } => {
                    assert_eq!(answered, id);
                    return Ok(result);
                }
                ServerFrame::Error {
                    id: Some(answered),
                    code,
                    message,
                } => {
                    assert_eq!(answered, id);
                    return Err((code, message));
                }
                ServerFrame::Error { id: None, .. }
                | ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. } => {}
                other => panic!("expected an answer for {id}, got {other:?}"),
            }
        }
    }

    /// Opens a home whose local ticket stays inside this test's directory.
    fn test_home(root: &std::path::Path) -> Home {
        test_home_in(root, &root.join("config"))
    }

    /// Opens a test home that shares `config` with other test Spaces.
    fn test_home_in(root: &std::path::Path, config: &std::path::Path) -> Home {
        Home::open_with_ticket_directory(root, config).unwrap()
    }

    fn add_bot(name: &str, computer: Option<Id>) -> CommandCall {
        add_bot_on(name, computer.map(BotComputer::Id))
    }

    fn add_bot_on(name: &str, computer: Option<BotComputer>) -> CommandCall {
        CommandCall::BotCreate(BotCreate {
            name: name.to_owned(),
            computer,
        })
    }

    /// Returns a distinct machine token so a Space can pair several fakes.
    fn machine_token_n(seed: u8) -> String {
        URL_SAFE_NO_PAD.encode([seed; 32])
    }

    /// Pairs a fake machine whose token is `token` and returns its Computer.
    async fn paired_computer_with_token(
        socket: &mut Socket,
        running: &Running,
        token: &str,
    ) -> (Id, String) {
        let response = pair_start(
            running,
            &pair_body("laptop", MACHINE_WORKSPACE, token, &machine_poll()),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        let code = body["code"].as_str().unwrap().to_owned();
        let added = command(socket, Id::new(), add_computer(&code)).await;
        let computer: Id = serde_json::from_value(added["id"].clone()).unwrap();
        assert_eq!(added["kind"], "local");

        (computer, token.to_owned())
    }

    /// Pairs a fake machine and approves it, returning its Computer and token.
    async fn paired_computer(socket: &mut Socket, running: &Running) -> (Id, String) {
        let (device, code) = pair_machine(running).await;
        let added = command(socket, Id::new(), add_computer(&code)).await;
        let computer: Id = serde_json::from_value(added["id"].clone()).unwrap();
        assert_eq!(added["kind"], "local");
        assert_eq!(added["online"], false);

        let approved = poll_pairing(running, &device, &machine_poll()).await;
        assert_eq!(approved.status(), reqwest::StatusCode::OK);
        let approved: Value = approved.json().await.unwrap();
        assert_eq!(approved["state"], "approved");
        assert_eq!(approved["computer"], json!(computer));

        (computer, machine_token())
    }

    /// Connects a fake machine with a Computer token, before sending Hello.
    async fn raw_computer(running: &Running, token: &str) -> Socket {
        try_raw_computer(running, token).await.unwrap()
    }

    /// Tries the channel handshake without panicking, to assert a refusal.
    async fn try_raw_computer(
        running: &Running,
        token: &str,
    ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{}/api/computer/socket", running.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;

        Ok(socket)
    }

    /// Sends a Hello frame as a fake machine would.
    async fn send_hello(socket: &mut Socket, version: u32, computer: &str, sandbox: bool) {
        let frame = DeviceFrame::Hello {
            version,
            computer: computer.to_owned(),
            workspace: MACHINE_WORKSPACE.to_owned(),
            sandbox,
        };
        socket
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .unwrap();
    }

    /// Connects a fake machine and greets as the Computer the token names.
    async fn connect_computer(running: &Running, token: &str, computer: Id) -> Socket {
        let mut socket = raw_computer(running, token).await;
        send_hello(&mut socket, CHANNEL_VERSION, &computer.to_string(), true).await;

        socket
    }

    /// Returns the next frame the Space sends a Computer, skipping heartbeats.
    async fn next_host_frame(socket: &mut Socket) -> HostFrame {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let Message::Text(text) = message else {
                continue;
            };
            let frame: HostFrame = serde_json::from_str(text.as_str()).unwrap();
            if frame != HostFrame::Ping {
                return frame;
            }
        }
    }

    async fn reply_action(socket: &mut Socket, id: u64, status: ActionStatus, text: &str) {
        let frame = DeviceFrame::Result {
            id,
            status,
            output: ToolOutput::full(text),
        };
        socket
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .unwrap();
    }

    /// Waits until a channel closes; returns whether it did.
    async fn closed(socket: &mut Socket) -> bool {
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.next()).await,
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_)))
        )
    }

    fn online_of(listed: &Value, computer: Id) -> bool {
        listed
            .as_array()
            .and_then(|views| views.iter().find(|view| view["id"] == json!(computer)))
            .is_some_and(|view| view["online"] == json!(true))
    }

    /// Waits until a Computer's channel is up.
    async fn wait_online(socket: &mut Socket, computer: Id) {
        for _ in 0..500 {
            let listed = command(socket, Id::new(), CommandCall::ComputerList).await;
            if online_of(&listed, computer) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("Computer {computer} never came online");
    }

    /// Waits until a Computer's channel is gone.
    async fn wait_offline(socket: &mut Socket, computer: Id) {
        for _ in 0..500 {
            let listed = command(socket, Id::new(), CommandCall::ComputerList).await;
            if !online_of(&listed, computer) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("Computer {computer} stayed online");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_a_machine_over_http_adds_a_real_computer() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        // A managed Computer exists first, so creation order is observable.
        let managed = command(
            &mut socket,
            Id::new(),
            add_bot_on("Managed", Some(BotComputer::Hosted)),
        )
        .await;
        let managed_computer: Id =
            serde_json::from_value(managed["computer"]["id"].clone()).unwrap();

        let (device, code) = pair_machine(&running).await;
        let pending = poll_pairing(&running, &device, &machine_poll()).await;
        assert_eq!(pending.status(), reqwest::StatusCode::OK);
        let pending: Value = pending.json().await.unwrap();
        assert_eq!(pending, json!({"state": "pending"}));
        assert_eq!(
            poll_pairing(&running, &device, "AAAA").await.status(),
            reqwest::StatusCode::NOT_FOUND,
            "a wrong poll secret names no pairing"
        );
        assert_eq!(
            poll_pairing(&running, "unknown-device", &machine_poll())
                .await
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );

        let added = command(&mut socket, Id::new(), add_computer(&code)).await;
        let computer: Id = serde_json::from_value(added["id"].clone()).unwrap();
        assert_eq!(added["kind"], "local");
        assert_eq!(added["online"], false);
        assert_eq!(added["workspace"], MACHINE_WORKSPACE);

        // The machine learns which Computer it controls.
        let approved: Value = poll_pairing(&running, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(approved, json!({"state": "approved", "computer": computer}));

        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        let views = listed.as_array().unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0]["id"], json!(managed_computer));
        assert_eq!(views[0]["kind"], "hosted");
        assert_eq!(views[0]["online"], true);
        assert_eq!(views[1]["id"], json!(computer));
        assert_eq!(views[1]["kind"], "local");
        assert_eq!(views[1]["online"], false);

        // The code is used once.
        let refused = try_command(&mut socket, Id::new(), add_computer(&code)).await;
        assert!(matches!(refused, Err((ErrorCode::InvalidArgument, _))));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_machine_sees_approved_only_after_its_computer_is_recorded() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let token = session["token"].as_str().unwrap().to_owned();
        let mut socket = open_socket(&running, &token).await;
        let (device, code) = pair_machine(&running).await;
        let computer = Id::new();
        // The window the command closes before its append: the code is claimed
        // and the token is final, but no record names the Computer yet.
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, computer)
                .await
                .unwrap()
        );
        assert!(
            running
                .server
                .identity
                .finalize_pairing(owner, computer)
                .await
                .unwrap()
        );

        // The poll must not hand the machine a pairing it could save...
        let polled: Value = poll_pairing(&running, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            polled,
            json!({"state": "pending"}),
            "an unrecorded Computer must not answer approved"
        );
        // ...and its final token must not open a channel yet.
        assert!(
            try_raw_computer(&running, &machine_token()).await.is_err(),
            "a token without a recorded Computer must not upgrade a channel"
        );

        // The append lands: both doors open with no other change.
        let draft = neha::record::EventDraft::new(
            EventKind::ComputerCreated,
            neha::record::Author::User(owner),
            json!({
                "computer": computer,
                "owner": owner,
                "kind": "real",
                "workspace": MACHINE_WORKSPACE,
            }),
        );
        let appended = running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                format!("window:{computer}"),
                vec![draft],
            ))
            .await
            .unwrap();
        {
            use crate::store::Projection as _;
            let mut state = running.server.state.lock().unwrap();
            for event in &appended.events {
                state.apply(event).unwrap();
            }
        }

        let approved: Value = poll_pairing(&running, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            approved,
            json!({"state": "approved", "computer": computer}),
            "once recorded, the machine learns it is approved"
        );
        let mut device_socket = raw_computer(&running, &machine_token()).await;
        send_hello(
            &mut device_socket,
            CHANNEL_VERSION,
            &computer.to_string(),
            true,
        )
        .await;
        wait_online(&mut socket, computer).await;

        device_socket.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_rolls_back_a_claim_that_recorded_no_computer() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home.clone(), 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let token = session["token"].as_str().unwrap().to_owned();
        let (device, code) = pair_machine(&running).await;
        // The command claimed the code, then a crash stopped it before the
        // Computer reached the record.
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, Id::new())
                .await
                .unwrap()
        );
        running.shutdown().await;

        let restarted = start(home, 0).await.unwrap();
        let pending: Value = poll_pairing(&restarted, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            pending,
            json!({"state": "pending"}),
            "a claim without a recorded Computer is rolled back"
        );
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            None,
            "the reservation's token is deleted with the rollback"
        );
        assert!(
            restarted
                .server
                .identity
                .pairing_claims()
                .await
                .unwrap()
                .is_empty()
        );
        // The code is usable again through the real command.
        let mut socket = open_socket(&restarted, &token).await;
        let added = command(&mut socket, Id::new(), add_computer(&code)).await;
        assert_eq!(added["kind"], "local");
        let approved: Value = poll_pairing(&restarted, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(approved["state"], "approved");
        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_rolls_back_a_token_that_was_finalized_but_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home.clone(), 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let token = session["token"].as_str().unwrap().to_owned();
        let (device, code) = pair_machine(&running).await;
        let computer = Id::new();
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, computer)
                .await
                .unwrap()
        );
        // The command finalized the token, then a crash stopped it before the
        // Computer reached the record.
        assert!(
            running
                .server
                .identity
                .finalize_pairing(owner, computer)
                .await
                .unwrap()
        );
        assert_eq!(
            running
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(computer),
            "before the crash the token is active"
        );
        running.shutdown().await;

        let restarted = start(home, 0).await.unwrap();
        let pending: Value = poll_pairing(&restarted, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(pending, json!({"state": "pending"}));
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            None,
            "an unrecorded Computer never keeps a token after a restart"
        );
        assert!(
            restarted
                .server
                .identity
                .pairing_claims()
                .await
                .unwrap()
                .is_empty()
        );
        // The code is usable again through the real command.
        let mut socket = open_socket(&restarted, &token).await;
        let added = command(&mut socket, Id::new(), add_computer(&code)).await;
        assert_eq!(added["kind"], "local");
        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_keeps_a_recorded_and_finalized_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home.clone(), 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let (device, code) = pair_machine(&running).await;
        let computer = Id::new();
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, computer)
                .await
                .unwrap()
        );
        assert!(
            running
                .server
                .identity
                .finalize_pairing(owner, computer)
                .await
                .unwrap()
        );
        let draft = neha::record::EventDraft::new(
            EventKind::ComputerCreated,
            neha::record::Author::User(owner),
            json!({
                "computer": computer,
                "owner": owner,
                "kind": "real",
                "workspace": MACHINE_WORKSPACE,
            }),
        );
        let appended = running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                format!("crash:{computer}"),
                vec![draft],
            ))
            .await
            .unwrap();
        {
            use crate::store::Projection as _;
            let mut state = running.server.state.lock().unwrap();
            for event in &appended.events {
                state.apply(event).unwrap();
            }
        }
        running.shutdown().await;

        let restarted = start(home, 0).await.unwrap();
        let approved: Value = poll_pairing(&restarted, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(approved, json!({"state": "approved", "computer": computer}));
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(computer),
            "a crash after the record leaves the pairing working"
        );
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_finishes_a_claim_that_recorded_its_computer() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home.clone(), 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let (device, code) = pair_machine(&running).await;
        let computer = Id::new();
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, computer)
                .await
                .unwrap()
        );
        // The command appended its Computer, then the crash came before it
        // could write the machine's token.
        let draft = neha::record::EventDraft::new(
            EventKind::ComputerCreated,
            neha::record::Author::User(owner),
            json!({
                "computer": computer,
                "owner": owner,
                "kind": "real",
                "workspace": MACHINE_WORKSPACE,
            }),
        );
        let appended = running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                format!("crash:{computer}"),
                vec![draft],
            ))
            .await
            .unwrap();
        {
            use crate::store::Projection as _;
            let mut state = running.server.state.lock().unwrap();
            for event in &appended.events {
                state.apply(event).unwrap();
            }
        }
        running.shutdown().await;

        let restarted = start(home, 0).await.unwrap();
        let approved: Value = poll_pairing(&restarted, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            approved,
            json!({"state": "approved", "computer": computer}),
            "a recorded Computer finishes its claim at start"
        );
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(computer),
            "the machine can authenticate once the claim is finished"
        );
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_finishes_a_claim_whose_code_already_expired() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home.clone(), 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let (device, code) = pair_machine(&running).await;
        let computer = Id::new();
        assert!(
            running
                .server
                .identity
                .reserve_pairing(&code, owner, computer)
                .await
                .unwrap()
        );
        let draft = neha::record::EventDraft::new(
            EventKind::ComputerCreated,
            neha::record::Author::User(owner),
            json!({
                "computer": computer,
                "owner": owner,
                "kind": "real",
                "workspace": MACHINE_WORKSPACE,
            }),
        );
        let appended = running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                format!("crash:{computer}"),
                vec![draft],
            ))
            .await
            .unwrap();
        {
            use crate::store::Projection as _;
            let mut state = running.server.state.lock().unwrap();
            for event in &appended.events {
                state.apply(event).unwrap();
            }
        }
        // The claim's code expires in the crash window: the expiry sweeper
        // must leave a claimed row alone, and the reconciliation finishes it.
        running
            .server
            .identity
            .expire_pairing_for_test(owner, computer)
            .await
            .unwrap();
        running.shutdown().await;

        let restarted = start(home, 0).await.unwrap();
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(computer),
            "an expired claim still gets its machine a working token"
        );
        assert!(
            restarted
                .server
                .identity
                .pairing_claims()
                .await
                .unwrap()
                .first()
                .is_some_and(|claim| claim.finalized)
        );
        let poll = poll_pairing(&restarted, &device, &machine_poll()).await;
        assert_eq!(
            poll.status(),
            reqwest::StatusCode::NOT_FOUND,
            "the expired pairing itself is gone"
        );
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_computer_add_records_one_computer() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (device, code) = pair_machine(&running).await;
        let command_id = Id::new();
        let first = command(&mut socket, command_id, add_computer(&code)).await;
        let second = command(&mut socket, command_id, add_computer(&code)).await;
        assert_eq!(first, second, "a replay answers from the record");
        let computer: Id = serde_json::from_value(first["id"].clone()).unwrap();
        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        assert_eq!(
            listed.as_array().map(Vec::len),
            Some(1),
            "a replayed add must leave one Computer"
        );
        assert_eq!(
            running
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(computer),
            "the replayed Computer's token is usable"
        );
        let approved: Value = poll_pairing(&running, &device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(approved["computer"], json!(computer));

        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let events = record(&root).await;
        let created = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::ComputerCreated
                    && event.data["computer"] == json!(computer)
            })
            .count();
        assert_eq!(created, 1, "the replay must not record a second Computer");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_machine_cannot_reuse_a_paired_token() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        // Both fake machines present the same token; pairing hands each its
        // own code and Computer identifier.
        let (_first_device, first_code) = pair_machine(&running).await;
        let first = command(&mut socket, Id::new(), add_computer(&first_code)).await;
        let first_computer: Id = serde_json::from_value(first["id"].clone()).unwrap();
        let (second_device, second_code) = pair_machine(&running).await;

        let refused = try_command(&mut socket, Id::new(), add_computer(&second_code)).await;
        assert!(
            matches!(refused, Err((ErrorCode::Internal, _))),
            "the token's owner is reserved before any Computer is published"
        );
        // No second Computer, the first token still works, and the second
        // machine stays pending with its code untouched by a half-claim.
        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert_eq!(
            running
                .server
                .identity
                .authenticate_computer(&machine_token())
                .await
                .unwrap(),
            Some(first_computer)
        );
        let pending: Value = poll_pairing(&running, &second_device, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(pending, json!({"state": "pending"}));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_start_validates_its_fields_and_recycles_the_waiting_quota() {
        let (_dir, running, _invite) = running().await;
        let overlong = "m".repeat(MAX_PAIRING_FIELD_BYTES + 1);
        let overlong_workspace = format!("/{}", "w".repeat(MAX_PAIRING_FIELD_BYTES));
        let short_token = URL_SAFE_NO_PAD.encode([7_u8; 31]);
        let long_token = URL_SAFE_NO_PAD.encode([7_u8; 33]);
        let ansi_machine = "laptop\u{1b}[31mforged";
        let cases = [
            pair_body("  ", MACHINE_WORKSPACE, &machine_token(), &machine_poll()),
            pair_body("laptop", "relative/dir", &machine_token(), &machine_poll()),
            pair_body("laptop", MACHINE_WORKSPACE, "not base64!", &machine_poll()),
            pair_body("laptop", MACHINE_WORKSPACE, &machine_token(), ""),
            pair_body(
                &overlong,
                MACHINE_WORKSPACE,
                &machine_token(),
                &machine_poll(),
            ),
            pair_body(
                "laptop",
                &overlong_workspace,
                &machine_token(),
                &machine_poll(),
            ),
            pair_body(
                ansi_machine,
                MACHINE_WORKSPACE,
                &machine_token(),
                &machine_poll(),
            ),
            pair_body("laptop", "/tmp/ws\n/tmp", &machine_token(), &machine_poll()),
            pair_body("laptop", MACHINE_WORKSPACE, &short_token, &machine_poll()),
            pair_body("laptop", MACHINE_WORKSPACE, &machine_token(), &long_token),
        ];
        for body in cases {
            let refused = pair_start(&running, &body).await;
            assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
            let refused: Value = refused.json().await.unwrap();
            assert_eq!(refused["code"], "invalid_argument");
            let message = refused["message"].as_str().unwrap();
            assert!(
                !message.contains("forged") && !message.contains(&overlong),
                "a refusal must not echo the refused value: {message}"
            );
        }

        let mut machines = Vec::new();
        for _ in 0..MAX_PENDING_PAIRINGS {
            machines.push(pair_machine(&running).await);
        }
        // A full quota recycles a waiting machine instead of refusing the
        // legitimate one, so an anonymous caller cannot lock a User out.
        let newest = pair_machine(&running).await;
        let mut pending = 0;
        let mut evicted = 0;
        for (device, _code) in &machines {
            let status = poll_pairing(&running, device, &machine_poll())
                .await
                .status();
            match status {
                reqwest::StatusCode::OK => pending += 1,
                reqwest::StatusCode::NOT_FOUND => evicted += 1,
                other => panic!("unexpected poll status: {other}"),
            }
        }
        assert_eq!(evicted, 1, "only one waiting pairing is recycled");
        assert_eq!(pending, MAX_PENDING_PAIRINGS - 1);
        let newest: Value = poll_pairing(&running, &newest.0, &machine_poll())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(newest, json!({"state": "pending"}));

        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_start_accepts_the_longest_fields() {
        let (_dir, running, _invite) = running().await;
        let machine = "m".repeat(MAX_PAIRING_FIELD_BYTES);
        let workspace = format!("/{}", "w".repeat(MAX_PAIRING_FIELD_BYTES - 1));
        assert_eq!(workspace.len(), MAX_PAIRING_FIELD_BYTES);

        let started = pair_start(
            &running,
            &pair_body(&machine, &workspace, &machine_token(), &machine_poll()),
        )
        .await;

        assert_eq!(started.status(), reqwest::StatusCode::OK);
        running.shutdown().await;
    }

    #[test]
    fn pairing_attempts_are_reserved_atomically_and_settled_by_outcome() {
        let mut attempts = super::PairingAttempts::default();
        let ada = Id::new();
        let grace = Id::new();

        // A successful attempt frees its place at once: ten bursts of ten may
        // all start, one at a time.
        for _ in 0..10 {
            for _ in 0..10 {
                assert!(attempts.reserve(ada));
            }
            for _ in 0..10 {
                attempts.settle(ada, false);
            }
        }
        // A failed one stays counted: after ten failures the window is full.
        for _ in 0..10 {
            assert!(attempts.reserve(ada));
            attempts.settle(ada, true);
        }
        assert!(
            !attempts.reserve(ada),
            "a full window admits no more attempts"
        );
        // Another account has its own window.
        assert!(
            attempts.reserve(grace),
            "accounts are counted apart from each other"
        );
        assert!(!attempts.reserve(ada));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_computer_that_stops_reading_is_disconnected() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let _device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;
        let link = running
            .server
            .links
            .link(computer)
            .expect("a connected Computer has a live link");

        // The fake device never reads this frame and it cannot fit through the
        // socket buffers, so the writer's deadline passes: the request fails,
        // the link closes and the Computer is taken offline. A stuck Computer
        // must not grow the Space's memory or strand work.
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            link.write(
                999,
                Id::new(),
                "big.bin",
                vec![7_u8; 16 * 1024 * 1024],
                &Default::default(),
            ),
        )
        .await
        .expect("a blocked write must fail by its deadline");
        assert!(matches!(answer, Answer::Gone(_)), "{answer:?}");

        wait_offline(&mut socket, computer).await;
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_pairing_attempts_are_rate_limited_per_account() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        for _ in 0..10 {
            let refused = try_command(&mut socket, Id::new(), add_computer("ZZZZZZZZ")).await;
            assert!(matches!(refused, Err((ErrorCode::InvalidArgument, _))));
        }
        let limited = try_command(&mut socket, Id::new(), add_computer("ZZZZZZZZ")).await;
        assert!(
            matches!(limited, Err((ErrorCode::RateLimited, _))),
            "an account may fail ten pairing attempts a minute"
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_pairing_attempts_cannot_exceed_the_limit() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let attempts = 20;

        // One command per connection: twenty commands would not overlap on one
        // socket, and overlap is what the reservation must survive.
        let mut tasks = Vec::new();
        for _ in 0..attempts {
            let mut socket = open_socket(&running, &token).await;
            tasks.push(tokio::spawn(async move {
                try_command(&mut socket, Id::new(), add_computer("ZZZZZZZZ")).await
            }));
        }
        let mut refused = 0;
        let mut limited = 0;
        for result in futures_util::future::join_all(tasks).await {
            match result.unwrap() {
                Err((ErrorCode::InvalidArgument, _)) => refused += 1,
                Err((ErrorCode::RateLimited, _)) => limited += 1,
                other => panic!("unexpected pairing answer: {other:?}"),
            }
        }

        assert_eq!(refused, 10, "a burst gets exactly the window's attempts");
        assert_eq!(limited, attempts - 10);
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_channel_that_never_greets_is_closed() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let mut silent = raw_computer(&running, &machine).await;

        let started = std::time::Instant::now();
        assert!(
            closed(&mut silent).await,
            "a channel that never sends Hello must be closed"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the Hello timeout must close it promptly, took {:?}",
            started.elapsed()
        );
        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        assert!(
            !online_of(&listed, computer),
            "a channel that never greeted is not online"
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reconnect_closes_the_replaced_socket() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let mut first = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;

        // The same Computer opens a second channel: the Space keeps the new
        // one and closes the old connection instead of serving both.
        let mut second = connect_computer(&running, &machine, computer).await;
        assert!(closed(&mut first).await, "the replaced socket must close");
        wait_online(&mut socket, computer).await;

        second.close(None).await.unwrap();
        wait_offline(&mut socket, computer).await;
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_computer_channel_needs_its_token_and_a_valid_hello() {
        let (_dir, running, invite) = running().await;
        let (mut socket, account_token) = admin(&running, &invite).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let socket_url = format!("ws://{}/api/computer/socket", running.addr);

        assert!(
            tokio_tungstenite::connect_async(socket_url.clone())
                .await
                .is_err(),
            "a channel needs a Computer token"
        );
        let mut request = socket_url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {account_token}").parse().unwrap(),
        );
        assert!(
            tokio_tungstenite::connect_async(request).await.is_err(),
            "an account token is not a Computer token"
        );
        let mut request = socket_url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer not-a-token".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(request).await.is_err());

        // A Hello that names another Computer would act as that Computer.
        let mut wrong = raw_computer(&running, &machine).await;
        send_hello(&mut wrong, CHANNEL_VERSION, &Id::new().to_string(), true).await;
        assert!(closed(&mut wrong).await);

        // A daemon that cannot fence its machine never serves one.
        let mut unfenced = raw_computer(&running, &machine).await;
        send_hello(&mut unfenced, CHANNEL_VERSION, &computer.to_string(), false).await;
        assert!(closed(&mut unfenced).await);

        // Both ends must be the same release: the previous channel version and
        // a future one are refused, in either direction of upgrade.
        for version in [CHANNEL_VERSION - 1, CHANNEL_VERSION + 1] {
            let mut other = raw_computer(&running, &machine).await;
            send_hello(&mut other, version, &computer.to_string(), true).await;
            assert!(
                closed(&mut other).await,
                "version {version} must be refused"
            );
        }

        // The right Hello registers, and dropping it marks the Computer offline.
        let mut device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;
        device.close(None).await.unwrap();
        wait_offline(&mut socket, computer).await;

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_on_a_real_computer_round_trips_an_action() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "pwd"})),
            text("done from the machine"),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();

        let mut device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;
        let shown_bot = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown_bot["computer"]["id"], json!(computer));
        assert_eq!(shown_bot["computer"]["kind"], "local");
        assert_eq!(shown_bot["computer"]["online"], true);

        let queued = submit(&mut socket, chat, "run pwd", None).await;
        let run = run_of(&queued);
        allow_first_use(&mut socket, chat).await;
        let (id, named, action) = match next_host_frame(&mut device).await {
            HostFrame::Action {
                id, bot, action, ..
            } => (id, bot, action),
            other => panic!("expected an action frame, got {other:?}"),
        };
        assert_eq!(named, bot.to_string());
        assert_eq!(
            action,
            WireAction::Shell {
                command: "pwd".to_owned()
            }
        );
        reply_action(
            &mut device,
            id,
            ActionStatus::Succeeded,
            "/home/ada/.neha/workspace/scout",
        )
        .await;

        let shown = wait_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["actions"][0]["tool"], "shell");
        assert_eq!(shown["actions"][0]["status"], "succeeded");
        assert_eq!(
            shown["actions"][0]["output"],
            "/home/ada/.neha/workspace/scout"
        );
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(
            messages.as_array().unwrap().last().unwrap()["text"],
            "done from the machine"
        );

        device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnected_computer_leaves_its_action_unknown_and_goes_offline() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) =
            scripted_provider(vec![tool("shell", json!({"command": "pwd"}))]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let mut device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;

        let queued = submit(&mut socket, chat, "run pwd", None).await;
        let run = run_of(&queued);
        allow_first_use(&mut socket, chat).await;
        let HostFrame::Action { .. } = next_host_frame(&mut device).await else {
            panic!("expected an action frame");
        };
        // The machine leaves while the Space still owes the action an answer.
        device.close(None).await.unwrap();

        let shown = wait_run(&mut socket, run).await;
        assert_eq!(shown["status"], "failed");
        assert_eq!(
            shown["actions"][0]["status"], "unknown",
            "an action whose answer never returned is unknown"
        );
        assert!(
            shown["actions"][0]["error"]
                .as_str()
                .unwrap()
                .contains("the Computer went offline"),
            "a pending action is unknown when its Computer leaves: {}",
            shown["actions"][0]["error"]
        );
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(
            messages.as_array().unwrap().last().unwrap()["text"],
            "I lost my Computer before the action returned."
        );
        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        assert_eq!(listed[0]["online"], false);

        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_waits_for_an_offline_computer_and_starts_when_it_connects() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "echo hi"})),
            text("done"),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (computer, machine) = paired_computer(&mut socket, &running).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();

        // No daemon is connected: the run must wait, not fail and not overtake.
        let queued = submit(&mut socket, chat, "run echo", None).await;
        let run = run_of(&queued);
        assert_eq!(show_run(&mut socket, run).await["status"], "queued");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            show_run(&mut socket, run).await["status"],
            "queued",
            "an offline Computer holds its queue"
        );

        let mut device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;
        allow_first_use(&mut socket, chat).await;
        let (id, named, _action) = match next_host_frame(&mut device).await {
            HostFrame::Action {
                id, bot, action, ..
            } => (id, bot, action),
            other => panic!("expected an action frame, got {other:?}"),
        };
        assert_eq!(named, bot.to_string());
        reply_action(&mut device, id, ActionStatus::Succeeded, "hi").await;

        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_joins_a_real_computer_without_creating_one() {
        let (dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let managed = command(
            &mut socket,
            Id::new(),
            add_bot_on("Managed", Some(BotComputer::Hosted)),
        )
        .await;
        let managed_computer: Id =
            serde_json::from_value(managed["computer"]["id"].clone()).unwrap();
        let refused = try_command(
            &mut socket,
            Id::new(),
            add_bot("Nope", Some(managed_computer)),
        )
        .await;
        assert!(matches!(refused, Err((ErrorCode::InvalidArgument, _))));
        let missing = try_command(&mut socket, Id::new(), add_bot("Nope", Some(Id::new()))).await;
        assert!(matches!(missing, Err((ErrorCode::NotFound, _))));

        let (computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        assert_eq!(created["bot"]["computer"], json!(computer));
        assert_eq!(created["computer"]["id"], json!(computer));
        assert_eq!(created["computer"]["kind"], "local");
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["computer"]["id"], json!(computer));

        // A second Bot may join the same real Computer.
        let second = command(&mut socket, Id::new(), add_bot("Other", Some(computer))).await;
        assert_eq!(second["computer"]["id"], json!(computer));

        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let events = record(&root).await;
        let for_real: Vec<_> = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::ComputerCreated
                    && event.data["computer"] == json!(computer)
            })
            .collect();
        assert_eq!(
            for_real.len(),
            1,
            "a Bot on a real Computer creates no Computer: the pairing did"
        );
        let shared = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::BotCreated && event.data["computer"] == json!(computer)
            })
            .count();
        assert_eq!(shared, 2, "two Bots share the one real Computer");
        assert_eq!(dir.path(), root, "the record lives in the Space's home");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_moves_to_another_real_computer() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (first, _first_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(1)).await;
        let (second, _second_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(2)).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(first))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();

        let command_id = Id::new();
        let moved = command(
            &mut socket,
            command_id,
            CommandCall::BotMove {
                bot,
                computer: second,
            },
        )
        .await;
        assert_eq!(moved["bot"]["id"], json!(bot));
        assert_eq!(moved["bot"]["computer"], json!(second));
        assert_eq!(moved["from"]["id"], json!(first));
        assert_eq!(moved["from"]["kind"], "local");
        assert_eq!(moved["to"]["id"], json!(second));
        assert_eq!(moved["to"]["kind"], "local");

        // Replaying the command answers the recorded result and writes nothing.
        let again = command(
            &mut socket,
            command_id,
            CommandCall::BotMove {
                bot,
                computer: second,
            },
        )
        .await;
        assert_eq!(again, moved);

        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["computer"]["id"], json!(second));
        let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        assert_eq!(
            listed.as_array().map(Vec::len),
            Some(2),
            "a move never creates a Computer"
        );

        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let events = record(&root).await;
        let moves: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::BotMoved)
            .collect();
        assert_eq!(moves.len(), 1, "one command writes one move event");
        assert_eq!(
            moves[0].data,
            json!({"bot": bot, "from": first, "to": second})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_checks_the_bot_and_both_computers() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (first, _first_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(3)).await;
        let (second, _second_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(4)).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(first))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let managed = command(
            &mut socket,
            Id::new(),
            add_bot_on("Managed", Some(BotComputer::Hosted)),
        )
        .await;
        let managed_bot: Id = serde_json::from_value(managed["bot"]["id"].clone()).unwrap();
        let managed_computer: Id =
            serde_json::from_value(managed["computer"]["id"].clone()).unwrap();

        let same = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot,
                computer: first,
            },
        )
        .await;
        match same {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("already works on that Computer"),
                "{message}"
            ),
            other => panic!("a Bot cannot move to its own Computer: {other:?}"),
        }

        let unknown_computer = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot,
                computer: Id::new(),
            },
        )
        .await;
        assert!(matches!(unknown_computer, Err((ErrorCode::NotFound, _))));

        let unknown_bot = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot: Id::new(),
                computer: second,
            },
        )
        .await;
        assert!(matches!(unknown_bot, Err((ErrorCode::NotFound, _))));

        let managed_target = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot,
                computer: managed_computer,
            },
        )
        .await;
        match managed_target {
            Err((ErrorCode::InvalidArgument, message)) => {
                assert!(
                    message.contains("a Bot moves to a Local Computer"),
                    "{message}"
                );
            }
            other => panic!("a managed target is refused: {other:?}"),
        }

        let managed_source = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot: managed_bot,
                computer: second,
            },
        )
        .await;
        match managed_source {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("cannot move yet"),
                "a managed source explains the gap: {message}"
            ),
            other => panic!("a Bot on a managed Computer cannot move: {other:?}"),
        }

        // Another User's Bot and Computer are not found, exactly like missing
        // ones: the caller learns nothing about them.
        let identity = crate::identity::Identity::open(&Home::open(running.server_home()).unwrap())
            .await
            .unwrap();
        identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let grace_token = sign_in(&running, "Grace").await;
        let mut grace = open_socket(&running, &grace_token).await;
        let (grace_computer, _grace_machine) =
            paired_computer_with_token(&mut grace, &running, &machine_token_n(5)).await;
        let grace_created = command(
            &mut grace,
            Id::new(),
            add_bot("Grace Bot", Some(grace_computer)),
        )
        .await;
        let grace_bot: Id = serde_json::from_value(grace_created["bot"]["id"].clone()).unwrap();
        let foreign_bot = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot: grace_bot,
                computer: second,
            },
        )
        .await;
        assert!(matches!(foreign_bot, Err((ErrorCode::NotFound, _))));
        let foreign_computer = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot,
                computer: grace_computer,
            },
        )
        .await;
        assert!(matches!(foreign_computer, Err((ErrorCode::NotFound, _))));

        // None of the refusals moved the Bot.
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["computer"]["id"], json!(first));
        let moved = command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove {
                bot,
                computer: second,
            },
        )
        .await;
        assert_eq!(moved["bot"]["computer"], json!(second));

        grace.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_run_follows_its_bot_to_another_computer() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "pwd"})),
            text("done on the new Computer"),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (old, _old_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(6)).await;
        let (new, new_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(7)).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(old))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let mut device = connect_computer(&running, &new_machine, new).await;
        wait_online(&mut socket, new).await;

        // The old Computer is offline, so the run waits for it.
        let queued = submit(&mut socket, chat, "run pwd", None).await;
        let run = run_of(&queued);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(show_run(&mut socket, run).await["status"], "queued");

        // Moving the Bot wakes its pump: the waiting run starts on the new
        // Computer without the old one ever connecting.
        let moved = command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove { bot, computer: new },
        )
        .await;
        assert_eq!(moved["bot"]["computer"], json!(new));
        allow_first_use(&mut socket, chat).await;
        let (id, named, action) = match next_host_frame(&mut device).await {
            HostFrame::Action {
                id, bot, action, ..
            } => (id, bot, action),
            other => panic!("expected an action frame, got {other:?}"),
        };
        assert_eq!(named, bot.to_string());
        assert_eq!(
            action,
            WireAction::Shell {
                command: "pwd".to_owned()
            }
        );
        reply_action(
            &mut device,
            id,
            ActionStatus::Succeeded,
            "/home/ada/.neha/workspace/scout",
        )
        .await;

        let shown = wait_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["actions"][0]["status"], "succeeded");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(
            messages.as_array().unwrap().last().unwrap()["text"],
            "done on the new Computer"
        );

        device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_moves_of_one_bot_record_one_event() {
        let (dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let mut first = open_socket(&running, &token).await;
        let mut second = open_socket(&running, &token).await;
        let (a, _a_machine) =
            paired_computer_with_token(&mut first, &running, &machine_token_n(10)).await;
        let (b, _b_machine) =
            paired_computer_with_token(&mut first, &running, &machine_token_n(11)).await;
        let (c, _c_machine) =
            paired_computer_with_token(&mut first, &running, &machine_token_n(12)).await;
        let created = command(&mut first, Id::new(), add_bot("Scout", Some(a))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();

        // Hold the Bot's lock so both move commands are observably waiting:
        // each reads the Bot's Computer (A) before taking the lock. Once it is
        // released the first command moves; the other still holds A as what it
        // saw, so it is refused instead of becoming a second, different move.
        let held = running.server.runs.admit(bot).await;
        let first_id = Id::new();
        let second_id = Id::new();
        send_command(
            &mut first,
            first_id,
            CommandCall::BotMove { bot, computer: b },
        )
        .await;
        send_command(
            &mut second,
            second_id,
            CommandCall::BotMove { bot, computer: c },
        )
        .await;
        expect_accepted(&mut first, first_id).await;
        expect_accepted(&mut second, second_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(held);

        let one = expect_answer(&mut first, first_id).await;
        let two = expect_answer(&mut second, second_id).await;
        let (winner, loser) = match (one, two) {
            (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
            (one, two) => panic!("exactly one move must win: {one:?} / {two:?}"),
        };
        assert!(
            matches!(loser.0, ErrorCode::InvalidArgument),
            "the losing move is refused: {loser:?}"
        );
        assert!(
            loser.1.contains("moved while the command was waiting"),
            "{loser:?}"
        );
        let winner_computer: Id =
            serde_json::from_value(winner["bot"]["computer"].clone()).unwrap();
        assert!(winner_computer == b || winner_computer == c);
        let target: Id = serde_json::from_value(winner["to"]["id"].clone()).unwrap();
        assert_eq!(target, winner_computer);

        let root = running.server_home();
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        running.shutdown().await;
        let events = record(&root).await;
        let moves: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::BotMoved)
            .collect();
        assert_eq!(moves.len(), 1, "only the winning move is recorded");
        assert_eq!(moves[0].data["from"], json!(a));
        assert_eq!(moves[0].data["to"], json!(winner_computer));

        // The record replays: a restart keeps the Bot on the winning Computer.
        let restarted = start(test_home(dir.path()), 0).await.unwrap();
        let restarted_token = sign_in(&restarted, "Ada").await;
        let mut restarted_socket = open_socket(&restarted, &restarted_token).await;
        let shown = command(
            &mut restarted_socket,
            Id::new(),
            CommandCall::BotShow { bot },
        )
        .await;
        assert_eq!(shown["computer"]["id"], json!(winner_computer));
        restarted_socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_and_a_move_agree_on_the_records_order() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let rounds = 30;
        let mut script = Vec::new();
        for round in 0..rounds {
            script.push(tool("shell", json!({"command": "echo hi"})));
            script.push(text(&format!("done {round}")));
        }
        let (base, provider_task) = scripted_provider(script).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (old, old_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(20)).await;
        let (new, new_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(21)).await;
        let mut old_device = connect_computer(&running, &old_machine, old).await;
        let mut new_device = connect_computer(&running, &new_machine, new).await;
        wait_online(&mut socket, old).await;
        wait_online(&mut socket, new).await;

        for round in 0..rounds {
            let created = command(
                &mut socket,
                Id::new(),
                add_bot(&format!("Scout{round}"), Some(old)),
            )
            .await;
            let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
            let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
            let queued = submit(&mut socket, chat, "run echo", None).await;
            let run = run_of(&queued);
            // The pump and this move race for the Bot's admit lock. Whichever
            // records first decides which Computer the run uses.
            let moved = command(
                &mut socket,
                Id::new(),
                CommandCall::BotMove { bot, computer: new },
            )
            .await;
            assert_eq!(moved["bot"]["computer"], json!(new));
            allow_first_use(&mut socket, chat).await;

            let (from_old, frame) =
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::select! {
                        frame = next_host_frame(&mut old_device) => (true, frame),
                        frame = next_host_frame(&mut new_device) => (false, frame),
                    }
                })
                .await
                .expect("the run must reach one of the Computers");
            let (id, named, action) = match frame {
                HostFrame::Action {
                    id, bot, action, ..
                } => (id, bot, action),
                other => panic!("expected an action frame, got {other:?}"),
            };
            assert_eq!(named, bot.to_string());
            assert_eq!(
                action,
                WireAction::Shell {
                    command: "echo hi".to_owned()
                }
            );
            let answer = format!("done {round}");
            if from_old {
                reply_action(&mut old_device, id, ActionStatus::Succeeded, &answer).await;
            } else {
                reply_action(&mut new_device, id, ActionStatus::Succeeded, &answer).await;
            }
            assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");

            let events = running
                .server
                .store
                .events_from(Seq::ZERO, 100_000)
                .await
                .unwrap()
                .events;
            let started = events
                .iter()
                .find(|event| {
                    event.kind == EventKind::RunStarted && event.data["run"] == json!(run)
                })
                .expect("the run started")
                .seq;
            let moved = events
                .iter()
                .find(|event| event.kind == EventKind::BotMoved && event.data["bot"] == json!(bot))
                .expect("the move was recorded")
                .seq;
            if started < moved {
                assert!(
                    from_old,
                    "round {round}: a run started before the move must use the old Computer"
                );
            } else {
                assert!(
                    !from_old,
                    "round {round}: a run started after the move must use the new Computer"
                );
            }
        }

        // Every round's interleaving replays without a projection error.
        let root = running.server_home();
        old_device.close(None).await.unwrap();
        new_device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let restarted = start(test_home(&root), 0).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_does_not_outrun_a_run_that_saw_the_old_computer() {
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let mut admin = open_socket(&running, &token).await;
        let mut mover = open_socket(&running, &token).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "echo hi"})),
            text("done"),
        ])
        .await;
        command(&mut admin, Id::new(), provider_add(base)).await;
        let (old, old_machine) =
            paired_computer_with_token(&mut admin, &running, &machine_token_n(30)).await;
        let (new, new_machine) =
            paired_computer_with_token(&mut admin, &running, &machine_token_n(31)).await;
        let mut old_device = connect_computer(&running, &old_machine, old).await;
        let mut new_device = connect_computer(&running, &new_machine, new).await;
        wait_online(&mut admin, old).await;
        wait_online(&mut admin, new).await;
        let created = command(&mut admin, Id::new(), add_bot("Scout", Some(old))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();

        // Pause the pump after it snapshot the run on the old Computer.
        let (reached, release) = crate::runs::pause_start_for_test(bot);
        let queued = submit(&mut admin, chat, "run echo", None).await;
        let run = run_of(&queued);
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified())
            .await
            .expect("the pump reaches the pause");

        // The move lands while the pump stands between its snapshot and
        // `RunStarted`. With the per-Bot lock the move cannot commit until the
        // pump records the start; without it, the move commits first.
        let move_id = Id::new();
        let mover_task = tokio::spawn(async move {
            try_command(
                &mut mover,
                move_id,
                CommandCall::BotMove { bot, computer: new },
            )
            .await
        });
        for _ in 0..50 {
            if running
                .server
                .store
                .command(move_id)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        release.notify_one();
        let moved = mover_task.await.unwrap().expect("the move succeeds");
        assert_eq!(moved.1["bot"]["computer"], json!(new));
        allow_first_use(&mut admin, chat).await;

        // The run still starts on the Computer it snapshot (the old one)...
        let (from_old, frame) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                frame = next_host_frame(&mut old_device) => (true, frame),
                frame = next_host_frame(&mut new_device) => (false, frame),
            }
        })
        .await
        .expect("the run starts");
        assert!(
            from_old,
            "a run paused after its snapshot keeps the old Computer"
        );
        let (id, named, action) = match frame {
            HostFrame::Action {
                id, bot, action, ..
            } => (id, bot, action),
            other => panic!("expected an action frame, got {other:?}"),
        };
        assert_eq!(named, bot.to_string());
        assert_eq!(
            action,
            WireAction::Shell {
                command: "echo hi".to_owned()
            }
        );
        reply_action(&mut old_device, id, ActionStatus::Succeeded, "hi").await;
        assert_eq!(wait_run(&mut admin, run).await["status"], "succeeded");

        // ...and the record puts the move after that start.
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let started = events
            .iter()
            .find(|event| event.kind == EventKind::RunStarted && event.data["run"] == json!(run))
            .expect("the run started")
            .seq;
        let moved_seq = events
            .iter()
            .find(|event| event.kind == EventKind::BotMoved && event.data["bot"] == json!(bot))
            .expect("the move was recorded")
            .seq;
        assert!(
            started < moved_seq,
            "RunStarted must be recorded before the interleaved move"
        );

        old_device.close(None).await.unwrap();
        new_device.close(None).await.unwrap();
        provider_task.abort();
        admin.close(None).await.unwrap();
        running.shutdown().await;
    }
    fn local_ticket_file(dir: &std::path::Path) -> std::path::PathBuf {
        crate::ticket::path(&test_home(dir)).expect("a test configuration directory")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_hands_the_admin_a_fresh_local_ticket() {
        let (dir, running, invite) = running().await;
        assert!(
            running.local_ticket.is_none(),
            "a Space with no Admin has no ticket to hand out"
        );
        join(&running, &invite, "Ada", "correct horse battery").await;
        running.shutdown().await;

        let started = start(test_home(dir.path()), 0).await.unwrap();
        let path = started
            .local_ticket
            .clone()
            .expect("the Admin gets a ticket");
        assert_eq!(path, local_ticket_file(dir.path()));
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ticket = body["ticket"].as_str().unwrap().to_owned();
        assert_eq!(body["url"], started.url());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "only the Space's User can read the ticket");
        }
        started.shutdown().await;

        // A ticket binds one start of the Space: the next start replaces it.
        let next = start(test_home(dir.path()), 0).await.unwrap();
        let next_body: Value =
            serde_json::from_str(&std::fs::read_to_string(local_ticket_file(dir.path())).unwrap())
                .unwrap();
        assert_ne!(
            next_body["ticket"],
            json!(ticket),
            "a restart replaces the ticket"
        );
        let refused = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", next.url()))
            .json(&json!({"ticket": ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
        let refused: Value = refused.json().await.unwrap();
        assert_eq!(refused["message"], "the ticket is not valid");
        next.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ticket_from_the_endpoint_needs_a_session_and_pairs_once() {
        let (_dir, running, invite) = running().await;
        let unauthenticated = reqwest::Client::new()
            .post(format!("{}/api/computer/ticket", running.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();

        let issued = reqwest::Client::new()
            .post(format!("{}/api/computer/ticket", running.url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(issued.status(), reqwest::StatusCode::OK);
        let issued: Value = issued.json().await.unwrap();
        let ticket = issued["ticket"].as_str().unwrap().to_owned();

        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", running.url()))
            .json(&json!({"ticket": ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        let paired: Value = paired.json().await.unwrap();
        let computer: Id = serde_json::from_value(paired["computer"].clone()).unwrap();
        let credential = paired["credential"].as_str().unwrap().to_owned();

        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 10_000)
            .await
            .unwrap()
            .events;
        let created: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::ComputerCreated)
            .collect();
        assert_eq!(created.len(), 1, "one pairing records one Computer");
        assert_eq!(
            created[0].data,
            json!({"computer": computer, "owner": owner, "kind": "local", "workspace": ""})
        );

        // The credential opens the Computer channel.
        let mut admin = open_socket(&running, &token).await;
        let mut device = connect_computer(&running, &credential, computer).await;
        wait_online(&mut admin, computer).await;
        let views = command(&mut admin, Id::new(), CommandCall::ComputerList).await;
        assert_eq!(views[0]["kind"], "local");

        let again = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", running.url()))
            .json(&json!({"ticket": issued["ticket"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::UNAUTHORIZED);

        device.close(None).await.unwrap();
        admin.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_serve_ticket_file_pairs_once_and_is_deleted() {
        let (dir, running, invite) = running().await;
        join(&running, &invite, "Ada", "correct horse battery").await;
        running.shutdown().await;
        let started = start(test_home(dir.path()), 0).await.unwrap();
        let path = started
            .local_ticket
            .clone()
            .expect("the Admin got a ticket");
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ticket = body["ticket"].as_str().unwrap().to_owned();

        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", started.url()))
            .json(&json!({"ticket": ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        assert!(
            !local_ticket_file(dir.path()).exists(),
            "the file disappears with its ticket"
        );
        started.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_start_ticket_file_disappears_with_its_ticket() {
        let (dir, running, invite) = running().await;
        join(&running, &invite, "Ada", "correct horse battery").await;
        running.shutdown().await;
        let started = start(test_home(dir.path()), 0).await.unwrap();
        let path = started
            .local_ticket
            .clone()
            .expect("the Admin got a ticket");
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let start_ticket = body["ticket"].as_str().unwrap().to_owned();
        let session = sign_in(&started, "Ada").await;
        let issued = reqwest::Client::new()
            .post(format!("{}/api/computer/ticket", started.url()))
            .bearer_auth(&session)
            .send()
            .await
            .unwrap();
        assert_eq!(issued.status(), reqwest::StatusCode::OK);

        // A ticket the endpoint issues is not the start ticket: pairing it
        // must leave the Admin's start file alone.
        let issued: Value = issued.json().await.unwrap();
        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", started.url()))
            .json(&json!({"ticket": issued["ticket"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        assert!(
            local_ticket_file(dir.path()).exists(),
            "another User's ticket does not delete the start ticket"
        );

        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", started.url()))
            .json(&json!({"ticket": start_ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        assert!(
            !local_ticket_file(dir.path()).exists(),
            "the start ticket deletes its own file"
        );
        started.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_forgets_tokens_whose_computer_the_record_lost() {
        let (dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap().to_owned();
        let mut admin = open_socket(&running, &token).await;
        let issued = reqwest::Client::new()
            .post(format!("{}/api/computer/ticket", running.url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let issued: Value = issued.json().await.unwrap();
        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", running.url()))
            .json(&json!({"ticket": issued["ticket"]}))
            .send()
            .await
            .unwrap();
        let paired: Value = paired.json().await.unwrap();
        let computer: Id = serde_json::from_value(paired["computer"].clone()).unwrap();
        let credential = paired["credential"].as_str().unwrap().to_owned();

        // A token written for a Computer the record never got: the crash
        // window after a token write and before its append.
        let orphan = Id::new();
        running
            .server
            .identity
            .record_computer_token(orphan, "orphan-credential")
            .await
            .unwrap();
        admin.close(None).await.unwrap();
        running.shutdown().await;

        let restarted = start(test_home(dir.path()), 0).await.unwrap();
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer("orphan-credential")
                .await
                .unwrap(),
            None,
            "a start forgets an orphan token"
        );
        assert_eq!(
            restarted
                .server
                .identity
                .authenticate_computer(&credential)
                .await
                .unwrap(),
            Some(computer),
            "a token for a recorded Computer survives the reconciliation"
        );
        let token = sign_in(&restarted, "Ada").await;
        let mut socket = open_socket(&restarted, &token).await;
        let mut device = connect_computer(&restarted, &credential, computer).await;
        wait_online(&mut socket, computer).await;
        device.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_clears_only_its_own_spaces_ticket_file() {
        let config = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        // The first Space joins an Admin, then restarts so its start issues a
        // ticket file in the shared configuration directory.
        let space = start(test_home_in(first.path(), config.path()), 0)
            .await
            .unwrap();
        let invite = space.invite.clone().expect("a fresh Space invites");
        join(&space, &invite, "Ada", "correct horse battery").await;
        space.shutdown().await;
        let first_space = start(test_home_in(first.path(), config.path()), 0)
            .await
            .unwrap();
        let first_file = first_space
            .local_ticket
            .clone()
            .expect("the first Space issued a ticket");
        assert!(first_file.starts_with(config.path()));
        let first_body: Value =
            serde_json::from_str(&std::fs::read_to_string(&first_file).unwrap()).unwrap();
        let first_ticket = first_body["ticket"].as_str().unwrap().to_owned();

        // The second Space uses the same configuration directory: its start
        // must not clear the first Space's waiting ticket.
        let second_space = start(test_home_in(second.path(), config.path()), 0)
            .await
            .unwrap();
        assert!(
            first_file.exists(),
            "another Space's start leaves the first ticket file alone"
        );
        let second_invite = second_space.invite.clone().expect("a fresh Space invites");
        join(
            &second_space,
            &second_invite,
            "Ada",
            "correct horse battery",
        )
        .await;
        second_space.shutdown().await;
        let second_space = start(test_home_in(second.path(), config.path()), 0)
            .await
            .unwrap();
        let second_file = second_space
            .local_ticket
            .clone()
            .expect("the second Space issued a ticket");
        assert_ne!(first_file, second_file, "each Space has its own folder");
        assert!(first_file.exists(), "the first Space's ticket still waits");
        assert!(second_file.exists());
        let second_body: Value =
            serde_json::from_str(&std::fs::read_to_string(&second_file).unwrap()).unwrap();
        let second_ticket = second_body["ticket"].as_str().unwrap().to_owned();

        // The first Space's ticket pairs once, against its own Space.
        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", first_space.url()))
            .json(&json!({"ticket": first_ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        assert!(!first_file.exists(), "its own ticket file is removed");
        assert!(second_file.exists(), "the other Space's file is untouched");
        let again = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", first_space.url()))
            .json(&json!({"ticket": first_ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The second Space's ticket pairs its own Space.
        let paired = reqwest::Client::new()
            .post(format!("{}/api/computer/pair", second_space.url()))
            .json(&json!({"ticket": second_ticket}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        assert!(!second_file.exists());

        first_space.shutdown().await;
        second_space.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_serve_without_a_configuration_directory_writes_no_ticket_file() {
        if std::env::var_os("NEHA_TEST_NO_CONFIG_CHILD").is_some() {
            let dir = tempfile::tempdir().unwrap();
            let running = start(Home::open(dir.path()).unwrap(), 0).await.unwrap();
            assert!(
                running.local_ticket.is_none(),
                "without a configuration directory no file ticket is written"
            );
            assert!(
                !running
                    .server_home()
                    .join("state")
                    .join(crate::ticket::TICKET_FILE)
                    .exists(),
                "the Space's state directory is not a ticket fallback"
            );

            // The signed-in CLI's HTTP fallback still works.
            let invite = running.invite.clone().expect("a fresh Space invites");
            let session = join(&running, &invite, "Ada", "correct horse battery").await;
            let token = session["token"].as_str().unwrap();
            let issued = reqwest::Client::new()
                .post(format!("{}/api/computer/ticket", running.url()))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            assert_eq!(issued.status(), reqwest::StatusCode::OK);
            let issued: Value = issued.json().await.unwrap();
            let paired = reqwest::Client::new()
                .post(format!("{}/api/computer/pair", running.url()))
                .json(&json!({"ticket": issued["ticket"]}))
                .send()
                .await
                .unwrap();
            assert_eq!(paired.status(), reqwest::StatusCode::OK);
            let paired: Value = paired.json().await.unwrap();
            assert!(paired["computer"].is_string());
            running.shutdown().await;
            println!("NO_CONFIG_CHILD_OK");

            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("server::tests::a_serve_without_a_configuration_directory_writes_no_ticket_file")
            .arg("--nocapture")
            .env_clear()
            .env("NEHA_TEST_NO_CONFIG_CHILD", "1")
            .output()
            .expect("the child test process starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the child Space failed: {stdout}\n{stderr}"
        );
        assert!(stdout.contains("NO_CONFIG_CHILD_OK"), "{stdout}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_attempts_are_rate_limited_per_address() {
        let (_dir, running, _invite) = running().await;
        let client = reqwest::Client::new();
        for attempt in 0..10 {
            let response = client
                .post(format!("{}/api/computer/pair", running.url()))
                .json(&json!({"ticket": "not-a-ticket"}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "attempt {attempt}"
            );
        }
        let limited = client
            .post(format!("{}/api/computer/pair", running.url()))
            .json(&json!({"ticket": "not-a-ticket"}))
            .send()
            .await
            .unwrap();
        assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let limited: Value = limited.json().await.unwrap();
        assert_eq!(limited["message"], "too many pairing attempts");
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_computers_workspace_can_be_reset() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("held answer")]).await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        std::fs::write(workspace.join("note.txt"), "keep me").unwrap();
        std::fs::create_dir(workspace.join("folder")).unwrap();
        std::fs::write(workspace.join("folder").join("inner.txt"), "also me").unwrap();

        let cursor_before = running.server.store.last_seq().await.unwrap();
        let command_id = Id::new();
        let (committed, result) = try_command(
            &mut socket,
            command_id,
            CommandCall::ComputerReset { computer },
        )
        .await
        .unwrap();
        assert!(committed.is_none(), "a reset writes no event");
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor_before
        );
        assert_eq!(result["computer"], json!(computer));
        assert_eq!(result["kind"], "hosted");
        assert!(
            std::fs::read_dir(&workspace).unwrap().next().is_none(),
            "reset clears every entry of the workspace"
        );
        assert!(workspace.is_dir(), "reset keeps the workspace directory");

        // A repeated command id answers from the record instead of clearing
        // again: work written after the first reset must survive, even while a
        // new run holds the Computer.
        std::fs::write(workspace.join("late.txt"), "after the reset").unwrap();
        let queued = submit(&mut socket, chat, "hold the next run", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "running").await;
        let cursor_before_retry = running.server.store.last_seq().await.unwrap();
        let (again_committed, again) = try_command(
            &mut socket,
            command_id,
            CommandCall::ComputerReset { computer },
        )
        .await
        .unwrap();
        assert!(again_committed.is_none());
        assert_eq!(again, result);
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor_before_retry
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("late.txt")).unwrap(),
            "after the reset"
        );
        gates[0].open();
        wait_run(&mut socket, run).await;

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_refuses_a_hosted_workspace_symlink() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![text("unused")]).await;
        let (_bot, _chat, computer) = tool_bot(&mut socket, base).await;
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        let target = running.server_home().join("not-a-workspace");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "do not delete").unwrap();
        std::fs::remove_dir_all(&workspace).unwrap();
        std::os::unix::fs::symlink(&target, &workspace).unwrap();

        let reset = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ComputerReset { computer },
        )
        .await;
        assert!(reset.is_err(), "a symlinked Workspace is not cleared");
        assert_eq!(
            std::fs::read_to_string(target.join("keep.txt")).unwrap(),
            "do not delete"
        );

        std::fs::remove_file(&workspace).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_reset_command_id_cannot_clear_two_computers() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![text("unused")]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let first = command(
            &mut socket,
            Id::new(),
            add_bot_on("First", Some(BotComputer::Hosted)),
        )
        .await;
        let second = command(
            &mut socket,
            Id::new(),
            add_bot_on("Second", Some(BotComputer::Hosted)),
        )
        .await;
        let first_computer: Id = serde_json::from_value(first["computer"]["id"].clone()).unwrap();
        let second_computer: Id = serde_json::from_value(second["computer"]["id"].clone()).unwrap();
        let second_workspace = running
            .server_home()
            .join("computers")
            .join(second_computer.to_string())
            .join("workspace");
        std::fs::write(second_workspace.join("keep.txt"), "keep").unwrap();

        let command_id = Id::new();
        command(
            &mut socket,
            command_id,
            CommandCall::ComputerReset {
                computer: first_computer,
            },
        )
        .await;
        let conflict = try_command(
            &mut socket,
            command_id,
            CommandCall::ComputerReset {
                computer: second_computer,
            },
        )
        .await;
        assert!(
            matches!(conflict, Err((ErrorCode::IdempotencyConflict, _))),
            "one command id cannot reset two Computers: {conflict:?}"
        );
        assert_eq!(
            std::fs::read_to_string(second_workspace.join("keep.txt")).unwrap(),
            "keep",
            "the second Computer is untouched"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_refuses_a_busy_hosted_computer_and_a_local_one() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (local, _machine) = paired_computer(&mut socket, &running).await;
        let local_bot = command(
            &mut socket,
            Id::new(),
            add_bot_on("Local", Some(BotComputer::Local)),
        )
        .await;
        assert_eq!(local_bot["computer"]["id"], json!(local));
        let local_reset = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ComputerReset { computer: local },
        )
        .await;
        match local_reset {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("only a Hosted Computer can be reset"),
                "{message}"
            ),
            other => panic!("a Local Computer cannot be reset: {other:?}"),
        }

        let (base, provider_task, gates, _recorded) =
            gated_provider(vec![hold("held answer")]).await;
        let (_bot, chat, computer) = tool_bot(&mut socket, base).await;
        let queued = submit(&mut socket, chat, "hold this run", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "running").await;

        let busy = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ComputerReset { computer },
        )
        .await;
        match busy {
            Err((ErrorCode::Conflict, message)) => {
                assert!(message.contains("run in progress"), "{message}")
            }
            other => panic!("a busy Hosted Computer is not reset: {other:?}"),
        }

        gates[0].open();
        wait_run(&mut socket, run).await;
        let reset = command(
            &mut socket,
            Id::new(),
            CommandCall::ComputerReset { computer },
        )
        .await;
        assert_eq!(reset["computer"], json!(computer));

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_defaults_to_hosted_and_takes_choices() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;

        // No Local Computer: the default is a fresh Hosted Computer, and the
        // Local choice names the command that makes one.
        let hosted = command(&mut socket, Id::new(), add_bot("First", None)).await;
        assert_eq!(hosted["computer"]["kind"], "hosted");
        let missing = try_command(
            &mut socket,
            Id::new(),
            add_bot_on("Local", Some(BotComputer::Local)),
        )
        .await;
        match missing {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("there is no Local Computer; run `neha computer pair`"),
                "{message}"
            ),
            other => panic!("a missing Local Computer is named: {other:?}"),
        }

        // Pairing a machine does not change the default: an unnamed Bot still
        // gets its own Hosted Computer. Choosing Local stays explicit.
        let (computer, _machine) = paired_computer(&mut socket, &running).await;
        let default = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        assert_eq!(default["computer"]["kind"], "hosted");
        assert_ne!(default["computer"]["id"], json!(computer));
        let explicit = command(
            &mut socket,
            Id::new(),
            add_bot_on("Explicit", Some(BotComputer::Local)),
        )
        .await;
        assert_eq!(explicit["computer"]["id"], json!(computer));
        let named = command(&mut socket, Id::new(), add_bot("Named", Some(computer))).await;
        assert_eq!(named["computer"]["id"], json!(computer));

        // An explicit Hosted Computer is one Bot's own.
        let own = command(
            &mut socket,
            Id::new(),
            add_bot_on("Own", Some(BotComputer::Hosted)),
        )
        .await;
        assert_eq!(own["computer"]["kind"], "hosted");
        let hosted_id: Id = serde_json::from_value(own["computer"]["id"].clone()).unwrap();
        let rejected = try_command(&mut socket, Id::new(), add_bot("Join", Some(hosted_id))).await;
        match rejected {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("leave the Computer out for Hosted"),
                "{message}"
            ),
            other => panic!("a Bot cannot join a Hosted Computer: {other:?}"),
        }

        let views = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        let views = views.as_array().unwrap();
        assert!(views.iter().any(|view| view["kind"] == "local"));
        assert!(views.iter().any(|view| view["kind"] == "hosted"));
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_record_with_old_computer_kinds_replays() {
        let (dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let owner: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let old_local = Id::new();
        let old_hosted = Id::new();
        running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                "an old record",
                vec![
                    EventDraft::new(
                        EventKind::ComputerCreated,
                        Author::User(owner),
                        json!({
                            "computer": old_local,
                            "owner": owner,
                            "kind": "real",
                            "workspace": "/home/ada/neha",
                        }),
                    ),
                    EventDraft::new(
                        EventKind::ComputerCreated,
                        Author::User(owner),
                        json!({
                            "computer": old_hosted,
                            "owner": owner,
                            "kind": "managed",
                            "workspace": "/space/workspace",
                        }),
                    ),
                ],
            ))
            .await
            .unwrap();
        running.shutdown().await;

        let restarted = start(test_home(dir.path()), 0).await.unwrap();
        let token = sign_in(&restarted, "Ada").await;
        let mut socket = open_socket(&restarted, &token).await;
        let views = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
        let views = views.as_array().unwrap();
        assert!(
            views
                .iter()
                .any(|view| view["id"] == json!(old_local) && view["kind"] == "local")
        );
        assert!(
            views
                .iter()
                .any(|view| view["id"] == json!(old_hosted) && view["kind"] == "hosted")
        );
        let named = command(&mut socket, Id::new(), add_bot("Scout", Some(old_local))).await;
        assert_eq!(named["computer"]["id"], json!(old_local));
        assert_eq!(named["computer"]["kind"], "local");
        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ui_root_that_contains_the_configuration_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // The Space's own home is elsewhere, so this refusal is the
        // configuration directory's, not the home's. The configuration
        // directory, and so the ticket folder, does not exist yet: the check
        // must not depend on it.
        let space = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let home = test_home_in(space.path(), &config);
        std::fs::write(
            dir.path().join("index.html"),
            "<!doctype html><title>Neha</title>",
        )
        .unwrap();

        let result = start_with_ui(home, 0, Some(dir.path().to_path_buf())).await;

        let Err(error) = result else {
            panic!("a UI root holding the configuration directory is refused");
        };
        let ServerError::UiOverlapsConfiguration(path) = error else {
            panic!("the wrong refusal: {error:?}");
        };
        assert_eq!(path, dir.path());
        let message = ServerError::UiOverlapsConfiguration(path).to_string();
        assert!(
            message.contains("configuration and credential"),
            "the refusal names the directory: {message}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ui_root_inside_the_configuration_directory_is_refused() {
        for (case, equals) in [("inside", false), ("equal", true)] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config");
            let ui = if equals {
                config.clone()
            } else {
                config.join("ui")
            };
            std::fs::create_dir_all(&ui).unwrap();
            std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
            let home = test_home_in(&dir.path().join("space"), &config);

            let result = start_with_ui(home, 0, Some(ui.clone())).await;

            let Err(error) = result else {
                panic!("a UI root {case} the configuration directory is refused");
            };
            let ServerError::UiOverlapsConfiguration(path) = error else {
                panic!("the wrong refusal for {case}: {error:?}");
            };
            assert_eq!(path, ui);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credential_named_in_the_environment_is_hidden_from_the_ui() {
        if std::env::var_os("NEHA_TEST_CREDENTIAL_CHILD").is_some() {
            let case = std::env::var("NEHA_TEST_CREDENTIAL_CASE").unwrap();
            let root = std::env::var_os("NEHA_TEST_HOME").expect("a test home");
            let ui = std::env::var_os("NEHA_TEST_UI_ROOT").expect("a test UI root");
            let home = Home::open(root).unwrap();
            let result = start_with_ui(home, 0, Some(std::path::PathBuf::from(ui))).await;
            if case == "absent" || case == "independent" {
                let running = result.expect("an independent UI root starts");
                assert!(
                    running.local_ticket.is_none(),
                    "without HOME or XDG no ticket file is written"
                );
                running.shutdown().await;
            } else {
                let Err(error) = result else {
                    panic!("the credential case {case} must be refused");
                };
                assert!(
                    matches!(&error, ServerError::UiOverlapsConfiguration(_)),
                    "the wrong refusal for {case}: {error:?}"
                );
                assert!(
                    error.to_string().contains("credential"),
                    "the refusal names the credential directory: {error}"
                );
            }
            println!("CREDENTIAL_CHILD_OK");

            return;
        }

        let spawn = |case: &str,
                     ui: &std::path::Path,
                     home: &std::path::Path,
                     credential: Option<&std::path::Path>| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("server::tests::a_credential_named_in_the_environment_is_hidden_from_the_ui")
                .arg("--nocapture")
                .env_clear()
                .env("NEHA_TEST_CREDENTIAL_CHILD", "1")
                .env("NEHA_TEST_CREDENTIAL_CASE", case)
                .env("NEHA_TEST_UI_ROOT", ui)
                .env("NEHA_TEST_HOME", home);
            if let Some(credential) = credential {
                command.env("NEHA_CREDENTIAL", credential);
            }
            let output = command.output().expect("the child test process starts");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the child Space failed for {case}: {stdout}\n{stderr}"
            );
            assert!(stdout.contains("CREDENTIAL_CHILD_OK"), "{stdout}");
        };
        let write_ui = |root: &std::path::Path| {
            let ui = root.join("ui");
            std::fs::create_dir_all(&ui).unwrap();
            std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
            ui
        };

        // The credential file sits directly in the UI root.
        let inside = tempfile::tempdir().unwrap();
        let ui = write_ui(inside.path());
        let home = inside.path().join("home");
        let credential = ui.join("credential.json");
        spawn("inside", &ui, &home, Some(&credential));

        // The UI root sits inside the credential's folder.
        let parent = tempfile::tempdir().unwrap();
        let ui = write_ui(parent.path());
        let home = parent.path().join("home");
        let credential = parent.path().join("credential.json");
        spawn("parent", &ui, &home, Some(&credential));

        // An independent UI root and a credential elsewhere start normally.
        let independent = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let ui = write_ui(independent.path());
        let home = independent.path().join("home");
        let credential = elsewhere.path().join("credential.json");
        spawn("independent", &ui, &home, Some(&credential));

        // Without NEHA_CREDENTIAL nothing changes.
        let absent = tempfile::tempdir().unwrap();
        let ui = write_ui(absent.path());
        let home = absent.path().join("home");
        spawn("absent", &ui, &home, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_computer_home_named_in_the_environment_is_hidden_from_the_ui() {
        if std::env::var_os("NEHA_TEST_COMPUTER_HOME_CHILD").is_some() {
            let case = std::env::var("NEHA_TEST_COMPUTER_HOME_CASE").unwrap();
            let root = std::env::var_os("NEHA_TEST_HOME").expect("a test home");
            let ui = std::env::var_os("NEHA_TEST_UI_ROOT").expect("a test UI root");
            let home = Home::open(root).unwrap();
            let result = start_with_ui(home, 0, Some(std::path::PathBuf::from(ui))).await;
            if case == "absent" || case == "independent" {
                let running = result.expect("an independent UI root starts");
                assert!(
                    running.local_ticket.is_none(),
                    "without HOME or XDG no ticket file is written"
                );
                running.shutdown().await;
            } else {
                let Err(error) = result else {
                    panic!("the computer-home case {case} must be refused");
                };
                assert!(
                    matches!(&error, ServerError::UiOverlapsConfiguration(_)),
                    "the wrong refusal for {case}: {error:?}"
                );
                assert!(
                    error.to_string().contains("machine configuration"),
                    "the refusal names the machine configuration directory: {error}"
                );
            }
            println!("COMPUTER_HOME_CHILD_OK");

            return;
        }

        let spawn = |case: &str,
                     ui: &std::path::Path,
                     home: &std::path::Path,
                     computer_home: Option<&std::path::Path>| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg(
                    "server::tests::a_computer_home_named_in_the_environment_is_hidden_from_the_ui",
                )
                .arg("--nocapture")
                .env_clear()
                .env("NEHA_TEST_COMPUTER_HOME_CHILD", "1")
                .env("NEHA_TEST_COMPUTER_HOME_CASE", case)
                .env("NEHA_TEST_UI_ROOT", ui)
                .env("NEHA_TEST_HOME", home);
            if let Some(computer_home) = computer_home {
                command.env("NEHA_COMPUTER_HOME", computer_home);
            }
            let output = command.output().expect("the child test process starts");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the child Space failed for {case}: {stdout}\n{stderr}"
            );
            assert!(stdout.contains("COMPUTER_HOME_CHILD_OK"), "{stdout}");
        };
        let write_ui = |root: &std::path::Path| {
            let ui = root.join("ui");
            std::fs::create_dir_all(&ui).unwrap();
            std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
            ui
        };

        // The machine configuration directory sits inside the UI root.
        let inside = tempfile::tempdir().unwrap();
        let ui = write_ui(inside.path());
        let home = inside.path().join("home");
        let machine = ui.join("machine");
        spawn("inside", &ui, &home, Some(&machine));

        // The UI root sits inside the machine configuration directory.
        let enclosing = tempfile::tempdir().unwrap();
        let ui = write_ui(enclosing.path());
        let home = enclosing.path().join("home");
        let machine = enclosing.path().to_path_buf();
        spawn("enclosing", &ui, &home, Some(&machine));

        // Independent places start normally.
        let independent = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let ui = write_ui(independent.path());
        let home = independent.path().join("home");
        let machine = elsewhere.path().join("machine");
        spawn("independent", &ui, &home, Some(&machine));

        // Without NEHA_COMPUTER_HOME nothing changes.
        let absent = tempfile::tempdir().unwrap();
        let ui = write_ui(absent.path());
        let home = absent.path().join("home");
        spawn("absent", &ui, &home, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_independent_ui_root_does_not_overlap_the_configuration_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let ui = dir.path().join("ui");
        std::fs::create_dir_all(&ui).unwrap();
        std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
        let home = test_home_in(&dir.path().join("space"), &config);

        let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_without_a_configuration_directory_has_nothing_to_overlap() {
        if std::env::var_os("NEHA_TEST_NO_CONFIG_UI_CHILD").is_some() {
            let dir = tempfile::tempdir().unwrap();
            let ui = dir.path().join("ui");
            std::fs::create_dir_all(&ui).unwrap();
            std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
            let home = Home::open(dir.path().join("space")).unwrap();

            let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
            assert!(
                running.local_ticket.is_none(),
                "without HOME or XDG no ticket file is written"
            );
            running.shutdown().await;
            println!("NO_CONFIG_UI_CHILD_OK");

            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("server::tests::a_start_without_a_configuration_directory_has_nothing_to_overlap")
            .arg("--nocapture")
            .env_clear()
            .env("NEHA_TEST_NO_CONFIG_UI_CHILD", "1")
            .output()
            .expect("the child test process starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the child Space failed: {stdout}\n{stderr}"
        );
        assert!(stdout.contains("NO_CONFIG_UI_CHILD_OK"), "{stdout}");
    }

    // ---- Approvals and lasting rules (PR7) ----

    /// A Bot on a paired local Computer whose daemon is connected.
    struct LocalBot {
        socket: Socket,
        device: Socket,
        computer: Id,
        machine: String,
        bot: Id,
        chat: Id,
        provider_task: tokio::task::JoinHandle<()>,
    }

    async fn local_bot(running: &Running, invite: &str, script: Vec<ScriptedReply>) -> LocalBot {
        let socket = admin_socket(running, invite).await;
        local_bot_with(running, socket, machine_token_n(40), script).await
    }

    async fn local_bot_with(
        running: &Running,
        mut socket: Socket,
        machine: String,
        script: Vec<ScriptedReply>,
    ) -> LocalBot {
        let (base, provider_task) = scripted_provider(script).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (computer, machine) = paired_computer_with_token(&mut socket, running, &machine).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let device = connect_computer(running, &machine, computer).await;
        wait_online(&mut socket, computer).await;

        LocalBot {
            socket,
            device,
            computer,
            machine,
            bot,
            chat,
            provider_task,
        }
    }

    async fn approvals_in(socket: &mut Socket, chat: Option<Id>) -> Vec<Value> {
        match command(socket, Id::new(), CommandCall::ApprovalList { chat }).await {
            Value::Array(list) => list,
            other => panic!("expected approvals, got {other}"),
        }
    }

    /// Waits for the approval of one run; the pump writes it after the reply.
    async fn approval_for_run(socket: &mut Socket, run: Id) -> Value {
        for _ in 0..500 {
            let approvals = approvals_in(socket, None).await;
            if let Some(found) = approvals.into_iter().find(|view| view["run"] == json!(run)) {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        panic!("run {run} never asked for an approval");
    }

    fn command_context(running: &Running) -> crate::commands::CommandContext<'_> {
        let server = &running.server;

        crate::commands::CommandContext {
            store: &server.store,
            state: &server.state,
            home: &server.home,
            secrets: &server.secrets,
            identity: &server.identity,
            links: &server.links,
            pairings: &server.pairings,
            approvals: &server.approvals,
            runs: &server.runs,
            events: &server.events,
        }
    }

    async fn decide_approval(
        socket: &mut Socket,
        approval: Id,
        allow: bool,
        lasting: bool,
    ) -> Value {
        command(
            socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval,
                allow,
                lasting,
            },
        )
        .await
    }

    fn add_rule(
        bot: Id,
        effect: neha::record::RuleEffect,
        tool: &str,
        command_prefix: Option<&str>,
        path_prefix: Option<&str>,
    ) -> CommandCall {
        CommandCall::RuleAdd {
            bot,
            effect,
            tool: tool.to_owned(),
            command_prefix: command_prefix.map(ToOwned::to_owned),
            path_prefix: path_prefix.map(ToOwned::to_owned),
        }
    }

    fn bot_update(
        bot: Id,
        full_access: Option<bool>,
        grants: Option<Vec<String>>,
        grant_add: Option<&str>,
        grant_remove: Option<&str>,
    ) -> CommandCall {
        CommandCall::BotUpdate(neha::surface::BotUpdate {
            bot,
            full_access,
            full_access_consent: (full_access == Some(true))
                .then_some(neha::record::FULL_ACCESS_CONSENT_VERSION),
            grants,
            grant_add: grant_add.map(ToOwned::to_owned),
            grant_remove: grant_remove.map(ToOwned::to_owned),
        })
    }

    /// Returns the one `neha.run.access` event an event names.
    async fn run_access_event(running: &Running, run: Id) -> neha::record::Event {
        running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events
            .into_iter()
            .find(|event| event.kind == EventKind::RunAccess && event.data["run"] == json!(run))
            .unwrap_or_else(|| panic!("run {run} recorded no access"))
    }

    /// Answers the next action a fake Computer receives.
    async fn answer_next_action(device: &mut Socket, output: &str) -> WireAction {
        let (id, _bot, action) = match next_host_frame(device).await {
            HostFrame::Action {
                id, bot, action, ..
            } => (id, bot, action),
            other => panic!("expected an action, got {other:?}"),
        };
        reply_action(device, id, ActionStatus::Succeeded, output).await;

        action
    }

    /// Answers the next action and returns its directory and write access.
    async fn answer_next_action_with_access(
        device: &mut Socket,
        output: &str,
    ) -> (
        WireAction,
        Option<String>,
        neha_computer::channel::AccessFrame,
    ) {
        let (id, action, working_dir, access) = match next_host_frame(device).await {
            HostFrame::Action {
                id,
                action,
                working_dir,
                access,
                ..
            } => (id, action, working_dir, access),
            other => panic!("expected an action, got {other:?}"),
        };
        reply_action(device, id, ActionStatus::Succeeded, output).await;

        (action, working_dir, access)
    }

    /// Returns whether no approval appears for a Chat while a run works.
    ///
    /// Each poll finishes its command, so a poll never cancels a frame the
    /// next command would read as its answer.
    async fn waits_without_approval(socket: &mut Socket, chat: Id) -> bool {
        for _ in 0..50 {
            if !approvals_in(socket, Some(chat)).await.is_empty() {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        true
    }

    /// Returns the `action.started` events of one run.
    async fn started_actions(running: &Running, run: Id) -> Vec<neha::record::Event> {
        running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events
            .into_iter()
            .filter(|event| {
                event.kind == EventKind::ActionStarted && event.data["run"] == json!(run)
            })
            .collect()
    }

    async fn last_message(socket: &mut Socket, chat: Id) -> Value {
        let messages = command(
            socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;

        messages.as_array().unwrap().last().unwrap().clone()
    }

    async fn approvals_of(socket: &mut Socket, bot: Id) -> Vec<Value> {
        let list = command(socket, Id::new(), CommandCall::RuleList { bot: Some(bot) }).await;

        list.as_array().cloned().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_run_asks_before_its_first_action() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![tool("shell", json!({"command": "pwd"})), text("done")],
        )
        .await;
        let queued = submit(&mut local.socket, local.chat, "run pwd", None).await;
        let run = run_of(&queued);

        let shown = wait_status(&mut local.socket, run, "waiting").await;
        assert!(
            shown["approval"].is_string(),
            "RunShow names the approval a waiting run waits on"
        );
        let approvals = approvals_in(&mut local.socket, Some(local.chat)).await;
        assert_eq!(approvals.len(), 1);
        let approval = &approvals[0];
        assert_eq!(approval["approval"], shown["approval"]);
        assert_eq!(approval["run"], json!(run));
        assert_eq!(approval["bot"], json!(local.bot));
        assert_eq!(approval["acting"], shown["acting"]);
        assert_eq!(approval["tool"], "shell");
        assert_eq!(approval["reason"], "first_use_local");
        assert!(approval["decided"].is_null());
        assert!(approval["rule"].is_null());
        assert!(
            approval["cwd"].is_null(),
            "a run that named no directory has none to show: {approval}"
        );
        assert!(
            approval["summary"].as_str().unwrap().contains("pwd"),
            "the card names the command: {}",
            approval["summary"]
        );

        // A waiting action never reaches the Computer before the answer.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                next_host_frame(&mut local.device)
            )
            .await
            .is_err(),
            "an action must not start before the User answers"
        );

        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, false).await;
        assert_eq!(decided["status"], "allowed");
        assert_eq!(decided["run"], json!(run));
        let action = answer_next_action(&mut local.device, "/home/ada/.neha/workspace/scout").await;
        assert_eq!(
            action,
            WireAction::Shell {
                command: "pwd".to_owned()
            }
        );

        let finished = wait_run(&mut local.socket, run).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(finished["approval"], Value::Null);
        assert_eq!(started_actions(&running, run).await.len(), 1);
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "done"
        );
        assert!(approvals_in(&mut local.socket, None).await.is_empty());

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn allowing_the_first_local_use_keeps_later_runs_unasked() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("first done"),
                text("second done"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "out").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        let second = run_of(&submit(&mut local.socket, local.chat, "hello again", None).await);
        let finished = wait_run(&mut local.socket, second).await;
        assert_eq!(finished["status"], "succeeded");
        assert!(
            started_actions(&running, second).await.is_empty(),
            "a text answer runs no action"
        );
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "second done"
        );
        assert!(
            approvals_in(&mut local.socket, None).await.is_empty(),
            "the allowed first use does not ask again"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denial_reaches_a_subscriber_in_record_order() {
        let (_dir, running, invite) = running().await;
        let (socket, token) = admin(&running, &invite).await;
        let mut local = local_bot_with(
            &running,
            socket,
            machine_token_n(40),
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("never reached"),
            ],
        )
        .await;
        let mut observer = open_socket(&running, &token).await;
        let _ = snapshot_events(&mut observer).await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval = approval_for_run(&mut local.socket, run).await;
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;

        let mut kinds = Vec::new();
        for _ in 0..200 {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(2), next_frame(&mut observer))
                    .await
                    .expect("the subscriber receives the denial");
            if let ServerFrame::Event { event } = frame
                && event.data["run"] == json!(run)
            {
                kinds.push(event.kind);
                if event.kind == EventKind::RunFinished {
                    break;
                }
            }
        }
        let decided = kinds
            .iter()
            .position(|kind| *kind == EventKind::ApprovalDecided);
        let said = decided.and_then(|at| {
            kinds
                .iter()
                .enumerate()
                .skip(at + 1)
                .find(|(_, kind)| **kind == EventKind::MessageAdded)
                .map(|(at, _)| at)
        });
        let finished = kinds
            .iter()
            .position(|kind| *kind == EventKind::RunFinished);
        assert!(
            decided < said && said < finished,
            "a subscriber sees decide, explain, finish in order: {kinds:?}"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == EventKind::ApprovalDecided)
                .count(),
            1,
            "the decision is broadcast once: {kinds:?}"
        );
        assert!(
            started_actions(&running, run).await.is_empty(),
            "a denied action never starts"
        );

        observer.close(None).await.unwrap();
        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denying_a_local_action_fails_the_run_and_asks_again() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                tool("shell", json!({"command": "pwd"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        let decided = decide_approval(&mut local.socket, approval, false, false).await;
        assert_eq!(decided["status"], "denied");

        let finished = wait_run(&mut local.socket, first).await;
        assert_eq!(finished["status"], "failed");
        assert!(
            finished["error"].as_str().unwrap().contains("denied"),
            "the run says it was denied: {}",
            finished["error"]
        );
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "You denied the action, so I stopped."
        );
        assert!(
            started_actions(&running, first).await.is_empty(),
            "a denied action never starts"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                next_host_frame(&mut local.device)
            )
            .await
            .is_err(),
            "a denied action never reaches the Computer"
        );

        // The denied first use was not allowed, so the next run asks again.
        let second = run_of(&submit(&mut local.socket, local.chat, "run pwd again", None).await);
        let again = wait_status(&mut local.socket, second, "waiting").await;
        assert_eq!(again["status"], "waiting");
        assert_eq!(
            approval_for_run(&mut local.socket, second).await["reason"],
            "first_use_local"
        );
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, second).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_run_lets_the_next_run_finish_then_resumes_once() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("second done"),
                text("first done"),
            ],
        )
        .await;
        let waiting = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval = approval_for_run(&mut local.socket, waiting).await;
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        assert_eq!(approval["reason"], "first_use_local");

        // The waiting run does not hold the queue: the next run answers.
        let later = run_of(&submit(&mut local.socket, local.chat, "hello", None).await);
        let finished = wait_run(&mut local.socket, later).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "second done"
        );
        assert_eq!(
            wait_status(&mut local.socket, waiting, "waiting").await["status"],
            "waiting",
            "the first run still waits after the second finished"
        );

        // Answering resumes the first run; its action runs exactly once.
        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "out").await;
        let finished = wait_run(&mut local.socket, waiting).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "first done"
        );
        assert_eq!(started_actions(&running, waiting).await.len(), 1);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                next_host_frame(&mut local.device)
            )
            .await
            .is_err(),
            "a resumed action never runs twice"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_acting_user_answers_once() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![tool("shell", json!({"command": "pwd"})), text("done")],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval = approval_for_run(&mut local.socket, run).await;
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();

        // A lasting answer cannot cover a first use.
        let refused = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await;
        assert!(matches!(
            refused,
            Err((ErrorCode::InvalidArgument, message))
                if message == "a lasting rule cannot cover the first use of a Local Computer"
        ));

        // Only the User the Bot acts for answers.
        let server = &running.server;
        let context = crate::commands::CommandContext {
            store: &server.store,
            state: &server.state,
            home: &server.home,
            secrets: &server.secrets,
            identity: &server.identity,
            links: &server.links,
            pairings: &server.pairings,
            approvals: &server.approvals,
            runs: &server.runs,
            events: &server.events,
        };
        let stranger = crate::identity::Account {
            id: Id::new(),
            display_name: "Eve".to_owned(),
            is_admin: false,
        };
        let error = crate::commands::execute(
            &context,
            &stranger,
            Id::new(),
            CommandCall::ApprovalList { chat: None },
        )
        .await
        .unwrap();
        assert_eq!(error.result, json!([]), "a stranger sees no approvals");
        let error = match crate::commands::execute(
            &context,
            &stranger,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: false,
            },
        )
        .await
        {
            Ok(_) => panic!("only the acting User answers"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::Forbidden);
        assert!(error.message.contains("acts for"));

        // An unknown approval is not found.
        let unknown = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: Id::new(),
                allow: true,
                lasting: false,
            },
        )
        .await;
        assert!(matches!(unknown, Err((ErrorCode::NotFound, _))));

        // The acting User answers, once.
        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "out").await;
        assert_eq!(
            wait_run(&mut local.socket, run).await["status"],
            "succeeded"
        );
        let twice = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: false,
                lasting: false,
            },
        )
        .await;
        assert!(matches!(twice, Err((ErrorCode::InvalidArgument, _))));

        // A rule belongs to its Bot's owner...
        let rule = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        let rule_id: Id = serde_json::from_value(rule["rule"].clone()).unwrap();
        let context = command_context(&running);
        let not_owner = crate::commands::execute(
            &context,
            &stranger,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        assert!(matches!(not_owner, Err(error) if error.code == ErrorCode::NotFound));
        // ...and only its maker or that owner removes it.
        let not_maker = crate::commands::execute(
            &context,
            &stranger,
            Id::new(),
            CommandCall::RuleRemove { rule: rule_id },
        )
        .await;
        assert!(matches!(not_maker, Err(error) if error.code == ErrorCode::Forbidden));

        // A prefix only fits the tool kind that takes it.
        let wrong_prefix = try_command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                Some("git"),
                None,
            ),
        )
        .await;
        assert!(matches!(wrong_prefix, Err((ErrorCode::InvalidArgument, _))));
        let wrong_kind = try_command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                None,
                Some("logs"),
            ),
        )
        .await;
        assert!(matches!(wrong_kind, Err((ErrorCode::InvalidArgument, _))));
        // A blank prefix is an error, never a silent tool-wide rule.
        let blank_command = try_command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                Some("   "),
                None,
            ),
        )
        .await;
        assert!(matches!(
            blank_command,
            Err((ErrorCode::InvalidArgument, message))
                if message == "a command prefix needs a command"
        ));
        let blank_path = try_command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                Some("  "),
            ),
        )
        .await;
        assert!(matches!(
            blank_path,
            Err((ErrorCode::InvalidArgument, message))
                if message == "a path prefix needs a path"
        ));

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_clients_cannot_answer_one_approval_twice() {
        let (_dir, running, invite) = running().await;
        let (socket, token) = admin(&running, &invite).await;
        let mut local = local_bot_with(
            &running,
            socket,
            machine_token_n(40),
            vec![tool("shell", json!({"command": "pwd"})), text("done")],
        )
        .await;
        let mut other = open_socket(&running, &token).await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();

        let decide = CommandCall::ApprovalDecide {
            approval,
            allow: true,
            lasting: false,
        };
        let (first, second) = tokio::join!(
            try_command(&mut local.socket, Id::new(), decide.clone()),
            try_command(&mut other, Id::new(), decide),
        );
        let results = [first, second];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one answer is accepted: {results:?}"
        );
        assert!(
            results
                .iter()
                .any(|result| matches!(result, Err((ErrorCode::InvalidArgument, _)))),
            "the other answer is refused: {results:?}"
        );

        let decided = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events
            .into_iter()
            .filter(|event| {
                event.kind == EventKind::ApprovalDecided
                    && event.data["approval"] == json!(approval)
            })
            .count();
        assert_eq!(decided, 1, "the record holds one decision");

        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, run).await["status"],
            "succeeded"
        );
        assert_eq!(started_actions(&running, run).await.len(), 1);

        other.close(None).await.unwrap();
        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_specific_allow_rule_beats_a_wide_ask_rule() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf 1"})),
                text("first"),
                tool("shell", json!({"command": "printf 2"})),
                text("second"),
                tool("shell", json!({"command": "printf 3"})),
                text("third"),
                tool("shell", json!({"command": "rm -rf build"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A wide ask rule asks for every shell command...
        let ask = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                None,
                None,
            ),
        )
        .await;
        let ask_id: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        // ...until the User always allows one command.
        let second = run_of(&submit(&mut local.socket, local.chat, "printf again", None).await);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        eprintln!(
            "DEBUG second run: {}",
            show_run(&mut local.socket, second).await
        );
        eprintln!(
            "DEBUG approvals: {:?}",
            approvals_in(&mut local.socket, None).await
        );
        eprintln!(
            "DEBUG messages: {:?}",
            command(
                &mut local.socket,
                Id::new(),
                CommandCall::ChatRead {
                    chat: local.chat,
                    from: None
                }
            )
            .await
        );
        eprintln!(
            "DEBUG rules: {:?}",
            approvals_of(&mut local.socket, local.bot).await
        );
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["rule"], json!(ask_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert_eq!(decided["status"], "allowed");
        assert!(decided["rule"].is_string());
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // The derived allow rule is narrower, so it wins over the wide ask:
        // the same command runs without asking...
        let third = run_of(&submit(&mut local.socket, local.chat, "printf once more", None).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "a matching allow rule does not ask again"
        );
        answer_next_action(&mut local.device, "o").await;
        let finished = wait_run(&mut local.socket, third).await;
        assert_eq!(finished["status"], "succeeded");

        // ...while other shell commands still ask.
        let fourth = run_of(&submit(&mut local.socket, local.chat, "rm build", None).await);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        eprintln!(
            "DEBUG fourth: {}",
            show_run(&mut local.socket, fourth).await
        );
        eprintln!(
            "DEBUG approvals4: {:?}",
            approvals_in(&mut local.socket, None).await
        );
        eprintln!(
            "DEBUG messages4: {:?}",
            command(
                &mut local.socket,
                Id::new(),
                CommandCall::ChatRead {
                    chat: local.chat,
                    from: None
                }
            )
            .await
        );
        let approval = approval_for_run(&mut local.socket, fourth).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["rule"], json!(ask_id), "the wide ask rule stays");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "failed"
        );

        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert_eq!(rules.len(), 2, "the wide ask and the derived allow rule");
        assert!(
            rules.iter().any(|rule| rule["rule"] == json!(ask_id)),
            "a wider ask rule stays for the commands it still covers"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_always_allowed_ask_rule_is_removed_with_its_decision() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf 1"})),
                text("first"),
                tool("shell", json!({"command": "printf 2"})),
                text("second"),
                tool("shell", json!({"command": "printf 3"})),
                text("third"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        let ask = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                Some("printf"),
                None,
            ),
        )
        .await;
        let ask_id: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        // Always allow answers the ask with a rule as specific as it is, so
        // the decision and the rule change land in one command.
        let second = run_of(&submit(&mut local.socket, local.chat, "printf again", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let command_id = Id::new();
        let (_, decided) = try_command(
            &mut local.socket,
            command_id,
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(decided["status"], "allowed");
        let rule: Id = serde_json::from_value(decided["rule"].clone()).unwrap();
        assert_eq!(decided["overridden"], json!(ask_id));

        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let decided_in = events
            .iter()
            .find(|event| {
                event.kind == EventKind::ApprovalDecided
                    && event.data["approval"] == json!(approval_id)
            })
            .unwrap();
        let added_in = events
            .iter()
            .find(|event| event.kind == EventKind::RuleAdded && event.data["rule"] == json!(rule))
            .unwrap();
        let removed_in = events
            .iter()
            .find(|event| {
                event.kind == EventKind::RuleRemoved && event.data["rule"] == json!(ask_id)
            })
            .unwrap();
        assert_eq!(
            (decided_in.command, added_in.command, removed_in.command),
            (Some(command_id), Some(command_id), Some(command_id)),
            "the decision, the rule, and the removed ask share one command"
        );
        assert_eq!(
            decided_in.data["rule"],
            json!(rule),
            "a lasting decision names the rule it derives"
        );

        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // The ask rule is gone and its allow replaces it.
        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["rule"], json!(rule));
        assert_eq!(rules[0]["effect"], "allow");

        let third = run_of(&submit(&mut local.socket, local.chat, "printf once more", None).await);
        answer_next_action(&mut local.device, "o").await;
        let finished = wait_run(&mut local.socket, third).await;
        assert_eq!(finished["status"], "succeeded");
        assert!(approvals_in(&mut local.socket, None).await.is_empty());

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_only_asks_for_its_own_tool_and_path() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("write_file", json!({"path": "/tmp/a", "content": "x"})),
                text("first"),
                tool("write_file", json!({"path": "/tmp/b", "content": "x"})),
                text("second"),
                tool("write_file", json!({"path": "/etc/passwd", "content": "x"})),
            ],
        )
        .await;
        command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotUpdate(neha::surface::BotUpdate {
                bot: local.bot,
                full_access: None,
                full_access_consent: None,
                grants: Some(vec!["/tmp".to_owned(), "/etc".to_owned()]),
                grant_add: None,
                grant_remove: None,
            }),
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "write", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A path rule only asks under its prefix.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "write_file",
                None,
                Some("/etc"),
            ),
        )
        .await;

        let second = run_of(&submit(&mut local.socket, local.chat, "write tmp", None).await);
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );
        assert!(approvals_in(&mut local.socket, None).await.is_empty());

        let third = run_of(&submit(&mut local.socket, local.chat, "write etc", None).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["tool"], "write_file");

        // A rule on another tool does not ask for this one.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(wait_run(&mut local.socket, third).await["status"], "failed");

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ask_rule_wins_a_specificity_tie_with_an_allow_rule() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf 1"})),
                text("first"),
                tool("shell", json!({"command": "printf 2"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // Same tool and same prefix: the cautious reading wins.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Allow,
                "shell",
                Some("printf"),
                None,
            ),
        )
        .await;
        let ask = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                Some("printf"),
                None,
            ),
        )
        .await;
        let ask_id: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        let second = run_of(&submit(&mut local.socket, local.chat, "printf again", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["rule"], json!(ask_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restored_run_without_a_directory_marker_keeps_its_directory_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = test_home(dir.path());
        let first = start(home.clone(), 0).await.unwrap();
        let invite = first.invite.clone().expect("a fresh Space invites");
        let mut socket = admin_socket(&first, &invite).await;
        command(
            &mut socket,
            Id::new(),
            provider_add("http://127.0.0.1:1/v1".to_owned()),
        )
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let owner: Id = serde_json::from_value(created["bot"]["owner"].clone()).unwrap();
        socket.close(None).await.unwrap();

        // Two records written before the marker existed: a queued run and a
        // started run that named a working directory, each with no writer
        // flag and no access snapshot.
        let queued_message = Id::new();
        let queued = Id::new();
        let queued_command = Id::new();
        let started_message = Id::new();
        let started = Id::new();
        let started_command = Id::new();
        let legacy = |message: Id, run: Id, command: Id, started: bool| {
            let mut events = vec![
                EventDraft::new(
                    EventKind::MessageAdded,
                    Author::User(owner),
                    json!({
                        "message": message,
                        "chat": chat,
                        "text": "work in the logs folder",
                        "run": run,
                    }),
                ),
                EventDraft::new(
                    EventKind::RunQueued,
                    Author::Bot(bot),
                    json!({
                        "run": run,
                        "chat": chat,
                        "bot": bot,
                        "message": message,
                        "command": command,
                    }),
                )
                .acting(owner),
                EventDraft::new(
                    EventKind::RunContext,
                    Author::Bot(bot),
                    json!({"run": run, "working_dir": "/var/tmp/legacy-logs "}),
                )
                .acting(owner),
            ];
            if started {
                events.push(
                    EventDraft::new(
                        EventKind::RunStarted,
                        Author::Bot(bot),
                        json!({
                            "run": run,
                            "chat": chat,
                            "bot": bot,
                            "message": message,
                            "command": command,
                        }),
                    )
                    .acting(owner),
                );
            }
            events
        };
        first
            .server
            .store
            .append(AppendRequest {
                command: queued_command,
                fingerprint: format!("legacy:{queued_command}"),
                events: legacy(queued_message, queued, queued_command, false),
                result: None,
                confirmation: None,
            })
            .await
            .unwrap();
        first
            .server
            .store
            .append(AppendRequest {
                command: started_command,
                fingerprint: format!("legacy:{started_command}"),
                events: legacy(started_message, started, started_command, true),
                result: None,
                confirmation: None,
            })
            .await
            .unwrap();
        first.shutdown().await;

        // The restore refuses to widen the old directory into a write root:
        // each run fails and the Bot asks the User to send the message again.
        let restarted = start(home, 0).await.unwrap();
        let token = sign_in(&restarted, "Ada").await;
        let mut socket = open_socket(&restarted, &token).await;
        for run in [queued, started] {
            let shown = wait_run(&mut socket, run).await;
            assert_eq!(shown["status"], "failed", "{shown}");
            let error = shown["error"].as_str().unwrap_or_default();
            assert!(
                error.contains("send the message again"),
                "the failure asks for a new message: {shown}"
            );
        }
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert!(
            messages
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("send the message again"))),
            "the Chat shows the request to send the message again: {messages}"
        );
        // No run started with the old directory as a write root.
        let bots = command(&mut socket, Id::new(), CommandCall::BotList).await;
        assert_eq!(bots[0]["grants"], json!([]));

        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_keeps_a_waiting_run_and_resumes_it_after_an_allow() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_home(dir.path());
        let first = start(root.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let mut local = local_bot(
            &first,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("after restart"),
            ],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();
        local.device.close(None).await.unwrap();
        local.socket.close(None).await.unwrap();
        first.shutdown().await;
        assert!(
            !record(root.root()).await.iter().any(
                |event| event.kind == EventKind::RunFinished && event.data["run"] == json!(run)
            ),
            "a waiting run does not end when the Space stops"
        );

        let second = start(root.clone(), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        assert_eq!(
            wait_status(&mut socket, run, "waiting").await["status"],
            "waiting",
            "an undecided approval keeps its run waiting after a restart"
        );
        let approvals = approvals_in(&mut socket, Some(local.chat)).await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["approval"], json!(approval));

        let mut device = connect_computer(&second, &local.machine, local.computer).await;
        wait_online(&mut socket, local.computer).await;
        let decided = decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(decided["status"], "allowed");
        answer_next_action(&mut device, "o").await;
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(
            last_message(&mut socket, local.chat).await["text"],
            "after restart"
        );
        assert_eq!(
            started_actions(&second, run).await.len(),
            1,
            "the action resumed once, not again"
        );

        device.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        second.shutdown().await;
        local.provider_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_keeps_an_allowed_first_use_and_a_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_home(dir.path());
        let first = start(root.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let mut local = local_bot(
            &first,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("first"),
                tool("shell", json!({"command": "pwd"})),
                text("second"),
            ],
        )
        .await;
        let first_run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first_run).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first_run).await["status"],
            "succeeded"
        );
        let rule = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        let rule_id: Id = serde_json::from_value(rule["rule"].clone()).unwrap();

        local.device.close(None).await.unwrap();
        local.socket.close(None).await.unwrap();
        first.shutdown().await;

        let second = start(root.clone(), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        let rules = approvals_of(&mut socket, local.bot).await;
        assert_eq!(rules.len(), 1, "a lasting rule replays");
        assert_eq!(rules[0]["rule"], json!(rule_id));

        let mut device = connect_computer(&second, &local.machine, local.computer).await;
        wait_online(&mut socket, local.computer).await;
        let second_run = run_of(&submit(&mut socket, local.chat, "run pwd again", None).await);
        assert!(
            waits_without_approval(&mut socket, local.chat).await,
            "the allowed first use replays, so the same Bot and User do not ask again"
        );
        answer_next_action(&mut device, "o").await;
        let finished = wait_run(&mut socket, second_run).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(
            last_message(&mut socket, local.chat).await["text"],
            "second"
        );

        device.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        second.shutdown().await;
        local.provider_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allowed_run_waits_for_its_offline_computer() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![tool("shell", json!({"command": "pwd"})), text("done")],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();

        local.device.close(None).await.unwrap();
        for _ in 0..500 {
            let listed = command(&mut local.socket, Id::new(), CommandCall::ComputerList).await;
            if listed.as_array().is_some_and(|views| {
                views.iter().any(|view| {
                    view["id"] == json!(local.computer) && view["online"] == json!(false)
                })
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        decide_approval(&mut local.socket, approval, true, false).await;
        let shown = wait_status(&mut local.socket, run, "running").await;
        assert_eq!(shown["status"], "running");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let shown = show_run(&mut local.socket, run).await;
        assert_eq!(
            shown["status"], "running",
            "an allowed run holds while its Computer is away"
        );
        assert!(shown["error"].is_null());

        // The Computer comes back and the run continues where it waited.
        let mut device = connect_computer(&running, &local.machine, local.computer).await;
        wait_online(&mut local.socket, local.computer).await;
        answer_next_action(&mut device, "o").await;
        let finished = wait_run(&mut local.socket, run).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(started_actions(&running, run).await.len(), 1);
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "done"
        );

        device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_fails_a_denied_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_home(dir.path());
        let first = start(root.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let mut local = local_bot(
            &first,
            &invite,
            vec![tool("shell", json!({"command": "pwd"}))],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();
        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        first.shutdown().await;

        let second = start(root.clone(), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        assert_eq!(
            wait_status(&mut socket, run, "waiting").await["status"],
            "waiting"
        );
        let decided = decide_approval(&mut socket, approval, false, false).await;
        assert_eq!(decided["status"], "denied");
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "failed");
        assert!(
            finished["error"].as_str().unwrap().contains("denied"),
            "the denied run fails after the restart: {}",
            finished["error"]
        );
        assert_eq!(
            last_message(&mut socket, local.chat).await["text"],
            "You denied the action, so I stopped."
        );

        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allowed_run_on_an_offline_computer_does_not_hold_another() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "pwd"})),
            text("done on b"),
            text("done on b again"),
            text("done on a"),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (a, a_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(41)).await;
        let (b, b_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(42)).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(a))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let mut a_device = connect_computer(&running, &a_machine, a).await;
        let _b_device = connect_computer(&running, &b_machine, b).await;
        wait_online(&mut socket, a).await;
        wait_online(&mut socket, b).await;

        let waiting = run_of(&submit(&mut socket, chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut socket, waiting).await["approval"].clone(),
        )
        .unwrap();

        // A leaves; the Bot moves to B. The waiting run keeps the Computer it
        // began on.
        a_device.close(None).await.unwrap();
        for _ in 0..500 {
            let listed = command(&mut socket, Id::new(), CommandCall::ComputerList).await;
            if listed.as_array().is_some_and(|views| {
                views
                    .iter()
                    .any(|view| view["id"] == json!(a) && view["online"] == json!(false))
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove { bot, computer: b },
        )
        .await;

        // B's queued work is not held by the allowed run whose Computer is
        // away.
        let later = run_of(&submit(&mut socket, chat, "hello", None).await);
        assert_eq!(wait_run(&mut socket, later).await["status"], "succeeded");

        // Allowing the old run does not hold B either.
        let decided = decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(decided["status"], "allowed");
        let after = run_of(&submit(&mut socket, chat, "hello again", None).await);
        assert_eq!(wait_run(&mut socket, after).await["status"], "succeeded");
        let shown = show_run(&mut socket, waiting).await;
        assert_eq!(shown["status"], "running");
        assert!(
            shown["error"].is_null(),
            "the allowed run just waits: {shown}"
        );

        // A reconnects: the run resumes on the Computer it began on.
        let mut a_device = connect_computer(&running, &a_machine, a).await;
        wait_online(&mut socket, a).await;
        let action = answer_next_action(&mut a_device, "out").await;
        assert_eq!(
            action,
            WireAction::Shell {
                command: "pwd".to_owned()
            }
        );
        let finished = wait_run(&mut socket, waiting).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(started_actions(&running, waiting).await.len(), 1);
        assert_eq!(last_message(&mut socket, chat).await["text"], "done on a");

        a_device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_always_allow_covers_the_ask_rule_it_answered() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "git status"})),
                text("first"),
                tool("shell", json!({"command": "git status"})),
                text("second"),
                tool("shell", json!({"command": "git status"})),
                text("third"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // The ask is narrower than the derived allow, so answering it always
        // removes it: `git` covers `git status`.
        let ask = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                Some("git status"),
                None,
            ),
        )
        .await;
        let ask_id: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        let second = run_of(&submit(&mut local.socket, local.chat, "git status", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["rule"], json!(ask_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert_eq!(decided["status"], "allowed");
        assert_eq!(decided["overridden"], json!(ask_id));
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert_eq!(rules.len(), 1, "the answered ask rule is gone");
        assert_eq!(rules[0]["effect"], "allow");
        assert_eq!(rules[0]["command_prefix"], "git");

        let third = run_of(&submit(&mut local.socket, local.chat, "git status", None).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "the derived `git` allow covers the same command"
        );
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_always_allow_of_a_bare_relative_file_covers_relative_paths() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("write_file", json!({"path": "notes.txt", "content": "one"})),
                text("first"),
                tool("write_file", json!({"path": "notes.txt", "content": "two"})),
                text("second"),
                tool(
                    "write_file",
                    json!({"path": "other.txt", "content": "three"}),
                ),
                text("third"),
                tool(
                    "write_file",
                    json!({"path": "/tmp/abs.txt", "content": "four"}),
                ),
            ],
        )
        .await;
        command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotUpdate(neha::surface::BotUpdate {
                bot: local.bot,
                full_access: None,
                full_access_consent: None,
                grants: Some(vec!["/tmp".to_owned()]),
                grant_add: None,
                grant_remove: None,
            }),
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "write one", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A tool-wide ask keeps asking, and a bare relative file always-allows
        // the workspace root: `.`.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "write_file",
                None,
                None,
            ),
        )
        .await;
        let second = run_of(&submit(&mut local.socket, local.chat, "write two", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert!(decided["rule"].is_string());
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert_eq!(rules.len(), 2, "the wider ask rule stays");
        let derived = rules.iter().find(|rule| rule["effect"] == "allow").unwrap();
        assert_eq!(derived["path_prefix"], ".");

        // A relative path does not ask...
        let third = run_of(&submit(&mut local.socket, local.chat, "write three", None).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "`.` covers relative paths"
        );
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );

        // ...an absolute one still does.
        let fourth = run_of(&submit(&mut local.socket, local.chat, "write four", None).await);
        let approval = approval_for_run(&mut local.socket, fourth).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["tool"], "write_file");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "failed"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    // ---- Event visibility, machine-scoped first use, and the approval fence ----

    /// Subscribes and returns the snapshot's cursor and events.
    async fn snapshot_events(socket: &mut Socket) -> (Seq, Vec<neha::record::Event>) {
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            match next_frame(socket).await {
                ServerFrame::Snapshot { cursor, events } => return (cursor, events),
                ServerFrame::Heartbeat { .. } => {}
                other => panic!("expected a snapshot, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_approval_cannot_last() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "true"})),
                text("first"),
                tool("shell", json!({"command": "printf hi"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                None,
                None,
            ),
        )
        .await;
        let second = run_of(&submit(&mut local.socket, local.chat, "run it", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, second).await["approval"].clone(),
        )
        .unwrap();
        let refused = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval,
                allow: false,
                lasting: true,
            },
        )
        .await;
        assert!(matches!(
            refused,
            Err((ErrorCode::InvalidArgument, message))
                if message == "a lasting answer can only be an allow"
        ));
        decide_approval(&mut local.socket, approval, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscription_shows_only_the_accounts_own_events() {
        let (_dir, running, invite) = running().await;
        let (mut a_socket, _a_token) = admin(&running, &invite).await;
        let (base, provider_task) =
            scripted_provider(vec![text("a done"), text("b done"), text("a again")]).await;
        command(&mut a_socket, Id::new(), provider_add(base)).await;
        let a_created = command(&mut a_socket, Id::new(), add_bot("Scout", None)).await;
        let a_bot: Id = serde_json::from_value(a_created["bot"]["id"].clone()).unwrap();
        let chat_a: Id = serde_json::from_value(a_created["chat"]["id"].clone()).unwrap();
        assert_eq!(
            send(&mut a_socket, chat_a, "a hello").await["status"],
            "succeeded"
        );

        // A second account signs in through the test-only account path.
        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let b_token = sign_in(&running, "Grace").await;
        let mut b_socket = open_socket(&running, &b_token).await;
        let b_created = command(&mut b_socket, Id::new(), add_bot("Reader", None)).await;
        let _b_bot: Id = serde_json::from_value(b_created["bot"]["id"].clone()).unwrap();
        let chat_b: Id = serde_json::from_value(b_created["chat"]["id"].clone()).unwrap();
        assert_eq!(
            send(&mut b_socket, chat_b, "b hello").await["status"],
            "succeeded"
        );

        let (_a_cursor, a_events) = snapshot_events(&mut a_socket).await;
        let (b_cursor, b_events) = snapshot_events(&mut b_socket).await;

        // Every account sees the Space-wide events.
        for events in [&a_events, &b_events] {
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == EventKind::UserJoined),
                "the joined User is Space-wide"
            );
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == EventKind::ProviderAdded),
                "the Provider is Space-wide"
            );
        }
        // A sees its own Chat and not B's; B mirrors.
        assert!(
            a_events
                .iter()
                .any(|event| event.kind == EventKind::MessageAdded
                    && event.data["chat"] == json!(chat_a))
        );
        assert!(
            a_events
                .iter()
                .all(|event| event.data["chat"] != json!(chat_b)),
            "a Client must not read another account's Chat"
        );
        assert!(
            b_events
                .iter()
                .any(|event| event.kind == EventKind::MessageAdded
                    && event.data["chat"] == json!(chat_b))
        );
        assert!(
            b_events
                .iter()
                .all(|event| event.data["chat"] != json!(chat_a)),
            "another account's Chat leaked into the snapshot"
        );
        // The run's own records (reply and steps) stay with their account.
        assert!(
            b_events
                .iter()
                .all(|event| event.data.get("bot") != Some(&json!(a_bot))),
            "a Bot leaked into another account's snapshot"
        );
        snapshot_cursor_covers(&running, b_cursor).await;

        // A live event is filtered the same way as a snapshot.
        let _ = submit(&mut a_socket, chat_a, "a again", None).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(400),
                next_frame(&mut b_socket)
            )
            .await
            .is_err(),
            "a live event of another account must not arrive"
        );

        // The first account keeps receiving its own live events.
        loop {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(5), next_frame(&mut a_socket))
                    .await
                    .expect("the first account keeps its stream");
            if let ServerFrame::Event { event } = frame
                && event.kind == EventKind::MessageAdded
                && event.data["chat"] == json!(chat_a)
                && event.data["text"] == "a again"
            {
                break;
            }
        }

        b_socket.close(None).await.unwrap();
        a_socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    /// Asserts a filtered snapshot's cursor covers the whole record.
    async fn snapshot_cursor_covers(running: &Running, cursor: Seq) {
        let last = running.server.store.last_seq().await.unwrap();
        assert!(
            cursor >= last,
            "a filtered cursor must still pass every event: {cursor} < {last}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_use_counts_per_local_computer() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("shell", json!({"command": "pwd"})),
            text("on a"),
            tool("shell", json!({"command": "pwd"})),
            text("on b"),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let (a, a_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(43)).await;
        let (b, b_machine) =
            paired_computer_with_token(&mut socket, &running, &machine_token_n(44)).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(a))).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let mut a_device = connect_computer(&running, &a_machine, a).await;
        let mut b_device = connect_computer(&running, &b_machine, b).await;
        wait_online(&mut socket, a).await;
        wait_online(&mut socket, b).await;

        let first = run_of(&submit(&mut socket, chat, "run pwd", None).await);
        let first_approval = approval_for_run(&mut socket, first).await;
        assert_eq!(first_approval["reason"], "first_use_local");
        assert_eq!(first_approval["computer"], json!(a));
        let approval: Id = serde_json::from_value(first_approval["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        answer_next_action(&mut a_device, "o").await;
        assert_eq!(wait_run(&mut socket, first).await["status"], "succeeded");

        // The Bot moves to another machine: its first use there asks again.
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMove { bot, computer: b },
        )
        .await;
        let second = run_of(&submit(&mut socket, chat, "run pwd again", None).await);
        let second_approval = approval_for_run(&mut socket, second).await;
        assert_eq!(
            second_approval["reason"], "first_use_local",
            "a move to another Local Computer starts a new first use"
        );
        assert_eq!(second_approval["computer"], json!(b));
        let approval: Id = serde_json::from_value(second_approval["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        answer_next_action(&mut b_device, "o").await;
        assert_eq!(wait_run(&mut socket, second).await["status"], "succeeded");

        a_device.close(None).await.unwrap();
        b_device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_approval_card_escapes_control_characters_and_keeps_long_commands() {
        let (_dir, running, invite) = running().await;
        let long: String = "x".repeat(5000);
        let evil = format!("printf \u{1b}[31mred\u{1b}[0m\nsecond line {long}");
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf hi"})),
                text("first"),
                tool("shell", json!({"command": evil})),
                tool("read_file", json!({"path": "logs/a.txt"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                None,
                None,
            ),
        )
        .await;
        let second = run_of(&submit(&mut local.socket, local.chat, "run it", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        let summary = approval["summary"].as_str().unwrap();
        assert!(!summary.contains('\u{1b}'), "a card escapes ESC: {summary}");
        assert!(
            !summary.contains('\n'),
            "a card escapes a newline: {summary}"
        );
        assert!(
            summary.contains("\\u{001b}") && summary.contains("\\n"),
            "the escapes stay readable: {summary}"
        );
        assert!(
            summary.contains("bytes omitted"),
            "a long summary says what it left out: {summary}"
        );
        assert_eq!(approval["undoable"], "no");
        assert_eq!(
            approval["lasting_scope"],
            "shell commands starting with \"printf\""
        );
        assert_eq!(
            approval["arguments"]["command"], evil,
            "the card's summary never replaces the recorded arguments"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                Some("logs"),
            ),
        )
        .await;
        let third = run_of(&submit(&mut local.socket, local.chat, "read it", None).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["lasting_scope"], "files under \"logs\"");
        assert_eq!(approval["undoable"], "unknown");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(wait_run(&mut local.socket, third).await["status"], "failed");

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ambiguous_command_or_path_is_never_covered_by_a_rule() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "true"})),
                text("one"),
                tool("shell", json!({"command": "echo hi; rm x"})),
                tool("read_file", json!({"path": " logs/../secret"})),
                tool("shell", json!({"command": "echo hi; rm x"})),
                text("two"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "shell",
                Some("rm"),
                None,
            ),
        )
        .await;
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                Some("."),
            ),
        )
        .await;

        // An ask rule for `rm` cannot be bypassed with a second command.
        let second = run_of(&submit(&mut local.socket, local.chat, "run it", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["tool"], "shell");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        // A path with `..` is never judged by a relative prefix rule.
        let third = run_of(&submit(&mut local.socket, local.chat, "read it", None).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["tool"], "read_file");
        assert!(
            approval["summary"]
                .as_str()
                .unwrap()
                .contains("logs/../secret"),
            "the card shows the executed path, without the leading blank"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let lasting = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await;
        assert!(
            matches!(lasting, Err((ErrorCode::InvalidArgument, _))),
            "a path that leaves the Workspace cannot make a lasting rule"
        );
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(wait_run(&mut local.socket, third).await["status"], "failed");

        // Without a rule for the tool, a combined command keeps its default.
        let created = command(
            &mut local.socket,
            Id::new(),
            add_bot("Second", Some(local.computer)),
        )
        .await;
        let second_bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let second_chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let fourth = run_of(&submit(&mut local.socket, second_chat, "run it", None).await);
        let approval = approval_for_run(&mut local.socket, fourth).await;
        assert_eq!(
            approval["reason"], "first_use_local",
            "the only ask is the first use, not a rule"
        );
        assert_eq!(approval["bot"], json!(second_bot));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, true, false).await;
        let action = answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            action,
            WireAction::Shell {
                command: "echo hi; rm x".to_owned()
            }
        );
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    /// Answers the channel frame a File delivery makes: reads the machine.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_rule_normalizes_relative_spellings() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("read_file", json!({"path": "./logs/a.txt"})),
                text("one"),
                tool("read_file", json!({"path": "logs//b.txt"})),
                tool("read_file", json!({"path": "logs/./c.txt"})),
                tool("read_file", json!({"path": "  ./logs/d.txt  "})),
                tool("read_file", json!({"path": "./a.txt"})),
                text("two"),
                tool("read_file", json!({"path": "/a.txt"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "read", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        let ask = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                Some("logs"),
            ),
        )
        .await;
        let ask_id: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        // `logs//x`, `logs/./x`, and outer whitespace all name `logs`.
        for text in ["read double", "read dot", "read padded"] {
            let run = run_of(&submit(&mut local.socket, local.chat, text, None).await);
            let approval = approval_for_run(&mut local.socket, run).await;
            assert_eq!(approval["reason"], "rule", "{text}");
            assert_eq!(approval["rule"], json!(ask_id), "{text}");
            assert_eq!(approval["ambiguous"], false, "{text}");
            let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
            decide_approval(&mut local.socket, approval_id, false, false).await;
            assert_eq!(wait_run(&mut local.socket, run).await["status"], "failed");
        }

        // `.` is the Workspace root: it covers a relative spelling...
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Allow,
                "read_file",
                None,
                Some("."),
            ),
        )
        .await;
        let relative = run_of(&submit(&mut local.socket, local.chat, "read root", None).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "the Workspace root rule covers ./a.txt"
        );
        let action = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            answer_next_action(&mut local.device, "o"),
        )
        .await
        .expect("the root rule lets the action run");
        assert_eq!(
            action,
            WireAction::ReadFile {
                path: "./a.txt".to_owned()
            }
        );
        assert_eq!(
            wait_run(&mut local.socket, relative).await["status"],
            "succeeded"
        );

        // ...and still asks when the path is absolute.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                Some("/"),
            ),
        )
        .await;
        let absolute = run_of(&submit(&mut local.socket, local.chat, "read abs", None).await);
        let shown = wait_status(&mut local.socket, absolute, "waiting").await;
        assert_eq!(
            shown["status"], "waiting",
            "an absolute path is not the root"
        );
        let approval = approval_for_run(&mut local.socket, absolute).await;
        assert_eq!(approval["reason"], "rule");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(
            wait_run(&mut local.socket, absolute).await["status"],
            "failed"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ambiguous_approval_never_deletes_a_rule() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "true"})),
                text("one"),
                tool("shell", json!({"command": "echo hi; rm x"})),
                text("two"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // Only an Allow rule exists, and a combined command is ambiguous: the
        // card asks, but no rule actually asked.
        let allow = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Allow,
                "shell",
                Some("rm"),
                None,
            ),
        )
        .await;
        let allow_id: Id = serde_json::from_value(allow["rule"].clone()).unwrap();
        let second = run_of(&submit(&mut local.socket, local.chat, "run it", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["ambiguous"], true);
        assert_eq!(approval["rule"], Value::Null, "no rule actually asked");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert_eq!(decided["status"], "allowed");
        assert!(decided["rule"].is_string(), "Always allow derives an allow");
        assert!(decided["overridden"].is_null(), "no trigger Ask to remove");
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(started_actions(&running, second).await.len(), 1);
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert!(
            rules.iter().any(|rule| rule["rule"] == json!(allow_id)),
            "the user's Allow rule survives an ambiguous answer"
        );
        assert_eq!(rules.len(), 2, "the original Allow and the derived allow");

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_directory_names_that_differ_by_a_space_stay_apart() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("read_file", json!({"path": "logs /report.txt"})),
                text("one"),
                tool("read_file", json!({"path": "logs /other.txt"})),
                text("two"),
                tool("read_file", json!({"path": "logs/other.txt"})),
                tool("read_file", json!({"path": "logs/report2.txt"})),
                text("three"),
                tool("read_file", json!({"path": "logs /more.txt"})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        let wide = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        let wide_id: Id = serde_json::from_value(wide["rule"].clone()).unwrap();

        // Always allowing the directory whose name ends in a space keeps that
        // exact space in the derived rule.
        let second = run_of(&submit(&mut local.socket, local.chat, "logs space", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["rule"], json!(wide_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        let spaced_rule: Id = serde_json::from_value(decided["rule"].clone()).unwrap();
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );
        let rules = approvals_of(&mut local.socket, local.bot).await;
        let spaced = rules
            .iter()
            .find(|rule| rule["rule"] == json!(spaced_rule))
            .expect("the derived rule");
        assert_eq!(spaced["path_prefix"], "logs ");
        assert_eq!(
            approval["lasting_scope"], "files under \"logs \"",
            "the card shows the exact directory"
        );

        // The `logs ` allow does not cover the `logs` directory.
        let third = run_of(&submit(&mut local.socket, local.chat, "logs plain", None).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["rule"], json!(wide_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(wait_run(&mut local.socket, third).await["status"], "failed");

        // Allow the plain `logs` directory as well, then the `logs ` allow
        // must not cover it either.
        command(
            &mut local.socket,
            Id::new(),
            CommandCall::RuleRemove { rule: spaced_rule },
        )
        .await;
        let fourth = run_of(&submit(&mut local.socket, local.chat, "logs again", None).await);
        let approval = approval_for_run(&mut local.socket, fourth).await;
        assert_eq!(approval["rule"], json!(wide_id));
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let _decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "succeeded"
        );
        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert!(
            rules.iter().any(|rule| rule["path_prefix"] == "logs"),
            "the plain logs allow was derived: {rules:?}"
        );

        let fifth = run_of(&submit(&mut local.socket, local.chat, "logs space again", None).await);
        let approval = approval_for_run(&mut local.socket, fifth).await;
        assert_eq!(
            approval["rule"],
            json!(wide_id),
            "the `logs` allow does not cover the `logs ` directory"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(wait_run(&mut local.socket, fifth).await["status"], "failed");

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_all_space_path_is_not_the_workspace_root() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("read_file", json!({"path": "a.txt"})),
                text("one"),
                tool("read_file", json!({"path": "   "})),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // The Workspace root allow must not cover a name made of spaces.
        command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Allow,
                "read_file",
                None,
                Some("."),
            ),
        )
        .await;
        let second = run_of(&submit(&mut local.socket, local.chat, "space name", None).await);
        let shown = wait_status(&mut local.socket, second, "waiting").await;
        assert_eq!(shown["status"], "waiting", "the path is not the root");
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "rule");
        assert_eq!(approval["ambiguous"], true);
        assert_eq!(approval["rule"], Value::Null);
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();

        // Such a path cannot make a lasting rule either.
        let lasting = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await;
        assert!(matches!(
            lasting,
            Err((ErrorCode::InvalidArgument, message))
                if message == "the action path cannot make a lasting rule"
        ));
        let decided = decide_approval(&mut local.socket, approval_id, false, false).await;
        assert_eq!(decided["status"], "denied");
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "failed"
        );

        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert_eq!(rules.len(), 1, "no derived rule for an all-space path");
        assert_eq!(rules[0]["effect"], "allow");
        assert_eq!(rules[0]["path_prefix"], ".");

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leading_space_action_derives_the_executed_directory() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("read_file", json!({"path": " logs/report.txt"})),
                text("one"),
                tool("read_file", json!({"path": " logs/other.txt"})),
                text("two"),
                tool("read_file", json!({"path": "logs/report2.txt"})),
                text("three"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "start", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        assert_eq!(
            answer_next_action(&mut local.device, "o").await,
            WireAction::ReadFile {
                path: " logs/report.txt".to_owned()
            },
            "the daemon's action keeps the model's spelling"
        );
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        let wide = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "read_file",
                None,
                None,
            ),
        )
        .await;
        let wide_id: Id = serde_json::from_value(wide["rule"].clone()).unwrap();
        let second = run_of(&submit(&mut local.socket, local.chat, "logs rest", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["rule"], json!(wide_id));
        assert_eq!(
            approval["summary"], "read_file: logs/other.txt",
            "the card shows the path the daemon will read"
        );
        assert_eq!(
            approval["lasting_scope"], "files under \"logs\"",
            "the derived directory is where the action runs"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert!(decided["rule"].is_string());
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // The derived `logs` rule covers the next action that runs in `logs`,
        // with or without a leading blank.
        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert!(
            rules.iter().any(|rule| rule["path_prefix"] == "logs"),
            "the derived rule names the executed directory: {rules:?}"
        );
        let third = run_of(&submit(&mut local.socket, local.chat, "logs again", None).await);
        let action = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            answer_next_action(&mut local.device, "o"),
        )
        .await
        .expect("the derived logs rule lets the action run");
        assert_eq!(
            action,
            WireAction::ReadFile {
                path: "logs/report2.txt".to_owned()
            }
        );
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );
        assert_eq!(started_actions(&running, third).await.len(), 1);

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }
    fn command_fingerprint(command: &CommandCall) -> String {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(serde_json::to_vec(command).unwrap());
        format!(
            "v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_command_answers_only_its_owner() {
        let (_dir, running, invite) = running().await;
        let (mut admin_socket, _admin_token) = admin(&running, &invite).await;
        let created = command(&mut admin_socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let made = command(
            &mut admin_socket,
            Id::new(),
            add_rule(bot, neha::record::RuleEffect::Allow, "shell", None, None),
        )
        .await;
        let admin: Id = serde_json::from_value(made["user"].clone()).unwrap();
        let rule: Id = serde_json::from_value(made["rule"].clone()).unwrap();

        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let grace_token = sign_in(&running, "Grace").await;
        let mut grace_socket = open_socket(&running, &grace_token).await;
        let call = CommandCall::RuleRemove { rule };
        let fingerprint = command_fingerprint(&call);
        let before = running.server.store.last_seq().await.unwrap();
        let command_id = Id::new();
        running
            .server
            .store
            .record_command_result(
                command_id,
                &fingerprint,
                json!({"rule": rule, "user": admin}),
            )
            .await
            .unwrap();

        let owner = try_command(&mut admin_socket, command_id, call.clone()).await;
        assert_eq!(
            owner.unwrap().1,
            json!({"rule": rule, "user": admin}),
            "the owner replays the exact confirmation"
        );
        let other = try_command(&mut grace_socket, command_id, call.clone()).await;
        assert!(
            matches!(other, Err((ErrorCode::Forbidden, _))),
            "another User cannot read the confirmation: {other:?}"
        );

        // A record made before results carried the owner is refused too.
        let legacy_id = Id::new();
        running
            .server
            .store
            .record_command_result(legacy_id, &fingerprint, json!({"rule": rule}))
            .await
            .unwrap();
        for socket in [&mut admin_socket, &mut grace_socket] {
            let legacy = try_command(socket, legacy_id, call.clone()).await;
            assert!(
                matches!(legacy, Err((ErrorCode::Forbidden, _))),
                "an unowned recorded result is not replayed: {legacy:?}"
            );
        }
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            before,
            "a replay writes no events"
        );

        grace_socket.close(None).await.unwrap();
        admin_socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_resumed_run_rebuilds_the_history_it_first_saw() {
        let (_dir, running, invite) = running().await;
        let (base, provider_task, gates, recorded) = gated_provider(vec![
            hold("A done"),
            tool("read_file", json!({"path": "a.txt"})),
            text("C done"),
            text("B done"),
        ])
        .await;
        let mut socket = admin_socket(&running, &invite).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let machine = machine_token_n(40);
        let (computer, machine) = paired_computer_with_token(&mut socket, &running, &machine).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", Some(computer))).await;
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let mut device = connect_computer(&running, &machine, computer).await;
        wait_online(&mut socket, computer).await;

        // A starts and holds its answer; B queues behind it before that answer
        // exists.
        let a = run_of(&submit(&mut socket, chat, "A task", None).await);
        let b = run_of(&submit(&mut socket, chat, "B task", None).await);
        gates[0].open();
        let approval: Id =
            serde_json::from_value(approval_for_run(&mut socket, b).await["approval"].clone())
                .unwrap();
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let seq_of = |kind: EventKind, run: Id| {
            events
                .iter()
                .find(|event| event.kind == kind && event.data["run"] == json!(run))
                .unwrap_or_else(|| panic!("no {kind:?} for {run}"))
                .seq
        };
        let queued = seq_of(EventKind::RunQueued, b);
        let answered = events
            .iter()
            .find(|event| {
                event.kind == EventKind::MessageAdded
                    && event.data["run"] == json!(a)
                    && matches!(event.author, Author::Bot(_))
            })
            .expect("A's answer")
            .seq;
        let started = seq_of(EventKind::RunStarted, b);
        assert!(
            queued < answered && answered < started,
            "B queued before A answered, and started after: {queued} < {answered} < {started}"
        );
        // C runs while B waits; B must resume on the history it first saw.
        let c = run_of(&submit(&mut socket, chat, "C task", None).await);
        assert_eq!(wait_run(&mut socket, c).await["status"], "succeeded");
        decide_approval(&mut socket, approval, true, false).await;
        answer_next_action(&mut device, "o").await;
        assert_eq!(wait_run(&mut socket, b).await["status"], "succeeded");

        let bodies = recorded.lock().unwrap().clone();
        assert_eq!(bodies.len(), 4, "A, B, C and B's resume each call once");
        let messages = |body: &serde_json::Value| body["messages"].clone();
        let initial = messages(&bodies[1]);
        let resumed = messages(&bodies[3]);
        assert_eq!(
            resumed.as_array().unwrap()[..initial.as_array().unwrap().len()],
            initial.as_array().unwrap()[..],
            "the resumed history keeps the exact prefix it first saw"
        );
        let serialized = initial.to_string();
        assert!(
            serialized.contains("A done"),
            "B's first input includes A's answer"
        );
        let resumed_text = resumed.to_string();
        assert!(
            !resumed_text.contains("C done"),
            "C's answer is not part of B's resumed history: {resumed_text}"
        );
        assert!(
            resumed_text.contains("call-1"),
            "B's own tool call is appended after the prefix"
        );

        device.close(None).await.unwrap();
        provider_task.abort();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_grant_cannot_become_lasting() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool(
                    "write_file",
                    json!({"path": "/tmp/first-grant.txt", "content": "x"}),
                ),
                text("done"),
            ],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "write", None).await);
        let approval = approval_for_run(&mut local.socket, run).await;
        assert_eq!(approval["reason"], "grant");
        assert!(approval["grant_folder"].is_string());
        assert!(
            approval["lasting_scope"].is_null(),
            "the first use cannot offer a lasting grant"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let refused = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await;
        assert!(refused.is_err(), "a first grant cannot become lasting");

        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, run).await["status"],
            "succeeded"
        );
        let shown = command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotShow { bot: local.bot },
        )
        .await;
        assert_eq!(shown["bot"]["grants"], json!([]));

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_outside_the_grants_asks_the_owner() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool(
                    "write_file",
                    json!({"path": "inside.txt", "content": "one"}),
                ),
                text("first"),
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "two"}),
                ),
                text("second"),
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "three"}),
                ),
                text("third"),
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "four"}),
                ),
                text("fourth"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "inside", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A write outside the Workspace and its grants asks the owner.
        let second = run_of(&submit(&mut local.socket, local.chat, "outside", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "grant");
        assert_eq!(approval["lasting_scope"], "writes under /tmp");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // One allow does not last: the next write asks again, and a lasting
        // answer adds the folder to the Bot.
        let third = run_of(&submit(&mut local.socket, local.chat, "outside again", None).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "grant");
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, true).await;
        assert_eq!(decided["grant"], "/tmp");
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );
        let shown = command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotShow { bot: local.bot },
        )
        .await;
        assert_eq!(shown["bot"]["grants"], json!(["/tmp"]));

        let fourth = run_of(&submit(&mut local.socket, local.chat, "granted", None).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "the lasting grant covers the next write"
        );
        answer_next_action(&mut local.device, "o").await;
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_inside_the_runs_directory_asks_no_grant() {
        let (_dir, running, invite) = running().await;
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let cwd = work.path().to_string_lossy().into_owned();
        let outside = elsewhere.path().join("out.txt");
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("write_file", json!({"path": "in.txt", "content": "one"})),
                text("first"),
                tool("write_file", json!({"path": "in.txt", "content": "two"})),
                text("second"),
                tool(
                    "write_file",
                    json!({"path": outside.to_string_lossy(), "content": "three"}),
                ),
                text("third"),
            ],
        )
        .await;

        // The first use of the Computer asks whatever the action writes; the
        // directory the run names is not a grant to ask for.
        let first = run_of(&submit_in(&mut local.socket, local.chat, "write", &cwd).await);
        let approval = approval_for_run(&mut local.socket, first).await;
        assert_eq!(approval["reason"], "first_use_local");
        assert_eq!(
            approval["cwd"],
            json!(cwd),
            "the card names the directory the run works in: {approval}"
        );
        assert!(
            approval["grant_folder"].is_null(),
            "a write inside the run's directory asks for no folder: {approval}"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, true, false).await;
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "wrote").await;
        assert_eq!(
            action,
            WireAction::WriteFile {
                path: "in.txt".to_owned(),
                content: "one".to_owned()
            }
        );
        assert_eq!(working_dir.as_deref(), Some(cwd.as_str()));
        assert!(
            access.write_roots.iter().any(|root| root == &cwd),
            "the run's directory travels as a write root: {:?}",
            access.write_roots
        );
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A write inside the directory asks for no grant.
        let second = run_of(&submit_in(&mut local.socket, local.chat, "again", &cwd).await);
        assert!(
            waits_without_approval(&mut local.socket, local.chat).await,
            "a write inside the run's directory needs no approval"
        );
        answer_next_action(&mut local.device, "wrote").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // A write outside it still asks for the folder, and one allow opens it
        // for that action only.
        let third = run_of(&submit_in(&mut local.socket, local.chat, "elsewhere", &cwd).await);
        let approval = approval_for_run(&mut local.socket, third).await;
        assert_eq!(approval["reason"], "grant");
        assert_eq!(
            approval["grant_folder"],
            json!(elsewhere.path().to_str().unwrap())
        );
        assert_eq!(
            approval["lasting_scope"],
            json!(format!(
                "writes under {}",
                elsewhere.path().to_str().unwrap()
            ))
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval_id, true, false).await;
        assert_eq!(decided["status"], "allowed");
        answer_next_action(&mut local.device, "wrote").await;
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );
        let shown = command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotShow { bot: local.bot },
        )
        .await;
        assert_eq!(shown["bot"]["grants"], json!([]));

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_run_never_asks_for_a_folder() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let cwd = work.path().to_string_lossy().into_owned();
        let outside = elsewhere.path().join("out.txt");
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "write_file",
                json!({"path": outside.to_string_lossy(), "content": "x"}),
            ),
            text("done"),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        // A Hosted Computer works only in its Workspace: a run that names a
        // host directory fails before it starts, and never asks for a folder.
        let queued = submit_in(&mut socket, chat, "write", &cwd).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "failed");
        assert!(
            finished["text"]
                .as_str()
                .unwrap()
                .contains("a Hosted Computer works only in its Workspace"),
            "{}",
            finished["text"]
        );
        assert!(
            approvals_in(&mut socket, None).await.is_empty(),
            "a Hosted Computer never asks for a folder"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restored_run_keeps_its_directory_as_a_write_root() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let first = start(home, 0).await.unwrap();
        let invite = first.invite.clone().expect("a fresh Space invites");
        let work = tempfile::tempdir().unwrap();
        let cwd = work.path().to_string_lossy().into_owned();
        let mut local = local_bot(
            &first,
            &invite,
            vec![
                tool("write_file", json!({"path": "in.txt", "content": "one"})),
                text("done"),
            ],
        )
        .await;
        let run = run_of(&submit_in(&mut local.socket, local.chat, "write", &cwd).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();

        // The Space stops while the run still waits for its answer; the
        // Computer's channel goes with it.
        local.device.close(None).await.unwrap();
        local.socket.close(None).await.unwrap();
        let root = first.server_home();
        first.shutdown().await;

        let second = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        assert_eq!(
            wait_status(&mut socket, run, "waiting").await["status"],
            "waiting",
            "the run waits where the record left it"
        );
        // The replay projects the run's directory onto the card again.
        let restored = approval_for_run(&mut socket, run).await;
        assert_eq!(
            restored["cwd"],
            json!(cwd),
            "the restored card still names the directory: {restored}"
        );
        let mut device = connect_computer(&second, &local.machine, local.computer).await;
        wait_online(&mut socket, local.computer).await;

        let decided = decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(decided["status"], "allowed");
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut device, "wrote").await;
        assert_eq!(
            action,
            WireAction::WriteFile {
                path: "in.txt".to_owned(),
                content: "one".to_owned()
            }
        );
        assert_eq!(working_dir.as_deref(), Some(cwd.as_str()));
        assert!(
            access.write_roots.iter().any(|root| root == &cwd),
            "the restored run still writes in the directory it began in: {:?}",
            access.write_roots
        );
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");

        device.close(None).await.unwrap();
        local.provider_task.abort();
        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_run_keeps_the_access_it_started_with() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "one"}),
                ),
                text("first done"),
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "two"}),
                ),
                text("second done"),
            ],
        )
        .await;
        // The run starts with the owner's grants.
        command(
            &mut local.socket,
            Id::new(),
            bot_update(
                local.bot,
                None,
                Some(vec!["/data/shared".to_owned()]),
                None,
                None,
            ),
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "write", None).await);
        let access = run_access_event(&running, run).await;
        assert_eq!(access.data["full_access"], json!(false));
        assert_eq!(access.data["grants"], json!(["/data/shared"]));
        let approval = approval_for_run(&mut local.socket, run).await;
        assert_eq!(approval["reason"], "grant");
        let approval: Id = serde_json::from_value(approval["approval"].clone()).unwrap();

        // The owner changes the Bot while the run waits: full access on and
        // the grants gone. The run keeps what it began with.
        let changed = command(
            &mut local.socket,
            Id::new(),
            bot_update(local.bot, Some(true), Some(Vec::new()), None, None),
        )
        .await;
        assert_eq!(changed["full_access"], json!(true));
        assert_eq!(changed["grants"], json!([]));

        let decided = decide_approval(&mut local.socket, approval, true, false).await;
        assert_eq!(decided["status"], "allowed");
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "wrote").await;
        assert_eq!(
            action,
            WireAction::WriteFile {
                path: "/tmp/outside.txt".to_owned(),
                content: "one".to_owned()
            }
        );
        assert_eq!(working_dir, None);
        assert_eq!(
            access,
            neha_computer::channel::AccessFrame {
                full_access: false,
                // The pinned grant, then the one-time grant that answers the
                // card for the action the User allowed.
                write_roots: vec!["/data/shared".to_owned(), "/tmp".to_owned()],
            },
            "a waiting run executes with the access it started with"
        );
        assert_eq!(
            wait_run(&mut local.socket, run).await["status"],
            "succeeded"
        );

        // A run that starts after the change uses the new access.
        let second = run_of(&submit(&mut local.socket, local.chat, "write again", None).await);
        let (_, _, access) = answer_next_action_with_access(&mut local.device, "wrote").await;
        assert_eq!(
            access,
            neha_computer::channel::AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
            "the next run starts with the Bot's current access"
        );
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restored_run_replays_its_pinned_access() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let first = start(home, 0).await.unwrap();
        let invite = first.invite.clone().expect("a fresh Space invites");
        let mut local = local_bot(
            &first,
            &invite,
            vec![
                tool(
                    "write_file",
                    json!({"path": "/tmp/outside.txt", "content": "one"}),
                ),
                text("done"),
            ],
        )
        .await;
        command(
            &mut local.socket,
            Id::new(),
            bot_update(
                local.bot,
                None,
                Some(vec!["/data/shared".to_owned()]),
                None,
                None,
            ),
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "write", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();
        // The Bot turns full access on while the run waits, and the Space
        // stops before the User answers.
        command(
            &mut local.socket,
            Id::new(),
            bot_update(local.bot, Some(true), None, None, None),
        )
        .await;
        local.device.close(None).await.unwrap();
        local.socket.close(None).await.unwrap();
        let root = first.server_home();
        first.shutdown().await;

        let second = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        assert_eq!(
            wait_status(&mut socket, run, "waiting").await["status"],
            "waiting",
            "the run waits where the record left it"
        );
        // The replay restores the pinned access, not today's Bot.
        let restored = run_access_event(&second, run).await;
        assert_eq!(restored.data["full_access"], json!(false));
        assert_eq!(restored.data["grants"], json!(["/data/shared"]));
        let shown = command(
            &mut socket,
            Id::new(),
            CommandCall::BotShow { bot: local.bot },
        )
        .await;
        assert_eq!(shown["bot"]["full_access"], json!(true));

        let mut device = connect_computer(&second, &local.machine, local.computer).await;
        wait_online(&mut socket, local.computer).await;
        let decided = decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(decided["status"], "allowed");
        let (_, _, access) = answer_next_action_with_access(&mut device, "wrote").await;
        assert_eq!(
            access,
            neha_computer::channel::AccessFrame {
                full_access: false,
                write_roots: vec!["/data/shared".to_owned(), "/tmp".to_owned()],
            },
            "a restored run keeps the access it started with"
        );
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");

        device.close(None).await.unwrap();
        local.provider_task.abort();
        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revoke_waits_for_the_run_start_it_races() {
        let (_dir, running, invite) = running().await;
        let (socket, token) = admin(&running, &invite).await;
        let mut local = local_bot_with(
            &running,
            socket,
            machine_token_n(40),
            vec![text("one"), text("two")],
        )
        .await;
        let bot = local.bot;
        // The run begins with a grant the owner takes away while it starts.
        command(
            &mut local.socket,
            Id::new(),
            bot_update(bot, None, Some(vec!["/data/shared".to_owned()]), None, None),
        )
        .await;

        // The pump reads the access, then pauses before recording the start.
        let (reached, release) = crate::runs::pause_start_for_test(bot);
        let first = run_of(&submit(&mut local.socket, local.chat, "one", None).await);
        reached.notified().await;

        // The revoke arrives while the start holds the Bot's admit lock: it
        // must queue behind the start instead of recording first.
        let mut revoke_socket = open_socket(&running, &token).await;
        let revoke = tokio::spawn(async move {
            try_command(
                &mut revoke_socket,
                Id::new(),
                bot_update(bot, None, None, None, Some("/data/shared")),
            )
            .await
        });
        loop {
            if crate::runs::admit_waiters(bot) > 0 {
                break;
            }
            assert!(
                !revoke.is_finished(),
                "the revoke must wait for the run start, not run beside it"
            );
            tokio::task::yield_now().await;
        }
        release.notify_one();
        let revoked = revoke.await.unwrap();
        assert!(revoked.is_ok(), "{revoked:?}");
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // The start recorded the access it read before the revoke, and the
        // record's order puts the start first.
        let access = run_access_event(&running, first).await;
        assert_eq!(access.data["grants"], json!(["/data/shared"]));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let revoked_event = events
            .iter()
            .rfind(|event| event.kind == EventKind::BotUpdated)
            .expect("the revoke is recorded");
        assert_eq!(revoked_event.data["grants"], json!([]));
        assert!(
            access.seq < revoked_event.seq,
            "the run start is recorded before the revoke it raced"
        );

        // A run that starts after the revoke cannot recover the grant.
        let second = run_of(&submit(&mut local.socket, local.chat, "two", None).await);
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );
        let access = run_access_event(&running, second).await;
        assert_eq!(
            access.data["grants"],
            json!([]),
            "a later run pins the revoked access"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unsafe_folder_cannot_become_a_lasting_grant() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("first done"),
                tool(
                    "write_file",
                    json!({"path": "/var/tmp/escape/../escaped.txt", "content": "x"}),
                ),
                text("second done"),
            ],
        )
        .await;
        // Answer the Computer's first use, so the next card judges only its
        // own folder.
        let first = run_of(&submit(&mut local.socket, local.chat, "first", None).await);
        let approval = approval_for_run(&mut local.socket, first).await;
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "/home/ada").await;
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A write whose parent climbs with `..` still asks, but its folder
        // cannot become a grant: the card offers no Always allow.
        let second = run_of(&submit(&mut local.socket, local.chat, "write", None).await);
        let approval = approval_for_run(&mut local.socket, second).await;
        assert_eq!(approval["reason"], "grant");
        assert!(
            approval["grant_folder"]
                .as_str()
                .is_some_and(|folder| folder.contains("..")),
            "the card names the folding parent: {approval}"
        );
        assert!(
            approval["lasting_scope"].is_null(),
            "an unsafe folder offers no Always allow: {approval}"
        );
        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        let refused = try_command(
            &mut local.socket,
            Id::new(),
            CommandCall::ApprovalDecide {
                approval: approval_id,
                allow: true,
                lasting: true,
            },
        )
        .await;
        assert!(
            matches!(
                refused,
                Err((ErrorCode::InvalidArgument, ref message))
                    if message.contains("cannot become lasting")
            ),
            "{refused:?}"
        );
        // The refused answer recorded nothing: the run still waits and no
        // grant was written.
        assert_eq!(
            wait_status(&mut local.socket, second, "waiting").await["status"],
            "waiting"
        );
        let shown = command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotShow { bot: local.bot },
        )
        .await;
        assert_eq!(shown["bot"]["grants"], json!([]));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        assert!(
            !events
                .iter()
                .any(|event| event.kind == EventKind::BotUpdated),
            "a refused lasting answer records no grant"
        );

        // The one action may still be allowed.
        decide_approval(&mut local.socket, approval_id, true, false).await;
        answer_next_action(&mut local.device, "wrote").await;
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_update_adds_and_removes_grants_atomically() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        // Grants belong to a Local Computer; a Hosted Bot has none to change.
        let (_computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(
            &mut socket,
            Id::new(),
            add_bot_on("Scout", Some(BotComputer::Local)),
        )
        .await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let add = |grant: &str| bot_update(bot, None, None, Some(grant), None);
        let remove = |grant: &str| bot_update(bot, None, None, None, Some(grant));

        // An add joins the current list; the stored spelling is normalized.
        let added = command(&mut socket, Id::new(), add("/data/shared")).await;
        assert_eq!(added["grants"], json!(["/data/shared"]));
        let folded = command(&mut socket, Id::new(), add("/data//team/./notes")).await;
        assert_eq!(
            folded["grants"],
            json!(["/data/shared", "/data/team/notes"])
        );

        // Adding a grant the Bot already has, and removing one it lacks, are
        // refused where the whole list would otherwise be written.
        let duplicate = try_command(&mut socket, Id::new(), add("/data/shared")).await;
        assert!(
            matches!(
                duplicate,
                Err((ErrorCode::InvalidArgument, ref message))
                    if message.contains("already has a grant")
            ),
            "{duplicate:?}"
        );
        let missing = try_command(&mut socket, Id::new(), remove("/data/never")).await;
        assert!(
            matches!(
                missing,
                Err((ErrorCode::InvalidArgument, ref message))
                    if message.contains("has no grant")
            ),
            "{missing:?}"
        );
        // A replacement and a one-grant change cannot share one command.
        let mixed = try_command(
            &mut socket,
            Id::new(),
            bot_update(
                bot,
                None,
                Some(vec!["/data/replaced".to_owned()]),
                Some("/data/added"),
                None,
            ),
        )
        .await;
        assert!(
            matches!(mixed, Err((ErrorCode::InvalidArgument, _))),
            "{mixed:?}"
        );

        // A remove takes exactly the folder it names, and the record still
        // holds the complete list in the event's existing shape.
        let removed = command(&mut socket, Id::new(), remove("/data/shared")).await;
        assert_eq!(removed["grants"], json!(["/data/team/notes"]));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let updates: Vec<&neha::record::Event> = events
            .iter()
            .filter(|event| event.kind == EventKind::BotUpdated)
            .collect();
        assert_eq!(updates.len(), 3, "each accepted change records one event");
        for event in updates {
            let data = event.data.as_object().expect("an object");
            assert!(
                data.keys()
                    .all(|key| ["bot", "full_access", "grants"].contains(&key.as_str())),
                "the event keeps its shape: {data:?}"
            );
            assert!(
                data.get("grants").is_some_and(Value::is_array),
                "the event writes the complete list: {data:?}"
            );
        }
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["bot"]["grants"], json!(["/data/team/notes"]));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_working_directory_needs_the_requests_write_authorization() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        command(
            &mut socket,
            Id::new(),
            provider_add("http://127.0.0.1:1/v1".to_owned()),
        )
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let before = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events
            .len();

        // A Client written before the field existed sends the directory and no
        // authorization; a request that says the directory is not writable
        // cannot be honored either. Both are refused before any run exists.
        for consent in [None, Some(false)] {
            let refused = try_command(
                &mut socket,
                Id::new(),
                CommandCall::MessageSend(MessageSend {
                    chat,
                    text: "work in the folder".to_owned(),
                    file: None,
                    cwd: Some("/var/tmp/legacy-logs /.".to_owned()),
                    cwd_writable: consent,
                }),
            )
            .await;
            assert!(
                matches!(
                    refused,
                    Err((ErrorCode::InvalidArgument, ref message))
                        if message.contains("cwd_writable")
                ),
                "{refused:?}"
            );
        }
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        assert_eq!(events.len(), before, "a refused request records no run");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages, json!([]), "a refused request adds no Message");

        // This build's Client names the directory and its authorization; the
        // record keeps both, so a restore knows what the run may write.
        let accepted = command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "work in the folder".to_owned(),
                file: None,
                cwd: Some("/var/tmp/legacy-logs /.".to_owned()),
                cwd_writable: Some(true),
            }),
        )
        .await;
        let run = run_of(&accepted);
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let context = events
            .iter()
            .find(|event| event.kind == EventKind::RunContext && event.data["run"] == json!(run))
            .expect("the accepted run records its directory");
        assert_eq!(context.data["working_dir"], json!("/var/tmp/legacy-logs "));
        assert_eq!(context.data["writable"], json!(true));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_named_directory_is_a_write_root_for_one_run_only() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf one"})),
                text("one done"),
                tool("shell", json!({"command": "printf two"})),
                text("two done"),
            ],
        )
        .await;
        let cwd = "/var/tmp/neha-one-run ";
        let spelling = "/var/tmp/neha-one-run /.";

        // The first use asks; the directory is a write root for this run.
        let first = run_of(&submit_in(&mut local.socket, local.chat, "one", spelling).await);
        allow_first_use(&mut local.socket, local.chat).await;
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "one").await;
        assert!(matches!(action, WireAction::Shell { .. }));
        assert_eq!(working_dir.as_deref(), Some(cwd));
        assert_eq!(access.write_roots, vec![cwd.to_owned()]);
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // A later run that names no directory inherits no write root.
        let second = run_of(&submit(&mut local.socket, local.chat, "two", None).await);
        let (_, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "two").await;
        assert_eq!(working_dir, None);
        assert!(
            access.write_roots.is_empty(),
            "a run's directory belongs to the run that named it"
        );
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );
        let shown = show_run(&mut local.socket, second).await;
        assert_eq!(shown["cwd"], json!(null));

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_directory_that_ends_in_a_space_keeps_its_identity() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "printf first"})),
                text("first done"),
                tool("write_file", json!({"path": "notes.txt", "content": "x"})),
                text("rule done"),
                tool("write_file", json!({"path": "notes.txt", "content": "y"})),
                text("folder done"),
            ],
        )
        .await;
        let spaced = "/var/tmp/neha-cwd-logs ";
        // The raw spelling keeps the space inside the string while the User
        // types it: the Space trims the boundary once and folds the `.` away,
        // so the recorded directory is the one whose name ends in a space.
        let spelling = "/var/tmp/neha-cwd-logs /.";

        // Answer the Computer's first use on a harmless shell command.
        let first = run_of(&submit(&mut local.socket, local.chat, "first", None).await);
        allow_first_use(&mut local.socket, local.chat).await;
        let action = answer_next_action(&mut local.device, "first").await;
        assert!(matches!(action, WireAction::Shell { .. }));
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // An Ask rule for the recorded directory must match a relative path
        // the run resolves against it, and the card must show that target.
        let rule = command(
            &mut local.socket,
            Id::new(),
            add_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "write_file",
                None,
                Some(spelling),
            ),
        )
        .await;
        let rule_id: Id = serde_json::from_value(rule["rule"].clone()).unwrap();
        let ruled =
            run_of(&submit_in(&mut local.socket, local.chat, "write notes", spelling).await);
        let approval = approval_for_run(&mut local.socket, ruled).await;
        assert_eq!(approval["reason"], "rule", "{approval}");
        assert_eq!(approval["cwd"], json!(spaced), "{approval}");
        let summary = approval["summary"].as_str().unwrap_or_default();
        assert!(
            summary.contains("/var/tmp/neha-cwd-logs /notes.txt"),
            "the card shows the directory the run works in: {summary}"
        );

        // The Ask rule is hit before the daemon is sent the action: no step
        // and no action is recorded while the run waits.
        {
            let state = running.server.state.lock().unwrap();
            let run = state.runs.get(&ruled).expect("the run exists");
            assert_eq!(run.status, crate::state::RunStatus::Waiting);
            assert!(
                run.steps.is_empty() && run.actions.is_empty(),
                "an action the User must answer is not sent to the Computer"
            );
        }

        let approval_id: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval_id, true, false).await;
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "wrote").await;
        assert!(
            matches!(&action, WireAction::WriteFile { path, .. } if path == "notes.txt"),
            "{action:?}"
        );
        assert_eq!(working_dir.as_deref(), Some(spaced));
        assert_eq!(
            access.write_roots,
            vec![spaced.to_owned()],
            "the daemon receives the directory name exactly as recorded"
        );
        assert_eq!(
            wait_run(&mut local.socket, ruled).await["status"],
            "succeeded"
        );

        // A relative write in that recorded directory needs no folder card:
        // the directory is a write root for the run, and trimming it would ask
        // for the folder next to it instead.
        command(
            &mut local.socket,
            Id::new(),
            CommandCall::RuleRemove { rule: rule_id },
        )
        .await;
        let folder =
            run_of(&submit_in(&mut local.socket, local.chat, "write more notes", spelling).await);
        let (action, working_dir, access) =
            answer_next_action_with_access(&mut local.device, "wrote").await;
        assert!(
            matches!(&action, WireAction::WriteFile { path, .. } if path == "notes.txt"),
            "{action:?}"
        );
        assert_eq!(working_dir.as_deref(), Some(spaced));
        assert_eq!(access.write_roots, vec![spaced.to_owned()]);
        {
            let state = running.server.state.lock().unwrap();
            assert!(
                state.runs[&folder].approval.is_none(),
                "a write in the run's directory asks no card"
            );
        }
        assert_eq!(
            wait_run(&mut local.socket, folder).await["status"],
            "succeeded"
        );

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_grant_name_that_ends_in_a_space_survives_the_grant_table() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        // Grants belong to a Local Computer; a Hosted Bot has none to change.
        let (_computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(
            &mut socket,
            Id::new(),
            add_bot_on("Scout", Some(BotComputer::Local)),
        )
        .await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let add = |grant: &str| bot_update(bot, None, None, Some(grant), None);

        // `logs` and `logs ` are two folders: a fold keeps them apart, and a
        // grant that ends in a space keeps its name when another is added.
        let folded = command(&mut socket, Id::new(), add("/data/logs/")).await;
        assert_eq!(folded["grants"], json!(["/data/logs"]));
        let spaced = command(&mut socket, Id::new(), add("/data/logs ")).await;
        assert_eq!(spaced["grants"], json!(["/data/logs", "/data/logs "]));
        let both = command(&mut socket, Id::new(), add("/data/archive")).await;
        assert_eq!(
            both["grants"],
            json!(["/data/archive", "/data/logs", "/data/logs "])
        );

        // A remove drops exactly the named folder, and the event keeps both
        // of the remaining names exactly.
        let removed = command(
            &mut socket,
            Id::new(),
            bot_update(bot, None, None, None, Some("/data/logs ")),
        )
        .await;
        assert_eq!(removed["grants"], json!(["/data/archive", "/data/logs"]));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let last = events
            .iter()
            .rfind(|event| event.kind == EventKind::BotUpdated)
            .expect("the update is recorded");
        assert_eq!(last.data["grants"], json!(["/data/archive", "/data/logs"]));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_bot_updates_do_not_lose_each_other() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        // Grants belong to a Local Computer; a Hosted Bot has none to change.
        let (_computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(
            &mut socket,
            Id::new(),
            add_bot_on("Scout", Some(BotComputer::Local)),
        )
        .await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();

        // Four commands from four connections arrive together; every grant
        // must survive, because each add joins the list the Space holds.
        let mut sockets = Vec::new();
        for _ in 0..4 {
            sockets.push(open_socket(&running, &token).await);
        }
        let mut pending = Vec::new();
        for (position, connection) in sockets.iter_mut().enumerate() {
            let grant = format!("/data/folder-{position}");
            pending.push(try_command(
                connection,
                Id::new(),
                bot_update(bot, None, None, Some(&grant), None),
            ));
        }
        let outcomes = futures_util::future::join_all(pending).await;
        for outcome in outcomes {
            assert!(outcome.is_ok(), "{outcome:?}");
        }
        let listed = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        let mut grants: Vec<String> = listed["bot"]["grants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|grant| grant.as_str().unwrap().to_owned())
            .collect();
        grants.sort();
        assert_eq!(
            grants,
            vec![
                "/data/folder-0".to_owned(),
                "/data/folder-1".to_owned(),
                "/data/folder-2".to_owned(),
                "/data/folder-3".to_owned(),
            ]
        );

        // An add and a remove that arrive together keep the other grants too.
        let mut adding = open_socket(&running, &token).await;
        let mut removing = open_socket(&running, &token).await;
        let (added, removed) = tokio::join!(
            try_command(
                &mut adding,
                Id::new(),
                bot_update(bot, None, None, Some("/data/added"), None)
            ),
            try_command(
                &mut removing,
                Id::new(),
                bot_update(bot, None, None, None, Some("/data/folder-0"))
            )
        );
        assert!(added.is_ok(), "{added:?}");
        assert!(removed.is_ok(), "{removed:?}");
        let listed = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        let grants = listed["bot"]["grants"].clone();
        assert!(
            grants.as_array().unwrap().contains(&json!("/data/added")),
            "the add survives the remove: {grants}"
        );
        assert!(
            !grants
                .as_array()
                .unwrap()
                .contains(&json!("/data/folder-0")),
            "the remove survives the add: {grants}"
        );
        for kept in ["/data/folder-1", "/data/folder-2", "/data/folder-3"] {
            assert!(
                grants.as_array().unwrap().contains(&json!(kept)),
                "{kept} survives: {grants}"
            );
        }

        for mut connection in sockets {
            connection.close(None).await.unwrap();
        }
        adding.close(None).await.unwrap();
        removing.close(None).await.unwrap();
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runs_directory_is_normalized_and_cannot_climb() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = fake_provider().await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let send = |cwd: Option<&str>| {
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "where".to_owned(),
                file: None,
                cwd_writable: cwd.is_some().then_some(true),
                cwd: cwd.map(ToOwned::to_owned),
            })
        };

        // A `..` component, a relative path, and a NUL name no directory the
        // Space can grant as the run's write root.
        for refused in ["/tmp/../etc", "relative/dir", "/tmp/dir\u{0}"] {
            let outcome = try_command(&mut socket, Id::new(), send(Some(refused))).await;
            assert!(
                matches!(outcome, Err((ErrorCode::InvalidArgument, _))),
                "{refused:?}: {outcome:?}"
            );
        }

        // An accepted directory is recorded in its one lexical form, so the
        // User sees the directory the run actually works in.
        let queued = command(&mut socket, Id::new(), send(Some("/tmp//run/./dir"))).await;
        assert_eq!(
            queued["cwd"],
            json!("/tmp/run/dir"),
            "the command names the granted directory"
        );
        let run = run_of(&queued);
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["cwd"], json!("/tmp/run/dir"));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let context = events
            .iter()
            .find(|event| event.kind == EventKind::RunContext && event.data["run"] == json!(run))
            .expect("the run records its directory");
        assert_eq!(context.data["working_dir"], json!("/tmp/run/dir"));

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_update_sets_full_access_and_grants_for_its_owner() {
        let (dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (_computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(
            &mut socket,
            Id::new(),
            add_bot_on("Scout", Some(BotComputer::Local)),
        )
        .await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let command_id = Id::new();
        let update = CommandCall::BotUpdate(neha::surface::BotUpdate {
            bot,
            full_access: Some(true),
            full_access_consent: Some(neha::record::FULL_ACCESS_CONSENT_VERSION),
            grants: Some(vec!["/data/shared".to_owned(), "/data/team".to_owned()]),
            grant_add: None,
            grant_remove: None,
        });
        let first = command(&mut socket, command_id, update.clone()).await;
        assert_eq!(first["full_access"], json!(true));
        assert_eq!(first["grants"], json!(["/data/shared", "/data/team"]));
        assert!(
            first["user"].is_string(),
            "the result names the User it belongs to"
        );
        // The answer that widens access is versioned in the record, so an old
        // record can never restore full access by itself.
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let widened = events
            .iter()
            .find(|event| event.kind == EventKind::BotUpdated)
            .expect("the update is recorded");
        assert_eq!(
            widened.data["full_access_version"],
            json!(neha::record::FULL_ACCESS_CONSENT_VERSION)
        );
        let retried = command(&mut socket, command_id, update).await;
        assert_eq!(
            first["id"], retried["id"],
            "a retry replays the confirmation"
        );

        let refused = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotUpdate(neha::surface::BotUpdate {
                bot,
                full_access: None,
                full_access_consent: None,
                grants: Some(vec!["relative/folder".to_owned()]),
                grant_add: None,
                grant_remove: None,
            }),
        )
        .await;
        assert!(
            matches!(refused, Err((ErrorCode::InvalidArgument, _))),
            "a relative grant is refused: {refused:?}"
        );
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["bot"]["full_access"], json!(true));

        // The versioned answer replays: a restart keeps full access on.
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let restarted = start(test_home(dir.path()), 0).await.unwrap();
        let token = sign_in(&restarted, "Ada").await;
        let mut socket = open_socket(&restarted, &token).await;
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(
            shown["bot"]["full_access"],
            json!(true),
            "a versioned full-access answer replays"
        );
        assert_eq!(
            shown["bot"]["grants"],
            json!(["/data/shared", "/data/team"])
        );
        socket.close(None).await.unwrap();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turning_full_access_on_needs_the_current_answer() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        // Full access applies to a Local Computer; a Hosted Bot has only its
        // Workspace and refuses the update before the answer is considered.
        let (_computer, _machine) = paired_computer(&mut socket, &running).await;
        let created = command(
            &mut socket,
            Id::new(),
            add_bot_on("Scout", Some(BotComputer::Local)),
        )
        .await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let request = |consent: Option<u16>| {
            CommandCall::BotUpdate(neha::surface::BotUpdate {
                bot,
                full_access: Some(true),
                full_access_consent: consent,
                grants: None,
                grant_add: None,
                grant_remove: None,
            })
        };

        // A request without the answer - what a CLI written before the answer
        // was versioned sends - cannot widen access.
        let refused = try_command(&mut socket, Id::new(), request(None)).await;
        assert!(
            matches!(
                refused,
                Err((ErrorCode::InvalidArgument, ref message))
                    if message.contains("current answer")
            ),
            "{refused:?}"
        );
        // An answer this build does not know cannot widen it either.
        let stale = try_command(
            &mut socket,
            Id::new(),
            request(Some(neha::record::FULL_ACCESS_CONSENT_VERSION + 1)),
        )
        .await;
        assert!(
            matches!(stale, Err((ErrorCode::InvalidArgument, _))),
            "{stale:?}"
        );
        // Neither refusal recorded anything or changed the Bot.
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["bot"]["full_access"], json!(false));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        assert!(
            !events
                .iter()
                .any(|event| event.kind == EventKind::BotUpdated),
            "a refused request records no Bot update"
        );

        // The current answer is recorded and turns full access on.
        let on = command(
            &mut socket,
            Id::new(),
            request(Some(neha::record::FULL_ACCESS_CONSENT_VERSION)),
        )
        .await;
        assert_eq!(on["full_access"], json!(true));
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let widened = events
            .iter()
            .rfind(|event| event.kind == EventKind::BotUpdated)
            .expect("the answer is recorded");
        assert_eq!(
            widened.data["full_access_version"],
            json!(neha::record::FULL_ACCESS_CONSENT_VERSION)
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_bot_refuses_file_access_changes() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        // The answer a Local Computer needs is sent too: this Bot is Hosted,
        // so the refusal must name the Computer rather than the answer.
        let refused = try_command(
            &mut socket,
            Id::new(),
            bot_update(bot, Some(true), None, None, None),
        )
        .await;
        match refused {
            Err((ErrorCode::InvalidArgument, message)) => assert!(
                message.contains("Hosted Computer works only in its Workspace"),
                "{message}"
            ),
            other => panic!("a Hosted Bot has no file access to change: {other:?}"),
        }
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_remove_replays_one_confirmation() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let made = command(
            &mut socket,
            Id::new(),
            add_rule(bot, neha::record::RuleEffect::Allow, "shell", None, None),
        )
        .await;
        let rule: Id = serde_json::from_value(made["rule"].clone()).unwrap();
        let call = CommandCall::RuleRemove { rule };
        let command_id = Id::new();
        let mut retry = open_socket(&running, &token).await;
        let (first, second) = tokio::join!(
            try_command(&mut socket, command_id, call.clone()),
            try_command(&mut retry, command_id, call.clone())
        );
        retry.close(None).await.unwrap();
        let (_, first) = first.expect("the first removal");
        let (_, second) = second.expect("the concurrent retry");
        assert_eq!(
            first, second,
            "one command identifier returns one confirmation"
        );
        assert!(
            first["user"].is_string(),
            "the confirmation says who removed the rule"
        );
        let rules = approvals_of(&mut socket, bot).await;
        assert!(
            rules.iter().all(|view| view["rule"] != json!(rule)),
            "the rule is removed once"
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_add_replays_its_recorded_result() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let command_id = Id::new();
        let call = add_rule(bot, neha::record::RuleEffect::Allow, "shell", None, None);
        let first = command(&mut socket, command_id, call.clone()).await;
        let second = command(&mut socket, command_id, call.clone()).await;
        assert_eq!(
            first, second,
            "a recorded rule add must confirm the same result on retry"
        );

        // Adding the same rule under another command is idempotent; a retry
        // after that rule is deleted must not create a new rule.
        let duplicate_id = Id::new();
        let duplicate = command(&mut socket, duplicate_id, call.clone()).await;
        assert_eq!(duplicate["rule"], first["rule"]);
        let rule: Id = serde_json::from_value(duplicate["rule"].clone()).unwrap();
        command(&mut socket, Id::new(), CommandCall::RuleRemove { rule }).await;
        let retried = command(&mut socket, duplicate_id, call).await;
        assert_eq!(
            retried["rule"], duplicate["rule"],
            "a confirmed duplicate rule add survives the rule's deletion"
        );
        let rules = approvals_of(&mut socket, bot).await;
        assert!(
            rules.iter().all(|view| view["rule"] != json!(rule)),
            "the retry must not recreate a deleted rule: {rules:?}"
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }
}
