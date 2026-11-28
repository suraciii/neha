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
use crate::catalog::CatalogStore;
use crate::commands;
use crate::computer_link::{self, AccessCapabilities, ComputerLinks};
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
/// How long a browser session cookie lasts: 400 days.
const SESSION_MAX_AGE_SECONDS: u64 = 400 * 24 * 60 * 60;
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
const COMPUTER_HELLO: Duration = if cfg!(any(test, feature = "test-support")) {
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
    /// The models.dev catalog and its cache.
    pub(crate) catalog: std::sync::Arc<CatalogStore>,
    pub(crate) model: ModelClient,
    pub(crate) events: broadcast::Sender<neha::record::Event>,
    pub(crate) tool_timeout: Duration,
    /// The live channel of each connected real Computer.
    pub(crate) links: ComputerLinks,
    /// Pairing attempts that are running or recently failed, per account.
    pub(crate) pairings: tokio::sync::Mutex<PairingAttempts>,
    /// Serializes approval answers and rule changes.
    pub(crate) approvals: tokio::sync::Mutex<()>,
    /// Serializes Provider writes, so a refresh and a default change cannot
    /// each build their Model list from a read the other replaced.
    pub(crate) provider_writes: tokio::sync::Mutex<()>,
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
    /// The authority the browser uses: the public origin or the local host and port.
    browser_authority: Mutex<Option<String>>,
    /// The public https origin a trusted TLS proxy forwards to this Space.
    public_url: Option<String>,
    /// The installed browser clients' push subscriptions.
    pub(crate) push: std::sync::Arc<crate::push::Push>,
}

impl SpaceServer {
    /// Opens a home and prepares its invitation when it has no account.
    pub async fn open(home: Home) -> Result<Self, ServerError> {
        Self::open_with(home, neha_computer::local::DEFAULT_TIMEOUT).await
    }

    async fn open_with(home: Home, tool_timeout: Duration) -> Result<Self, ServerError> {
        Self::open_with_ui(home, None, tool_timeout, None).await
    }

    /// Opens a home and serves the browser client from `ui`.
    async fn open_with_ui(
        home: Home,
        ui: Option<PathBuf>,
        tool_timeout: Duration,
        public_url: Option<String>,
    ) -> Result<Self, ServerError> {
        let session_cookie = session_cookie_name(home.root());
        let browser_host = public_url
            .as_deref()
            .and_then(|value| reqwest::Url::parse(value).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| browser_host(home.root()));
        let config = Config::load(&home)?;
        let storage = config.storage.clone();
        let catalog = std::sync::Arc::new(CatalogStore::open(&home, &config.catalog));
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
        let push_contact = public_url
            .clone()
            .unwrap_or_else(|| "mailto:neha@localhost".to_owned());
        let push = crate::push::Push::open(&home, &secrets, push_contact);
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
            catalog,
            model,
            events,
            tool_timeout,
            links: ComputerLinks::new(),
            pairings: tokio::sync::Mutex::new(PairingAttempts::default()),
            approvals: tokio::sync::Mutex::new(()),
            provider_writes: tokio::sync::Mutex::new(()),
            pairs: Mutex::new(PairLimiter::default()),
            start_ticket: Mutex::new(None),
            closed: AtomicBool::new(false),
            runs: Arc::new(RunQueue::new()),
            uploads: Mutex::new(std::collections::HashMap::new()),
            ui,
            session_cookie,
            browser_host,
            browser_authority: Mutex::new(None),
            public_url,
            push,
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
            .route("/api/push", get(push_status))
            .route(
                "/api/push/subscriptions",
                post(push_subscribe).delete(push_unsubscribe),
            )
            .route("/api/push/mute", post(push_mute))
            .route("/api/push/unmute", post(push_unmute))
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
    task: Option<JoinHandle<()>>,
    push_task: Option<JoinHandle<()>>,
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
        self.server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.browser_host, self.addr.port()))
    }

    /// Returns the public HTTPS origin, when the Space serves one.
    #[must_use]
    pub fn public_url(&self) -> Option<String> {
        self.server.public_url.clone()
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
    pub async fn shutdown(mut self) {
        self.server.close();
        self.server.runs.shutdown().await;
        if let Some(push) = self.push_task.take() {
            push.abort();
            let _ = push.await;
        }
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // A dropped Running stops its work even when the caller did not wait
        // for `shutdown`. The push task holds the server, so leaving it alive
        // would also keep the record's event sender open.
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(push) = &self.push_task {
            push.abort();
        }
    }
}

