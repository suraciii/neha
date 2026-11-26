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
    /// Set once the Space stops: no command may then start new work.
    closed: AtomicBool,
    runs: Arc<RunQueue>,
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
            .route("/api/computers/pair", post(computer_pair_start))
            .route("/api/computers/pair/{device}", post(computer_pair_poll))
            .route(
                "/api/chats/{chat}/files",
                post(file_upload).layer(DefaultBodyLimit::max(MAX_FILE_BYTES as usize)),
            )
            .route("/api/files/{id}", get(file_download))
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
    recover_interrupted(&server.store, &server.state).await?;
    reconcile_pairings(&server).await?;
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
    Ok(())
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
        Some(PairingState::Approved { computer }) => Ok(Json(PairingReply::Approved { computer })),
        None => Err(ApiError::not_found("no such pairing")),
    }
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
                            CommandCall::MessageSend(_) => {
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
                        };
                        let outcome = commands::execute(&context, &account, id, command).await;
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
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut cursor = match catch_up(&reader, &out, cursor, limit).await {
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
                    match catch_up(&reader, &out, cursor, limit).await {
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
        let complete = page.events.len();
        let first = page.events[0].seq;
        let mut events = page.events;
        let mut next = page.next;
        while !events.is_empty() {
            let candidate = if events.len() == complete {
                next
            } else {
                events.last().map(|event| event.seq).unwrap_or(cursor)
            };
            let frame = ServerFrame::Snapshot {
                cursor: candidate,
                events: events.clone(),
            };
            if frame_fits(&frame) {
                next = candidate;
                break;
            }
            events.pop();
            next = events.last().map(|event| event.seq).unwrap_or(cursor);
        }
        if events.is_empty() {
            return Err(StreamError::Oversized(first));
        }
        out.send(ServerFrame::Snapshot {
            cursor: next,
            events,
        })
        .await
        .map_err(|_| StreamError::Closed)?;
        sent = true;
        let trimmed = next < page.next;
        // A full page or a trimmed page leaves more to read.
        if complete < page_size && !trimmed {
            return Ok(next);
        }
        cursor = next;
    }
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
    let interrupted: Vec<_> = {
        let state = state
            .lock()
            .map_err(|_| StoreError::Corrupt("the Space state is locked".to_owned()))?;
        state
            .runs
            .values()
            .filter(|run| run.status == crate::state::RunStatus::Running)
            .cloned()
            .collect()
    };
    for run in interrupted {
        let message = Id::new();
        let text = "I was interrupted before I could answer.";
        let usage = run
            .steps
            .iter()
            .filter_map(|step| step.usage)
            .fold(neha::usage::Usage::default(), |total, usage| total + usage);
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
                    "error": "the Space stopped before the run finished",
                }),
            )
            .acting(run.acting),
        ];
        let appended = store
            .append(
                AppendRequest::new(Id::new(), format!("recover:{}", run.id), drafts)
                    .with_confirmation(
                        run.command,
                        serde_json::json!({
                            "run": run.id,
                            "status": "failed",
                            "reply": message,
                            "text": text,
                            "error": "the Space stopped before the run finished",
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
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{Running, ServerError, SpaceServer, StreamError, catch_up, start};
    use crate::blobs::test_s3;
    use crate::config::S3Storage;
    use crate::home::Home;
    use crate::store::{AppendRequest, Store};

    async fn running() -> (tempfile::TempDir, Running, String) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");

        (dir, running, invite)
    }

    /// Starts a fresh Space whose `config.toml` is written first.
    async fn running_with_config(config: &str) -> (tempfile::TempDir, Running, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let running = start(Home::open(dir.path()).unwrap(), 0).await.unwrap();
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
    use neha::record::EventKind;
    use neha::seq::Seq;
    use neha::surface::{
        BotCreate, CommandCall, MessageSend, ModelSpec, ProviderAdd, ProviderProtocol,
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
    ) -> (String, tokio::task::JoinHandle<()>, Vec<Gate>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

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
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let script = Arc::clone(&script);
                let requests = Arc::clone(&requests);
                let waits = Arc::clone(&waits);
                async move {
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

        (format!("http://{addr}/v1"), task, gates)
    }

    async fn scripted_provider(
        script: Vec<ScriptedReply>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let (base, task, _gates) = gated_provider(script).await;

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
        assert_eq!(bot["computer"]["kind"], "managed");

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
        use neha::record::{Author, EventDraft, EventKind};

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
                "I was interrupted before I could answer."
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
        let (base, provider_task, gates) = gated_provider(vec![hold("Hello from fake")]).await;
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
        use neha::record::{Author, EventDraft, EventKind};

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

        assert!(snapshots >= 2, "the history needs more than one page");
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            (1..=last).collect::<Vec<_>>(),
            "the stream skipped a recorded event"
        );
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_event_larger_than_one_frame_is_reported() {
        use neha::record::{Author, EventDraft, EventKind};

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

        let result = catch_up(&reader, &out, neha::seq::Seq::ZERO, None).await;

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
        let restarted = start(Home::open(&root).unwrap(), 0).await.unwrap();
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, _gates) =
            gated_provider(vec![hold("first answer"), text("second answer")]).await;
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

        let restarted = start(Home::open(&root).unwrap(), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        // The working run died with the old process; the waiting one runs now.
        assert_eq!(wait_run(&mut socket, first_run).await["status"], "failed");
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
        let (base, provider_task, _gates) =
            gated_provider(vec![hold("one"), text("two"), text("three")]).await;
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
        let restarted = start(Home::open(&root).unwrap(), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        assert_eq!(wait_run(&mut socket, first_run).await["status"], "failed");
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, gates) =
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
        let (base, provider_task, _gates) =
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

        let restarted = start(Home::open(&root).unwrap(), 0).await.unwrap();
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
        use neha::record::{Author, EventDraft, EventKind};

        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let provider = Id::new();
        let model = Id::new();
        let user = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let (message, run) = (Id::new(), Id::new());
        let (interrupted_message, interrupted_run) = (Id::new(), Id::new());
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
            // A run the previous process left working: a start recovers it.
            EventDraft::new(
                EventKind::MessageAdded,
                Author::User(user),
                json!({
                    "message": interrupted_message,
                    "chat": chat,
                    "text": "older",
                    "run": interrupted_run,
                }),
            ),
            EventDraft::new(
                EventKind::RunStarted,
                Author::Bot(bot),
                json!({
                    "run": interrupted_run,
                    "chat": chat,
                    "bot": bot,
                    "message": interrupted_message,
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
        let result = start(Home::open(directory.path()).unwrap(), port).await;
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

        // A start that listens recovers the run the previous process left working.
        let running = start(Home::open(directory.path()).unwrap(), 0)
            .await
            .unwrap();
        {
            let state = running.server.state.lock().unwrap();
            let recovered = &state.runs[&interrupted_run];
            assert_eq!(recovered.status, crate::state::RunStatus::Failed);
            let reply = recovered.reply.expect("an interrupted run answers");
            let answer = state
                .messages
                .iter()
                .find(|message| message.id == reply)
                .expect("the interruption Message");
            assert_eq!(answer.text, "I was interrupted before I could answer.");
        }
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_record_without_a_queued_event_replays() {
        use neha::record::{Author, EventDraft, EventKind};

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
        use neha::record::{Author, EventDraft, EventKind};

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
        store.close().await.unwrap();

        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().unwrap();
        let (mut socket, _token) = admin(&running, &invite).await;
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
        let mut seen = std::collections::BTreeSet::new();
        let mut snapshots = 0;
        while !seen.contains(&last.as_i64()) {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(10), next_frame(&mut socket))
                    .await
                    .expect("the stream keeps sending");
            match frame {
                ServerFrame::Snapshot { cursor, events } => {
                    snapshots += 1;
                    assert!(cursor >= events.last().map(|event| event.seq).unwrap_or(cursor));
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

        assert!(snapshots >= 2, "the record must be split across frames");
        let recorded: Vec<i64> = seen
            .iter()
            .copied()
            .filter(|seq| *seq <= last.as_i64())
            .collect();
        assert_eq!(recorded, (1..=last.as_i64()).collect::<Vec<_>>());
        socket.close(None).await.unwrap();
        running.shutdown().await;
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
    async fn a_file_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = start(Home::open(dir.path()).unwrap(), 0).await.unwrap();
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

        let second = start(Home::open(dir.path()).unwrap(), 0).await.unwrap();
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_takes_a_file_into_its_workspace_and_delivers_one() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (bot, chat, computer) = create_bot(&mut socket).await;
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
        let taken: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
        let (base, provider_task) = scripted_provider(vec![
            tool("take_file", json!({"file": taken})),
            tool(
                "write_file",
                json!({"path": "out/answer.txt", "content": "the answer"}),
            ),
            tool(
                "put_file",
                json!({"path": "out/answer.txt", "name": "answer.txt"}),
            ),
            text("I read your notes and sent the answer back."),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;

        let result = send_file(&mut socket, chat, taken).await;
        assert_eq!(result["status"], "succeeded");

        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read(workspace.join("notes.txt")).unwrap(),
            b"read me"
        );

        let files = file_list(&mut socket, chat).await;
        assert_eq!(files.len(), 2);
        let delivered = &files[1];
        assert_eq!(delivered["author"], json!({"kind": "bot", "id": bot}));
        assert_eq!(delivered["acting"], uploaded["author"]["id"]);
        assert_eq!(delivered["name"], "answer.txt");
        // A delivered File states the type its name implies.
        assert_eq!(delivered["media_type"], "text/plain");
        assert_eq!(delivered["size"], 10);
        let delivered: Id = serde_json::from_value(delivered["id"].clone()).unwrap();

        // The Bot's Message carries the delivered File and belongs to the run.
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let messages = messages.as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["file"], json!(taken));
        assert_eq!(messages[1]["file"], json!(delivered));
        assert_eq!(messages[1]["run"], result["run"]);
        assert_eq!(messages[1]["text"], "");
        assert_eq!(
            messages[2]["text"],
            "I read your notes and sent the answer back."
        );

        let response = download(&running, &token, delivered).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(
            response.headers()["content-disposition"],
            "attachment; filename*=UTF-8''answer.txt"
        );
        assert_eq!(response.bytes().await.unwrap(), b"the answer".as_slice());

        // A restart rebuilds the Chat with both Files and their Messages.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        let restarted = start(Home::open(&root).unwrap(), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        assert_eq!(file_list(&mut socket, chat).await.len(), 2);
        let again = download(&restarted, &token, delivered).await;
        assert_eq!(again.bytes().await.unwrap(), b"the answer".as_slice());
        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_taken_file_stays_downloadable_after_its_workspace_copy_is_gone() {
        let (_dir, running, invite) = running().await;
        let (mut socket, token) = admin(&running, &invite).await;
        let (_bot, chat, computer) = create_bot(&mut socket).await;
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

        let (base, provider_task) = scripted_provider(vec![
            tool("take_file", json!({"file": file})),
            text("I read the notes."),
        ])
        .await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let result = send(&mut socket, chat, "take the notes").await;
        assert_eq!(result["status"], "succeeded");

        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read(workspace.join("notes.txt")).unwrap(),
            b"read me"
        );
        std::fs::remove_file(workspace.join("notes.txt")).unwrap();

        // The Workspace copy belongs to the Bot; the Space still holds the
        // File, whatever happens to that copy.
        assert!(!workspace.join("notes.txt").exists());
        let response = download(&running, &token, file).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"read me");
        assert_eq!(file_list(&mut socket, chat).await, vec![uploaded]);
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_cannot_take_a_file_of_another_chat() {
        let (_dir, running, invite) = running().await;
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

        let (base, provider_task) = scripted_provider(vec![
            tool("take_file", json!({"file": file})),
            text("That File is not mine to take."),
        ])
        .await;
        let (_other_bot, other_chat, other_computer) = tool_bot(&mut socket, base).await;
        let result = send(&mut socket, other_chat, "take my notes").await;
        assert_eq!(result["status"], "succeeded");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["actions"][0]["tool"], "take_file");
        assert_eq!(shown["actions"][0]["status"], "failed");

        // The File of the first Chat is untouched: still listed, still
        // downloadable, and never copied into the other Chat or Workspace.
        assert_eq!(file_list(&mut socket, chat).await, vec![uploaded]);
        let response = download(&running, &token, file).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"read me");
        assert!(file_list(&mut socket, other_chat).await.is_empty());
        let workspace = running
            .server_home()
            .join("computers")
            .join(other_computer.to_string())
            .join("workspace");
        assert!(
            !workspace.join("notes.txt").exists(),
            "a File of another Chat never reaches this Workspace"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_cannot_take_or_deliver_what_is_not_a_file_of_the_chat() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "write_file",
                json!({"path": "answer.txt", "content": "the answer"}),
            ),
            tool("take_file", json!({"file": Id::new()})),
            tool(
                "put_file",
                json!({"path": "answer.txt", "name": "../answer.txt"}),
            ),
            text("Nothing moved."),
        ])
        .await;
        let (_bot, chat, _computer) = tool_bot(&mut socket, base).await;

        let result = send(&mut socket, chat, "take a File that is not there").await;

        assert_eq!(result["status"], "succeeded");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        let actions: Vec<(&str, &str)> = shown["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|action| {
                (
                    action["tool"].as_str().unwrap(),
                    action["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            actions,
            [
                ("write_file", "succeeded"),
                ("take_file", "failed"),
                ("put_file", "failed"),
            ]
        );

        // Nothing the Bot could not move was recorded.
        assert!(file_list(&mut socket, chat).await.is_empty());
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
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
            let restarted = start(Home::open(dir.path()).unwrap(), 0).await.unwrap();
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
    fn a_bot_moves_files_through_s3() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            let (_dir, running, invite) = running_with_config(&s3_config(&storage)).await;
            let (mut socket, token) = admin(&running, &invite).await;
            let (_bot, chat, computer) = create_bot(&mut socket).await;

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
            let taken: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();
            let (base, provider_task) = scripted_provider(vec![
                tool("take_file", json!({"file": taken})),
                tool(
                    "write_file",
                    json!({"path": "out/answer.txt", "content": "the answer"}),
                ),
                tool(
                    "put_file",
                    json!({"path": "out/answer.txt", "name": "answer.txt"}),
                ),
                text("I read your notes and sent the answer back."),
            ])
            .await;
            command(&mut socket, Id::new(), provider_add(base)).await;
            let result = send_file(&mut socket, chat, taken).await;
            assert_eq!(result["status"], "succeeded");

            // The Workspace copy is the Bot's; the File the Chat got back is bytes
            // in S3, one object per content.
            let workspace = running
                .server_home()
                .join("computers")
                .join(computer.to_string())
                .join("workspace");
            assert_eq!(
                std::fs::read(workspace.join("notes.txt")).unwrap(),
                b"read me"
            );
            let files = file_list(&mut socket, chat).await;
            assert_eq!(files.len(), 2);
            let delivered: Id = serde_json::from_value(files[1]["id"].clone()).unwrap();
            let response = download(&running, &token, delivered).await;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"the answer");
            let mut keys: Vec<String> = test_s3::objects(&storage)
                .await
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            let mut expected: Vec<String> = [
                uploaded["sha256"].as_str().unwrap(),
                files[1]["sha256"].as_str().unwrap(),
            ]
            .iter()
            .map(|sha| format!("{}/{}", &sha[..2], sha))
            .collect();
            keys.sort();
            expected.sort();
            assert_eq!(keys, expected, "both Files are one object each in S3");

            socket.close(None).await.unwrap();
            provider_task.abort();
            running.shutdown().await;
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

        let Err(error) = start(Home::open(dir.path()).unwrap(), 0).await else {
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

    fn add_bot(name: &str, computer: Option<Id>) -> CommandCall {
        CommandCall::BotCreate(BotCreate {
            name: name.to_owned(),
            computer,
        })
    }

    /// Pairs a fake machine and approves it, returning its Computer and token.
    async fn paired_computer(socket: &mut Socket, running: &Running) -> (Id, String) {
        let (device, code) = pair_machine(running).await;
        let added = command(socket, Id::new(), add_computer(&code)).await;
        let computer: Id = serde_json::from_value(added["id"].clone()).unwrap();
        assert_eq!(added["kind"], "real");
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
        let mut request = format!("ws://{}/api/computer/socket", running.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();

        socket
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
        let managed = command(&mut socket, Id::new(), add_bot("Managed", None)).await;
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
        assert_eq!(added["kind"], "real");
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
        assert_eq!(views[0]["kind"], "managed");
        assert_eq!(views[0]["online"], true);
        assert_eq!(views[1]["id"], json!(computer));
        assert_eq!(views[1]["kind"], "real");
        assert_eq!(views[1]["online"], false);

        // The code is used once.
        let refused = try_command(&mut socket, Id::new(), add_computer(&code)).await;
        assert!(matches!(refused, Err((ErrorCode::InvalidArgument, _))));

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
        assert_eq!(added["kind"], "real");
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
        assert_eq!(added["kind"], "real");
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
            link.write(999, Id::new(), "big.bin", vec![7_u8; 16 * 1024 * 1024]),
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

        let mut outdated = raw_computer(&running, &machine).await;
        send_hello(
            &mut outdated,
            CHANNEL_VERSION + 1,
            &computer.to_string(),
            true,
        )
        .await;
        assert!(closed(&mut outdated).await);

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
        assert_eq!(shown_bot["computer"]["kind"], "real");
        assert_eq!(shown_bot["computer"]["online"], true);

        let queued = submit(&mut socket, chat, "run pwd", None).await;
        let run = run_of(&queued);
        let (id, named, action) = match next_host_frame(&mut device).await {
            HostFrame::Action { id, bot, action } => (id, bot, action),
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
        let (id, named, _action) = match next_host_frame(&mut device).await {
            HostFrame::Action { id, bot, action } => (id, bot, action),
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
        let managed = command(&mut socket, Id::new(), add_bot("Managed", None)).await;
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
        assert_eq!(created["computer"]["kind"], "real");
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
}
