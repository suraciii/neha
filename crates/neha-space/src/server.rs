//! The Space's HTTP and WebSocket server.
//!
//! A client joins with the Space's invitation and password, then reaches
//! every capability through one WebSocket and sends and receives Files over
//! HTTP. The server listens on loopback until a remote Space is configured.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::blobs::{Blobs, BlobsError};
use crate::commands;
use crate::config::{Config, ConfigError, Storage};
use crate::home::Home;
use crate::identity::{Account, Identity, IdentityError};
use crate::model::{ModelClient, ModelError};
use crate::secrets::{Secrets, SecretsError};
use crate::state::State as SpaceState;
use crate::store::{AppendRequest, Projection, Store, StoreError, StoreReader};

const LOGIN_ATTEMPTS: usize = 5;
const SOURCE_ATTEMPTS: usize = 20;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LOGIN_NAMES: usize = 1024;
const HEARTBEAT: Duration = Duration::from_secs(30);

/// A Space ready to answer clients.
pub struct SpaceServer {
    home: Home,
    store: Store,
    identity: Identity,
    blobs: Blobs,
    storage: Storage,
    invite: Mutex<Option<Invite>>,
    login: Mutex<LoginLimiter>,
    state: Mutex<SpaceState>,
    secrets: Secrets,
    model: ModelClient,
    events: broadcast::Sender<neha::record::Event>,
    tool_timeout: Duration,
    chat_runs: Mutex<std::collections::HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
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
        recover_interrupted(&store, &mut state).await?;
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
            chat_runs: Mutex::new(std::collections::HashMap::new()),
            uploads: Mutex::new(std::collections::HashMap::new()),
            ui,
            session_cookie,
            browser_host,
        })
    }

    /// Returns the lock that serializes runs in one Chat.
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

    fn chat_lock(&self, chat: Id) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.chat_runs.lock().expect("chat lock");
        Arc::clone(
            locks
                .entry(chat)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
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
    pub async fn shutdown(self) {
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
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
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
                        if send(&out, ServerFrame::Accepted { id }).await.is_err() {
                            break;
                        }
                        let _run_guard = match &command {
                            CommandCall::MessageSend(message) => {
                                Some(server.chat_lock(message.chat).lock_owned().await)
                            }
                            _ => None,
                        };
                        let outcome = commands::execute(&server.store, &server.state, &server.home, &server.secrets, &account, id, command).await;
                        let result = match outcome {
                            Ok(outcome) => {
                                broadcast(&server, &outcome.events);
                                if let Some(seq) = outcome.committed
                                    && send(&out, ServerFrame::Committed { id, seq }).await.is_err()
                                {
                                    break;
                                }
                                match outcome.pending {
                                    Some(pending) => {
                                        match commands::finish_run(&server.store, &server.state, &server.home, &server.blobs, &server.model, &server.events, server.tool_timeout, pending).await {
                                            Ok((result, events)) => {
                                                broadcast(&server, &events);
                                                result
                                            }
                                            Err(error) => {
                                                let _ = send(&out, ServerFrame::Error { id: Some(id), code: error.code(), message: error.to_string() }).await;
                                                continue;
                                            }
                                        }
                                    }
                                    None => outcome.result,
                                }
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

/// Finishes runs the previous Space run left working.
///
/// A one-shot run is answered or failed; it is never silently dropped. The
/// Bot says the work was interrupted and the command that started the run
/// receives that failure as its result.
async fn recover_interrupted(store: &Store, state: &mut SpaceState) -> Result<(), StoreError> {
    let interrupted: Vec<_> = state
        .runs
        .values()
        .filter(|run| run.status == crate::state::RunStatus::Running)
        .cloned()
        .collect();
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

fn identity_error(error: IdentityError) -> ApiError {
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

    #[derive(Clone)]
    enum ScriptedReply {
        Text(String),
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

    async fn scripted_provider(
        script: Vec<ScriptedReply>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let script = Arc::new(script);
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let script = Arc::clone(&script);
                let requests = Arc::clone(&requests);
                async move {
                    let index = requests.fetch_add(1, Ordering::SeqCst);
                    let reply = script
                        .get(index)
                        .cloned()
                        .or_else(|| script.last().cloned())
                        .expect("a scripted reply");
                    let body = match reply {
                        ScriptedReply::Text(text) => {
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

        (format!("http://{addr}/v1"), task)
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
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let bot_id: Id = serde_json::from_value(bot["bot"]["id"].clone()).unwrap();
        assert_eq!(bot["computer"]["kind"], "managed");

        let sent = command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,
            }),
        )
        .await;
        assert_eq!(sent["status"], "succeeded");
        assert_eq!(sent["text"], "Hello from fake");
        let run: Id = serde_json::from_value(sent["run"].clone()).unwrap();

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

        let server = SpaceServer::open(home).await.unwrap();
        {
            let state = server.state.lock().unwrap();
            let recovered = &state.runs[&run];

            assert_eq!(recovered.status, crate::state::RunStatus::Failed);
            assert_eq!(recovered.usage.unwrap().budgeted(), 7);
            assert_eq!(recovered.reply, Some(state.messages.last().unwrap().id));
            assert_eq!(
                state.messages.last().unwrap().text,
                "I was interrupted before I could answer."
            );
        }
        let recorded = server.store.command(command).await.unwrap().unwrap();
        assert_eq!(recorded.result.unwrap()["status"], "failed");
        server.store.close().await.unwrap();
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
            }),
        )
        .await;
        let conflict = try_command(
            &mut socket,
            command_id,
            CommandCall::BotCreate(BotCreate {
                name: "Other".to_owned(),
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
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,
            }),
        )
        .await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "again".to_owned(),
                file: None,
            }),
        )
        .await;

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
        let second = command(&mut socket, send, message).await;

        assert_eq!(first, second);
        assert_eq!(first["status"], "succeeded");
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        let run: Id = serde_json::from_value(first["run"].clone()).unwrap();
        let shown = command(&mut socket, Id::new(), CommandCall::RunShow { run }).await;
        assert_eq!(shown["status"], "succeeded");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_while_the_run_works_waits_for_the_first_result() {
        let (_dir, running, invite) = running().await;
        let (mut first_socket, token) = admin(&running, &invite).await;
        let (base, provider_task) = slow_provider().await;
        command(&mut first_socket, Id::new(), provider_base(base)).await;
        let bot = command(
            &mut first_socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let command_id = Id::new();
        first_socket
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
        let first_frame = next_frame(&mut first_socket).await;
        let second_frame = next_frame(&mut first_socket).await;
        assert!(matches!(first_frame, ServerFrame::Accepted { .. }));
        assert!(matches!(second_frame, ServerFrame::Committed { .. }));

        let mut second_socket = open_socket(&running, &token).await;
        let result = command(
            &mut second_socket,
            command_id,
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,
            }),
        )
        .await;
        assert_eq!(result["status"], "succeeded");

        loop {
            match next_frame(&mut first_socket).await {
                ServerFrame::Confirmed { .. } => break,
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
        let messages = command(
            &mut second_socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);
        first_socket.close(None).await.unwrap();
        second_socket.close(None).await.unwrap();
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
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();

        let result = command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "hello".to_owned(),
                file: None,
            }),
        )
        .await;

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
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = command(&mut socket, Id::new(), CommandCall::RunShow { run }).await;
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

    async fn slow_provider() -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::post;

        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
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
            }),
        )
        .await;
        let bot_id: Id = serde_json::from_value(bot["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        let computer: Id = serde_json::from_value(bot["computer"]["id"].clone()).unwrap();

        (bot_id, chat, computer)
    }

    async fn send(socket: &mut Socket, chat: Id, text: &str) -> Value {
        command(
            socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: text.to_owned(),
                file: None,
            }),
        )
        .await
    }

    async fn show_run(socket: &mut Socket, run: Id) -> Value {
        command(socket, Id::new(), CommandCall::RunShow { run }).await
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
    async fn two_clients_cannot_run_one_chat_at_once() {
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

        let messages = command(
            &mut first,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let messages = messages.as_array().unwrap();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(messages[0]["author"]["kind"], "user");
        assert_eq!(messages[1]["author"]["kind"], "bot");
        assert_eq!(messages[2]["author"]["kind"], "user");
        assert_eq!(messages[3]["author"]["kind"], "bot");
        assert_ne!(messages[1]["text"], messages[3]["text"]);
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        provider_task.abort();
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
        let sent = command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: String::new(),
                file: Some(file),
            }),
        )
        .await;
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

        let result = command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: String::new(),
                file: Some(taken),
            }),
        )
        .await;
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
            let result = command(
                &mut socket,
                Id::new(),
                CommandCall::MessageSend(MessageSend {
                    chat,
                    text: String::new(),
                    file: Some(taken),
                }),
            )
            .await;
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
}