/// Binds a home on loopback and starts serving it.
pub async fn start(home: Home, port: u16) -> Result<Running, ServerError> {
    start_with(
        home,
        port,
        None,
        neha_computer::local::DEFAULT_TIMEOUT,
        public_url_from_env()?,
    )
    .await
}

/// Binds a home on loopback and starts serving it with a browser client.
pub async fn start_with_ui(
    home: Home,
    port: u16,
    ui: Option<PathBuf>,
) -> Result<Running, ServerError> {
    start_with_ui_public(home, port, ui, public_url_from_env()?).await
}

/// Starts a UI-serving Space with an explicit public origin.
///
/// The origin is a full `https://` origin. `--domain` builds one from a host;
/// tests pass an origin directly.
pub async fn start_with_ui_public(
    home: Home,
    port: u16,
    ui: Option<PathBuf>,
    public_url: Option<String>,
) -> Result<Running, ServerError> {
    start_with(
        home,
        port,
        ui,
        neha_computer::local::DEFAULT_TIMEOUT,
        normalize_public_url(public_url)?,
    )
    .await
}

/// Binds a home on loopback and starts serving it with a tool time limit.
pub async fn start_with_tool_timeout(
    home: Home,
    port: u16,
    tool_timeout: Duration,
) -> Result<Running, ServerError> {
    start_with(home, port, None, tool_timeout, public_url_from_env()?).await
}

async fn start_with(
    home: Home,
    port: u16,
    ui: Option<PathBuf>,
    tool_timeout: Duration,
    public_url: Option<String>,
) -> Result<Running, ServerError> {
    if let Some(dir) = &ui
        && !dir.join("index.html").is_file()
    {
        return Err(ServerError::Ui(dir.clone()));
    }
    let server = Arc::new(SpaceServer::open_with_ui(home, ui, tool_timeout, public_url).await?);
    validate_client_directory(&server).await?;
    // The port is bound before anything is written or woken: a start that
    // cannot listen must not make a run, reach a Provider, or record a failure.
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let browser_authority = match &server.public_url {
        Some(public) => public.strip_prefix("https://").unwrap_or(public).to_owned(),
        None => format!("{}:{}", server.browser_host, addr.port()),
    };
    *server
        .browser_authority
        .lock()
        .expect("browser authority lock") = Some(browser_authority);
    if let Ok(mut state) = server.state.lock() {
        state.url = Some(format!("http://{addr}"));
    }
    // Capture the cursor before recovery. A run that recovery fails during
    // startup is a RunFinished and belongs to this start's notification stream.
    let push_cursor = server.store.last_seq().await.map_err(ServerError::Store)?;
    recover_interrupted(&server.store, &server.state).await?;
    server.catalog.clone().spawn_background();
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
    // Start after recovery so the follower catches up through the ordered store
    // and can notify the runs recovery just failed. `wake_all` runs after this
    // spawn, and the task's catch-up covers the window before its subscription.
    let push_task = crate::push::spawn(Arc::clone(&server), push_cursor);
    server.runs.wake_all(&server);
    let invite_base = server
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{addr}"));
    let invite = server
        .invite_secret()
        .map(|secret| format!("{invite_base}/invite#{secret}"));
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
        task: Some(task),
        push_task: Some(push_task),
    })
}

async fn guard(State(server): State<Arc<SpaceServer>>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Some(host) = host.as_deref() else {
        return ApiError::forbidden("the Host header must name this Space").into_response();
    };
    let browser_host = is_browser_authority(&server, host);
    if !loopback_host(host) && !browser_host {
        return ApiError::forbidden("the Host header must name this Space").into_response();
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
        let same_site = same_authority(origin, host, server.browser_scheme());
        if (!same_site || !browser_host) && !bearer {
            return ApiError::forbidden("a browser Origin must name this Space").into_response();
        }
    }

    let mut response = next.run(request).await;
    add_security_headers(&mut response, &server, host);
    if path == "/healthz" || path == "/api" || path.starts_with("/api/") {
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
    }
    response
}

/// Adds the headers every response uses, and HSTS on a public origin.
fn add_security_headers(response: &mut Response, server: &SpaceServer, host: &str) {
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    if let Ok(value) = HeaderValue::from_str(&content_security_policy(server, host)) {
        headers.insert("content-security-policy", value);
    }
    if server.public_url.is_some() {
        headers.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000"),
        );
    }
}

/// Names the origin the client may load from and talk to.
fn content_security_policy(server: &SpaceServer, host: &str) -> String {
    let (http, ws) = match &server.public_url {
        Some(public) => (public.clone(), public.replacen("https://", "wss://", 1)),
        None => (format!("http://{host}"), format!("ws://{host}")),
    };
    format!(
        "default-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'; \
         form-action 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         font-src 'self'; manifest-src 'self'; worker-src 'self'; \
         connect-src 'self' {http} {ws}"
    )
}

fn same_authority(origin: &str, host: &str, scheme: &str) -> bool {
    let Some((origin_scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    // A local Space serves plain HTTP; a trusted TLS proxy presents https.
    // Accepting the other scheme would either claim transport security the
    // listener does not provide or break a real https page.
    if origin_scheme != scheme {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    canonical_authority(authority, scheme)
        .zip(canonical_authority(host, scheme))
        .is_some_and(|(origin, host)| origin == host)
}

/// Returns the host and non-default port of an authority, lowercased.
fn canonical_authority(value: &str, scheme: &str) -> Option<String> {
    let (host, port) = split_authority(value)?;
    let default = if scheme == "https" { 443 } else { 80 };
    let port = match port {
        None => None,
        Some(port) => {
            let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
            (port != default).then_some(port)
        }
    };
    let host = host.to_ascii_lowercase();
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host
    };
    Some(match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Returns whether `host` is the authority the browser uses for this Space.
fn is_browser_authority(server: &SpaceServer, host: &str) -> bool {
    let scheme = server.browser_scheme();
    match server.browser_authority() {
        Some(expected) => canonical_authority(&expected, scheme)
            .zip(canonical_authority(host, scheme))
            .is_some_and(|(expected, host)| expected == host),
        None => split_authority(host)
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case(&server.browser_host)),
    }
}

fn public_url_from_env() -> Result<Option<String>, ServerError> {
    let Some(value) = std::env::var_os("NEHA_PUBLIC_URL") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| ServerError::PublicUrl("the public URL is not UTF-8".to_owned()))?;
    normalize_public_url(Some(value.to_owned()))
}

/// Validates a public HTTPS origin and returns its canonical form.
///
/// `--domain` builds `https://<domain>`; `NEHA_PUBLIC_URL` supplies a full
/// origin. Both end here, so the Space stores one canonical origin.
fn normalize_public_url(public_url: Option<String>) -> Result<Option<String>, ServerError> {
    let Some(value) = public_url else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(&value)
        .map_err(|error| ServerError::PublicUrl(format!("{value}: {error}")))?;
    if url.scheme() != "https" {
        return Err(ServerError::PublicUrl(
            "the public URL must use https".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ServerError::PublicUrl(
            "the public URL must not carry credentials".to_owned(),
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(ServerError::PublicUrl(
            "the public URL must be an origin with no path, query, or fragment".to_owned(),
        ));
    }
    Ok(Some(url.origin().ascii_serialization()))
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

/// Returns the client address, trusting a forwarded one only from loopback.
///
/// A TLS proxy on the Space's machine connects from loopback and appends the
/// client to `X-Forwarded-For`; a proxy on another machine does not get to
/// name the client. Only the final appended value counts.
fn client_ip(peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.is_loopback() {
        return peer;
    }
    // Select the final raw field first. A malformed field must fail closed to
    // the peer, not expose an earlier client-supplied field.
    let Some(value) = headers.get_all("x-forwarded-for").iter().next_back() else {
        return peer;
    };
    let Some(value) = value.to_str().ok() else {
        return peer;
    };
    forwarded_ip(value)
        .filter(|address| !address.is_loopback())
        .unwrap_or(peer)
}

/// Returns the final address in one forwarded field, or `None` when it is not
/// an address. Earlier values are the client's claim, not the proxy's.
fn forwarded_ip(value: &str) -> Option<IpAddr> {
    let last = value.split(',').next_back()?.trim().trim_matches('"');
    let last = last
        .strip_prefix('[')
        .and_then(|last| last.strip_suffix(']'))
        .unwrap_or(last);
    last.parse().ok()
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
    if !host.is_some_and(|host| is_browser_authority(&server, host)) {
        let port = host
            .and_then(split_authority)
            .and_then(|(_, port)| port)
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        let base = server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}{}", server.browser_host, port));
        let location = format!("{}{}", base.trim_end_matches('/'), uri.path());
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
    let cache = cache_control(&root, &target);
    let mut response = ([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response();
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static(cache));
    response
}

/// Returns how long a browser may keep a served client file.
fn cache_control(root: &Path, target: &Path) -> &'static str {
    let relative = target.strip_prefix(root).unwrap_or(target);
    if relative.starts_with("assets") {
        // Vite names every built asset after its content.
        "public, max-age=31536000, immutable"
    } else if relative.starts_with("fonts") {
        "public, max-age=604800"
    } else if relative.starts_with("icons") {
        "public, max-age=86400"
    } else {
        // The entry point, the worker, and the manifest name the next build.
        "no-cache"
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") | Some("map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
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
    account: Account,
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
    let source = client_ip(source.ip(), &headers);
    if !server
        .login
        .lock()
        .expect("login lock")
        .allows(&request.display_name, source)
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
                .record(&request.display_name, source);
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
    /// Returns the authority the browser uses for this Space.
    fn browser_authority(&self) -> Option<String> {
        self.browser_authority
            .lock()
            .ok()
            .and_then(|authority| authority.clone())
    }

    fn browser_scheme(&self) -> &'static str {
        if self.public_url.is_some() {
            "https"
        } else {
            "http"
        }
    }

    fn session_headers(&self, token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let secure = if self.public_url.is_some() {
            "; Secure"
        } else {
            ""
        };
        let cookie = format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_MAX_AGE_SECONDS}{secure}",
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
    let host = headers.get(HOST).and_then(|value| value.to_str().ok());
    origin.is_some_and(|origin| {
        host.is_some_and(|host| {
            is_browser_authority(server, host)
                && same_authority(origin, host, server.browser_scheme())
        })
    })
}

async fn me(Authed { account }: Authed) -> Json<Account> {
    Json(account)
}

#[derive(Serialize)]
struct PushStatusBody {
    key: String,
    muted_until: Option<u64>,
}

#[derive(Deserialize)]
struct PushSubscribeBody {
    endpoint: String,
    keys: PushSubscribeKeys,
}

#[derive(Deserialize)]
struct PushSubscribeKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct PushUnsubscribeBody {
    endpoint: String,
}

#[derive(Deserialize)]
struct PushMuteBody {
    minutes: u64,
}

/// Returns the VAPID public key and the Space's mute, when push is configured.
async fn push_status(
    State(server): State<Arc<SpaceServer>>,
    Authed { account: _ }: Authed,
) -> Result<Json<PushStatusBody>, ApiError> {
    let status = server
        .push
        .status()
        .ok_or_else(|| ApiError::unavailable("notifications are not configured"))?;

    Ok(Json(PushStatusBody {
        key: status.key,
        muted_until: status.muted_until,
    }))
}

/// Stores one browser's subscription for the signed-in account.
async fn push_subscribe(
    State(server): State<Arc<SpaceServer>>,
    Authed { account }: Authed,
    Json(body): Json<PushSubscribeBody>,
) -> Result<Json<Value>, ApiError> {
    server
        .push
        .subscribe(account.id, body.endpoint, body.keys.p256dh, body.keys.auth)
        .map_err(push_error)?;

    Ok(Json(serde_json::json!({"subscribed": true})))
}

/// Forgets one browser's subscription for the signed-in account.
async fn push_unsubscribe(
    State(server): State<Arc<SpaceServer>>,
    Authed { account }: Authed,
    Json(body): Json<PushUnsubscribeBody>,
) -> Result<Json<Value>, ApiError> {
    server
        .push
        .unsubscribe(account.id, &body.endpoint)
        .map_err(push_error)?;

    Ok(Json(serde_json::json!({"subscribed": false})))
}

/// Mutes the whole Space for a number of minutes.
async fn push_mute(
    State(server): State<Arc<SpaceServer>>,
    Authed { account: _ }: Authed,
    Json(body): Json<PushMuteBody>,
) -> Result<Json<Value>, ApiError> {
    let until = server.push.mute(body.minutes).map_err(push_error)?;

    Ok(Json(serde_json::json!({"muted_until": until})))
}

/// Ends a mute.
async fn push_unmute(
    State(server): State<Arc<SpaceServer>>,
    Authed { account: _ }: Authed,
) -> Result<Json<Value>, ApiError> {
    server.push.unmute().map_err(push_error)?;

    Ok(Json(serde_json::json!({"muted_until": null})))
}

fn push_error(error: crate::push::SubscribeError) -> ApiError {
    match error {
        crate::push::SubscribeError::Invalid(message) => ApiError::invalid_argument(message),
        crate::push::SubscribeError::TooMany => ApiError::conflict(error.message()),
        crate::push::SubscribeError::Store => ApiError::internal(error.message()),
    }
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

/// Whether the Space operator explicitly accepted an unsandboxed Local
/// Computer. The Windows desktop sets this for its bundled Space; a remote
/// Space operator must set it deliberately.
fn unsandboxed_computers_allowed() -> bool {
    std::env::var("NEHA_ALLOW_UNSANDBOXED_COMPUTERS")
        .ok()
        .is_some_and(|value| allows_unsandboxed(Some(value.as_str())))
}

fn allows_unsandboxed(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Serves one Computer's channel until it goes away.
///
/// The first frame must be a `Hello` naming the Computer the token belongs to,
/// speaking this Space's version, and stating that the machine fences its
/// Bots. The channel then registers, the run queue wakes, and frames flow both
/// ways until the connection drops, the Computer stays silent for too long, or
/// the Computer breaks the protocol.
/// Returns the access a Computer's greeting declares.
///
/// A greeting written before the capability fields takes them from its
/// `sandbox` flag: a sandboxed machine had the access model, and an
/// unsandboxed machine (an opted-in Windows daemon) had none of it. The
/// explicit fields win when a newer daemon sends them.
fn hello_access(
    sandbox: bool,
    supports_grants: Option<bool>,
    supports_full_access: Option<bool>,
) -> AccessCapabilities {
    AccessCapabilities {
        grants: supports_grants.unwrap_or(sandbox),
        full_access: supports_full_access.unwrap_or(sandbox),
        sandboxed: sandbox,
    }
}

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
                    supports_grants,
                    supports_full_access,
                }) => Some((
                    version,
                    named,
                    sandbox,
                    hello_access(sandbox, supports_grants, supports_full_access),
                )),
                _ => None,
            }
        }
        _ => None,
    };
    // A daemon that cannot fence its machine never runs unless the Space
    // operator explicitly opted in to unsandboxed Local Computers. A connection
    // that names another Computer would act as that Computer: both are refused.
    let Some((version, named, sandbox, access)) = hello else {
        return;
    };
    if version != CHANNEL_VERSION
        || named != computer.to_string()
        || (!sandbox && !unsandboxed_computers_allowed())
    {
        eprintln!(
            "computer handshake rejected: version={version} named={named} sandbox={sandbox} unsandboxed_allowed={}",
            unsandboxed_computers_allowed()
        );
        return;
    }

    let (sink, mut stream) = socket.split();
    let (frames, mut outgoing) = mpsc::channel::<HostFrame>(computer_link::FRAME_QUEUE);
    let link = server.links.connect(computer, frames.clone(), access);
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
                            provider_writes: &server.provider_writes,
                            runs: &server.runs,
                            events: &server.events,
                            catalog: &server.catalog,
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
/// to the User a Bot wrote it for, or to the Space itself. A Space-wide event
/// is one every account can already read through `provider list` or
/// `model list`: the Provider's address and its Models are not per-account
/// secrets, so a settings page open in another account follows them too.
fn visible_to(account: Id, event: &neha::record::Event) -> bool {
    if matches!(event.author, neha::record::Author::User(author) if author == account) {
        return true;
    }
    if event.acting == Some(account) {
        return true;
    }

    matches!(
        event.kind,
        neha::record::EventKind::UserJoined
            | neha::record::EventKind::ProviderAdded
            | neha::record::EventKind::ProviderModelsUpdated
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
                    .is_some_and(|host| is_browser_authority(server, host));
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

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            message,
        )
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
    /// The public URL for a Space behind a trusted TLS proxy is invalid.
    #[error("public URL error: {0}")]
    PublicUrl(String),
}

#[cfg(test)]
mod tests {
    use neha::id::Id;

    use crate::computer_link::AccessCapabilities;

    #[test]
    fn a_greeting_written_before_access_fields_derives_them_from_sandbox() {
        assert_eq!(
            super::hello_access(false, None, None),
            AccessCapabilities {
                grants: false,
                full_access: false,
                sandboxed: false,
            }
        );
        assert_eq!(
            super::hello_access(true, None, None),
            AccessCapabilities::SUPPORTED
        );
        // A newer greeting declares each capability itself.
        assert_eq!(
            super::hello_access(true, Some(false), Some(true)),
            AccessCapabilities {
                grants: false,
                full_access: true,
                sandboxed: true,
            }
        );
        // An unsandboxed greeting says so, whatever its capabilities claim.
        assert_eq!(
            super::hello_access(false, Some(false), Some(false)),
            AccessCapabilities {
                grants: false,
                full_access: false,
                sandboxed: false,
            }
        );
    }

    #[test]
    fn unsandboxed_local_computers_need_an_explicit_opt_in() {
        assert!(!super::allows_unsandboxed(None));
        assert!(!super::allows_unsandboxed(Some("0")));
        assert!(!super::allows_unsandboxed(Some("yes")));
        assert!(super::allows_unsandboxed(Some("1")));
        assert!(super::allows_unsandboxed(Some("TRUE")));
    }

    #[test]
    fn a_public_origin_keeps_an_explicit_port() {
        assert_eq!(
            super::normalize_public_url(Some("https://space.example.com:443/".to_owned()))
                .unwrap()
                .unwrap(),
            "https://space.example.com"
        );
        assert_eq!(
            super::normalize_public_url(Some("https://space.example.com:8443".to_owned()))
                .unwrap()
                .unwrap(),
            "https://space.example.com:8443"
        );
        assert!(super::normalize_public_url(Some("http://space.example.com".to_owned())).is_err());
        assert!(
            super::normalize_public_url(Some("https://space.example.com/path".to_owned())).is_err()
        );
        assert!(
            super::normalize_public_url(Some("https://user@space.example.com".to_owned())).is_err()
        );
        assert!(super::normalize_public_url(None).unwrap().is_none());
    }

    #[test]
    fn an_authority_omits_only_its_default_port() {
        assert!(super::same_authority(
            "https://space.example.com",
            "space.example.com:443",
            "https"
        ));
        assert!(super::same_authority(
            "https://space.example.com:8443",
            "space.example.com:8443",
            "https"
        ));
        assert!(super::same_authority(
            "https://space.example.com",
            "SPACE.EXAMPLE.COM",
            "https"
        ));
        assert!(super::same_authority(
            "http://neha-abc.localhost:7337",
            "neha-abc.localhost:7337",
            "http"
        ));
        assert!(!super::same_authority(
            "https://space.example.com",
            "space.example.com:8443",
            "https"
        ));
        assert!(!super::same_authority(
            "https://space.example.com",
            "space.example.com",
            "http"
        ));
        assert!(!super::same_authority(
            "https://space.example.com:notaport",
            "space.example.com",
            "https"
        ));
        assert!(!super::same_authority(
            "https://space.example.com",
            "127.0.0.1:443",
            "https"
        ));
    }

    #[test]
    fn a_forwarded_address_is_used_only_from_a_loopback_proxy() {
        use axum::http::{HeaderMap, HeaderValue};

        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
            headers
        };
        let proxy = std::net::IpAddr::from([127, 0, 0, 1]);
        let client = std::net::IpAddr::from([203, 0, 113, 7]);
        let remote = std::net::IpAddr::from([192, 0, 2, 1]);
        assert_eq!(
            super::client_ip(proxy, &headers("10.0.0.4, 203.0.113.7")),
            client
        );
        assert_eq!(super::client_ip(remote, &headers("203.0.113.7")), remote);
        assert_eq!(super::client_ip(proxy, &headers("garbage")), proxy);
        assert_eq!(
            super::client_ip(proxy, &headers("203.0.113.7, 127.0.0.1")),
            proxy
        );
        assert_eq!(super::forwarded_ip("10.0.0.4, 203.0.113.7"), Some(client));
        assert_eq!(super::forwarded_ip("203.0.113.7, garbage"), None);
        assert_eq!(super::forwarded_ip("127.0.0.1"), Some(proxy));

        let mut fields = HeaderMap::new();
        fields.append("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
        fields.append("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        assert_eq!(super::client_ip(proxy, &fields), client);

        let mut invalid = HeaderMap::new();
        invalid.append("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        invalid.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(&[0xFF]).expect("obs-text is a header value"),
        );
        assert_eq!(super::client_ip(proxy, &invalid), proxy);
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
}

/// What the integration tests need from inside this module.
///
/// Integration tests live in `tests/` and see only the crate's public
/// interface. The fixtures and accessors for the internals the tests inspect
/// live here, behind the `test-support` feature, so a production build keeps
/// them out.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::{ApiError, PairingAttempts, Running, StreamError};
    use crate::catalog::CatalogStore;
    use crate::computer_link::ComputerLinks;
    use crate::home::{Home, HomeError};
    use crate::identity::Identity;
    use crate::runs::RunQueue;
    use crate::secrets::Secrets;
    use crate::state::State;
    use crate::store::{Store, StoreReader};

    /// The longest machine name or workspace a pairing request may carry.
    pub const MAX_PAIRING_FIELD_BYTES: usize = super::MAX_PAIRING_FIELD_BYTES;

    /// Why sending a reader's recorded pages stopped, as the socket reports it.
    #[derive(Debug)]
    pub enum CatchUpError {
        /// The record could not be read.
        Store,
        /// The page did not fit one frame.
        Oversized(neha::seq::Seq),
        /// The socket closed.
        Closed,
    }

    /// The private state of a running Space, for tests that inspect it.
    pub struct Fields<'a> {
        /// The Space's home directory.
        pub home: &'a Home,
        /// The record.
        pub store: &'a Store,
        /// The in-process projection.
        pub state: &'a std::sync::Mutex<State>,
        /// The Provider keys.
        pub secrets: &'a Secrets,
        /// Accounts, device tokens, and pairings.
        pub identity: &'a Identity,
        /// The live Computer channels.
        pub links: &'a ComputerLinks,
        /// The Space's pairing attempts.
        pub pairings: &'a tokio::sync::Mutex<PairingAttempts>,
        /// Serializes approval answers and rule changes.
        pub approvals: &'a tokio::sync::Mutex<()>,
        /// Serializes Provider writes.
        pub provider_writes: &'a tokio::sync::Mutex<()>,
        /// The run queue.
        pub runs: &'a RunQueue,
        /// The live event stream.
        pub events: &'a tokio::sync::broadcast::Sender<neha::record::Event>,
        /// The models.dev catalog and its cache.
        pub catalog: &'a Arc<CatalogStore>,
    }

    impl Running {
        /// Returns the private state a test inspects without moving the Space.
        pub fn test_fields(&self) -> Fields<'_> {
            let server = &self.server;

            Fields {
                home: &server.home,
                store: &server.store,
                state: &server.state,
                secrets: &server.secrets,
                identity: &server.identity,
                links: &server.links,
                pairings: &server.pairings,
                approvals: &server.approvals,
                provider_writes: &server.provider_writes,
                runs: &server.runs,
                events: &server.events,
                catalog: &server.catalog,
            }
        }
    }

    impl ApiError {
        /// The error code the API answered with.
        pub fn test_code(&self) -> neha::error::ErrorCode {
            self.code
        }

        /// The message the API answered with.
        pub fn test_message(&self) -> &str {
            &self.message
        }
    }

    /// Sends every recorded page that fits a frame, as the socket would.
    pub async fn catch_up(
        reader: &StoreReader,
        out: &tokio::sync::mpsc::Sender<neha::surface::ServerFrame>,
        cursor: neha::seq::Seq,
        limit: Option<usize>,
        account: neha::id::Id,
    ) -> Result<neha::seq::Seq, CatchUpError> {
        super::catch_up(reader, out, cursor, limit, account)
            .await
            .map_err(|error| match error {
                StreamError::Store => CatchUpError::Store,
                StreamError::Oversized(cursor) => CatchUpError::Oversized(cursor),
                StreamError::Closed => CatchUpError::Closed,
            })
    }

    /// Stores or updates one browser push subscription.
    pub fn subscribe_push(
        running: &Running,
        account: neha::id::Id,
        endpoint: String,
        p256dh: String,
        auth: String,
    ) -> Result<(), String> {
        running
            .server
            .push
            .subscribe(account, endpoint, p256dh, auth)
            .map_err(|error| error.message().to_owned())
    }

    /// Mutes this Space's push notifications for a while.
    pub fn mute_push(running: &Running, minutes: u64) -> Result<u64, String> {
        running
            .server
            .push
            .mute(minutes)
            .map_err(|error| error.message().to_owned())
    }

    /// Opens a Home whose ticket file stays inside the test's own directory.
    pub fn open_home_with_ticket_directory(
        root: impl Into<PathBuf>,
        directory: impl Into<PathBuf>,
    ) -> Result<Home, HomeError> {
        Home::open_with_ticket_directory(root, directory)
    }

    /// Installs a pause for one Bot's next run start; returns (reached, release).
    pub fn pause_start_for_test(bot: neha::id::Id) -> (Arc<Notify>, Arc<Notify>) {
        crate::runs::pause_start_for_test(bot)
    }

    /// Returns how many callers wait for one Bot's admit lock.
    pub fn admit_waiters(bot: neha::id::Id) -> usize {
        crate::runs::admit_waiters(bot)
    }
}
