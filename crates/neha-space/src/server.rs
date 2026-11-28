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
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, HOST, ORIGIN,
    SET_COOKIE,
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
use neha::record::{Author, EventDraft, EventKind, McpScope};
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

/// How long a browser sign-in stays usable, in seconds.
///
/// The browser flow is a person's walk through an authorization server; ten
/// minutes covers it without leaving a state token acceptable for long.
pub(crate) const SIGN_IN_TTL: Duration = Duration::from_secs(600);

/// The fixed explanation a sign-in the person cancelled or the authority
/// refused reports.
pub(crate) const SIGN_IN_CANCELLED: &str = "the sign-in was cancelled or refused";

/// The fixed explanation a sign-in whose exchange did not finish reports.
pub(crate) const SIGN_IN_INCOMPLETE: &str = "the sign-in could not be completed";

/// The fixed explanation an unknown or expired sign-in reports.
pub(crate) const SIGN_IN_EXPIRED: &str = "the sign-in expired or is unknown";

/// The fixed explanation a browser sign-in a newer one replaced reports.
///
/// The Space keeps one pending browser flow per account: a second start
/// supersedes the first, whose callback then finds its state refused and whose
/// poll reads this sentence.
pub(crate) const SIGN_IN_REPLACED: &str = "a newer sign-in replaced this one";

/// The prefix of the cookie that binds a browser flow to its browser.
///
/// The sign-in's id is appended to the name (see [`sign_in_cookie_name`]), so
/// one flow's binding is only ever written or cleared by that flow's own
/// steps.
const SIGN_IN_COOKIE_PREFIX: &str = "neha_signin";

/// The most browser sign-ins one Space keeps at once.
pub(crate) const MAX_PENDING_SIGN_INS: usize = 64;

/// A browser sign-in the Space is waiting on.
pub(crate) struct PendingSignIn {
    /// The account that started it, and the only account that may read it.
    pub account: Id,
    /// An existing server it attaches a credential to; `None` registers a new
    /// one when it finishes.
    pub server: Option<Id>,
    /// The name a new server takes.
    pub name: Option<String>,
    /// The address the sign-in speaks to.
    pub url: String,
    /// Where the credential belongs.
    pub scope: McpScope,
    /// The browser flow the OAuth client planned.
    pub plan: crate::oauth::SignInPlan,
    /// The random value the initiating browser's cookie must return.
    pub nonce: String,
    /// Whether a command line owns the redirect instead of the browser
    /// callback.
    pub cli: bool,
    /// When the client asked for the sign-in.
    pub created_at: Instant,
    /// What the client polls.
    pub status: SignInStatus,
}

/// What became of a browser sign-in.
pub(crate) enum SignInStatus {
    /// The browser has not answered yet.
    Pending,
    /// A callback or command line finish admitted the flow and is exchanging
    /// its code; the sign-ins lock is not held while the token endpoint runs.
    Executing,
    /// The credential is in the Vault and the record holds the server.
    Finished {
        /// The connected server.
        server: Id,
    },
    /// The sign-in did not finish.
    Failed {
        /// A fixed explanation the client may show.
        reason: String,
    },
}

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
    /// Serializes changes to the skills of both libraries.
    pub(crate) library: tokio::sync::Mutex<()>,
    /// Whether this Space treats skills as instructions only.
    pub(crate) instructions_only: bool,
    /// Where this Space may install a package from.
    pub(crate) plugins: crate::config::PluginsConfig,
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
    /// The public https origin a trusted TLS proxy forwards to this Space.
    public_url: Option<String>,
    /// The browser sign-ins the Space is waiting on, by sign-in.
    ///
    /// Lock order: a sign-in's token request runs outside every lock, but its
    /// commit holds this mutex across the admission re-check and the Vault and
    /// record writes, taking the library lock inside it. `mcp_sign_in_start`
    /// holds [`signin_start`](Self::signin_start) and then this mutex and never
    /// the library, and no path takes the library before this mutex.
    pub(crate) signins: tokio::sync::Mutex<HashMap<Id, PendingSignIn>>,
    /// Serializes the start of one sign-in: a redelivered command waits for
    /// the delivery already planning, then reads the recorded answer instead
    /// of planning a second flow and a second external registration.
    pub(crate) signin_start: tokio::sync::Mutex<()>,
    /// The OAuth client that plans and finishes a browser sign-in.
    pub(crate) oauth: crate::oauth::OAuthClient,
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
        let instructions_only = config.skills.instructions_only;
        let plugins = config.plugins.clone();
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
            library: tokio::sync::Mutex::new(()),
            instructions_only,
            plugins,
            pairs: Mutex::new(PairLimiter::default()),
            start_ticket: Mutex::new(None),
            closed: AtomicBool::new(false),
            runs: Arc::new(RunQueue::new()),
            uploads: Mutex::new(std::collections::HashMap::new()),
            ui,
            session_cookie,
            browser_host,
            public_url,
            signins: tokio::sync::Mutex::new(HashMap::new()),
            signin_start: tokio::sync::Mutex::new(()),
            oauth: crate::oauth::OAuthClient::new(),
        })
    }

    /// Returns the origin the browser client uses for this Space.
    ///
    /// A trusted TLS proxy's origin when configured, otherwise the Space's
    /// stable browser hostname on the loopback port it bound. An authorization
    /// server sends the browser to exactly this origin, so it must be the one
    /// the Host guard and the session cookie name.
    pub(crate) fn browser_origin(&self) -> String {
        if let Some(public_url) = &self.public_url {
            return public_url.clone();
        }
        let port = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.url.clone())
            .and_then(|url| reqwest::Url::parse(&url).ok())
            .and_then(|url| url.port());
        match port {
            Some(port) => format!("http://{}:{port}", self.browser_host),
            None => format!("http://{}", self.browser_host),
        }
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
            .route("/api/mcp/signin/bind", get(mcp_sign_in_bind))
            .route("/api/mcp/signin/{key}/callback", get(mcp_sign_in_callback))
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
        self.server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.browser_host, self.addr.port()))
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

/// Starts a UI-serving Space with an explicit public origin. Tests use this;
/// `start_with_ui` reads `NEHA_PUBLIC_URL`.
pub(crate) async fn start_with_ui_public(
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
        public_url,
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
    if let Ok(mut state) = server.state.lock() {
        state.url = Some(format!("http://{addr}"));
    }
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
        let same_site =
            host.is_some_and(|host| same_authority(origin, host, server.browser_scheme()));
        if (!same_site || !browser_host) && !bearer {
            return ApiError::forbidden("a browser Origin must name this Space").into_response();
        }
    }

    next.run(request).await
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
    authority.eq_ignore_ascii_case(host)
}

fn public_url_from_env() -> Result<Option<String>, ServerError> {
    let Some(value) = std::env::var_os("NEHA_PUBLIC_URL") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| ServerError::PublicUrl("the public URL is not UTF-8".to_owned()))?;
    let url = reqwest::Url::parse(value)
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

/// The query the bind route carries.
#[derive(Deserialize)]
struct SignInBindQuery {
    /// The sign-in the start answered with.
    sign_in: Id,
}

/// The query an authorization server returns the browser with.
#[derive(Deserialize)]
struct SignInCallbackQuery {
    /// The authorization code, when the person approved.
    #[serde(default)]
    code: Option<String>,
    /// The state token the plan named.
    #[serde(default)]
    state: Option<String>,
    /// The authorization server's error, when it refused or failed. It is
    /// never rendered or polled.
    #[serde(default)]
    error: Option<String>,
    /// The issuer the authorization server named, when it names one here
    /// (RFC 9207): the plan's issuer checked byte for byte.
    #[serde(default)]
    iss: Option<String>,
}

/// The page a finished sign-in shows.
///
/// The pages are fixed: the browser's history must not keep the state token,
/// the code, or a token endpoint's error, so none of them is ever rendered.
const SIGN_IN_FINISHED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>MCP sign-in finished</title></head><body><h1>MCP sign-in finished</h1>\
<p>The Space stored the credential. You can close this window and return to Neha.</p></body></html>";

/// The page a sign-in that did not finish shows.
const SIGN_IN_FAILED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>MCP sign-in failed</title></head><body><h1>MCP sign-in failed</h1>\
<p>The sign-in did not finish. You can close this window and try again from Neha.</p></body></html>";

fn sign_in_page(status: StatusCode, page: &'static str) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        page,
    )
        .into_response()
}

/// The name of the cookie that binds one browser flow to its browser.
///
/// The sign-in's id is part of the name, and a UUID's text is letters, digits,
/// and hyphens, which a cookie name admits. The distinct name is what keeps two
/// flows of one account apart: an older flow's bind answer that reaches the
/// browser after a newer flow bound it writes its own name, so the browser
/// keeps the newer flow's binding instead of overwriting it.
fn sign_in_cookie_name(sign_in: Id) -> String {
    format!("{SIGN_IN_COOKIE_PREFIX}_{sign_in}")
}

/// The cookie that binds one browser flow to the browser that started it.
///
/// The cookie is first-party: the bind route runs on the Space's own origin,
/// so the authorization server's redirect carries it back. It is scoped to the
/// sign-in paths, script cannot read it, and it lasts no longer than the flow.
fn sign_in_cookie(sign_in: Id, nonce: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}={nonce}; Path=/api/mcp/signin; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        sign_in_cookie_name(sign_in),
        SIGN_IN_TTL.as_secs()
    )
}

/// The header that clears one flow's binding once the callback consumed it.
fn sign_in_cookie_cleared(sign_in: Id, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}=; Path=/api/mcp/signin; HttpOnly; SameSite=Lax; Max-Age=0{secure}",
        sign_in_cookie_name(sign_in)
    )
}

/// Answers one browser step with a page that also clears the flow's binding
/// cookie.
fn sign_in_response(
    server: &SpaceServer,
    sign_in: Id,
    status: StatusCode,
    page: &'static str,
) -> Response {
    let mut response = sign_in_page(status, page);
    if let Ok(value) = HeaderValue::from_str(&sign_in_cookie_cleared(
        sign_in,
        server.public_url.is_some(),
    )) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    response
}

/// Binds one browser flow to the browser that started it.
///
/// The route is same-origin and first-party. A page of this Space opens it
/// with `window.open` on the Space's own origin; that is a same-site
/// top-level navigation, so the browser sends the initiating account's
/// `SameSite=Strict` session cookie. A browser flow requires that session and
/// checks it is the account that started the sign-in before it sets the nonce
/// cookie: a URL forwarded to another browser gets a generic failure page that
/// names neither the sign-in nor the account, and nothing is set.
///
/// A command line flow is different: the authenticated command line is the
/// initiator, and its finish is an authenticated command, so the browser that
/// opens the bind URL need not be signed in to this Space (a fresh browser
/// profile has no session at all). Such a flow binds without a session and
/// redirects to the provider, but sets no nonce cookie: the loopback receiver
/// and the authenticated finish are what bind the second hop, and a command
/// line flow owns no browser binding.
///
/// Only a pending flow may bind. A finished, failed, replaced, or exchanging
/// one already has or had its browser, so binding it again reads the same
/// failure page and sets nothing: a terminal flow's bind must not displace the
/// binding of the flow that replaced it.
///
/// The initiating browser's session is authenticated before the flow's record
/// is checked, and the check and the answer are one critical section under the
/// sign-ins lock: a flow a second start replaced while this request was
/// authenticating is refused instead of being answered from a snapshot taken
/// before the wait. Each flow's cookie carries its own name (see
/// [`sign_in_cookie_name`]), so a bind answer decided just before a
/// replacement writes only the older flow's name and cannot disturb the newer
/// flow's binding even when the browser receives it later.
///
/// The authorization server's redirect back is cross-site and the callback
/// route therefore never asks for the session cookie; the nonce cookie binds
/// the second hop to the browser the first hop ran in.
async fn mcp_sign_in_bind(
    State(server): State<Arc<SpaceServer>>,
    headers: HeaderMap,
    Query(query): Query<SignInBindQuery>,
) -> Response {
    // A command line flow's browser needs no Space session, so first read which
    // kind of flow the URL names. Only that immutable flag is read here; the
    // whole record is read again under the lock below, after the identity
    // lookup, so a replacement meanwhile is still seen.
    let cli = {
        let signins = server.signins.lock().await;
        signins
            .get(&query.sign_in)
            .is_some_and(|pending| pending.cli)
    };
    // Authenticate the initiating browser before the flow's record is read: the
    // identity lookup may wait, and a flow a second start replaces while it does
    // must be refused by the check under the lock, not answered from a snapshot
    // taken before the wait.
    let session = if cli {
        None
    } else {
        match browser_session(&server, &headers).await {
            Some(session) => Some(session),
            None => return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE),
        }
    };
    // Decide and build the whole answer under the lock, and await nothing after
    // releasing it: a second start cannot replace the flow between the checks
    // and the Set-Cookie the browser receives.
    let mut signins = server.signins.lock().await;
    commands::sweep_sign_ins(&mut signins);
    // An unknown or expired sign-in and a foreign browser all read the same
    // page: it names neither the sign-in nor the account.
    let Some(pending) = signins.get(&query.sign_in) else {
        return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
    };
    // Only a flow still waiting for its browser may bind. A flow that is
    // exchanging, finished, failed, or replaced has already been spent (or was
    // replaced by one whose binding this browser holds), so its bind sets no
    // cookie.
    if !matches!(pending.status, SignInStatus::Pending) {
        return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
    }
    if pending.created_at.elapsed() >= SIGN_IN_TTL {
        return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
    }
    // Only the account that started a browser flow lets its browser bind the
    // flow. Another account learns nothing: the failure names neither the
    // sign-in nor the account it belongs to. A command line flow's bind is
    // authenticated by the command that started it, not by this browser.
    if !pending.cli {
        let Some(session) = session.as_ref() else {
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        };
        if pending.account != session.id {
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        }
    }
    let mut response = StatusCode::FOUND.into_response();
    let response_headers = response.headers_mut();
    // The nonce cookie is the browser flow's binding, and its name carries this
    // flow's sign-in, so no other flow writes it. A command line flow sets
    // none: it owns no browser binding.
    if !pending.cli
        && let Ok(cookie) = HeaderValue::from_str(&sign_in_cookie(
            query.sign_in,
            &pending.nonce,
            server.public_url.is_some(),
        ))
    {
        response_headers.insert(SET_COOKIE, cookie);
    }
    if let Ok(location) = HeaderValue::from_str(pending.plan.authorize_url.as_str()) {
        response_headers.insert(axum::http::header::LOCATION, location);
    }
    if let Ok(value) = HeaderValue::from_str("no-store") {
        response_headers.insert(CACHE_CONTROL, value);
    }
    response
}

/// Finishes a browser sign-in.
///
/// The route needs no session cookie: a browser navigation arrives from the
/// authorization server without one. It is authenticated by the state token,
/// by the nonce cookie the bind route set for that flow (its name carries the
/// sign-in id, so another flow's cookie never matches), and by the issuer key
/// in its path, so a callback forwarded to, or replayed by, another browser or
/// addressed to another issuer's callback is refused. The state is single use
/// and cleared before the token exchange, so a replayed callback finds nothing.
async fn mcp_sign_in_callback(
    State(server): State<Arc<SpaceServer>>,
    PathParams(key): PathParams<String>,
    headers: HeaderMap,
    Query(query): Query<SignInCallbackQuery>,
) -> Response {
    let Some(state) = query.state.as_deref().filter(|state| !state.is_empty()) else {
        return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
    };
    // Claim the flow under the lock and copy what the exchange needs: the
    // token endpoint wait must not hold the sign-ins.
    let pending = {
        let mut signins = server.signins.lock().await;
        commands::sweep_sign_ins(&mut signins);
        let Some(sign_in) = signins
            .iter()
            .find(|(_, pending)| {
                !pending.cli && !pending.plan.state.is_empty() && pending.plan.state == state
            })
            .map(|(sign_in, _)| *sign_in)
        else {
            // An unknown or spent state names no flow. This answer may arrive
            // in the browser a newer flow has since bound, so it must set and
            // clear no cookie: its page must leave that binding alone.
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        };
        // The binding cookie is read only once the flow is matched: its name is
        // the flow's own (see [`sign_in_cookie_name`]), so one flow's cookie
        // cannot satisfy another, and this answer clears only this flow's.
        let cookie = cookie_token(&headers, &sign_in_cookie_name(sign_in));
        let pending = signins
            .get_mut(&sign_in)
            .expect("the sign-in was just found");
        // The redirect URI the authorization server returned to carries the
        // issuer key the plan was discovered from. A callback under another
        // issuer's key is a mix-up (or a wrong path): it is refused before the
        // cookie is compared and before any code is exchanged, the flow is not
        // spent by it, and nothing is set.
        if key != crate::oauth::issuer_key(&pending.plan.issuer) {
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        }
        // Only the browser the bind route gave the nonce to completes the
        // flow; the state token alone confirms nothing about who is here. A
        // foreign browser's request sets and clears nothing.
        if cookie != Some(pending.nonce.as_str()) {
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        }
        // An expired flow is spent without being exchanged. It may have been
        // abandoned before a newer flow was bound, so it clears no cookie
        // either; only a flow this request claims may clear the binding.
        if pending.created_at.elapsed() >= SIGN_IN_TTL {
            pending.plan.state.clear();
            pending.status = SignInStatus::Failed {
                reason: SIGN_IN_EXPIRED.to_owned(),
            };
            return sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE);
        }
        // RFC 9207's `iss` names the authorization server that answered. An
        // answer that names another one would hand this flow's code to the
        // wrong issuer, and a server whose metadata advertised the parameter
        // always names its issuer. The request matched the state and the
        // nonce, so its failure clears the binding cookie it consumed.
        if !commands::accepts_response_issuer(&pending.plan, query.iss.as_deref()) {
            pending.plan.state.clear();
            pending.status = SignInStatus::Failed {
                reason: SIGN_IN_INCOMPLETE.to_owned(),
            };
            return sign_in_response(
                &server,
                sign_in,
                StatusCode::BAD_REQUEST,
                SIGN_IN_FAILED_PAGE,
            );
        }
        if query.error.is_some() {
            // The authority's own words never reach the browser or a client.
            pending.plan.state.clear();
            pending.status = SignInStatus::Failed {
                reason: SIGN_IN_CANCELLED.to_owned(),
            };
            return sign_in_response(
                &server,
                sign_in,
                StatusCode::BAD_REQUEST,
                SIGN_IN_FAILED_PAGE,
            );
        }
        if query.code.as_deref().is_none_or(|code| code.is_empty()) {
            pending.plan.state.clear();
            pending.status = SignInStatus::Failed {
                reason: SIGN_IN_CANCELLED.to_owned(),
            };
            return sign_in_response(
                &server,
                sign_in,
                StatusCode::BAD_REQUEST,
                SIGN_IN_FAILED_PAGE,
            );
        }
        // Admit the exchange: spending the state and marking the flow
        // executing are one step under the lock, so a replayed callback finds
        // nothing and a newer sign-in can still replace the flow while the
        // token request runs outside the lock.
        pending.plan.state.clear();
        pending.status = SignInStatus::Executing;
        commands::PendingExchange {
            sign_in,
            account: pending.account,
            server: pending.server,
            name: pending.name.clone(),
            url: pending.url.clone(),
            scope: pending.scope,
            plan: pending.plan.clone(),
        }
    };
    let code = query.code.as_deref().unwrap_or_default();
    let token = match server.oauth.finish(&pending.plan, code).await {
        Ok(token) => token,
        Err(_) => {
            // The token endpoint's own error is not a client's to read. A
            // replacement that already wrote a terminal status keeps it, and
            // this stale answer clears no cookie.
            let owned = commands::fail_executing_sign_in(&server.signins, pending.sign_in).await;
            return sign_in_failed_response(&server, pending.sign_in, owned);
        }
    };
    if !neha::record::McpServerAdded::valid_credential(&token) {
        let owned = commands::fail_executing_sign_in(&server.signins, pending.sign_in).await;
        return sign_in_failed_response(&server, pending.sign_in, owned);
    }
    let writes = commands::SignInWrites {
        store: &server.store,
        state: &server.state,
        secrets: &server.secrets,
        library: &server.library,
    };
    match commands::commit_executing_sign_in(&server.signins, &pending, &token, writes).await {
        commands::SignInCommit::Committed { events } => {
            broadcast(&server, &events);
            sign_in_response(
                &server,
                pending.sign_in,
                StatusCode::OK,
                SIGN_IN_FINISHED_PAGE,
            )
        }
        // The record did not change, so the client reads a fixed sentence.
        commands::SignInCommit::Failed { .. } => sign_in_response(
            &server,
            pending.sign_in,
            StatusCode::BAD_REQUEST,
            SIGN_IN_FAILED_PAGE,
        ),
        // A newer sign-in replaced this flow while its token was in flight:
        // the token is discarded and no binding is cleared. The newer flow's
        // cookie is its own name, so this answer cannot disturb it.
        commands::SignInCommit::Discarded => {
            sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE)
        }
    }
}

/// Answers a browser callback whose exchange produced no credential.
///
/// Only a delivery that still owns its flow clears that flow's binding cookie:
/// a replaced flow's answer may arrive after a newer flow bound the same
/// browser, and clearing only this flow's cookie leaves the newer flow's
/// binding alone.
fn sign_in_failed_response(server: &SpaceServer, sign_in: Id, owned: bool) -> Response {
    if owned {
        sign_in_response(
            server,
            sign_in,
            StatusCode::BAD_REQUEST,
            SIGN_IN_FAILED_PAGE,
        )
    } else {
        sign_in_page(StatusCode::BAD_REQUEST, SIGN_IN_FAILED_PAGE)
    }
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
    let appended = server
        .store
        .append(AppendRequest::new(
            Id::new(),
            format!("join:{}", account.id),
            vec![joined],
        ))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    // The projection holds the people a row can name by their display name.
    // The join has to reach it now, not only when a later start replays the
    // record.
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
            "{}={token}; Path=/; HttpOnly; SameSite=Strict{secure}",
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
                .is_some_and(|host| same_authority(origin, host, server.browser_scheme()))
    })
}

/// Returns the account a browser request's session cookie names, when it
/// carries one.
///
/// The session cookie is first-party and `SameSite=Strict`; a browser sends it
/// on a same-site top-level navigation, which is what `window.open` from one
/// of this Space's pages is. Like [`Authed`], the cookie is read only when the
/// Host names the Space's own browser hostname, so another site cannot borrow
/// the session.
async fn browser_session(server: &SpaceServer, headers: &HeaderMap) -> Option<Account> {
    let browser_host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(split_authority)
        .is_some_and(|(host, _)| host.eq_ignore_ascii_case(&server.browser_host));
    if !browser_host {
        return None;
    }
    let token = cookie_token(headers, &server.session_cookie)?;
    server.identity.authenticate(token).await.ok().flatten()
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
    // A daemon that cannot fence its machine never runs unless the Space
    // operator explicitly opted in to unsandboxed Local Computers. A connection
    // that names another Computer would act as that Computer: both are refused.
    let Some((version, named, sandbox)) = hello else {
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
                            blobs: &server.blobs,
                            secrets: &server.secrets,
                            identity: &server.identity,
                            links: &server.links,
                            pairings: &server.pairings,
                            approvals: &server.approvals,
                            library: &server.library,
                            runs: &server.runs,
                            events: &server.events,
                            catalog: &server.catalog,
                            plugins: &server.plugins,
                            signins: &server.signins,
                            signin_start: &server.signin_start,
                            oauth: &server.oauth,
                            browser_origin: server.browser_origin(),
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
/// This milestone has no Groups: an event belongs to the User who wrote it, to
/// the User a Bot wrote it for, or to the Space itself. What the Space offers
/// every member is Space-wide: a Space MCP server or skill, and the changes to
/// them. A personal credential, a personal server, a Bot's skill, and a
/// package's installation stay with the account they belong to. A package's
/// removal is every account's: the payload is only the package's identifier,
/// and a member whose open views still show the package's servers and skills
/// must refresh when the Admin takes it away.
///
/// The record is what decides: an event that names no scope is a historical
/// one, and a reader treats it the way the command surface treats a personal
/// resource, so it never widens what an older record lets a member read.
fn visible_to(account: Id, event: &neha::record::Event) -> bool {
    use neha::record::EventKind;

    if matches!(event.author, neha::record::Author::User(author) if author == account) {
        return true;
    }
    if event.acting == Some(account) {
        return true;
    }

    match event.kind {
        EventKind::UserJoined | EventKind::ProviderAdded => true,
        // A Space server is every member's; a personal one is its owner's.
        EventKind::McpServerAdded => {
            event.data.get("scope").and_then(Value::as_str) == Some("space")
        }
        // Only a Space server's change is every member's: the update payload
        // names no person, so the scope the writer recorded decides. A
        // personal credential's change records the User scope even on a Space
        // server, so attaching a credential never reveals who did it.
        EventKind::McpServerUpdated | EventKind::McpServerRemoved => {
            event.data.get("scope").and_then(Value::as_str) == Some("space")
        }
        // A Space skill is every member's; a Bot's is its owner's.
        EventKind::SkillAdded => event.data.get("library").and_then(Value::as_str) == Some("space"),
        EventKind::SkillRemoved => {
            event.data.get("library").and_then(Value::as_str) == Some("space")
        }
        // A removal carries only the package id, so every member may learn
        // that the package left and refresh what it offered. An installation
        // and an enabled/disabled change carry source, commit, and digest
        // metadata and stay the Admin's, as do the package events of a record
        // this rule does not name.
        EventKind::PluginRemoved => true,
        _ => false,
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
        match commands::prepare_recovery(state, &run, &commands::McpCatalog::default()) {
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
    #[test]
    fn unsandboxed_local_computers_need_an_explicit_opt_in() {
        assert!(!super::allows_unsandboxed(None));
        assert!(!super::allows_unsandboxed(Some("0")));
        assert!(!super::allows_unsandboxed(Some("yes")));
        assert!(super::allows_unsandboxed(Some("1")));
        assert!(super::allows_unsandboxed(Some("TRUE")));
    }

    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{
        Running, ServerError, SpaceServer, StreamError, catch_up, start, start_with_ui,
        start_with_ui_public,
    };
    use crate::blobs::test_s3;
    use crate::config::S3Storage;
    use crate::home::Home;
    use crate::secrets::Secrets;
    use crate::store::{AppendRequest, ContinuationWrite, Store};

    /// Decodes one command frame exactly where the socket decodes it.
    ///
    /// The browser builds the command object as JSON text; this test anchors
    /// the shapes it sends to the production `ClientFrame` decoder instead of
    /// a mock.
    fn decode_command_frame(command: &str) -> Result<ClientFrame, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"type":"command","id":"{}","command":{command}}}"#,
            Id::new()
        ))
    }

    #[test]
    fn browser_command_frames_decode_exactly_where_the_socket_decodes_them() {
        // A command with no parameters is sent without `args`: an empty object
        // is not the unit variant's shape.
        assert!(matches!(
            decode_command_frame(r#"{"name":"provider_list"}"#),
            Ok(ClientFrame::Command {
                command: CommandCall::ProviderList,
                ..
            })
        ));
        assert!(
            decode_command_frame(r#"{"name":"provider_list","args":{}}"#).is_err(),
            "a unit command that carries args is not a frame this Space takes"
        );
        // A command whose parameters are all optional takes an empty object,
        // the shape the browser sends for a parameterless struct argument.
        assert!(matches!(
            decode_command_frame(r#"{"name":"skill_list","args":{}}"#),
            Ok(ClientFrame::Command {
                command: CommandCall::SkillList { bot: None },
                ..
            })
        ));
    }

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
    async fn a_public_tls_proxy_uses_its_https_origin_and_secure_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let ui = tempfile::tempdir().unwrap();
        std::fs::write(ui.path().join("index.html"), "<!doctype html>").unwrap();
        let running = start_with_ui_public(
            test_home(dir.path()),
            0,
            Some(ui.path().to_path_buf()),
            Some("https://space.example.com".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(running.browser_url(), "https://space.example.com");
        let invite = running.invite.clone().expect("a fresh Space invites");
        assert!(invite.starts_with("https://space.example.com/invite#"));
        join(&running, &invite, "Ada", "correct horse battery").await;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // A loopback request must move to the public TLS origin, not to the
        // Space's private browser hostname.
        let loopback = client
            .get(running.url())
            .header("host", "127.0.0.1")
            .send()
            .await
            .unwrap();
        assert_eq!(loopback.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            loopback.headers().get("location").unwrap(),
            "https://space.example.com/"
        );

        // A browser request through the proxy carries the public Host and the
        // matching https Origin; it receives the Secure session cookie.
        let login = client
            .post(format!("{}/api/login", running.url()))
            .header("host", "space.example.com")
            .header("origin", "https://space.example.com")
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), reqwest::StatusCode::OK);
        let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("; Secure"), "{cookie}");

        // A page on another scheme is not this Space.
        let wrong_scheme = client
            .post(format!("{}/api/login", running.url()))
            .header("host", "space.example.com")
            .header("origin", "http://space.example.com")
            .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_scheme.status(), reqwest::StatusCode::FORBIDDEN);

        running.shutdown().await;
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
    use neha::record::SkillLibrary;
    use neha::record::{Author, EventDraft, EventKind};
    use neha::seq::Seq;
    use neha::surface::{
        BotComputer, BotCreate, BotUpdate, CommandCall, MessageSend, ModelSpec, ProviderAdd,
        ProviderConnect, ProviderProtocol, SkillAdd, SkillAddScript, ThinkingLevel,
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
    #[derive(Debug, Clone)]
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

    /// A scripted Provider that keeps the requests it saw, for a test that
    /// asserts what reached the Model.
    async fn scripted_provider_recording(
        script: Vec<ScriptedReply>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        let (base, task, _gates, recorded) = gated_provider(script).await;

        (base, task, recorded)
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

    /// Renders one SSE event carrying JSON data.
    fn sse(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// The script and the requests one [`protocol_provider`] serves.
    #[derive(Clone, Default)]
    struct ProtocolProvider {
        /// The raw SSE replies, in order.
        replies: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
        /// Every JSON request body, in order.
        recorded: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl ProtocolProvider {
        /// Returns the next scripted SSE reply.
        fn next_reply(&self) -> String {
            self.replies.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    async fn protocol_reply(
        axum::extract::State(provider): axum::extract::State<ProtocolProvider>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::response::Response {
        provider.recorded.lock().unwrap().push(body);
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from(provider.next_reply()))
            .unwrap()
    }

    /// A scripted Provider speaking one wire protocol, and the requests it saw.
    ///
    /// Every call pops the next raw SSE reply, so a test drives one run step
    /// per reply without sleeping or reaching the network.
    async fn protocol_provider(
        protocol: ProviderProtocol,
        replies: Vec<String>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        use axum::Router;
        use axum::routing::post;

        let route = match protocol {
            ProviderProtocol::ChatCompletions => "/v1/chat/completions",
            ProviderProtocol::OpenAiResponses => "/v1/responses",
            ProviderProtocol::AnthropicMessages => "/v1/messages",
        };
        let provider = ProtocolProvider {
            replies: std::sync::Arc::new(std::sync::Mutex::new(replies.into())),
            recorded: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let recorded = std::sync::Arc::clone(&provider.recorded);
        let app = Router::new()
            .route(route, post(protocol_reply))
            .with_state(provider);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task, recorded)
    }

    type GatedReply = (String, Option<Gate>);
    type GatedReplies = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<GatedReply>>>;

    /// A scripted protocol Provider where each reply may wait on its gate.
    #[derive(Clone, Default)]
    struct GatedProtocolProvider {
        replies: GatedReplies,
        recorded: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    async fn gated_protocol_reply(
        axum::extract::State(provider): axum::extract::State<GatedProtocolProvider>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::response::Response {
        provider.recorded.lock().unwrap().push(body);
        let (body, gate) = provider
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        if let Some(gate) = gate {
            let mut receiver = gate.0.subscribe();
            wait_for_gate(receiver.clone()).await;
            let _ = receiver.borrow_and_update();
        }
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    async fn gated_protocol_provider(
        protocol: ProviderProtocol,
        replies: Vec<(String, Option<Gate>)>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        use axum::Router;
        use axum::routing::post;

        let route = match protocol {
            ProviderProtocol::ChatCompletions => "/v1/chat/completions",
            ProviderProtocol::OpenAiResponses => "/v1/responses",
            ProviderProtocol::AnthropicMessages => "/v1/messages",
        };
        let provider = GatedProtocolProvider {
            replies: std::sync::Arc::new(std::sync::Mutex::new(replies.into())),
            recorded: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let recorded = std::sync::Arc::clone(&provider.recorded);
        let app = Router::new()
            .route(route, post(gated_protocol_reply))
            .with_state(provider);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task, recorded)
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
                thinks: false,

                tools: true,
                thinking_levels: Vec::new(),
            }],
            default_model: Some("test-model".to_owned()),
            thinking: None,

            vendor: None,
            discover: false,
        })
    }

    /// The `ProviderAdd` that connects one Model over the given protocol.
    fn provider_add_on(
        base_url: String,
        protocol: ProviderProtocol,
        thinks: bool,
        thinking: Option<ThinkingLevel>,
    ) -> CommandCall {
        CommandCall::ProviderAdd(ProviderAdd {
            protocol,
            base_url,
            api_key: "sk-test".to_owned(),
            models: vec![ModelSpec {
                name: "test-model".to_owned(),
                context_limit: 128_000,
                max_output: 8_192,
                reads_images: false,
                thinks,

                tools: true,
                thinking_levels: Vec::new(),
            }],
            default_model: Some("test-model".to_owned()),
            thinking,

            vendor: None,
            discover: false,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
            thinking: None,

            model: None,
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
                thinking: None,

                model: None,
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
    async fn chat_list_projects_a_working_chat_and_an_idle_one() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, gates, _recorded) = gated_provider(vec![hold("done")]).await;
        command(&mut socket, Id::new(), provider_add(base)).await;
        let created = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
                thinking: None,

                model: None,
            }),
        )
        .await;
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();

        // A Chat with no run is idle, in the list and under its Bot.
        let view = chat_view(&mut socket, chat).await;
        assert_eq!(view["status"], "idle");
        assert!(
            view["active_run"].is_null(),
            "an idle Chat follows no run: {view}"
        );
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["chat"]["status"], "idle");
        assert!(shown["chat"]["active_run"].is_null());

        // A queued run makes the Chat work before the Model answers.
        let queued = submit(&mut socket, chat, "hello", None).await;
        let run = run_of(&queued);
        let view = chat_view(&mut socket, chat).await;
        assert_eq!(view["status"], "working", "a queued run works: {view}");
        assert_eq!(view["active_run"], json!(run));
        wait_status(&mut socket, run, "running").await;
        assert_eq!(chat_view(&mut socket, chat).await["status"], "working");

        // The reply ends the run, and the Chat settles back to idle.
        gates[0].open();
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded");
        let view = chat_view(&mut socket, chat).await;
        assert_eq!(view["status"], "idle");
        assert!(
            view["active_run"].is_null(),
            "an ended Chat follows no run: {view}"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_replays_the_chat_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_home(dir.path());
        let first = start(root.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let mut local = local_bot(
            &first,
            &invite,
            vec![tool("shell", json!({"command": "pwd"})), text("done")],
        )
        .await;
        let run = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, run).await["approval"].clone(),
        )
        .unwrap();
        let view = chat_view(&mut local.socket, local.chat).await;
        assert_eq!(view["status"], "waiting");
        assert_eq!(view["active_run"], json!(run));

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        first.shutdown().await;

        // A fresh start projects the same state from the record again.
        let second = start(root.clone(), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        let view = chat_view(&mut socket, local.chat).await;
        assert_eq!(
            view["status"], "waiting",
            "a restart projects the waiting run: {view}"
        );
        assert_eq!(view["active_run"], json!(run));

        // Answering the approval fails the run, and the projection follows.
        decide_approval(&mut socket, approval, false, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "failed");
        let view = chat_view(&mut socket, local.chat).await;
        assert_eq!(view["status"], "failed");
        assert!(
            view["active_run"].is_null(),
            "a failed Chat follows no run: {view}"
        );

        // A new run in the Chat works again as soon as it queues.
        let next = run_of(&submit(&mut socket, local.chat, "run pwd again", None).await);
        let view = chat_view(&mut socket, local.chat).await;
        assert_eq!(view["status"], "working");
        assert_eq!(view["active_run"], json!(next));

        socket.close(None).await.unwrap();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chat_list_keeps_the_later_failure_after_an_earlier_success() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("second done"),
            ],
        )
        .await;
        let first = run_of(&submit(&mut local.socket, local.chat, "run pwd", None).await);
        let approval: Id = serde_json::from_value(
            approval_for_run(&mut local.socket, first).await["approval"].clone(),
        )
        .unwrap();
        assert_eq!(
            chat_view(&mut local.socket, local.chat).await["status"],
            "waiting"
        );

        // A waiting run does not hold the Bot's queue, so the next Message's
        // run starts and succeeds while the first one waits.
        let second = run_of(&submit(&mut local.socket, local.chat, "second", None).await);
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );
        let view = chat_view(&mut local.socket, local.chat).await;
        assert_eq!(
            view["status"], "waiting",
            "the wait outranks the newer run: {view}"
        );
        assert_eq!(view["active_run"], json!(first));

        // Denying the first run fails it. Its end is the latest event that
        // ended a run, so the Chat stays failed; the newer success must not
        // hide it.
        decide_approval(&mut local.socket, approval, false, false).await;
        assert_eq!(wait_run(&mut local.socket, first).await["status"], "failed");
        let view = chat_view(&mut local.socket, local.chat).await;
        assert_eq!(
            view["status"], "failed",
            "the later failure outlives the earlier success: {view}"
        );
        assert!(view["active_run"].is_null());

        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
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
                thinking: None,

                model: None,
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
                        thinking: None,

                        model: None,
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
                thinking: None,

                model: None,
            }),
        )
        .await;
        let conflict = try_command(
            &mut socket,
            command_id,
            CommandCall::BotCreate(BotCreate {
                name: "Other".to_owned(),
                computer: None,
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
            thinking: None,

            model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                        thinking: None,

                        model: None,
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
                thinking: None,

                model: None,
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
        tool_bot_on(socket, base, ProviderProtocol::ChatCompletions, false, None).await
    }

    /// Connects a Provider speaking `protocol` and creates the Hosted Bot the
    /// tool tests run.
    async fn tool_bot_on(
        socket: &mut Socket,
        base: String,
        protocol: ProviderProtocol,
        thinks: bool,
        thinking: Option<ThinkingLevel>,
    ) -> (Id, Id, Id) {
        command(
            socket,
            Id::new(),
            provider_add_on(base, protocol, thinks, thinking),
        )
        .await;
        let bot = command(
            socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
                thinking: None,

                model: None,
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

    /// Returns the ChatList snapshot of one Chat.
    async fn chat_view(socket: &mut Socket, chat: Id) -> Value {
        let chats = command(socket, Id::new(), CommandCall::ChatList).await;
        chats
            .as_array()
            .and_then(|views| views.iter().find(|view| view["id"] == json!(chat)))
            .cloned()
            .unwrap_or_else(|| panic!("ChatList has no Chat {chat}"))
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

    /// The first OpenAI Responses reply: a reasoning item and a `write_file` call.
    fn responses_write_file_reply() -> String {
        let arguments = r#"{"path":"note.txt","content":"hello"}"#;
        [
            sse(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "write_file",
                        "arguments": "",
                    },
                }),
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": arguments,
                }),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "write_file",
                        "arguments": arguments,
                    },
                }),
            ),
            sse(
                "response.completed",
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "output": [
                            {"type": "reasoning", "id": "rs_1", "summary": []},
                            {
                                "type": "function_call",
                                "id": "fc_1",
                                "call_id": "call_1",
                                "name": "write_file",
                                "arguments": arguments,
                            },
                        ],
                        "usage": {
                            "input_tokens": 120,
                            "output_tokens": 30,
                            "input_tokens_details": {
                                "cached_tokens": 20,
                                "cache_write_tokens": 5,
                            },
                            "output_tokens_details": {"reasoning_tokens": 10},
                        },
                    },
                }),
            ),
        ]
        .concat()
    }

    /// The second OpenAI Responses reply: the final text.
    fn responses_text_reply() -> String {
        let message = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "done"}],
        });
        [
            sse(
                "response.output_text.delta",
                json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_1",
                    "output_index": 0,
                    "delta": "done",
                }),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": message,
                }),
            ),
            sse(
                "response.completed",
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_2",
                        "status": "completed",
                        "output": [message],
                        "usage": {"input_tokens": 150, "output_tokens": 12},
                    },
                }),
            ),
        ]
        .concat()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_sends_its_thinking_level_to_each_protocol() {
        let (_dir1, running1, invite1) = running().await;
        let (mut socket, _token) = admin(&running1, &invite1).await;

        let (base, provider_task, recorded) = protocol_provider(
            ProviderProtocol::OpenAiResponses,
            vec![responses_text_reply(), responses_text_reply()],
        )
        .await;
        let (_bot, chat, _computer) = tool_bot_on(
            &mut socket,
            base,
            ProviderProtocol::OpenAiResponses,
            true,
            Some(ThinkingLevel::High),
        )
        .await;
        send(&mut socket, chat, "answer").await;
        let low = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Low".to_owned(),
                computer: None,
                thinking: Some(ThinkingLevel::Low),

                model: None,
            }),
        )
        .await;
        let low_chat: Id = serde_json::from_value(low["chat"]["id"].clone()).unwrap();
        send(&mut socket, low_chat, "answer").await;
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["reasoning"]["effort"], "high");
            assert_eq!(requests[1]["reasoning"]["effort"], "low");
        }
        socket.close(None).await.unwrap();
        provider_task.abort();
        running1.shutdown().await;

        let (_dir2, running2, invite2) = running().await;
        let (mut socket2, _token2) = admin(&running2, &invite2).await;
        let (base2, provider_task2, recorded2) = protocol_provider(
            ProviderProtocol::AnthropicMessages,
            vec![anthropic_text_reply()],
        )
        .await;
        let (_bot2, chat2, _computer2) = tool_bot_on(
            &mut socket2,
            base2,
            ProviderProtocol::AnthropicMessages,
            true,
            Some(ThinkingLevel::High),
        )
        .await;
        send(&mut socket2, chat2, "answer").await;
        {
            let requests2 = recorded2.lock().unwrap();
            assert_eq!(requests2.len(), 1);
            assert_eq!(requests2[0]["thinking"]["type"], "enabled");
            assert_eq!(requests2[0]["thinking"]["budget_tokens"], 8191);
        }
        socket2.close(None).await.unwrap();
        provider_task2.abort();
        running2.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovered_unknown_write_reloads_its_own_continuation() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let store = Store::open(&home).await.unwrap();
        let (base, provider_task, recorded) = protocol_provider(
            ProviderProtocol::OpenAiResponses,
            vec![responses_text_reply()],
        )
        .await;

        let user = Id::new();
        let provider = Id::new();
        let model = Id::new();
        let computer = Id::new();
        let bot = Id::new();
        let chat = Id::new();
        let message = Id::new();
        let run = Id::new();
        let command = Id::new();
        let action = Id::new();
        let workspace = home.workspace_dir(computer);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("note.txt"), "hello").unwrap();
        let continuation = neha_loop::Continuation {
            protocol: "openai_responses".to_owned(),
            provider: provider.to_string(),
            model: "test-model".to_owned(),
            state: json!({"items": [{"type": "reasoning", "id": "rs_recovered"}]}),
        };
        let drafts = vec![
            EventDraft::new(
                EventKind::UserJoined,
                Author::User(user),
                json!({"display_name": "Ada", "is_admin": true}),
            ),
            EventDraft::new(
                EventKind::ProviderAdded,
                Author::User(user),
                json!({
                    "provider": provider,
                    "protocol": "openai_responses",
                    "base_url": base,
                    "models": [{
                        "id": model,
                        "name": "test-model",
                        "context_limit": 128000,
                        "max_output": 8192,
                        "reads_images": false,
                        "thinks": true,
                    }],
                    "default_model": model,
                    "thinking": "high",
                }),
            ),
            EventDraft::new(
                EventKind::ComputerCreated,
                Author::User(user),
                json!({
                    "computer": computer,
                    "owner": user,
                    "kind": "hosted",
                    "workspace": workspace.to_string_lossy(),
                }),
            ),
            EventDraft::new(
                EventKind::BotCreated,
                Author::User(user),
                json!({
                    "bot": bot,
                    "owner": user,
                    "name": "Scout",
                    "computer": computer,
                    "chat": chat,
                    "thinking": null,
                }),
            ),
            EventDraft::new(
                EventKind::ChatCreated,
                Author::User(user),
                json!({"chat": chat, "owner": user, "bot": bot}),
            ),
            EventDraft::new(
                EventKind::MessageAdded,
                Author::User(user),
                json!({"message": message, "chat": chat, "text": "write it", "run": run}),
            ),
            EventDraft::new(
                EventKind::RunQueued,
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
                json!({
                    "run": run,
                    "step": 1,
                    "usage": {"uncached_input": 1, "cache_read": 0,
                        "cache_write": 0, "output": 1, "reasoning": 0},
                    "tool_calls": 1,
                }),
            )
            .acting(user),
            EventDraft::new(
                EventKind::RunReply,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "step": 1,
                    "content": null,
                    "calls": [{
                        "call": "call-1",
                        "tool": "write_file",
                        "arguments": {"path": "note.txt", "content": "hello"},
                        "refused": null,
                    }],
                    "usage": null,
                }),
            )
            .acting(user),
            EventDraft::new(
                EventKind::ActionStarted,
                Author::Bot(bot),
                json!({
                    "run": run,
                    "step": 1,
                    "action": action,
                    "tool": "write_file",
                    "arguments": {"path": "note.txt", "content": "hello"},
                }),
            )
            .acting(user),
        ];
        store
            .append(
                AppendRequest::new(command, "seed-unknown-write", drafts).with_continuation(
                    ContinuationWrite {
                        run,
                        step: 1,
                        continuation: Some(continuation),
                    },
                ),
            )
            .await
            .unwrap();
        Secrets::load(&home)
            .unwrap()
            .store_provider_key(provider, "sk-test")
            .unwrap();
        store.close().await.unwrap();

        let running = start(home, 0).await.unwrap();
        for _ in 0..500 {
            let status = running
                .server
                .state
                .lock()
                .unwrap()
                .runs
                .get(&run)
                .map(|run| run.status);
            if matches!(
                status,
                Some(crate::state::RunStatus::Succeeded | crate::state::RunStatus::Failed)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        {
            let state = running.server.state.lock().unwrap();
            assert_eq!(
                state.runs[&run].status,
                crate::state::RunStatus::Succeeded,
                "the verified write lets the run continue: {:?}",
                state.runs[&run].error
            );
        }
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(
                requests[0]["input"].to_string().contains("rs_recovered"),
                "the recovered step loads its own continuation: {}",
                requests[0]
            );
        }
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovered_run_reuses_the_native_continuation() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");
        let (mut socket, token) = admin(&running, &invite).await;

        let (sender, _receiver) = tokio::sync::watch::channel(false);
        let gate = Gate(std::sync::Arc::new(sender));
        let (base, provider_task, recorded) = gated_protocol_provider(
            ProviderProtocol::OpenAiResponses,
            vec![
                (responses_write_file_reply(), None),
                (responses_text_reply(), Some(gate.clone())),
                (responses_text_reply(), None),
            ],
        )
        .await;
        let (_bot, chat, _computer) = tool_bot_on(
            &mut socket,
            base,
            ProviderProtocol::OpenAiResponses,
            false,
            None,
        )
        .await;

        let queued = submit(&mut socket, chat, "write note.txt", None).await;
        let run = run_of(&queued);
        for _ in 0..500 {
            if recorded.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 2, "the step asks for its next reply");
            assert!(
                requests[1]["input"].to_string().contains("rs_1"),
                "the live next step replays the reasoning item: {}",
                requests[1]
            );
        }

        // Stop the Space while the second request is in flight. The first
        // step and its native state are already recorded.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;

        let restarted = start(test_home(&root), 0).await.unwrap();
        let mut socket = open_socket(&restarted, &token).await;
        gate.open();
        let shown = wait_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        {
            let requests = recorded.lock().unwrap();
            assert!(requests.len() >= 3);
            assert!(
                requests[2]["input"].to_string().contains("rs_1"),
                "the recovered step reloads the reasoning item: {}",
                requests[2]
            );
        }

        socket.close(None).await.unwrap();
        provider_task.abort();
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_writes_a_file_over_openai_responses() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task, recorded) = protocol_provider(
            ProviderProtocol::OpenAiResponses,
            vec![responses_write_file_reply(), responses_text_reply()],
        )
        .await;
        let (_bot, chat, computer) = tool_bot_on(
            &mut socket,
            base,
            ProviderProtocol::OpenAiResponses,
            false,
            None,
        )
        .await;

        let result = send(&mut socket, chat, "write note.txt").await;

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["text"], "done");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["steps"].as_array().unwrap().len(), 2);
        let actions = shown["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["tool"], "write_file");
        assert_eq!(actions[0]["status"], "succeeded");
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "hello"
        );
        assert_eq!(shown["usage"]["uncached_input"], 245);
        assert_eq!(shown["usage"]["cache_read"], 20);
        assert_eq!(shown["usage"]["cache_write"], 5);
        assert_eq!(shown["usage"]["output"], 42);
        assert_eq!(shown["usage"]["reasoning"], 10);

        // The protocol the Admin named is the one the Space records.
        let providers = command(&mut socket, Id::new(), CommandCall::ProviderList).await;
        assert_eq!(providers.as_array().unwrap().len(), 1);
        assert_eq!(providers[0]["protocol"], "openai_responses");

        // The second call replays the Provider's native output, its reasoning
        // item included, and does not repeat the normalized function call.
        let bodies = recorded.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one call per step");
        let tools = bodies[0]["tools"].as_array().unwrap();
        let write = tools
            .iter()
            .find(|tool| tool["name"] == "write_file")
            .expect("write_file is offered");
        assert_eq!(write["type"], "function");
        assert!(write["parameters"]["properties"]["content"].is_object());
        let input = bodies[1]["input"].as_array().unwrap();
        let reasoning = input
            .iter()
            .find(|item| item["type"] == "reasoning")
            .expect("the reasoning item reaches the next call");
        assert_eq!(reasoning["id"], "rs_1");
        let calls = input
            .iter()
            .filter(|item| item["type"] == "function_call" && item["call_id"] == "call_1")
            .count();
        assert_eq!(calls, 1, "the native call replaces the normalized one");
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call_1"),
            "the tool result reaches the next call"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    /// The first Anthropic Messages reply: thinking, then a `write_file` call.
    fn anthropic_write_file_reply() -> String {
        let arguments = r#"{"path":"note.txt","content":"hello"}"#;
        [
            sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "test-model",
                        "usage": {
                            "input_tokens": 100,
                            "cache_read_input_tokens": 10,
                            "cache_creation_input_tokens": 5,
                            "output_tokens": 1,
                        },
                    },
                }),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""},
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "I should write the note."},
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-abc"},
                }),
            ),
            sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "write_file",
                        "input": {},
                    },
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": arguments},
                }),
            ),
            sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {
                        "output_tokens": 20,
                        "output_tokens_details": {"thinking_tokens": 6},
                    },
                }),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ]
        .concat()
    }

    /// The second Anthropic Messages reply: the final text.
    fn anthropic_text_reply() -> String {
        [
            sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_2",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "test-model",
                        "usage": {
                            "input_tokens": 140,
                            "cache_read_input_tokens": 30,
                            "cache_creation_input_tokens": 3,
                            "output_tokens": 1,
                        },
                    },
                }),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "done"},
                }),
            ),
            sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {
                        "output_tokens": 12,
                        "output_tokens_details": {"thinking_tokens": 2},
                    },
                }),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ]
        .concat()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_writes_a_file_over_anthropic_messages() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task, recorded) = protocol_provider(
            ProviderProtocol::AnthropicMessages,
            vec![anthropic_write_file_reply(), anthropic_text_reply()],
        )
        .await;
        let (_bot, chat, computer) = tool_bot_on(
            &mut socket,
            base,
            ProviderProtocol::AnthropicMessages,
            true,
            Some(ThinkingLevel::Medium),
        )
        .await;

        let result = send(&mut socket, chat, "write note.txt").await;

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["text"], "done");
        let run: Id = serde_json::from_value(result["run"].clone()).unwrap();
        let shown = show_run(&mut socket, run).await;
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["steps"].as_array().unwrap().len(), 2);
        let actions = shown["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["tool"], "write_file");
        assert_eq!(actions[0]["status"], "succeeded");
        let workspace = running
            .server_home()
            .join("computers")
            .join(computer.to_string())
            .join("workspace");
        assert_eq!(
            std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "hello"
        );
        assert_eq!(shown["usage"]["uncached_input"], 240);
        assert_eq!(shown["usage"]["cache_read"], 40);
        assert_eq!(shown["usage"]["cache_write"], 8);
        assert_eq!(shown["usage"]["output"], 32);
        assert_eq!(shown["usage"]["reasoning"], 8);

        // The protocol the Admin named is the one the Space records.
        let providers = command(&mut socket, Id::new(), CommandCall::ProviderList).await;
        assert_eq!(providers.as_array().unwrap().len(), 1);
        assert_eq!(providers[0]["protocol"], "anthropic_messages");

        let bodies = recorded.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one call per step");
        assert_eq!(bodies[0]["thinking"]["type"], "enabled");
        let tools = bodies[1]["tools"].as_array().unwrap();
        let write = tools
            .iter()
            .find(|tool| tool["name"] == "write_file")
            .expect("write_file is offered");
        assert!(write["input_schema"]["properties"]["content"].is_object());
        // The second call replays the Provider's native assistant blocks, so
        // the thinking block keeps the signature that proves its provenance.
        let messages = bodies[1]["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("the assistant turn reaches the next call");
        let blocks = assistant["content"].as_array().unwrap();
        let thinking = blocks
            .iter()
            .find(|block| block["type"] == "thinking")
            .expect("the thinking block reaches the next call");
        assert_eq!(thinking["thinking"], "I should write the note.");
        assert_eq!(thinking["signature"], "sig-abc");
        assert!(
            blocks
                .iter()
                .any(|block| block["type"] == "tool_use" && block["id"] == "toolu_1"),
            "the native tool_use block replaces the normalized call"
        );
        assert!(
            messages.iter().any(|message| {
                message["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["type"] == "tool_result" && block["tool_use_id"] == "toolu_1"
                    })
                })
            }),
            "the tool result reaches the next call"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn provider_add_remembers_each_protocol() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        for (protocol, wire) in [
            (ProviderProtocol::ChatCompletions, "chat_completions"),
            (ProviderProtocol::OpenAiResponses, "openai_responses"),
            (ProviderProtocol::AnthropicMessages, "anthropic_messages"),
        ] {
            let added = command(
                &mut socket,
                Id::new(),
                provider_add_on(format!("http://127.0.0.1:1/{wire}"), protocol, false, None),
            )
            .await;
            assert_eq!(added["models"][0]["name"], "test-model");
        }

        let providers = command(&mut socket, Id::new(), CommandCall::ProviderList).await;
        let mut protocols: Vec<&str> = providers
            .as_array()
            .unwrap()
            .iter()
            .map(|provider| provider["protocol"].as_str().unwrap())
            .collect();
        protocols.sort_unstable();
        assert_eq!(
            protocols,
            ["anthropic_messages", "chat_completions", "openai_responses"]
        );

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    /// A Chat Completions reply with one text part.
    fn chat_text_reply(text: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    /// A provider with two Models: one plain and one that thinks.
    fn provider_add_models(base_url: String, models: Vec<ModelSpec>) -> CommandCall {
        CommandCall::ProviderAdd(ProviderAdd {
            protocol: ProviderProtocol::ChatCompletions,
            base_url,
            api_key: "sk-test".to_owned(),
            models,
            default_model: Some("fast".to_owned()),
            thinking: None,
            vendor: None,
            discover: false,
        })
    }

    fn model_spec(name: &str, thinks: bool, tools: bool, levels: Vec<ThinkingLevel>) -> ModelSpec {
        ModelSpec {
            name: name.to_owned(),
            context_limit: 128_000,
            max_output: 8_192,
            reads_images: false,
            thinks,
            tools,
            thinking_levels: levels,
        }
    }

    /// Serves one small models.dev catalog with an ETag.
    async fn fake_catalog() -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::http::header::{CONTENT_TYPE, ETAG, IF_NONE_MATCH};
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;

        let body = json!({
            "fake": {
                "id": "fake",
                "name": "Fake",
                "npm": "@ai-sdk/openai-compatible",
                "api": "http://127.0.0.1:9/v1",
                "env": ["FAKE_KEY"],
                "models": {
                    "fake-tool": {
                        "id": "fake-tool",
                        "name": "Fake Tool",
                        "reasoning": true,
                        "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
                        "tool_call": true,
                        "attachment": false,
                        "limit": {"context": 1000, "output": 100},
                        "release_date": "2026-01-01",
                    },
                    "fake-plain": {
                        "id": "fake-plain",
                        "name": "Fake Plain",
                        "tool_call": false,
                        "attachment": false,
                        "limit": {"context": 1000, "output": 100},
                    },
                },
            },
            "unsupported": {
                "id": "unsupported",
                "name": "Unsupported",
                "npm": "@ai-sdk/google",
                "models": {
                    "other": {
                        "id": "other",
                        "name": "Other",
                        "tool_call": true,
                        "limit": {"context": 1000, "output": 100},
                    },
                },
            },
        })
        .to_string();
        let app = Router::new().route(
            "/api.json",
            get(move |headers: axum::http::HeaderMap| {
                let body = body.clone();
                async move {
                    let if_none_match = headers
                        .get(IF_NONE_MATCH)
                        .and_then(|value| value.to_str().ok());
                    if if_none_match == Some("\"v1\"") {
                        return StatusCode::NOT_MODIFIED.into_response();
                    }
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .header(ETAG, "\"v1\"")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/api.json"), task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_catalog_gates_and_connects_a_known_vendor() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;

        let providers = command(&mut socket, Id::new(), CommandCall::CatalogList).await;
        let providers = providers.as_array().unwrap();
        assert!(providers.len() > 300, "the seed is the whole catalog");
        let openai = providers
            .iter()
            .find(|provider| provider["id"] == "openai")
            .unwrap();
        assert_eq!(openai["selectable"], true);
        assert_eq!(openai["protocol"], "openai_responses");
        let google = providers
            .iter()
            .find(|provider| provider["id"] == "google")
            .unwrap();
        assert_eq!(google["selectable"], false);
        assert!(google["protocol"].is_null());
        let templated = providers
            .iter()
            .find(|provider| provider["id"] == "databricks")
            .unwrap();
        assert_eq!(
            templated["selectable"], false,
            "a templated base URL needs an explicit override"
        );
        assert!(templated["api"].is_null());

        let no_tools = providers
            .iter()
            .find(|provider| provider["id"] == "meragpt")
            .unwrap();
        assert_eq!(no_tools["selectable"], false);
        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "meragpt".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: None,
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert!(matches!(error, Err((ErrorCode::InvalidArgument, _))));

        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "databricks".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: None,
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert!(
            matches!(error, Err((ErrorCode::InvalidArgument, _))),
            "a templated vendor asks for --base-url"
        );

        let connected = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "openai".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: None,
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert_eq!(connected["protocol"], "openai_responses");
        assert_eq!(connected["base_url"], "https://api.openai.com/v1");
        let models = connected["models"].as_array().unwrap();
        assert!(!models.is_empty());
        assert!(models.iter().all(|model| model["tools"].is_boolean()));

        let providers = command(&mut socket, Id::new(), CommandCall::ProviderList).await;
        assert_eq!(providers[0]["vendor"], "openai");

        let overridden = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "databricks".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: Some("https://example.test/v1".to_owned()),
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert_eq!(overridden["base_url"], "https://example.test/v1");

        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "google".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: None,
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert!(
            matches!(error, Err((ErrorCode::InvalidArgument, _))),
            "an unsupported vendor is visible but not selectable"
        );
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_catalog_refreshes_and_uses_its_cache() {
        let (base, catalog_task) = fake_catalog().await;
        let (_dir, running, invite) = running_with_config(&format!(
            "[catalog]\nurl = \"{base}\"\nrefresh_hours = 24\n"
        ))
        .await;
        let mut socket = admin_socket(&running, &invite).await;

        let status = command(&mut socket, Id::new(), CommandCall::CatalogStatus).await;
        assert_eq!(status["builtin"], true);

        let refreshed = command(&mut socket, Id::new(), CommandCall::CatalogRefresh).await;
        assert_eq!(refreshed["changed"], true);
        assert_eq!(refreshed["status"]["builtin"], false);
        assert_eq!(refreshed["status"]["source"], base);
        assert_eq!(refreshed["status"]["provider_count"], 2);
        assert_eq!(refreshed["status"]["model_count"], 3);
        assert!(refreshed["status"]["etag"].is_string());
        let cache = running.server_home().join("catalog").join("catalog.json");
        assert!(cache.is_file(), "the fetched catalog is cached");

        let second = command(&mut socket, Id::new(), CommandCall::CatalogRefresh).await;
        assert_eq!(second["changed"], false, "the ETag answers 304");

        let providers = command(&mut socket, Id::new(), CommandCall::CatalogList).await;
        let providers = providers.as_array().unwrap();
        let fake = providers.iter().find(|p| p["id"] == "fake").unwrap();
        assert_eq!(fake["selectable"], true);
        assert_eq!(fake["model_count"], 2);
        let unsupported = providers.iter().find(|p| p["id"] == "unsupported").unwrap();
        assert_eq!(unsupported["selectable"], false);

        let connected = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderConnect(ProviderConnect {
                vendor: "fake".to_owned(),
                api_key: "sk-test".to_owned(),
                base_url: None,
                protocol: None,
                thinking: None,
            }),
        )
        .await;
        assert_eq!(connected["protocol"], "chat_completions");
        let models = connected["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        let thinking = models
            .iter()
            .find(|model| model["name"] == "fake-tool")
            .unwrap();
        assert_eq!(thinking["thinks"], true);
        assert_eq!(thinking["tools"], true);
        assert_eq!(
            thinking["thinking_levels"],
            json!(["low", "high"]),
            "the catalog caps the accepted levels"
        );
        let models = command(&mut socket, Id::new(), CommandCall::ModelList).await;
        let default = models
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["is_default"] == true)
            .unwrap();
        assert_eq!(
            default["name"], "fake-tool",
            "a vendor defaults to a tool-calling Model"
        );

        socket.close(None).await.unwrap();
        catalog_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_discovers_and_refreshes_its_models() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::Router;
        use axum::routing::get;

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&calls);
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let seen = std::sync::Arc::clone(&seen);
                async move {
                    let names = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        vec!["m1", "m2"]
                    } else {
                        vec!["m1", "m2", "m3"]
                    };
                    axum::Json(json!({
                        "data": names
                            .into_iter()
                            .map(|name| json!({"id": name}))
                            .collect::<Vec<_>>(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let models_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let added = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderAdd(ProviderAdd {
                protocol: ProviderProtocol::ChatCompletions,
                base_url: format!("http://{addr}/v1"),
                api_key: "sk-test".to_owned(),
                models: Vec::new(),
                default_model: None,
                thinking: None,
                vendor: None,
                discover: true,
            }),
        )
        .await;
        let provider: Id = serde_json::from_value(added["provider"].clone()).unwrap();
        let names: Vec<&str> = added["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["m1", "m2"]);

        let refreshed = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderRefresh { provider },
        )
        .await;
        let models = refreshed["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[2]["name"], "m3");
        assert_eq!(models[0]["name"], "m1");
        assert_eq!(
            models[0]["id"], added["models"][0]["id"],
            "a still-present Model keeps its identifier"
        );

        // Refreshing a Provider that is not the Space default must not take
        // the default away from the Provider that holds it.
        command(
            &mut socket,
            Id::new(),
            provider_add_models(
                format!("http://{addr}/v1"),
                vec![model_spec("other", false, true, Vec::new())],
            ),
        )
        .await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderRefresh { provider },
        )
        .await;
        let models = command(&mut socket, Id::new(), CommandCall::ModelList).await;
        let default = models
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["is_default"] == true)
            .unwrap();
        assert_eq!(default["name"], "other");

        socket.close(None).await.unwrap();
        models_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_without_discovery_is_declared_manually() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::get;

        let app = Router::new().route(
            "/v1/models",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "no list") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderAdd(ProviderAdd {
                protocol: ProviderProtocol::ChatCompletions,
                base_url: format!("http://{addr}/v1"),
                api_key: "sk-test".to_owned(),
                models: Vec::new(),
                default_model: None,
                thinking: None,
                vendor: None,
                discover: true,
            }),
        )
        .await;
        let Err((code, message)) = error else {
            panic!("a failed discovery must fail");
        };
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(
            message.contains("--model"),
            "a failed discovery asks for manual Models: {message}"
        );
        let providers = command(&mut socket, Id::new(), CommandCall::ProviderList).await;
        assert!(providers.as_array().unwrap().is_empty());

        let added = command(
            &mut socket,
            Id::new(),
            provider_add_models(
                format!("http://{addr}/v1"),
                vec![model_spec("manual", false, true, Vec::new())],
            ),
        )
        .await;
        assert_eq!(added["models"][0]["name"], "manual");

        socket.close(None).await.unwrap();
        task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_responses_provider_refreshes_its_discovered_models() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new().route(
            "/v1/models",
            get(|| async {
                axum::Json(json!({
                    "data": [{"id": "one"}, {"id": "two"}],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;

        let added = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderAdd(ProviderAdd {
                protocol: ProviderProtocol::OpenAiResponses,
                base_url: format!("http://{addr}/v1"),
                api_key: "sk-test".to_owned(),
                models: Vec::new(),
                default_model: None,
                thinking: None,
                vendor: None,
                discover: true,
            }),
        )
        .await;
        let provider: Id = serde_json::from_value(added["provider"].clone()).unwrap();
        let refreshed = command(
            &mut socket,
            Id::new(),
            CommandCall::ProviderRefresh { provider },
        )
        .await;
        let names: Vec<&str> = refreshed["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["one", "two"]);

        socket.close(None).await.unwrap();
        task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_uses_the_model_and_thinking_its_owner_chose() {
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let (base, provider_task, recorded) = protocol_provider(
            ProviderProtocol::ChatCompletions,
            vec![chat_text_reply("done"), chat_text_reply("done again")],
        )
        .await;
        let added = command(
            &mut socket,
            Id::new(),
            provider_add_models(
                base,
                vec![
                    model_spec("fast", false, true, Vec::new()),
                    model_spec(
                        "thinker",
                        true,
                        true,
                        vec![ThinkingLevel::Low, ThinkingLevel::High],
                    ),
                    model_spec("plain", false, false, Vec::new()),
                ],
            ),
        )
        .await;
        let models = added["models"].as_array().unwrap();
        let thinker: Id = serde_json::from_value(
            models
                .iter()
                .find(|model| model["name"] == "thinker")
                .unwrap()["id"]
                .clone(),
        )
        .unwrap();
        let plain: Id = serde_json::from_value(
            models
                .iter()
                .find(|model| model["name"] == "plain")
                .unwrap()["id"]
                .clone(),
        )
        .unwrap();

        let bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
                thinking: Some(ThinkingLevel::High),
                model: Some(thinker),
            }),
        )
        .await;
        let bot_id: Id = serde_json::from_value(bot["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
        assert_eq!(bot["bot"]["model"], json!(thinker));
        send(&mut socket, chat, "hello").await;
        for _ in 0..500 {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let body = recorded.lock().unwrap().pop().unwrap();
        assert_eq!(body["model"], "thinker");
        assert_eq!(body["reasoning_effort"], "high");

        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot: bot_id }).await;
        assert_eq!(shown["bot"]["model"], json!(thinker));
        assert_eq!(shown["bot"]["thinking"], "high");

        let updated = command(
            &mut socket,
            Id::new(),
            CommandCall::BotUpdate(BotUpdate {
                bot: bot_id,
                full_access: None,
                full_access_consent: None,
                grants: None,
                grant_add: None,
                grant_remove: None,
                model: None,
                thinking: Some(ThinkingLevel::Low),
            }),
        )
        .await;
        assert_eq!(updated["thinking"], "low");
        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotUpdate(BotUpdate {
                bot: bot_id,
                full_access: None,
                full_access_consent: None,
                grants: None,
                grant_add: None,
                grant_remove: None,
                model: None,
                thinking: Some(ThinkingLevel::Minimal),
            }),
        )
        .await;
        assert!(
            matches!(error, Err((ErrorCode::InvalidArgument, _))),
            "a level the Model does not offer is refused"
        );

        let default_bot = command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Default".to_owned(),
                computer: None,
                thinking: None,
                model: None,
            }),
        )
        .await;
        let default_chat: Id = serde_json::from_value(default_bot["chat"]["id"].clone()).unwrap();
        send(&mut socket, default_chat, "hello").await;
        for _ in 0..500 {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let body = recorded.lock().unwrap().pop().unwrap();
        assert_eq!(body["model"], "fast");
        assert!(
            body.get("reasoning_effort").is_none(),
            "the default Model cannot think"
        );

        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotCreate(BotCreate {
                name: "Plain".to_owned(),
                computer: None,
                thinking: None,
                model: Some(plain),
            }),
        )
        .await;
        assert!(
            matches!(error, Err((ErrorCode::InvalidArgument, _))),
            "a Model without tools is never chosen for a Bot"
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
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
                thinking: None,

                model: None,
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
                    thinking: None,

                    model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
                thinking: None,

                model: None,
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
    async fn a_bot_file_message_names_the_run_that_sent_it() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (bot, chat, _computer) = create_bot(&mut socket).await;
        let run = Id::new();
        let acting = {
            let state = running.server.state.lock().unwrap();
            state.bots[&bot].owner
        };
        let bot_token = {
            let mut state = running.server.state.lock().unwrap();
            state.issue_bot_token(crate::state::BotToken {
                bot,
                chat,
                run,
                acting,
                expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
            })
        };
        let client = reqwest::Client::new();
        let uploaded: Value = client
            .post(format!("{}/api/bot/files", running.url()))
            .query(&[("name", "out.txt"), ("media_type", "text/plain")])
            .bearer_auth(&bot_token)
            .body("from the bot".to_owned())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let file: Id = serde_json::from_value(uploaded["id"].clone()).unwrap();

        // The Bot's run token posts the File Message, which names its run.
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
            let view = state
                .messages
                .iter()
                .find(|view| view.id == message)
                .expect("the File Message is recorded");
            assert_eq!(view.author, Author::Bot(bot));
            assert_eq!(view.file, Some(file));
            assert_eq!(
                view.run,
                Some(run),
                "a Bot's File Message names the run that sent it"
            );
        }

        // The Chat read the Web client follows carries the same binding.
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let wire = messages
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["id"] == json!(message))
            .expect("the File Message is readable");
        assert_eq!(wire["author"]["kind"], "bot");
        assert_eq!(wire["file"], json!(file));
        assert_eq!(wire["run"], json!(run));

        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_from_before_runs_were_named_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_home(dir.path());
        let first = start(root.clone(), 0).await.unwrap();
        let invite = first.invite.clone().unwrap();
        let (mut socket, _token) = admin(&first, &invite).await;
        let (bot, chat, _computer) = create_bot(&mut socket).await;
        let owner = {
            let state = first.server.state.lock().unwrap();
            state.bots[&bot].owner
        };
        socket.close(None).await.unwrap();
        first.shutdown().await;

        // A record written before a Message named its Run holds no run.
        let message = Id::new();
        let store = Store::open(&root).await.unwrap();
        store
            .append(AppendRequest::new(
                Id::new(),
                "legacy-message",
                vec![EventDraft::new(
                    EventKind::MessageAdded,
                    Author::User(owner),
                    json!({"message": message, "chat": chat, "text": "old words"}),
                )],
            ))
            .await
            .unwrap();
        store.close().await.unwrap();

        let second = start(root.clone(), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let read = messages
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["id"] == json!(message))
            .expect("the old Message is readable");
        assert_eq!(read["text"], "old words");
        assert_eq!(read["run"], Value::Null);

        socket.close(None).await.unwrap();
        second.shutdown().await;
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
            thinking: None,

            model: None,
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
            blobs: &server.blobs,
            secrets: &server.secrets,
            identity: &server.identity,
            links: &server.links,
            pairings: &server.pairings,
            approvals: &server.approvals,
            library: &server.library,
            runs: &server.runs,
            events: &server.events,
            catalog: &server.catalog,
            plugins: &server.plugins,
            signins: &server.signins,
            signin_start: &server.signin_start,
            oauth: &server.oauth,
            browser_origin: server.browser_origin(),
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

    fn add_script_rule(
        bot: Id,
        effect: neha::record::RuleEffect,
        skill: &str,
        script: &str,
    ) -> CommandCall {
        CommandCall::RuleAdd {
            bot,
            effect,
            tool: "run_skill_script".to_owned(),
            command_prefix: None,
            path_prefix: None,
            skill: Some(skill.to_owned()),
            script: Some(script.to_owned()),
            server: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
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
            skill: None,
            script: None,
            server: None,
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

            model: None,
            thinking: None,
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
            blobs: &server.blobs,
            secrets: &server.secrets,
            identity: &server.identity,
            links: &server.links,
            pairings: &server.pairings,
            approvals: &server.approvals,
            library: &server.library,
            runs: &server.runs,
            events: &server.events,
            catalog: &server.catalog,
            plugins: &server.plugins,
            signins: &server.signins,
            signin_start: &server.signin_start,
            oauth: &server.oauth,
            browser_origin: server.browser_origin(),
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

                model: None,
                thinking: None,
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
                continuation: None,
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
                continuation: None,
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

                model: None,
                thinking: None,
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
                ServerFrame::Heartbeat { .. } | ServerFrame::Event { .. } => {}
                other => panic!("expected a snapshot, got {other:?}"),
            }
        }
    }

    /// Reads the live events a subscribed socket carries until the stream is
    /// quiet for a moment. A heartbeat is not an event.
    async fn live_events(socket: &mut Socket) -> Vec<neha::record::Event> {
        let mut events = Vec::new();
        while let Ok(frame) =
            tokio::time::timeout(std::time::Duration::from_millis(300), next_frame(socket)).await
        {
            match frame {
                ServerFrame::Event { event } => events.push(event),
                ServerFrame::Heartbeat { .. } => {}
                other => panic!("expected a stream frame, got {other:?}"),
            }
        }

        events
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

            model: None,
            thinking: None,
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

                model: None,
                thinking: None,
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

                model: None,
                thinking: None,
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
    /// The `SkillAdd` that adds one skill with its scripts.
    fn add_skill(bot: Option<Id>, document: &str, scripts: &[(&str, &str)]) -> CommandCall {
        CommandCall::SkillAdd(SkillAdd {
            library: if bot.is_some() {
                SkillLibrary::Bot
            } else {
                SkillLibrary::Space
            },
            bot,
            document: document.to_owned(),
            scripts: scripts
                .iter()
                .map(|(name, content)| SkillAddScript {
                    name: (*name).to_owned(),
                    content: (*content).to_owned(),
                })
                .collect(),
        })
    }

    /// The tool names in one recorded Model request.
    fn tool_names(request: &Value) -> Vec<String> {
        request["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The system text in one recorded Model request.
    fn system_text(request: &Value) -> String {
        request["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "system")
                    .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_skill_enters_the_bot_context_only_when_enabled() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task, recorded) =
            scripted_provider_recording(vec![text("done"), text("done")]).await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: weekly-report\ndescription: Write the weekly status report.\n---\nCollect the docs.",
                &[],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        // The Space's library holds it; the Bot has not enabled it.
        let listed = command(
            &mut socket,
            Id::new(),
            CommandCall::SkillList { bot: Some(bot) },
        )
        .await;
        assert_eq!(listed, json!([]), "an added skill is not enabled");
        let queued = submit(&mut socket, chat, "hello", None).await;
        finish(&mut socket, chat, queued).await;
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let names = tool_names(&requests[0]);
            assert!(
                !names.iter().any(|name| name == "skill"),
                "a Bot without skills is offered no skill tool: {names:?}"
            );
            assert!(!system_text(&requests[0]).contains("weekly-report"));
        }

        let enabled = command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        assert_eq!(enabled["enabled"], json!(true));
        let queued = submit(&mut socket, chat, "again", None).await;
        finish(&mut socket, chat, queued).await;
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let names = tool_names(&requests[1]);
            assert!(
                names.iter().any(|name| name == "skill"),
                "an enabled skill offers the skill tool: {names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "run_skill_script"),
                "a skill without scripts offers no script tool: {names:?}"
            );
            let system = system_text(&requests[1]);
            assert!(
                system.contains("weekly-report: Write the weekly status report."),
                "{system}"
            );
        }

        // Disabling takes the skill's name and tool away again.
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillDisable { bot, skill },
        )
        .await;
        let queued = submit(&mut socket, chat, "third", None).await;
        finish(&mut socket, chat, queued).await;
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(!tool_names(&requests[2]).iter().any(|name| name == "skill"));
            assert!(!system_text(&requests[2]).contains("weekly-report"));
        }
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_loads_a_skill_body_without_an_approval() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let body = "1. Collect the docs.\n2. Summarize under a page.";
        let (base, provider_task, recorded) = scripted_provider_recording(vec![
            tool("skill", json!({"name": "weekly-report"})),
            text("done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                &format!(
                    "---\nname: weekly-report\ndescription: Write the weekly status report.\n---\n{body}"
                ),
                &[],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;

        let queued = submit(&mut socket, chat, "run it", None).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert!(
            approvals_in(&mut socket, None).await.is_empty(),
            "loading a skill touches no Computer and asks nobody"
        );
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let tool_messages: Vec<&Value> = requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            let text = tool_messages
                .last()
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            assert!(
                text.contains(body),
                "the skill's body reaches the Model: {text}"
            );
            assert!(
                !text.contains("description:"),
                "the frontmatter stays out of the body: {text}"
            );
        }
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_asks_before_its_first_run_and_always_allow_covers_it() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task, recorded) = scripted_provider_recording(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["world"]}),
            ),
            text("done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["again"]}),
            ),
            text("second done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun the script.",
                &[("hello.sh", "printf 'hello %s' \"$1\"")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;

        let queued = submit(&mut socket, chat, "run it", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["reason"], "first_script_run");
        assert_eq!(approvals[0]["tool"], "run_skill_script");
        assert_eq!(approvals[0]["arguments"]["skill"], "hello");
        assert_eq!(approvals[0]["arguments"]["script"], "hello.sh");
        let summary = approvals[0]["summary"].as_str().unwrap_or_default();
        assert!(
            summary.contains("hello") && summary.contains("hello.sh"),
            "the card names the skill and the script it will run: {summary}"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        let decided = decide_approval(&mut socket, approval, true, true).await;
        assert_eq!(decided["status"], "allowed");
        assert!(
            decided["rule"].is_string(),
            "Always allow writes a rule: {decided}"
        );
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");
        {
            // The script's own output is the action's result.
            let requests = recorded.lock().unwrap();
            let results: Vec<&str> = requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .filter_map(|message| message["content"].as_str())
                .collect();
            assert!(
                results.iter().any(|text| text.contains("hello world")),
                "the script ran with its argument: {results:?}"
            );
        }

        // The rule names the skill and its script.
        let rules = approvals_of(&mut socket, bot).await;
        let rule = rules
            .iter()
            .find(|rule| rule["rule"] == decided["rule"])
            .unwrap_or_else(|| panic!("the derived rule is listed: {rules:?}"));
        assert_eq!(rule["tool"], "run_skill_script");
        assert_eq!(rule["skill_name"], "hello");
        assert_eq!(rule["script"], "hello.sh");
        assert_eq!(rule["effect"], "allow");

        // The second run needs no answer.
        let second = submit(&mut socket, chat, "again", None).await;
        let second_run = run_of(&second);
        let finished = wait_run(&mut socket, second_run).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert!(
            approvals_in(&mut socket, Some(chat)).await.is_empty(),
            "a lasting rule answers the second run"
        );
        assert_eq!(last_message(&mut socket, chat).await["text"], "second done");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_script_call_marks_no_run_and_breaks_no_replay() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            // The Model names the pair but adds a field the tool refuses.
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "surprise": true}),
            ),
            text("first done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["one"]}),
            ),
            text("second done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun the script.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;

        // The refused call is recorded as an action and stops nothing: the
        // record still projects, so the run finishes.
        let queued = submit(&mut socket, chat, "run it", None).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert_eq!(last_message(&mut socket, chat).await["text"], "first done");

        // It also marks no run: the pair's first real run still asks.
        let queued = submit(&mut socket, chat, "run it properly", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(
            approvals[0]["reason"], "first_script_run",
            "a refused call is not a first run: {approvals:?}"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_ask_rule_for_a_script_asks_every_run() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["one"]}),
            ),
            text("first done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["two"]}),
            ),
            text("second done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": ["three"]}),
            ),
            text("third done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun the script.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let ask = command(
            &mut socket,
            Id::new(),
            add_script_rule(bot, neha::record::RuleEffect::Ask, "hello", "hello.sh"),
        )
        .await;
        let ask: Id = serde_json::from_value(ask["rule"].clone()).unwrap();

        // A Space-served call has its own approvals on every Computer kind.
        let queued = submit(&mut socket, chat, "run it", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "rule", "{approvals:?}");
        assert_eq!(approvals[0]["rule"], json!(ask));
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");

        // Allow once leaves the Ask rule: the next run asks again.
        let queued = submit(&mut socket, chat, "again", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "rule", "{approvals:?}");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        let decided = decide_approval(&mut socket, approval, true, true).await;
        assert_eq!(decided["overridden"], json!(ask));
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");

        // The covered Ask rule is gone, so the next run starts without a card.
        let queued = submit(&mut socket, chat, "one more", None).await;
        let run = run_of(&queued);
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        assert!(
            approvals_in(&mut socket, Some(chat)).await.is_empty(),
            "no rule asks after Always allow covers the Ask rule"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_script_run_asks_once_and_its_rule_answers_later_runs() {
        let (_dir, running, invite) = running().await;
        let mut local = local_bot(
            &running,
            &invite,
            vec![
                tool("shell", json!({"command": "pwd"})),
                text("first done"),
                tool(
                    "run_skill_script",
                    json!({"skill": "hello", "script": "hello.sh", "arguments": ["one"]}),
                ),
                text("script done"),
                tool(
                    "run_skill_script",
                    json!({"skill": "hello", "script": "hello.sh", "arguments": ["two"]}),
                ),
                text("second script done"),
                tool(
                    "run_skill_script",
                    json!({"skill": "hello", "script": "hello.sh", "arguments": ["three"]}),
                ),
                text("third script done"),
                tool(
                    "run_skill_script",
                    json!({"skill": "hello", "script": "hello.sh", "arguments": ["four"]}),
                ),
                text("fourth script done"),
            ],
        )
        .await;
        let added = command(
            &mut local.socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun the script.",
                &[("hello.sh", "printf 'hello %s' \"$1\"")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut local.socket,
            Id::new(),
            CommandCall::BotSkillEnable {
                bot: local.bot,
                skill,
            },
        )
        .await;

        // The machine's first use asks first; one Allow answers it.
        let first = run_of(&submit(&mut local.socket, local.chat, "pwd", None).await);
        let approval = approval_for_run(&mut local.socket, first).await;
        assert_eq!(approval["reason"], "first_use_local");
        let approval: Id = serde_json::from_value(approval["approval"].clone()).unwrap();
        decide_approval(&mut local.socket, approval, true, false).await;
        let action = answer_next_action(&mut local.device, "/home/ada/.neha/workspace").await;
        assert!(matches!(action, WireAction::Shell { .. }));
        assert_eq!(
            wait_run(&mut local.socket, first).await["status"],
            "succeeded"
        );

        // The script's own first run asks, and Always allow makes its rule.
        let second = run_of(&submit(&mut local.socket, local.chat, "run the script", None).await);
        wait_status(&mut local.socket, second, "waiting").await;
        let approvals = approvals_in(&mut local.socket, Some(local.chat)).await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["reason"], "first_script_run");
        assert_eq!(approvals[0]["arguments"]["skill"], "hello");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval, true, true).await;
        assert!(decided["rule"].is_string(), "{decided}");
        // The script the Space delivered is the one the skill carries.
        let action = answer_next_action(&mut local.device, "hello one").await;
        match action {
            WireAction::RunScript {
                name,
                content,
                arguments,
            } => {
                assert_eq!(name, "hello.sh");
                assert_eq!(content, "printf 'hello %s' \"$1\"");
                assert_eq!(arguments, vec!["one".to_owned()]);
            }
            other => panic!("expected a script action, got {other:?}"),
        }
        assert_eq!(
            wait_run(&mut local.socket, second).await["status"],
            "succeeded"
        );

        // The lasting rule answers the next run: it starts without a card.
        let third = run_of(&submit(&mut local.socket, local.chat, "run it again", None).await);
        let action = answer_next_action(&mut local.device, "hello two").await;
        assert!(matches!(action, WireAction::RunScript { .. }));
        assert_eq!(
            wait_run(&mut local.socket, third).await["status"],
            "succeeded"
        );
        assert!(
            approvals_in(&mut local.socket, Some(local.chat))
                .await
                .is_empty(),
            "the rule the first script run made answers later runs"
        );
        assert_eq!(
            last_message(&mut local.socket, local.chat).await["text"],
            "second script done"
        );

        // An Ask rule for the same pair asks, and the answer names it: the
        // Allow rule it makes covers that Ask rule, so Neha removes it.
        let ask = command(
            &mut local.socket,
            Id::new(),
            add_script_rule(
                local.bot,
                neha::record::RuleEffect::Ask,
                "hello",
                "hello.sh",
            ),
        )
        .await;
        let ask: Id = serde_json::from_value(ask["rule"].clone()).unwrap();
        let fourth = run_of(&submit(&mut local.socket, local.chat, "again", None).await);
        wait_status(&mut local.socket, fourth, "waiting").await;
        let approvals = approvals_in(&mut local.socket, Some(local.chat)).await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["reason"], "rule", "{approvals:?}");
        assert_eq!(approvals[0]["rule"], json!(ask));
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        let decided = decide_approval(&mut local.socket, approval, true, true).await;
        assert_eq!(decided["overridden"], json!(ask), "{decided}");
        let action = answer_next_action(&mut local.device, "hello three").await;
        assert!(matches!(action, WireAction::RunScript { .. }));
        assert_eq!(
            wait_run(&mut local.socket, fourth).await["status"],
            "succeeded"
        );
        let rules = approvals_of(&mut local.socket, local.bot).await;
        assert!(
            rules.iter().all(|rule| rule["rule"] != json!(ask)),
            "the covered Ask rule is removed: {rules:?}"
        );

        // The next run starts without a card: the Ask rule is gone and the
        // Allow rule answers for the pair.
        let fifth = run_of(&submit(&mut local.socket, local.chat, "one more", None).await);
        let action = answer_next_action(&mut local.device, "hello four").await;
        assert!(matches!(action, WireAction::RunScript { .. }));
        assert_eq!(
            wait_run(&mut local.socket, fifth).await["status"],
            "succeeded"
        );
        assert!(
            approvals_in(&mut local.socket, Some(local.chat))
                .await
                .is_empty(),
            "no rule asks any more"
        );
        local.device.close(None).await.unwrap();
        local.provider_task.abort();
        local.socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_skill_cannot_be_added_twice_or_from_another_user() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let call = add_skill(
            None,
            "---\nname: weekly-report\ndescription: Write the weekly report.\n---\nBody.",
            &[],
        );
        command(&mut socket, Id::new(), call.clone()).await;
        let (code, message) = try_command(&mut socket, Id::new(), call).await.unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(message.contains("already holds"), "{message}");

        // A document that is not a skill is refused before anything is
        // recorded, and the Space says which rule it broke.
        let (code, message) = try_command(
            &mut socket,
            Id::new(),
            add_skill(None, "---\nname: Bad Name\ndescription: d\n---\n", &[]),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(message.contains("skill name"), "{message}");

        // A script name that is not one path component is refused too.
        let (code, _message) = try_command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: escaped\ndescription: d\n---\n",
                &[("../escape.sh", "true")],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_add_asks_for_a_scripts_pair() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        // The tool alone names nothing to match.
        let (code, message) = try_command(
            &mut socket,
            Id::new(),
            add_rule(
                bot,
                neha::record::RuleEffect::Ask,
                "run_skill_script",
                None,
                None,
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(message.contains("names its skill"), "{message}");
        // A pair names an installed skill and one of its scripts.
        let (code, _message) = try_command(
            &mut socket,
            Id::new(),
            add_script_rule(bot, neha::record::RuleEffect::Ask, "hello", "hello.sh"),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::NotFound);
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let (code, message) = try_command(
            &mut socket,
            Id::new(),
            add_script_rule(bot, neha::record::RuleEffect::Ask, "hello", "other.sh"),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(message.contains("carries no such script"), "{message}");
        let added_rule = command(
            &mut socket,
            Id::new(),
            add_script_rule(bot, neha::record::RuleEffect::Allow, "hello", "hello.sh"),
        )
        .await;
        let rule: Id = serde_json::from_value(added_rule["rule"].clone()).unwrap();
        let rules = approvals_of(&mut socket, bot).await;
        assert!(
            rules.iter().any(|view| view["rule"] == json!(rule)
                && view["skill"] == json!(skill)
                && view["skill_name"] == "hello"
                && view["script"] == "hello.sh"),
            "the rule names the installed skill and its script: {rules:?}"
        );
        // A pair on a tool that is not run_skill_script is refused.
        let (code, message) = try_command(
            &mut socket,
            Id::new(),
            CommandCall::RuleAdd {
                bot,
                effect: neha::record::RuleEffect::Ask,
                tool: "shell".to_owned(),
                command_prefix: None,
                path_prefix: None,
                skill: Some("hello".to_owned()),
                script: Some("hello.sh".to_owned()),
                server: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(message.contains("no other tool"), "{message}");
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_skill_takes_it_away_from_every_bot() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (bot, _chat, _computer) =
            tool_bot(&mut socket, "http://127.0.0.1:9/v1".to_owned()).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(None, "---\nname: shared\ndescription: d\n---\nBody.", &[]),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(shown["bot"]["skills"], json!([skill]));

        command(&mut socket, Id::new(), CommandCall::SkillRemove { skill }).await;
        let shown = command(&mut socket, Id::new(), CommandCall::BotShow { bot }).await;
        assert_eq!(
            shown["bot"]["skills"],
            json!([]),
            "the Bot forgets a removed skill"
        );
        let listed = command(&mut socket, Id::new(), CommandCall::SkillList { bot: None }).await;
        assert_eq!(listed, json!([]));
        // Removing it again names no skill rather than throwing.
        let (code, _message) =
            try_command(&mut socket, Id::new(), CommandCall::SkillRemove { skill })
                .await
                .unwrap_err();
        assert_eq!(code, ErrorCode::NotFound);
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }
    /// Reads one command's answer without assuming the order two commands
    /// answered in.
    async fn settled(socket: &mut Socket, wanted: Id) -> Result<Value, (ErrorCode, String)> {
        loop {
            match next_frame(socket).await {
                ServerFrame::Confirmed { id, result } if id == wanted => return Ok(result),
                ServerFrame::Error {
                    id: Some(id),
                    code,
                    message,
                } if id == wanted => return Err((code, message)),
                ServerFrame::Heartbeat { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Snapshot { .. }
                | ServerFrame::Accepted { .. }
                | ServerFrame::Committed { .. }
                | ServerFrame::Confirmed { .. }
                | ServerFrame::Error { .. } => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    /// The bytes the Space holds under one access rule, as a count.
    fn staged_blob_count(running: &Running) -> usize {
        std::fs::read_dir(running.server.home.blobs_dir())
            .map(|entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| std::fs::read_dir(entry.path()).map_or(0, |files| files.count()))
                    .sum()
            })
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_the_skill_lacks_never_asks() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "missing.sh", "arguments": []}),
            ),
            text("done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;

        let queued = submit(&mut socket, chat, "run it", None).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert!(
            approvals_in(&mut socket, Some(chat)).await.is_empty(),
            "a script the skill does not carry asks nobody"
        );
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answered_card_never_runs_a_replacement_script() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("ran the replacement"),
        ])
        .await;
        let (bot, chat, computer) = tool_bot(&mut socket, base.clone()).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'old' > result.txt")],
            ),
        )
        .await;
        let first_skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable {
                bot,
                skill: first_skill,
            },
        )
        .await;

        let queued = submit(&mut socket, chat, "run it", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "first_script_run");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();

        // The card waits while the skill it named is replaced under the same
        // names by another script.
        command(
            &mut socket,
            Id::new(),
            CommandCall::SkillRemove { skill: first_skill },
        )
        .await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'new' > result.txt")],
            ),
        )
        .await;
        let second_skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable {
                bot,
                skill: second_skill,
            },
        )
        .await;
        // The run keeps the Computer it began on, and that workspace is where
        // either script would write.
        let workspace = running
            .server
            .state
            .lock()
            .unwrap()
            .computers
            .get(&computer)
            .map(|view| view.workspace.clone())
            .unwrap();

        decide_approval(&mut socket, approval, true, false).await;
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert!(
            !std::path::Path::new(&workspace).join("result.txt").exists(),
            "no replacement script ran under the old card"
        );
        let messages = command(
            &mut socket,
            Id::new(),
            CommandCall::ChatRead { chat, from: None },
        )
        .await;
        let text = messages.as_array().unwrap().last().unwrap()["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(text, "done");

        // The replacement never ran, so it must ask before its first run
        // instead of inheriting the old skill's consent.
        let queued = submit(&mut socket, chat, "run the replacement", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let latest = approvals.last().expect("the replacement asks");
        assert_eq!(
            latest["reason"], "first_script_run",
            "the replacement asks for its own first run: {approvals:?}"
        );
        assert_eq!(latest["arguments"]["script"], "hello.sh");
        let approval: Id = serde_json::from_value(latest["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, true).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&workspace).join("result.txt")).unwrap(),
            "new",
            "the replacement runs under its own card"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_script_call_survives_a_restart() {
        let (directory, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "surprise": true}),
            ),
            text("first done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("second done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let queued = submit(&mut socket, chat, "run it", None).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        socket.close(None).await.unwrap();
        provider_task.abort();

        // The record replays: the refused call it holds projects as before.
        let root = running.server_home();
        running.shutdown().await;
        let (second_base, second_provider) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("replayed done"),
        ])
        .await;
        let second = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut socket = open_socket(&second, &token).await;
        command(&mut socket, Id::new(), provider_add(second_base)).await;
        let _ = directory;
        let skills = command(&mut socket, Id::new(), CommandCall::SkillList { bot: None }).await;
        assert_eq!(skills.as_array().map(Vec::len), Some(1), "{skills}");
        let queued = submit(&mut socket, chat, "run it again", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(
            approvals[0]["reason"], "first_script_run",
            "the replay still knows the refused call never ran: {approvals:?}"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        socket.close(None).await.unwrap();
        second_provider.abort();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_clients_cannot_add_the_same_skill_name() {
        let (_dir, running, invite) = running().await;
        let (mut first, token) = admin(&running, &invite).await;
        let mut second = open_socket(&running, &token).await;
        let call = add_skill(
            None,
            "---\nname: shared\ndescription: One skill.\n---\nBody.",
            &[],
        );
        let (first_id, second_id) = (Id::new(), Id::new());
        send_command(&mut first, first_id, call.clone()).await;
        send_command(&mut second, second_id, call).await;
        let answered = tokio::join!(async { settled(&mut first, first_id).await }, async {
            settled(&mut second, second_id).await
        },);
        let ok = [&answered.0, &answered.1]
            .iter()
            .filter(|answer| answer.is_ok())
            .count();
        assert_eq!(ok, 1, "only one add wins: {answered:?}");
        let skills = command(&mut first, Id::new(), CommandCall::SkillList { bot: None }).await;
        assert_eq!(
            skills.as_array().map(Vec::len),
            Some(1),
            "the library holds one skill: {skills}"
        );
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_instructions_only_space_offers_no_scripts() {
        let (_dir, running, invite) =
            running_with_config("[skills]\ninstructions_only = true\n").await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task, recorded) = scripted_provider_recording(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'hello'")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let queued = submit(&mut socket, chat, "run it", None).await;
        let finished = finish(&mut socket, chat, queued).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        {
            let requests = recorded.lock().unwrap();
            let names = tool_names(&requests[0]);
            assert!(
                names.iter().any(|name| name == "skill")
                    && !names.iter().any(|name| name == "run_skill_script"),
                "an instructions-only Space offers the body but no script: {names:?}"
            );
        }
        assert!(
            approvals_in(&mut socket, Some(chat)).await.is_empty(),
            "no card is made for a script the Space will never run"
        );

        // The refused call is in the record, but it never ran. Turning the
        // setting off and restarting must still ask for the first real run.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
        std::fs::write(
            root.join("config.toml"),
            "[skills]\ninstructions_only = false\n",
        )
        .unwrap();
        let (second_base, second_provider, second_recorded) = scripted_provider_recording(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("ran"),
        ])
        .await;
        let second = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut second_socket = open_socket(&second, &token).await;
        command(&mut second_socket, Id::new(), provider_add(second_base)).await;
        let queued = submit(&mut second_socket, chat, "run it for real", None).await;
        let run = run_of(&queued);
        wait_status(&mut second_socket, run, "waiting").await;
        {
            let requests = second_recorded.lock().unwrap();
            // The first request of the run names the tools the Space offers.
            let names = tool_names(&requests[0]);
            assert!(
                names.iter().any(|name| name == "run_skill_script"),
                "a Space that runs scripts offers the tool: {names:?}"
            );
        }
        let approvals = approvals_in(&mut second_socket, Some(chat)).await;
        assert_eq!(
            approvals[0]["reason"], "first_script_run",
            "a call that never ran leaves the first run to ask: {approvals:?}"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut second_socket, approval, true, false).await;
        assert_eq!(
            wait_run(&mut second_socket, run).await["status"],
            "succeeded"
        );
        second_socket.close(None).await.unwrap();
        second_provider.abort();
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_script_does_not_consume_consent_after_a_switch_to_instructions_only() {
        let (directory, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("first done"),
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("second done"),
        ])
        .await;
        let (bot, chat, computer) = tool_bot(&mut socket, base.clone()).await;
        let added = command(
            &mut socket,
            Id::new(),
            add_skill(
                None,
                "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
                &[("hello.sh", "printf 'ran' > marker.txt")],
            ),
        )
        .await;
        let skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        let workspace = running
            .server
            .state
            .lock()
            .unwrap()
            .computers
            .get(&computer)
            .map(|view| view.workspace.clone())
            .unwrap();

        let queued = submit(&mut socket, chat, "run it", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();

        // The Admin turns scripts off while the card waits.
        let root = running.server_home();
        socket.close(None).await.unwrap();
        running.shutdown().await;
        std::fs::write(
            root.join("config.toml"),
            "[skills]\ninstructions_only = true\n",
        )
        .unwrap();
        let second = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&second, "Ada").await;
        let mut second_socket = open_socket(&second, &token).await;
        decide_approval(&mut second_socket, approval, true, false).await;
        assert_eq!(
            wait_run(&mut second_socket, run).await["status"],
            "succeeded",
            "the run ends rather than waiting forever"
        );
        assert!(
            !std::path::Path::new(&workspace).join("marker.txt").exists(),
            "an answer given before the switch never reaches a script"
        );
        // The withheld call recorded no identity, so the answer consumed no
        // first run, and the card, the action's start, and its result name one
        // action: a policy refusal does not replace the approved identifier.
        let events = second
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let of_run = |kind: EventKind| -> Vec<Value> {
            events
                .iter()
                .filter(|event| event.kind == kind && event.data["run"] == json!(run))
                .map(|event| event.data.clone())
                .collect()
        };
        let actions = started_actions(&second, run).await;
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].data["skill"], json!(null));
        assert_eq!(actions[0].data["tool"], "run_skill_script");
        let requested = of_run(EventKind::ApprovalRequested);
        let finished = of_run(EventKind::ActionFinished);
        assert_eq!(requested.len(), 1);
        assert_eq!(finished.len(), 1);
        assert_eq!(
            requested[0]["action"], actions[0].data["action"],
            "the card names the action that started"
        );
        assert_eq!(
            finished[0]["action"], actions[0].data["action"],
            "the result names the action that started"
        );

        // Turning scripts back on asks again for the still-unrun script.
        second_socket.close(None).await.unwrap();
        second.shutdown().await;
        std::fs::write(
            root.join("config.toml"),
            "[skills]\ninstructions_only = false\n",
        )
        .unwrap();
        let third = start(test_home(&root), 0).await.unwrap();
        let token = sign_in(&third, "Ada").await;
        let mut third_socket = open_socket(&third, &token).await;
        let queued = submit(&mut third_socket, chat, "run it again", None).await;
        let run = run_of(&queued);
        wait_status(&mut third_socket, run, "waiting").await;
        let approvals = approvals_in(&mut third_socket, Some(chat)).await;
        assert_eq!(
            approvals.last().unwrap()["reason"],
            "first_script_run",
            "the withheld answer consumed no consent: {approvals:?}"
        );
        let approval: Id =
            serde_json::from_value(approvals.last().unwrap()["approval"].clone()).unwrap();
        decide_approval(&mut third_socket, approval, true, true).await;
        assert_eq!(
            wait_run(&mut third_socket, run).await["status"],
            "succeeded"
        );
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&workspace).join("marker.txt")).unwrap(),
            "ran"
        );
        let _ = directory;
        third_socket.close(None).await.unwrap();
        provider_task.abort();
        third.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_enables_one_skill_under_each_name() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (base, provider_task) = scripted_provider(vec![
            tool(
                "run_skill_script",
                json!({"skill": "hello", "script": "hello.sh", "arguments": []}),
            ),
            text("done"),
        ])
        .await;
        let (bot, chat, _computer) = tool_bot(&mut socket, base).await;
        let document = "---\nname: hello\ndescription: Say hello.\n---\nRun it.";
        let shared = command(
            &mut socket,
            Id::new(),
            add_skill(None, document, &[("hello.sh", "printf 'a'")]),
        )
        .await;
        let shared: Id = serde_json::from_value(shared["skill"].clone()).unwrap();
        let own = command(
            &mut socket,
            Id::new(),
            add_skill(Some(bot), document, &[("hello.sh", "printf 'b'")]),
        )
        .await;
        let own: Id = serde_json::from_value(own["skill"].clone()).unwrap();
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill: shared },
        )
        .await;
        // A script call resolves one enabled skill, so a second enabled skill
        // under the same name is refused where it is enabled.
        let (code, message) = try_command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill: own },
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert!(
            message.contains("already has a skill under this name"),
            "{message}"
        );

        let queued = submit(&mut socket, chat, "run it", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        let actions = started_actions(&running, run).await;
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].data["skill"],
            json!(shared),
            "the call ran the one enabled skill"
        );
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");
        socket.close(None).await.unwrap();
        provider_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_account_cannot_add_a_skill_to_another_library() {
        let (_dir, running, invite) = running().await;
        let (mut admin_socket, _token) = admin(&running, &invite).await;
        let created = command(&mut admin_socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let grace_token = sign_in(&running, "Grace").await;
        let mut grace = open_socket(&running, &grace_token).await;

        let before = staged_blob_count(&running);
        let (code, message) = try_command(
            &mut grace,
            Id::new(),
            add_skill(
                None,
                "---\nname: shared\ndescription: One skill.\n---\nBody.",
                &[("run.sh", "printf 'x'")],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::Forbidden, "{message}");
        let (code, message) = try_command(
            &mut grace,
            Id::new(),
            add_skill(
                Some(bot),
                "---\nname: shared\ndescription: One skill.\n---\nBody.",
                &[("run.sh", "printf 'x'")],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(code, ErrorCode::Forbidden, "{message}");
        assert_eq!(
            staged_blob_count(&running),
            before,
            "a refused add stores no bytes"
        );
        let skills = command(&mut grace, Id::new(), CommandCall::SkillList { bot: None }).await;
        assert_eq!(skills, json!([]), "the refused adds recorded nothing");
        admin_socket.close(None).await.unwrap();
        grace.close(None).await.unwrap();
        running.shutdown().await;
    }
    /// One request the fake MCP server received.
    type McpLog = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

    /// Runs a fake MCP server on a loopback port, answering the handshake and
    /// listing one tool. Returns its URL and a log of what it was asked.
    async fn fake_mcp_server(
        tool_answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> (String, McpLog, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::body::{Body, Bytes};
        use axum::http::{HeaderMap, header::CONTENT_TYPE};
        use axum::response::Response;
        use axum::routing::post;

        let log: McpLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = log.clone();
        let answer = std::sync::Arc::new(tool_answer);
        let app = Router::new().route(
            "/",
            post(move |_headers: HeaderMap, body: Bytes| {
                let log = seen.clone();
                let answer = answer.clone();
                async move {
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(message.clone());
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let result = match message["method"].as_str() {
                        Some("initialize") => serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "docs", "version": "1"},
                        }),
                        Some("notifications/initialized") => return Response::new(Body::empty()),
                        Some("tools/list") => serde_json::json!({
                            "tools": [{
                                "name": "search",
                                "description": "Search the docs.",
                                "inputSchema": {"type": "object"},
                            }],
                        }),
                        Some("tools/call") => answer(&message),
                        _ => Value::Null,
                    };
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .header("mcp-session-id", "session-1")
                        .body(Body::from(
                            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
                                .to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{address}"), log, task)
    }

    /// Adds an MCP server to the Space under a name and returns its id.
    async fn add_mcp_server(
        socket: &mut Socket,
        name: &str,
        url: &str,
        credential: Option<&str>,
    ) -> Id {
        let result = command(
            socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: name.to_owned(),
                url: url.to_owned(),
                scope: neha::record::McpScope::Space,
                credential: credential.map(ToOwned::to_owned),
            }),
        )
        .await;
        assert_eq!(result["name"], json!(name));
        serde_json::from_value(result["server"].clone()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_calls_an_mcp_tool_after_one_answer() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, log, server_task) = fake_mcp_server(|call| {
            assert_eq!(call["params"]["name"], json!("search"));
            json!({"content": [{"type": "text", "text": "the answer"}]})
        })
        .await;
        let server = add_mcp_server(&mut socket, "docs", &url, Some("token-1")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
            tool("mcp__docs__search", json!({"query": "again"})),
            text("second done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;

        let queued = submit(&mut socket, chat, "search", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "first_mcp_call");
        assert_eq!(approvals[0]["server"], json!(server));
        assert_eq!(approvals[0]["computer"], json!(server));
        let summary = approvals[0]["summary"].as_str().unwrap();
        assert!(
            summary.contains("docs") && summary.contains("search"),
            "{summary}"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, true).await;
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded", "{finished}");
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");
        {
            let calls = log.lock().unwrap();
            let calls: Vec<&Value> = calls
                .iter()
                .filter(|message| message["method"] == "tools/call")
                .collect();
            assert_eq!(calls.len(), 1, "one tool call reached the server");
            assert_eq!(calls[0]["params"]["name"], json!("search"));
        }
        // The record names the server and the tool's original name.
        let events = started_actions(&running, run).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["server"], json!(server));
        assert_eq!(events[0].data["tool"], json!("search"));
        // The Always allow wrote a rule, so the next call asks nothing.
        let second = run_of(&submit(&mut socket, chat, "again", None).await);
        assert_eq!(wait_run(&mut socket, second).await["status"], "succeeded");
        assert_eq!(
            show_run(&mut socket, second).await["status"],
            "succeeded",
            "the second run ended without a card"
        );
        let calls = log
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message["method"] == "tools/call")
            .count();
        assert_eq!(calls, 2, "the rule let the second call through");
        socket.close(None).await.unwrap();
        provider_task.abort();
        server_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_mcp_tool_without_a_credential_is_not_connected() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, log, server_task) = fake_mcp_server(|_call| json!({"content": []})).await;
        let server = add_mcp_server(&mut socket, "docs", &url, None).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;

        let queued = submit(&mut socket, chat, "search", None).await;
        let run = run_of(&queued);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(
            wait_run(&mut socket, run).await["status"],
            "succeeded",
            "the run answers that the server is not connected"
        );
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .all(|message| message["method"] != "tools/call"),
            "an unconnected server is sent no call"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        server_task.abort();
        running.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn an_always_allowed_mcp_call_writes_a_rule_for_that_server() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, log, server_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "ok"}]})).await;
        let server = add_mcp_server(&mut socket, "docs", &url, Some("token-1")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;
        let run = run_of(&submit(&mut socket, chat, "search", None).await);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, true).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .any(|message| message["method"] == "tools/call")
        );
        // The lasting rule names the same server and tool the card named.
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let added: Vec<&Value> = events
            .iter()
            .filter(|event| event.kind == EventKind::RuleAdded)
            .map(|event| &event.data)
            .collect();
        assert_eq!(added.len(), 1, "Always allow writes one rule");
        assert_eq!(added[0]["server"], json!(server));
        assert_eq!(added[0]["tool"], json!("search"));
        socket.close(None).await.unwrap();
        provider_task.abort();
        server_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_disabled_before_the_answer_runs_nothing() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, log, server_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "ok"}]})).await;
        let server = add_mcp_server(&mut socket, "docs", &url, Some("token-1")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;
        let run = run_of(&submit(&mut socket, chat, "search", None).await);
        wait_status(&mut socket, run, "waiting").await;
        // The owner disables the server while the card waits.
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpDisable { bot, server },
        )
        .await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        let finished = wait_run(&mut socket, run).await;
        assert_eq!(finished["status"], "succeeded");
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .all(|message| message["method"] != "tools/call"),
            "a disabled server is sent no call"
        );
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100_000)
            .await
            .unwrap()
            .events;
        let finished: Vec<&Value> = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::ActionFinished && event.data["run"] == json!(run)
            })
            .map(|event| &event.data)
            .collect();
        assert_eq!(finished.len(), 1);
        let error = finished[0]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("not offered to this run") || error.contains("no longer enabled"),
            "{error}"
        );
        // The refusal names the tool the Model used, and the reply's calls
        // keep that same name, so a replay reads one name for the action.
        let started: Vec<&Value> = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::ActionStarted && event.data["run"] == json!(run)
            })
            .map(|event| &event.data)
            .collect();
        let replies: Vec<&Value> = events
            .iter()
            .filter(|event| event.kind == EventKind::RunReply && event.data["run"] == json!(run))
            .map(|event| &event.data)
            .collect();
        assert_eq!(started.len(), 1, "{started:?}");
        let carded = replies
            .iter()
            .find(|reply| reply["step"] == json!(1))
            .expect("the waiting step's reply");
        assert_eq!(started[0]["tool"], json!("mcp__docs__search"));
        assert_eq!(
            carded["calls"][0]["model_name"],
            json!("mcp__docs__search"),
            "the reply keeps the name the Model used"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        server_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_mcp_call_asks_even_when_an_ask_rule_names_it() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, _log, server_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "ok"}]})).await;
        let server = add_mcp_server(&mut socket, "docs", &url, Some("token-1")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;
        // An Allow-shaped rule may not answer for the first call.
        command(
            &mut socket,
            Id::new(),
            CommandCall::RuleAdd {
                bot,
                effect: neha::record::RuleEffect::Allow,
                tool: "search".to_owned(),
                command_prefix: None,
                path_prefix: None,
                skill: None,
                script: None,
                server: Some("docs".to_owned()),
            },
        )
        .await;
        let run = run_of(&submit(&mut socket, chat, "search", None).await);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "first_mcp_call");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        socket.close(None).await.unwrap();
        provider_task.abort();
        server_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_list_reports_whether_the_space_reaches_each_server() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, _log, server_task) = fake_mcp_server(|_call| json!({"content": []})).await;
        let reachable = add_mcp_server(&mut socket, "docs", &url, Some("token-1")).await;
        let unreachable = add_mcp_server(
            &mut socket,
            "broken",
            "http://127.0.0.1:9/mcp",
            Some("token-2"),
        )
        .await;
        let list = command(&mut socket, Id::new(), CommandCall::McpList).await;
        let rows = list.as_array().unwrap();
        let by_server = |server: Id| {
            rows.iter()
                .find(|row| row["server"] == json!(server))
                .cloned()
                .unwrap()
        };
        assert_eq!(by_server(reachable)["connected"], json!(true));
        assert_eq!(
            by_server(unreachable)["connected"],
            json!(false),
            "a server the Space cannot reach is not connected"
        );
        socket.close(None).await.unwrap();
        server_task.abort();
        running.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_token_leaves_the_server_unconnected() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (url, _log, server_task) = fake_mcp_server(|_call| json!({"content": []})).await;
        let result = command(
            &mut socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: "docs".to_owned(),
                url,
                scope: neha::record::McpScope::Space,
                credential: Some(String::new()),
            }),
        )
        .await;
        assert_eq!(result["connected"], json!(false));
        let list = command(&mut socket, Id::new(), CommandCall::McpList).await;
        assert_eq!(list[0]["connected"], json!(false));
        socket.close(None).await.unwrap();
        server_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wider_ask_rule_asks_for_another_servers_tool() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (docs_url, docs_log, docs_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "ok"}]})).await;
        let (other_url, other_log, other_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "ok"}]})).await;
        let docs = add_mcp_server(&mut socket, "docs", &docs_url, Some("token-1")).await;
        let other = add_mcp_server(&mut socket, "other", &other_url, Some("token-2")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "a"})),
            text("first done"),
            tool("mcp__other__search", json!({"query": "b"})),
            text("second done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        for server in [docs, other] {
            command(
                &mut socket,
                Id::new(),
                CommandCall::BotMcpEnable { bot, server },
            )
            .await;
        }
        // The first call of the first server's tool asks, and Always allow
        // writes a rule for that server alone.
        let first = run_of(&submit(&mut socket, chat, "search docs", None).await);
        wait_status(&mut socket, first, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "first_mcp_call");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, true).await;
        assert_eq!(wait_run(&mut socket, first).await["status"], "succeeded");
        // A rule that names no server asks for every server's tool of that name.
        command(
            &mut socket,
            Id::new(),
            CommandCall::RuleAdd {
                bot,
                effect: neha::record::RuleEffect::Ask,
                tool: "search".to_owned(),
                command_prefix: None,
                path_prefix: None,
                skill: None,
                script: None,
                server: None,
            },
        )
        .await;
        let second = run_of(&submit(&mut socket, chat, "search other", None).await);
        wait_status(&mut socket, second, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(
            approvals[0]["reason"], "rule",
            "the wider Ask rule asks for another server's tool"
        );
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, second).await["status"], "succeeded");
        assert!(
            other_log
                .lock()
                .unwrap()
                .iter()
                .any(|message| message["method"] == "tools/call")
        );
        assert!(
            docs_log
                .lock()
                .unwrap()
                .iter()
                .any(|message| message["method"] == "tools/call")
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        docs_task.abort();
        other_task.abort();
        running.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replacement_server_never_runs_a_pending_call() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let (old_url, _old_log, old_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "old"}]})).await;
        let old = add_mcp_server(&mut socket, "docs", &old_url, Some("token-1")).await;
        let (base, provider_task) = scripted_provider(vec![
            tool("mcp__docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server: old },
        )
        .await;
        let run = run_of(&submit(&mut socket, chat, "search", None).await);
        wait_status(&mut socket, run, "waiting").await;
        // The server the card was made for leaves, and a new one takes its name.
        command(
            &mut socket,
            Id::new(),
            CommandCall::McpRemove { server: old },
        )
        .await;
        let (new_url, new_log, new_task) =
            fake_mcp_server(|_call| json!({"content": [{"type": "text", "text": "new"}]})).await;
        let new = add_mcp_server(&mut socket, "docs", &new_url, Some("token-2")).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server: new },
        )
        .await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["server"], json!(old));
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        assert!(
            new_log
                .lock()
                .unwrap()
                .iter()
                .all(|message| message["method"] != "tools/call"),
            "a replacement server is sent no call the answer was not made for"
        );
        socket.close(None).await.unwrap();
        provider_task.abort();
        old_task.abort();
        new_task.abort();
        running.shutdown().await;
    }
    /// Writes a package directory with one skill and one MCP server.
    fn write_plugin_package(root: &std::path::Path, mcp_url: &str) -> std::path::PathBuf {
        let package = root.join("reports-plugin");
        std::fs::create_dir_all(package.join("skills").join("weekly-report").join("scripts"))
            .unwrap();
        std::fs::write(
            package.join("plugin.json"),
            json!({
                "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                "name": "reports",
                "version": "1.2.0",
                "description": "Weekly reports",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            package
                .join("skills")
                .join("weekly-report")
                .join("SKILL.md"),
            "---\nname: weekly-report\ndescription: Write the weekly report.\n---\nCollect and summarize.",
        )
        .unwrap();
        std::fs::write(
            package
                .join("skills")
                .join("weekly-report")
                .join("scripts")
                .join("collect.sh"),
            "printf collected",
        )
        .unwrap();
        std::fs::write(
            package.join("mcp.json"),
            json!({
                "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
                "mcpServers": {
                    "docs": {
                        "type": "streamable-http",
                        "url": mcp_url,
                        "headers": {"X-Tenant": "acme"},
                    },
                },
            })
            .to_string(),
        )
        .unwrap();
        package
    }

    /// A fake MCP server that records the headers of every request.
    async fn fake_mcp_server_with_headers() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::Router;
        use axum::body::{Body, Bytes};
        use axum::http::{HeaderMap, header::CONTENT_TYPE};
        use axum::response::Response;
        use axum::routing::post;

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = log.clone();
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: Bytes| {
                let log = seen.clone();
                async move {
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(headers);
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let result = match message["method"].as_str() {
                        Some("initialize") => json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "docs", "version": "1"},
                        }),
                        Some("notifications/initialized") => return Response::new(Body::empty()),
                        Some("tools/list") => json!({
                            "tools": [{
                                "name": "search",
                                "description": "Search the docs.",
                                "inputSchema": {"type": "object"},
                            }],
                        }),
                        _ => json!({"content": [{"type": "text", "text": "found"}]}),
                    };
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{address}"), log, task)
    }

    /// Packs a package's files into GitHub's `<root>/...` tarball layout.
    fn plugin_tarball(files: &[(&str, &str)]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as _;

        let mut tar = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, content.as_bytes())
                .unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    /// Serves one package tarball at `/repo/archive/<anything>.tar.gz`.
    async fn serve_plugin_archive(tarball: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::Response;
        use axum::routing::get;

        let app = Router::new().route(
            "/repo/archive/{file}",
            get(move || {
                let tarball = tarball.clone();
                async move {
                    Response::builder()
                        .header(CONTENT_TYPE, "application/gzip")
                        .body(Body::from(tarball))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{address}"), task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_admin_installs_a_package_and_its_bot_uses_it() {
        let (mcp_url, mcp_headers, mcp_task) = fake_mcp_server_with_headers().await;
        // A repository archive rather than a directory, so the package path is
        // the same on every platform a Space runs on.
        let commit = "3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b";
        let mcp_json = json!({
            "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
            "mcpServers": {
                "docs": {
                    "type": "streamable-http",
                    "url": mcp_url,
                    "headers": {"X-Tenant": "acme"},
                },
            },
        })
        .to_string();
        let tarball = plugin_tarball(&[
            (
                "reports-plugin/plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"reports\",\"version\":\"1.2.0\",\"description\":\"Weekly reports\"}",
            ),
            (
                "reports-plugin/skills/weekly-report/SKILL.md",
                "---\nname: weekly-report\ndescription: Write the weekly report.\n---\nCollect and summarize.",
            ),
            (
                "reports-plugin/skills/weekly-report/scripts/collect.sh",
                "printf collected",
            ),
            ("reports-plugin/mcp.json", &mcp_json),
        ]);
        let (archive, archive_task) = serve_plugin_archive(tarball).await;
        let config = format!("[plugins]\nsources = [\"{archive}/\"]\n");
        let (_home, running, invite) = running_with_config(&config).await;
        let (mut socket, _token) = admin(&running, &invite).await;

        let installed = command(
            &mut socket,
            Id::new(),
            CommandCall::PluginInstall {
                source: format!("{archive}/repo@{commit}"),
            },
        )
        .await;
        let plugin: Id = serde_json::from_value(installed["plugin"].clone()).unwrap();
        assert_eq!(installed["name"], json!("reports"));
        assert_eq!(installed["version"], json!("1.2.0"));
        assert_eq!(installed["commit"], json!(commit));
        assert_eq!(installed["servers"].as_array().unwrap().len(), 1);
        assert_eq!(installed["servers"][0]["name"], json!("reports-docs"));
        assert_eq!(installed["skills"][0]["name"], json!("weekly-report"));
        let server: Id = serde_json::from_value(installed["servers"][0]["server"].clone()).unwrap();
        let skill: Id = serde_json::from_value(installed["skills"][0]["skill"].clone()).unwrap();
        assert!(
            installed["digest"].as_str().unwrap().len() == 64,
            "{installed}"
        );

        // The package's entries are in the Space's libraries.
        let skills = command(&mut socket, Id::new(), CommandCall::SkillList { bot: None }).await;
        assert!(
            skills
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == json!(skill)),
            "{skills}"
        );
        let servers = command(&mut socket, Id::new(), CommandCall::McpList).await;
        assert!(
            servers
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["server"] == json!(server)),
            "{servers}"
        );
        let listed = command(&mut socket, Id::new(), CommandCall::PluginList).await;
        assert_eq!(listed[0]["plugin"], json!(plugin));
        assert_eq!(listed[0]["enabled"], json!(true));

        // The Admin attaches a Space credential, so the package's header reaches
        // the server with the call.
        let attached = command(
            &mut socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: "reports-docs".to_owned(),
                url: installed["servers"][0]["url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                scope: neha::record::McpScope::Space,
                credential: Some("token-1".to_owned()),
            }),
        )
        .await;
        assert_eq!(attached["connected"], json!(true));

        // A Bot uses both: the skill loads, and the server's tool asks first.
        let (base, provider_task) = scripted_provider(vec![
            tool("skill", json!({"name": "weekly-report"})),
            tool("mcp__reports-docs__search", json!({"query": "hi"})),
            text("done"),
        ])
        .await;
        let created = command(&mut socket, Id::new(), add_bot("Scout", None)).await;
        let bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let chat: Id = serde_json::from_value(created["chat"]["id"].clone()).unwrap();
        command(&mut socket, Id::new(), provider_add(base)).await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotSkillEnable { bot, skill },
        )
        .await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::BotMcpEnable { bot, server },
        )
        .await;
        let run = run_of(&submit(&mut socket, chat, "use it", None).await);
        wait_status(&mut socket, run, "waiting").await;
        let approvals = approvals_in(&mut socket, Some(chat)).await;
        assert_eq!(approvals[0]["reason"], "first_mcp_call");
        let approval: Id = serde_json::from_value(approvals[0]["approval"].clone()).unwrap();
        decide_approval(&mut socket, approval, true, false).await;
        assert_eq!(wait_run(&mut socket, run).await["status"], "succeeded");
        // The package's configured header reached the server together with the
        // Vault credential the Admin attached.
        let saw_header = {
            let headers = mcp_headers.lock().unwrap();
            headers.iter().any(|headers| {
                headers
                    .get("x-tenant")
                    .and_then(|value| value.to_str().ok())
                    == Some("acme")
            })
        };
        assert!(saw_header, "the package's header reached the server");
        assert_eq!(last_message(&mut socket, chat).await["text"], "done");

        // A disabled package offers neither its skill nor its server.
        command(
            &mut socket,
            Id::new(),
            CommandCall::PluginDisable { plugin },
        )
        .await;
        let listed = command(&mut socket, Id::new(), CommandCall::PluginList).await;
        assert_eq!(listed[0]["enabled"], json!(false));

        // Removing it removes its entries and keeps the record replayable.
        command(&mut socket, Id::new(), CommandCall::PluginRemove { plugin }).await;
        assert!(
            command(&mut socket, Id::new(), CommandCall::PluginList)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            command(&mut socket, Id::new(), CommandCall::SkillList { bot: None })
                .await
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["id"] != json!(skill))
        );

        socket.close(None).await.unwrap();
        provider_task.abort();
        mcp_task.abort();
        archive_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_install_from_a_source_the_admin_did_not_allow_is_refused() {
        let (_dir, running, invite) = running().await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let dir = tempfile::tempdir().unwrap();
        let package = write_plugin_package(dir.path(), "https://example.com/mcp");
        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::PluginInstall {
                source: package.display().to_string(),
            },
        )
        .await
        .expect_err("a Space allows no source by default");
        assert_eq!(error.0, ErrorCode::Forbidden, "{}", error.1);
        assert!(
            command(&mut socket, Id::new(), CommandCall::PluginList)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
        socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_manifest_rejects_the_package() {
        // A repository archive keeps this the same on every platform.
        let tarball = plugin_tarball(&[("broken/plugin.json", "{not json")]);
        let (archive, archive_task) = serve_plugin_archive(tarball).await;
        let config = format!("[plugins]\nsources = [\"{archive}/\"]\n");
        let (_home, running, invite) = running_with_config(&config).await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let error = try_command(
            &mut socket,
            Id::new(),
            CommandCall::PluginInstall {
                source: format!("{archive}/repo@3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b"),
            },
        )
        .await
        .expect_err("an invalid manifest rejects the package");
        assert_eq!(error.0, ErrorCode::InvalidArgument, "{}", error.1);
        assert!(
            command(&mut socket, Id::new(), CommandCall::PluginList)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
        socket.close(None).await.unwrap();
        archive_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repository_package_is_fetched_at_its_commit() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::header::CONTENT_TYPE;
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use std::sync::{Arc, Mutex};

        let tarball = plugin_tarball(&[(
            "repo/plugin.json",
            "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"pinned\",\"version\":\"0.1.0\"}",
        )]);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let app = Router::new().route(
            "/repo/archive/{file}",
            get(
                move |axum::extract::Path(file): axum::extract::Path<String>| {
                    let seen = seen.clone();
                    let tarball = tarball.clone();
                    async move {
                        let commit = file.strip_suffix(".tar.gz").unwrap_or(&file).to_owned();
                        seen.lock().unwrap().push(commit);
                        Response::builder()
                            .header(CONTENT_TYPE, "application/gzip")
                            .body(Body::from(tarball))
                            .unwrap()
                            .into_response()
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let archive_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let config = format!("[plugins]\nsources = [\"http://{address}/\"]\n");
        let (_home, running, invite) = running_with_config(&config).await;
        let (mut socket, _token) = admin(&running, &invite).await;
        let installed = command(
            &mut socket,
            Id::new(),
            CommandCall::PluginInstall {
                source: format!("http://{address}/repo@3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b"),
            },
        )
        .await;
        assert_eq!(
            installed["commit"],
            json!("3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b")
        );
        assert_eq!(installed["name"], json!("pinned"));
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            ["3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b"]
        );
        socket.close(None).await.unwrap();
        archive_task.abort();
        running.shutdown().await;
    }

    /// The text one `application/x-www-form-urlencoded` field carries.
    fn form_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                    match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                        Some(byte) => {
                            decoded.push(byte);
                            index += 3;
                        }
                        None => {
                            decoded.push(b'%');
                            index += 1;
                        }
                    }
                }
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }

        String::from_utf8_lossy(&decoded).into_owned()
    }

    /// A fake authorization server and a fake MCP server that asks for OAuth.
    ///
    /// The MCP server answers its uncleared `initialize` with a 401 that names
    /// the authorization server's protected resource metadata, and accepts one
    /// bearer token afterwards. The authorization server serves the two
    /// metadata documents, registers the public client, and exchanges the
    /// code. Both listen on loopback ports, the only HTTP the OAuth rules
    /// admit.
    #[allow(clippy::type_complexity)]
    async fn fake_sign_in_landscape() -> (
        String,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        fake_sign_in_landscape_with(false).await
    }

    /// A fake authorization server's gate on the first token exchange.
    ///
    /// The exchange announces itself through `arrived`, then waits for
    /// `release`, so a test can start another sign-in while one token request
    /// is in flight.
    #[derive(Clone)]
    struct TokenDelay {
        arrived: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    impl TokenDelay {
        fn new() -> Self {
            Self {
                arrived: std::sync::Arc::new(tokio::sync::Notify::new()),
                release: std::sync::Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    /// The same fake landscape, with the authorization server optionally
    /// advertising that every authorization response carries RFC 9207's `iss`.
    ///
    /// The token exchange count, the discovery requests, the dynamic client
    /// registrations, and both tasks are returned to the test.
    #[allow(clippy::type_complexity)]
    async fn fake_sign_in_landscape_with(
        iss_required: bool,
    ) -> (
        String,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        fake_sign_in_landscape_controlled(iss_required, None).await
    }

    /// The same landscape with the first token exchange held until the test
    /// releases it.
    ///
    /// Each accepted exchange answers a different token, numbered in arrival
    /// order, so a test can tell which sign-in's credential the Vault kept.
    #[allow(clippy::type_complexity)]
    async fn fake_sign_in_landscape_delayed() -> (
        (
            String,
            String,
            std::sync::Arc<std::sync::Mutex<Vec<String>>>,
            std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            tokio::task::JoinHandle<()>,
            tokio::task::JoinHandle<()>,
        ),
        TokenDelay,
    ) {
        let delay = TokenDelay::new();
        let landscape = fake_sign_in_landscape_controlled(false, Some(delay.clone())).await;
        (landscape, delay)
    }

    #[allow(clippy::type_complexity)]
    async fn fake_sign_in_landscape_controlled(
        iss_required: bool,
        delay: Option<TokenDelay>,
    ) -> (
        String,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::Router;
        use axum::body::{Body, Bytes};
        use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE, header::WWW_AUTHENTICATE};
        use axum::response::Response;
        use axum::routing::{get, post};

        let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mcp_address = mcp_listener.local_addr().unwrap();
        let mcp_url = format!("http://{mcp_address}");
        let as_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let as_address = as_listener.local_addr().unwrap();
        let issuer = format!("http://{as_address}");

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registrations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let challenge =
            format!("Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource\"");
        let counted = calls.clone();
        let mcp_app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: Bytes| {
                let log = log.clone();
                let challenge = challenge.clone();
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(value) = headers
                        .get("x-package")
                        .and_then(|value| value.to_str().ok())
                    {
                        log.lock().unwrap().push(format!("x-package:{value}"));
                    }
                    let authorization = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().unwrap().push(authorization.clone());
                    // The delayed landscape numbers its tokens, so the flow
                    // that wins the race may hold the second one.
                    if authorization != "Bearer token-1" && authorization != "Bearer token-2" {
                        return Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .header(WWW_AUTHENTICATE, challenge)
                            .body(Body::empty())
                            .unwrap();
                    }
                    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    if message["method"] == "notifications/initialized" {
                        return Response::builder()
                            .status(StatusCode::ACCEPTED)
                            .body(Body::empty())
                            .unwrap();
                    }
                    let result = match message["method"].as_str() {
                        Some("initialize") => json!({
                            "protocolVersion": crate::mcp::PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "docs", "version": "1"},
                        }),
                        _ => json!({"tools": []}),
                    };
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .header("mcp-session-id", "session-1")
                        .body(Body::from(
                            json!({"jsonrpc": "2.0", "id": message["id"], "result": result})
                                .to_string(),
                        ))
                        .unwrap()
                }
            }),
        );

        let forms = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_forms = forms.clone();
        let counted_registrations = registrations.clone();
        let exchanges = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let numbered = delay.is_some();
        let resource_for_metadata = mcp_url.clone();
        let resource_for_token = mcp_url.clone();
        let issuer_for_metadata = issuer.clone();
        let issuer_for_document = issuer.clone();
        let as_app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(move || {
                    let resource = resource_for_metadata.clone();
                    let issuer = issuer_for_metadata.clone();
                    async move {
                        axum::Json(json!({
                            "resource": resource,
                            "authorization_servers": [issuer],
                        }))
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let issuer = issuer_for_document.clone();
                    async move {
                        let mut metadata = json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "registration_endpoint": format!("{issuer}/register"),
                            "code_challenge_methods_supported": ["S256"],
                        });
                        if iss_required {
                            metadata["authorization_response_iss_parameter_supported"] =
                                json!(true);
                        }
                        axum::Json(metadata)
                    }
                }),
            )
            .route(
                "/register",
                post(move |body: Bytes| {
                    let registrations = counted_registrations.clone();
                    async move {
                        registrations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // The answer describes a public client for the exact
                        // redirect it was asked for, the way a registration server
                        // does.
                        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let redirects = request
                            .get("redirect_uris")
                            .cloned()
                            .unwrap_or_else(|| json!([]));
                        axum::Json(json!({
                            "client_id": "test-client",
                            "redirect_uris": redirects,
                            "token_endpoint_auth_method": "none",
                        }))
                    }
                }),
            )
            .route(
                "/token",
                post(move |body: Bytes| {
                    let forms = seen_forms.clone();
                    let resource = resource_for_token.clone();
                    let exchanges = exchanges.clone();
                    let gate = delay.clone();
                    async move {
                        let text = String::from_utf8_lossy(&body);
                        let form: std::collections::HashMap<String, String> = text
                            .split('&')
                            .filter_map(|pair| pair.split_once('='))
                            .map(|(name, value)| (form_decode(name), form_decode(value)))
                            .collect();
                        let accepted = form.get("grant_type").map(String::as_str)
                            == Some("authorization_code")
                            && form.get("code").map(String::as_str) == Some("fake-code")
                            && form
                                .get("code_verifier")
                                .is_some_and(|value| !value.is_empty())
                            && form
                                .get("redirect_uri")
                                .is_some_and(|value| !value.is_empty())
                            && form.get("client_id").map(String::as_str) == Some("test-client")
                            && form.get("resource").map(String::as_str) == Some(resource.as_str());
                        forms.lock().unwrap().push(form);
                        if !accepted {
                            return Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .header(CONTENT_TYPE, "application/json")
                                .body(Body::from(
                                    json!({
                                        "error": "invalid_grant",
                                        "error_description": "the-provider-secret-code",
                                    })
                                    .to_string(),
                                ))
                                .unwrap();
                        }
                        let order = exchanges.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if order == 1
                            && let Some(gate) = &gate
                        {
                            gate.arrived.notify_one();
                            gate.release.notified().await;
                        }
                        let token = if numbered {
                            format!("token-{order}")
                        } else {
                            "token-1".to_owned()
                        };
                        Response::builder()
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                json!({"access_token": token, "token_type": "Bearer"}).to_string(),
                            ))
                            .unwrap()
                    }
                }),
            )
            .route("/authorize", get(|| async { "authorize" }));

        let mcp_task = tokio::spawn(async move {
            let _ = axum::serve(mcp_listener, mcp_app).await;
        });
        let as_task = tokio::spawn(async move {
            let _ = axum::serve(as_listener, as_app).await;
        });

        (
            mcp_url,
            issuer,
            seen,
            forms,
            calls,
            registrations,
            mcp_task,
            as_task,
        )
    }

    /// The value of one query parameter of a URL.
    fn query_parameter(url: &str, name: &str) -> Option<String> {
        reqwest::Url::parse(url)
            .ok()?
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    #[test]
    fn a_sign_in_cookie_is_scoped_to_the_flow() {
        let sign_in = Id::new();
        let name = super::sign_in_cookie_name(sign_in);
        // The name carries the sign-in, and a UUID's text is a cookie-name
        // token: letters, digits, and hyphens.
        assert_eq!(name, format!("neha_signin_{sign_in}"));
        assert!(
            sign_in
                .to_string()
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-'),
            "a UUID's text is a cookie-name token"
        );
        let cookie = super::sign_in_cookie(sign_in, "a-nonce", false);
        assert!(
            cookie.starts_with(&format!("{name}=a-nonce;")),
            "the cookie's name is the flow's and carries its nonce: {cookie}"
        );
        for attribute in [
            "HttpOnly",
            "SameSite=Lax",
            "Path=/api/mcp/signin",
            "Max-Age=600",
        ] {
            assert!(
                cookie.contains(attribute),
                "{attribute} missing from {cookie}"
            );
        }
        assert!(
            !cookie.contains("Secure"),
            "a plain-HTTP Space sets no Secure cookie: {cookie}"
        );
        assert!(super::sign_in_cookie(sign_in, "a-nonce", true).contains("; Secure"));
        let cleared = super::sign_in_cookie_cleared(sign_in, true);
        assert!(
            cleared.starts_with(&format!("{name}=;")),
            "the clearing header names the flow's cookie: {cleared}"
        );
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("; Secure"), "{cleared}");
        // Two flows never share a name, so one flow's binding is another's
        // never to write or clear.
        assert_ne!(name, super::sign_in_cookie_name(Id::new()));
    }

    /// Signs an account in the way its browser page does: on the Space's own
    /// browser hostname and with that page's Origin. Returns the session cookie
    /// the bind route requires.
    async fn browser_session(running: &Running, name: &str) -> String {
        let page = running.browser_url();
        let login = reqwest::Client::new()
            .post(format!("{page}/api/login"))
            .header("origin", page.clone())
            .json(&json!({"display_name": name, "password": "correct horse battery"}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), reqwest::StatusCode::OK);
        login
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .expect("a browser sign-in sets the session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    /// The callback URL one issuer's browser flow returns to.
    fn sign_in_callback(running: &Running, issuer: &str) -> String {
        format!(
            "{}/api/mcp/signin/{}/callback",
            running.url(),
            crate::oauth::issuer_key(issuer)
        )
    }

    /// Binds one browser flow and returns the binding cookie the browser must
    /// send back, the nonce that cookie carries, and the provider URL the bind
    /// route redirects to.
    ///
    /// The request is the initiating browser's: it addresses the Space's own
    /// hostname and carries the session cookie the page that opened the URL
    /// holds. A command line flow's browser need not be signed in to the
    /// Space, so its tests bind with `None` (an empty cookie jar).
    ///
    /// The binding cookie is the flow's own: its name carries the sign-in, so a
    /// later answer of an older flow can never overwrite it.
    async fn bind_sign_in(
        client: &reqwest::Client,
        running: &Running,
        sign_in: Id,
        session_cookie: Option<&str>,
    ) -> (String, String, String) {
        let mut request = client
            .get(format!("{}/api/mcp/signin/bind", running.browser_url()))
            .query(&[("sign_in", sign_in.to_string())]);
        if let Some(session_cookie) = session_cookie {
            request = request.header("cookie", session_cookie);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .expect("the bind answer sets a cookie")
            .to_owned();
        for attribute in [
            "HttpOnly",
            "SameSite=Lax",
            "Path=/api/mcp/signin",
            "Max-Age=600",
        ] {
            assert!(
                set_cookie.contains(attribute),
                "{attribute} missing from {set_cookie}"
            );
        }
        assert!(
            !set_cookie.contains("Secure"),
            "a plain-HTTP Space sets no Secure cookie: {set_cookie}"
        );
        let cookie = set_cookie
            .split(';')
            .next()
            .expect("the cookie's name and value")
            .to_owned();
        let (name, nonce) = cookie.split_once('=').expect("a cookie name and value");
        assert_eq!(
            name,
            super::sign_in_cookie_name(sign_in),
            "the bind answer sets this flow's cookie"
        );
        assert_eq!(
            nonce.len(),
            43,
            "32 random bytes, base64url without padding"
        );
        let nonce = nonce.to_owned();
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("the bind answer redirects")
            .to_owned();

        (cookie, nonce, location)
    }

    /// Binds one command line flow and returns the provider URL the bind route
    /// redirects to.
    ///
    /// A command line flow sets no browser cookie: its loopback receiver and
    /// the authenticated finish bind the second hop, and a command line flow
    /// owns no browser binding.
    async fn bind_command_line_sign_in(
        client: &reqwest::Client,
        running: &Running,
        sign_in: Id,
    ) -> String {
        let response = client
            .get(format!("{}/api/mcp/signin/bind", running.browser_url()))
            .query(&[("sign_in", sign_in.to_string())])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        assert!(
            response.headers().get("set-cookie").is_none(),
            "a command line flow binds no browser cookie"
        );

        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("the bind answer redirects")
            .to_owned()
    }

    /// The path of one URL: what a loopback receiver passes as the callback's
    /// path.
    fn url_path(url: &str) -> String {
        reqwest::Url::parse(url)
            .expect("a valid URL")
            .path()
            .to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_sign_in_connects_a_new_mcp_server() {
        let (
            mcp_url,
            authorization_server,
            tokens,
            forms,
            calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let account: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let mut socket = open_socket(&running, token).await;
        // The browser that starts a sign-in is signed in to this Space.
        let session_cookie = browser_session(&running, "Ada").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let callback = sign_in_callback(&running, &authorization_server);
        let scope_callback = format!(
            "{}/api/mcp/signin/{}/callback",
            running.server.browser_origin(),
            crate::oauth::issuer_key(&authorization_server)
        );

        // The Admin starts a sign-in for a new server; the default scope is the
        // Space's. The answer names a first-party bind URL, never the provider.
        let start = neha::surface::McpSignInStart {
            server: None,
            name: Some("docs".to_owned()),
            url: Some(mcp_url.clone()),
            scope: None,
            redirect_uri: None,
        };
        let command_id = Id::new();
        let started = command(
            &mut socket,
            command_id,
            CommandCall::McpSignInStart(start.clone()),
        )
        .await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        assert_eq!(
            started["url"],
            json!(format!(
                "{}/api/mcp/signin/bind?sign_in={sign_in}",
                running.server.browser_origin()
            )),
            "the command answers the browser's first-party URL"
        );

        // A redelivered command is answered once: one pending sign-in, the
        // same answer, no second discovery, and no event.
        let discovered = calls.load(std::sync::atomic::Ordering::SeqCst);
        let last = running.server.store.last_seq().await.unwrap();
        let replayed = command(&mut socket, command_id, CommandCall::McpSignInStart(start)).await;
        assert_eq!(replayed, started, "a redelivered start repeats its answer");
        assert_eq!(
            running.server.signins.lock().await.len(),
            1,
            "one pending sign-in, not a second flow"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            discovered,
            "a replay asks the MCP server nothing"
        );
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            last,
            "a replay appends no event"
        );

        // The bind route sets the browser's cookie and sends the browser to the
        // provider with a state the callback must return.
        let (cookie, nonce, authorize_url) =
            bind_sign_in(&client, &running, sign_in, Some(&session_cookie)).await;
        assert!(
            authorize_url.starts_with(&format!("{authorization_server}/authorize")),
            "{authorize_url}"
        );
        assert_eq!(
            query_parameter(&authorize_url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(
            query_parameter(&authorize_url, "redirect_uri").as_deref(),
            Some(scope_callback.as_str()),
            "the browser flow comes back to the Space"
        );
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        assert!(!state.is_empty());

        // A callback without the cookie is refused and stores nothing, even
        // with the right state: the state token alone does not say who is here.
        let without = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(without.status(), reqwest::StatusCode::BAD_REQUEST);
        let body = without.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&state) && !body.contains("fake-code"),
            "the page echoes neither the state nor the code: {body}"
        );
        assert!(
            !running
                .server
                .store
                .events_from(Seq::ZERO, 100)
                .await
                .unwrap()
                .events
                .iter()
                .any(|event| event.kind == EventKind::McpServerAdded),
            "a callback without its cookie stores nothing"
        );
        let still_pending = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(still_pending["state"], json!("pending"));

        // A callback with another browser's cookie is refused the same way,
        // and leaves the flow usable. The flow's cookie name is exact: even its
        // own nonce under another flow's name is no binding at all.
        let wrong = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .header(
                "cookie",
                format!("{}=a-different-nonce", super::sign_in_cookie_name(sign_in)),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::BAD_REQUEST);
        let borrowed = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .header(
                "cookie",
                format!("{}={nonce}", super::sign_in_cookie_name(Id::new())),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(borrowed.status(), reqwest::StatusCode::BAD_REQUEST);

        // A token endpoint's error never reaches the page or the poll: the
        // callback spends the flow, the exchange is refused, and both read one
        // fixed sentence.
        let failed = client
            .get(&callback)
            .query(&[
                ("code", "wrong-code"),
                ("state", state.as_str()),
                ("iss", authorization_server.as_str()),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(failed.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            failed
                .headers()
                .get("set-cookie")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Max-Age=0"),
            "the callback clears the binding cookie"
        );
        let body = failed.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains("the-provider-secret-code")
                && !body.contains("invalid_grant")
                && !body.contains(&state)
                && !body.contains("wrong-code"),
            "no provider text, state, or code reaches the page: {body}"
        );
        let failed_status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(failed_status["state"], json!("failed"));
        assert_eq!(
            failed_status["reason"],
            json!("the sign-in could not be completed")
        );
        let text = failed_status.to_string();
        assert!(
            !text.contains("the-provider-secret-code") && !text.contains("invalid_grant"),
            "no provider text reaches the poll: {text}"
        );
        assert!(
            !running
                .server
                .store
                .events_from(Seq::ZERO, 100)
                .await
                .unwrap()
                .events
                .iter()
                .any(|event| event.kind == EventKind::McpServerAdded),
            "a refused exchange stores nothing"
        );

        // A refusal the person made reads one fixed sentence too: the
        // authority's own error text never reaches the page or the poll.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("docs".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let cancelled: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, cancelled, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let refused = client
            .get(&callback)
            .query(&[
                ("error", "access_denied"),
                ("error_description", "provider-secret"),
                ("state", state.as_str()),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
        let body = refused.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains("provider-secret")
                && !body.contains("access_denied")
                && !body.contains(&state),
            "no provider text or state reaches the page: {body}"
        );
        let cancelled_status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: cancelled },
        )
        .await;
        assert_eq!(cancelled_status["state"], json!("failed"));
        assert_eq!(
            cancelled_status["reason"],
            json!("the sign-in was cancelled or refused")
        );

        // An issuer another authorization server named is a mix-up: the
        // callback spends the flow and reads one fixed sentence.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("docs".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let mixed_up: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, mixed_up, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        // The callback path names the issuer the plan was discovered from. A
        // callback under another issuer's key, a malformed key, and the old
        // unkeyed path are refused before any code is exchanged, and none of
        // them spends the flow.
        let wrong_key = format!(
            "{}/api/mcp/signin/{}/callback",
            running.url(),
            crate::oauth::issuer_key("https://another.example")
        );
        let malformed_key = format!("{}/api/mcp/signin/not-a-key/callback", running.url());
        let unkeyed = format!("{}/api/mcp/signin/callback", running.url());
        let exchanges_before = forms.lock().unwrap().len();
        for refused_path in [&wrong_key, &malformed_key] {
            let refused = client
                .get(refused_path)
                .query(&[("code", "fake-code"), ("state", state.as_str())])
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(
                refused.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{refused_path}"
            );
            let body = refused.text().await.unwrap();
            assert!(body.contains("failed"), "{body}");
            assert!(
                !body.contains(&state) && !body.contains("fake-code"),
                "the page echoes neither the state nor the code: {body}"
            );
        }
        let old_path = client
            .get(&unkeyed)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(
            old_path.status(),
            reqwest::StatusCode::NOT_FOUND,
            "the old unkeyed callback path no longer exists"
        );
        assert_eq!(
            forms.lock().unwrap().len(),
            exchanges_before,
            "a callback under the wrong key exchanges nothing"
        );
        let crossed = client
            .get(&callback)
            .query(&[
                ("code", "fake-code"),
                ("state", state.as_str()),
                ("iss", "https://another.example"),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(crossed.status(), reqwest::StatusCode::BAD_REQUEST);
        let body = crossed.text().await.unwrap();
        assert!(
            !body.contains("another.example") && !body.contains(&state),
            "the page echoes no issuer or state: {body}"
        );
        let crossed_status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: mixed_up },
        )
        .await;
        assert_eq!(crossed_status["state"], json!("failed"));
        assert_eq!(
            crossed_status["reason"],
            json!("the sign-in could not be completed")
        );

        // The successful flow begins with a bind request and completes in the
        // browser that received the cookie.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("docs".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, sign_in, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let page = client
            .get(&callback)
            .query(&[
                ("code", "fake-code"),
                ("state", state.as_str()),
                ("iss", authorization_server.as_str()),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        assert_eq!(page.headers().get("cache-control").unwrap(), "no-store");
        assert!(
            page.headers()
                .get("set-cookie")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Max-Age=0"),
            "the callback clears the binding cookie"
        );
        assert!(
            page.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = page.text().await.unwrap();
        assert!(body.contains("finished"), "{body}");
        assert!(
            !body.contains(&state) && !body.contains("fake-code"),
            "the page echoes neither the state nor the code: {body}"
        );

        // The token exchange carried the PKCE verifier and the plan's binding.
        {
            let exchanged = forms.lock().unwrap();
            assert_eq!(
                exchanged.len(),
                2,
                "the refused exchange and the finished one"
            );
            let finished = &exchanged[1];
            assert_eq!(finished.get("code").map(String::as_str), Some("fake-code"));
            assert_eq!(
                finished.get("resource").map(String::as_str),
                Some(mcp_url.as_str()),
                "the token is bound to the resource"
            );
            assert_eq!(
                finished.get("redirect_uri").map(String::as_str),
                Some(scope_callback.as_str()),
                "the token is bound to the Space's callback"
            );
            assert!(
                finished
                    .get("code_verifier")
                    .is_some_and(|value| !value.is_empty()),
                "the exchange carries the PKCE verifier"
            );
        }

        // The record holds the new server and the Vault holds the Space's
        // credential, so the client that started the sign-in sees it connected.
        let status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(status["state"], json!("finished"));
        let server: Id = serde_json::from_value(status["server"].clone()).unwrap();
        assert_eq!(status["connected"], json!(true));
        assert_eq!(status["reason"], json!(null));
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("token-1"),
            "an Admin's Space sign-in stores the Space's credential"
        );
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100)
            .await
            .unwrap()
            .events;
        let added = events
            .iter()
            .find(|event| {
                event.kind == EventKind::McpServerAdded && event.data["server"] == json!(server)
            })
            .expect("the record holds the connection");
        assert_eq!(added.data["name"], json!("docs"));
        assert_eq!(added.data["url"], json!(mcp_url));
        assert_eq!(added.data["scope"], json!("space"));
        assert_eq!(added.data["user"], json!(null));
        assert_eq!(added.data["by"], json!(account));
        assert_eq!(added.data["plugin"], json!(null));
        assert_eq!(added.data["headers"], json!({}));

        let listed = command(&mut socket, Id::new(), CommandCall::McpList).await;
        let row = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["server"] == json!(server))
            .expect("the list holds the server");
        assert_eq!(row["connected"], json!(true));
        assert_eq!(row["credential"], json!("space"));
        assert_eq!(row["by_name"], json!("Ada"));
        assert!(
            tokens
                .lock()
                .unwrap()
                .iter()
                .any(|seen| seen == "Bearer token-1"),
            "the list's probe sent the stored token"
        );

        // The state is single use: the same callback is refused and the page
        // echoes neither it nor the code.
        let replay = client
            .get(&callback)
            .query(&[("code", "another-code"), ("state", state.as_str())])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), reqwest::StatusCode::BAD_REQUEST);
        let body = replay.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&state) && !body.contains("another-code"),
            "the page echoes nothing: {body}"
        );

        // A state no sign-in made renders the same generic failure.
        for (code, state) in [("fake-code", "not-a-state"), ("", "")] {
            let unknown = client
                .get(&callback)
                .query(&[("code", code), ("state", state)])
                .send()
                .await
                .unwrap();
            assert_eq!(unknown.status(), reqwest::StatusCode::BAD_REQUEST);
            let body = unknown.text().await.unwrap();
            assert!(body.contains("failed"), "{body}");
        }

        // A bind for a sign-in no Space holds renders the same failure.
        let missing = client
            .get(format!("{}/api/mcp/signin/bind", running.browser_url()))
            .query(&[("sign_in", Id::new().to_string())])
            .header("cookie", &session_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(missing.headers().get("set-cookie").is_none());
        let body = missing.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");

        // The metadata does not advertise RFC 9207's parameter, so a response
        // that omits `iss` is admitted: the flow is not refused for a promise
        // this authorization server never made.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("docs-omitted-iss".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, sign_in, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let page = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        let body = page.text().await.unwrap();
        assert!(body.contains("finished"), "{body}");
        let status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(status["state"], json!("finished"));

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_line_sign_in_finishes_with_the_loopback_code() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let account: Id = serde_json::from_value(session["account"]["id"].clone()).unwrap();
        let mut socket = open_socket(&running, token).await;
        // The browser that opens a command line flow's bind URL is a fresh
        // browser profile: it holds no Space session at all. The command line
        // is the authenticated initiator, and its finish is an authenticated
        // command.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let loopback = "http://127.0.0.1:4711";
        let loopback_callback = format!(
            "{loopback}/{}/callback",
            crate::oauth::issuer_key(&authorization_server)
        );
        // What the loopback receiver forwards as the callback's path.
        let cli_path = url_path(&loopback_callback);
        let start = neha::surface::McpSignInStart {
            server: None,
            name: Some("docs".to_owned()),
            url: Some(mcp_url.clone()),
            scope: None,
            redirect_uri: Some(loopback.to_owned()),
        };

        // A remote or TLS redirect is no command line's receiver.
        let refused = try_command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                redirect_uri: Some("https://example.com/api/mcp/signin/callback".to_owned()),
                ..start.clone()
            }),
        )
        .await;
        assert!(
            matches!(refused, Err((ErrorCode::InvalidArgument, _))),
            "a non-loopback redirect is refused: {refused:?}"
        );

        // The command answers the same first-party bind URL the browser flow
        // does; the bind redirect names the command line's loopback receiver.
        let started = command(&mut socket, Id::new(), CommandCall::McpSignInStart(start)).await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        assert_eq!(
            started["url"],
            json!(format!(
                "{}/api/mcp/signin/bind?sign_in={sign_in}",
                running.server.browser_origin()
            ))
        );
        // The browser has no cookie jar and needs none: a command line flow
        // binds without a session and without setting a browser cookie.
        let authorize_url = bind_command_line_sign_in(&client, &running, sign_in).await;
        assert!(
            authorize_url.starts_with(&format!("{authorization_server}/authorize")),
            "{authorize_url}"
        );
        assert_eq!(
            query_parameter(&authorize_url, "redirect_uri").as_deref(),
            Some(loopback_callback.as_str()),
            "the command line's receiver is the redirect, keyed by the issuer"
        );
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");

        // The Space's keyed callback cannot finish a flow a command line owns.
        let callback = sign_in_callback(&running, &authorization_server);
        let crossed = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(crossed.status(), reqwest::StatusCode::BAD_REQUEST);
        let pending = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(pending["state"], json!("pending"));

        // An issuer that names another authorization server is a mix-up: the
        // flow is spent and the answer never repeats the named issuer.
        let probe_started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("other".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: Some(loopback.to_owned()),
            }),
        )
        .await;
        let probe: Id = serde_json::from_value(probe_started["sign_in"].clone()).unwrap();
        let authorize_url = bind_command_line_sign_in(&client, &running, probe).await;
        let probe_state =
            query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let mixed_up = try_command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInFinish {
                sign_in: probe,
                code: "fake-code".to_owned(),
                state: probe_state,
                path: Some(cli_path.clone()),
                iss: Some("https://another.example".to_owned()),
            },
        )
        .await;
        let mixed_up = match mixed_up {
            Err((ErrorCode::InvalidArgument, message)) => message,
            other => panic!("a mix-up must be refused: {other:?}"),
        };
        assert!(
            !mixed_up.contains("another.example"),
            "the refusal echoes no issuer: {mixed_up}"
        );
        let probe_status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: probe },
        )
        .await;
        assert_eq!(probe_status["state"], json!("failed"));
        assert_eq!(
            probe_status["reason"],
            json!("the sign-in could not be completed")
        );

        // The command line sends what its loopback receiver got. It received
        // no issuer, which an authorization server may omit.
        let finish_command = Id::new();
        let finished = command(
            &mut socket,
            finish_command,
            CommandCall::McpSignInFinish {
                sign_in,
                code: "fake-code".to_owned(),
                state: state.clone(),
                path: Some(cli_path.clone()),
                iss: None,
            },
        )
        .await;
        assert_eq!(finished["sign_in"], json!(sign_in));
        assert_eq!(finished["state"], json!("finished"));
        assert_eq!(finished["reason"], json!(null));
        let server: Id = serde_json::from_value(finished["server"].clone()).unwrap();
        assert_eq!(finished["connected"], json!(true));
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("token-1"),
            "the command line's sign-in stores the Space's credential"
        );
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100)
            .await
            .unwrap()
            .events;
        let added: Vec<&neha::record::Event> = events
            .iter()
            .filter(|event| {
                event.kind == EventKind::McpServerAdded && event.data["server"] == json!(server)
            })
            .collect();
        assert_eq!(added.len(), 1, "the flow records one connection");
        assert_eq!(added[0].data["scope"], json!("space"));
        assert_eq!(added[0].data["by"], json!(account));

        // The exchange carried the loopback redirect the browser was sent to,
        // keyed by the issuer the plan was discovered from.
        {
            let exchanged = forms.lock().unwrap();
            assert_eq!(exchanged.len(), 1);
            assert_eq!(
                exchanged[0].get("redirect_uri").map(String::as_str),
                Some(loopback_callback.as_str())
            );
            assert_eq!(
                exchanged[0].get("resource").map(String::as_str),
                Some(mcp_url.as_str())
            );
        }

        // A redelivered finish is idempotent: the finished status is answered
        // again, and neither the Vault nor the record changes. The same
        // command id and a fresh id that names the same sign-in read the same
        // answer.
        let cursor = running.server.store.last_seq().await.unwrap();
        for (command_id, why) in [
            (finish_command, "a redelivered finish repeats its answer"),
            (Id::new(), "a fresh command id repeats the finished answer"),
        ] {
            let again = command(
                &mut socket,
                command_id,
                CommandCall::McpSignInFinish {
                    sign_in,
                    code: "fake-code".to_owned(),
                    state: state.clone(),
                    path: Some(cli_path.clone()),
                    iss: None,
                },
            )
            .await;
            assert_eq!(again, finished, "{why}");
        }
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor,
            "a repeated finish appends no event"
        );
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("token-1"),
            "a repeated finish rewrites no credential"
        );
        let polled = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(polled["state"], json!("finished"));
        assert_eq!(polled["server"], json!(server));

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bind_url_belongs_to_the_account_that_started_the_sign_in() {
        let (
            mcp_url,
            _authorization_server,
            _tokens,
            _forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut socket = open_socket(&running, token).await;
        // The initiating browser's session; another account's browser on the
        // same Space has its own.
        let ada_cookie = browser_session(&running, "Ada").await;
        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let grace_cookie = browser_session(&running, "Grace").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // The bind URL is the only handle to a flow, but it is handed to a
        // browser: an unauthenticated browser cannot carry it off.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("docs".to_owned()),
                url: Some(mcp_url),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        for cookie in [None, Some("neha_session_not-a-token")] {
            let mut request = client
                .get(format!("{}/api/mcp/signin/bind", running.browser_url()))
                .query(&[("sign_in", sign_in.to_string())]);
            if let Some(cookie) = cookie {
                request = request.header("cookie", cookie);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
            assert!(
                response.headers().get("set-cookie").is_none(),
                "an unauthenticated bind sets nothing"
            );
            let body = response.text().await.unwrap();
            assert!(body.contains("failed"), "{body}");
            assert!(
                !body.contains(&sign_in.to_string()),
                "the page echoes the sign-in id: {body}"
            );
        }

        // Another account's browser cannot bind the flow either; the failure
        // is the same generic page and the flow stays usable for its owner.
        let foreign = client
            .get(format!("{}/api/mcp/signin/bind", running.browser_url()))
            .query(&[("sign_in", sign_in.to_string())])
            .header("cookie", &grace_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(foreign.headers().get("set-cookie").is_none());
        let body = foreign.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&sign_in.to_string()),
            "the page echoes the sign-in id: {body}"
        );
        let pending = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in },
        )
        .await;
        assert_eq!(pending["state"], json!("pending"));

        // The initiating browser binds normally: the session cookie arrives on
        // the first-party navigation and names the account that started it.
        let (_cookie, nonce, authorize_url) =
            bind_sign_in(&client, &running, sign_in, Some(&ada_cookie)).await;
        assert_eq!(nonce.len(), 43);
        assert!(
            authorize_url.contains("state="),
            "the redirect names the flow's state: {authorize_url}"
        );

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_command_line_flow_cannot_exchange_its_code() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        // A server the Space already holds, with a credential that a finished
        // sign-in would replace.
        let server = add_mcp_server(&mut socket, "docs", &mcp_url, Some("old-token")).await;
        let loopback = "http://127.0.0.1:4712/api/mcp/signin/callback";
        let cli_path = format!(
            "/{}/callback",
            crate::oauth::issuer_key(&authorization_server)
        );
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: Some(server),
                name: None,
                url: None,
                scope: Some(neha::record::McpScope::Space),
                redirect_uri: Some(loopback.to_owned()),
            }),
        )
        .await;
        let sign_in: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let authorize_url = bind_command_line_sign_in(&client, &running, sign_in).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");

        // Age the flow past its usable life while it is still pending: the
        // sweeper marks it failed on the next read and keeps its state token,
        // so only the status and the age can stop a late exchange.
        {
            let mut signins = running.server.signins.lock().await;
            let pending = signins.get_mut(&sign_in).expect("the flow is pending");
            pending.created_at = std::time::Instant::now()
                - (super::SIGN_IN_TTL + std::time::Duration::from_secs(1));
        }
        let cursor = running.server.store.last_seq().await.unwrap();

        let refused = try_command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInFinish {
                sign_in,
                code: "fake-code".to_owned(),
                state,
                path: Some(cli_path),
                iss: None,
            },
        )
        .await;
        match refused {
            Err((ErrorCode::InvalidArgument, message)) => {
                assert_eq!(message, super::SIGN_IN_EXPIRED)
            }
            other => panic!("an expired flow must refuse its code: {other:?}"),
        }
        assert!(
            forms.lock().unwrap().is_empty(),
            "an expired flow exchanges no code"
        );
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor,
            "an expired flow appends no event"
        );
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("old-token"),
            "an expired flow leaves the Vault unchanged"
        );
        {
            let signins = running.server.signins.lock().await;
            match &signins
                .get(&sign_in)
                .expect("the failed flow is kept")
                .status
            {
                super::SignInStatus::Failed { reason } => {
                    assert_eq!(reason, super::SIGN_IN_EXPIRED);
                }
                _ => panic!("an expired flow reads as failed"),
            }
        }

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_callback_must_name_the_issuer_the_advertised_metadata_promises() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape_with(true).await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut socket = open_socket(&running, token).await;
        let session_cookie = browser_session(&running, "Ada").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let callback = sign_in_callback(&running, &authorization_server);

        // The authorization server's metadata says every authorization
        // response carries `iss`, so a response that omits it is a mix-up
        // risk: the callback refuses it before the code is exchanged.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("omitted-iss".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let omitted: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, omitted, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let cursor = running.server.store.last_seq().await.unwrap();
        let refused = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", state.as_str())])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
        let body = refused.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&state) && !body.contains("fake-code"),
            "the page echoes neither the state nor the code: {body}"
        );
        assert!(
            forms.lock().unwrap().is_empty(),
            "a response promised an issuer exchanges nothing without it"
        );
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor,
            "a refused response appends no event"
        );
        let status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: omitted },
        )
        .await;
        assert_eq!(status["state"], json!("failed"));
        assert_eq!(status["reason"], json!(super::SIGN_IN_INCOMPLETE));

        // A named issuer that is not the plan's is a mix-up whether or not the
        // metadata promised the parameter.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("crossed-iss".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let crossed: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, crossed, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let refused = client
            .get(&callback)
            .query(&[
                ("code", "fake-code"),
                ("state", state.as_str()),
                ("iss", "https://another.example"),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(forms.lock().unwrap().is_empty());

        // The issuer the metadata named completes the flow.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("named-iss".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let named: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (cookie, _nonce, authorize_url) =
            bind_sign_in(&client, &running, named, Some(&session_cookie)).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let page = client
            .get(&callback)
            .query(&[
                ("code", "fake-code"),
                ("state", state.as_str()),
                ("iss", authorization_server.as_str()),
            ])
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        let body = page.text().await.unwrap();
        assert!(body.contains("finished"), "{body}");
        let status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: named },
        )
        .await;
        assert_eq!(status["state"], json!("finished"));
        assert_eq!(forms.lock().unwrap().len(), 1, "one code was exchanged");

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_line_finish_must_name_the_issuer_the_advertised_metadata_promises() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape_with(true).await;
        let (_dir, running, invite) = running().await;
        let mut socket = admin_socket(&running, &invite).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let loopback = "http://127.0.0.1:4713/api/mcp/signin/callback";
        let cli_path = format!(
            "/{}/callback",
            crate::oauth::issuer_key(&authorization_server)
        );

        // The same requirement holds for the command line's finish: a response
        // that omits the issuer the metadata promises is refused before the
        // token exchange.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("cli-omitted".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: Some(loopback.to_owned()),
            }),
        )
        .await;
        let omitted: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let authorize_url = bind_command_line_sign_in(&client, &running, omitted).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let cursor = running.server.store.last_seq().await.unwrap();
        let refused = try_command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInFinish {
                sign_in: omitted,
                code: "fake-code".to_owned(),
                state,
                path: Some(cli_path.clone()),
                iss: None,
            },
        )
        .await;
        match refused {
            Err((ErrorCode::InvalidArgument, message)) => {
                assert_eq!(message, super::SIGN_IN_INCOMPLETE)
            }
            other => panic!("a missing issuer must be refused: {other:?}"),
        }
        assert!(forms.lock().unwrap().is_empty());
        assert_eq!(running.server.store.last_seq().await.unwrap(), cursor);

        // A crossed issuer is refused the same way.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("cli-crossed".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: Some(loopback.to_owned()),
            }),
        )
        .await;
        let crossed: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let authorize_url = bind_command_line_sign_in(&client, &running, crossed).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let refused = try_command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInFinish {
                sign_in: crossed,
                code: "fake-code".to_owned(),
                state,
                path: Some(cli_path.clone()),
                iss: Some("https://another.example".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(refused, Err((ErrorCode::InvalidArgument, _))),
            "a crossed issuer must be refused: {refused:?}"
        );
        assert!(forms.lock().unwrap().is_empty());

        // The issuer the metadata named finishes the flow.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("cli-named".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: Some(loopback.to_owned()),
            }),
        )
        .await;
        let named: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let authorize_url = bind_command_line_sign_in(&client, &running, named).await;
        let state = query_parameter(&authorize_url, "state").expect("the URL carries a state");
        let finished = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInFinish {
                sign_in: named,
                code: "fake-code".to_owned(),
                state,
                path: Some(cli_path),
                iss: Some(authorization_server),
            },
        )
        .await;
        assert_eq!(finished["state"], json!("finished"));
        assert_eq!(forms.lock().unwrap().len(), 1, "one code was exchanged");

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_start_deliveries_plan_one_sign_in() {
        let (
            mcp_url,
            _authorization_server,
            _tokens,
            _forms,
            calls,
            registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let (mut first, token) = admin(&running, &invite).await;
        let mut second = open_socket(&running, &token).await;
        let start = neha::surface::McpSignInStart {
            server: None,
            name: Some("docs".to_owned()),
            url: Some(mcp_url),
            scope: None,
            redirect_uri: None,
        };
        let command_id = Id::new();
        let cursor = running.server.store.last_seq().await.unwrap();

        // Both sockets deliver the same command before either answer: one
        // plans discovery and registration, the other waits and replays.
        let (one, two) = tokio::join!(
            command(
                &mut first,
                command_id,
                CommandCall::McpSignInStart(start.clone())
            ),
            command(
                &mut second,
                command_id,
                CommandCall::McpSignInStart(start.clone())
            ),
        );

        assert_eq!(one, two, "both deliveries read the same recorded answer");
        assert_eq!(
            running.server.signins.lock().await.len(),
            1,
            "one pending sign-in, not two flows"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one discovery request"
        );
        assert_eq!(
            registrations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one dynamic client registration"
        );
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor,
            "a start writes no event"
        );

        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_package_servers_configured_headers_reach_discovery() {
        let (
            mcp_url,
            _authorization_server,
            seen,
            _forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let directory = tempfile::tempdir().unwrap();
        let home = test_home(directory.path());
        let server = Id::new();
        let by = Id::new();
        let store = Store::open(&home).await.unwrap();
        store
            .append(AppendRequest::new(
                Id::new(),
                "seed",
                vec![EventDraft::new(
                    EventKind::McpServerAdded,
                    Author::User(by),
                    json!({
                        "server": server,
                        "name": "packaged",
                        "url": mcp_url,
                        "scope": "space",
                        "user": null,
                        "by": by,
                        "plugin": null,
                        "headers": {"x-package": "yes"},
                    }),
                )],
            ))
            .await
            .unwrap();
        store.close().await.unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");
        let mut socket = admin_socket(&running, &invite).await;

        // A package's fixed headers are how the server admits the discovery
        // request: the sign-in must carry them.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: Some(server),
                name: None,
                url: None,
                scope: Some(neha::record::McpScope::Space),
                redirect_uri: None,
            }),
        )
        .await;
        serde_json::from_value::<Id>(started["sign_in"].clone()).unwrap();
        assert!(
            seen.lock().unwrap().contains(&"x-package:yes".to_owned()),
            "the configured header reaches discovery: {:?}",
            seen.lock().unwrap()
        );

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_sees_space_library_changes_and_not_personal_ones() {
        let (_dir, running, invite) = running().await;
        let mut admin_socket = admin_socket(&running, &invite).await;
        let admin = running
            .server
            .identity
            .admin_account()
            .await
            .unwrap()
            .expect("the Space has an Admin");
        let member = running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let member_token = sign_in(&running, "Grace").await;
        let mut member_socket = open_socket(&running, &member_token).await;
        // Both sockets read the stream live from here on.
        snapshot_events(&mut admin_socket).await;
        snapshot_events(&mut member_socket).await;

        // The member's own events stay theirs and arrive.
        let created = command(&mut member_socket, Id::new(), add_bot("Reader", None)).await;
        let _member_bot: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();

        let (url, _log, mcp_task) = fake_mcp_server(|_| json!({"content": []})).await;
        // The Admin adds a server for the whole Space and one of their own.
        let space_server =
            add_mcp_server(&mut admin_socket, "space-docs", &url, Some("token-1")).await;
        let personal = command(
            &mut admin_socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: "personal-docs".to_owned(),
                url: url.clone(),
                scope: neha::record::McpScope::User,
                credential: Some("token-2".to_owned()),
            }),
        )
        .await;
        let personal: Id = serde_json::from_value(personal["server"].clone()).unwrap();
        // Attaching a credential updates the server. A Space credential's
        // change is the Space's and every member sees it; a personal
        // credential's change records the User scope even on the Space's
        // server, so the member never learns who attached it.
        command(
            &mut admin_socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: "space-docs".to_owned(),
                url: url.clone(),
                scope: neha::record::McpScope::Space,
                credential: Some("attached-token".to_owned()),
            }),
        )
        .await;
        command(
            &mut admin_socket,
            Id::new(),
            CommandCall::McpConnect {
                server: personal,
                credential: Some("attached-token".to_owned()),
            },
        )
        .await;
        // A Space skill enters and leaves the Space's library.
        let added = command(
            &mut admin_socket,
            Id::new(),
            add_skill(None, "---\nname: shared\ndescription: d\n---\nBody.", &[]),
        )
        .await;
        let space_skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut admin_socket,
            Id::new(),
            CommandCall::SkillRemove { skill: space_skill },
        )
        .await;
        // A Bot's skill enters and leaves that Bot's own library.
        let created = command(&mut admin_socket, Id::new(), add_bot("Writer", None)).await;
        let writer: Id = serde_json::from_value(created["bot"]["id"].clone()).unwrap();
        let added = command(
            &mut admin_socket,
            Id::new(),
            add_skill(
                Some(writer),
                "---\nname: private\ndescription: d\n---\nBody.",
                &[],
            ),
        )
        .await;
        let bot_skill: Id = serde_json::from_value(added["skill"].clone()).unwrap();
        command(
            &mut admin_socket,
            Id::new(),
            CommandCall::SkillRemove { skill: bot_skill },
        )
        .await;
        // Both servers leave: a Space server's removal is every member's, a
        // personal one's stays its owner's.
        for server in [space_server, personal] {
            command(
                &mut admin_socket,
                Id::new(),
                CommandCall::McpRemove { server },
            )
            .await;
        }
        // A package's installation and enablement name what the Admin
        // installed: the command surface offers them to no member, so the
        // stream keeps them the Admin's. A removal carries only the package
        // id and is every member's (its visibility has its own test). Dispatch
        // it the way the socket's command path does.
        let plugin = Id::new();
        let appended = running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                "plugin-enabled".to_owned(),
                vec![EventDraft::new(
                    EventKind::PluginEnabled,
                    Author::User(admin.id),
                    json!({"plugin": plugin}),
                )],
            ))
            .await
            .unwrap();
        super::broadcast(&running.server, &appended.events);

        let events = live_events(&mut member_socket).await;
        assert!(
            events.iter().any(|event| {
                event.kind == EventKind::McpServerUpdated
                    && event.data["server"] == json!(space_server)
                    && event.data["scope"] == json!("space")
            }),
            "a member sees a Space server's update"
        );
        assert!(
            events.iter().any(|event| {
                event.kind == EventKind::McpServerRemoved
                    && event.data["server"] == json!(space_server)
                    && event.data["scope"] == json!("space")
            }),
            "a member sees a Space server's removal"
        );
        assert!(
            events.iter().any(|event| {
                event.kind == EventKind::SkillRemoved
                    && event.data["skill"] == json!(space_skill)
                    && event.data["library"] == json!("space")
            }),
            "a member sees a Space skill leave"
        );
        assert!(
            events.iter().any(|event| {
                event.kind == EventKind::SkillAdded
                    && event.data["skill"] == json!(space_skill)
                    && event.data["library"] == json!("space")
            }),
            "a member sees the Space's skill enter"
        );
        assert!(
            events
                .iter()
                .all(|event| event.data.get("server") != Some(&json!(personal))),
            "the Admin's personal server's changes stay the Admin's"
        );
        assert!(
            events.iter().all(|event| {
                !(event.kind == EventKind::SkillRemoved && event.data["skill"] == json!(bot_skill))
            }),
            "a Bot's skill stays its owner's"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event.kind,
                EventKind::PluginInstalled | EventKind::PluginEnabled | EventKind::PluginDisabled
            )),
            "a member receives no installation or enablement event"
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == EventKind::BotCreated
                    && event.author == Author::User(member.id)),
            "the member's own events still arrive"
        );

        // The Admin is the author of every filtered event, so the record the
        // Admin reads proves those events exist and only the member's stream
        // was narrowed. They are read from the catch-up path: the commands
        // above already consumed the live frames queued for the Admin.
        let (_cursor, admin_events) = snapshot_events(&mut admin_socket).await;
        for expected in [
            (EventKind::McpServerUpdated, json!(personal)),
            (EventKind::McpServerRemoved, json!(personal)),
            (EventKind::SkillRemoved, json!(bot_skill)),
            (EventKind::PluginEnabled, json!(plugin)),
        ] {
            let (kind, named) = expected;
            assert!(
                admin_events.iter().any(|event| {
                    event.kind == kind
                        && (event.data.get("server") == Some(&named)
                            || event.data.get("skill") == Some(&named)
                            || event.data.get("plugin") == Some(&named))
                }),
                "the Admin sees their own {kind:?}"
            );
        }

        member_socket.close(None).await.unwrap();
        admin_socket.close(None).await.unwrap();
        mcp_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_personal_credential_attachment_stays_the_members_own() {
        let (_dir, running, invite) = running().await;
        let mut admin_socket = admin_socket(&running, &invite).await;
        let grace = running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        running
            .server
            .identity
            .add_account_for_test("Lin", "correct horse battery")
            .await
            .unwrap();
        let grace_token = sign_in(&running, "Grace").await;
        let lin_token = sign_in(&running, "Lin").await;
        let mut grace_socket = open_socket(&running, &grace_token).await;
        let mut lin_socket = open_socket(&running, &lin_token).await;

        let (url, _log, mcp_task) = fake_mcp_server(|_| json!({"content": []})).await;
        let server = add_mcp_server(&mut admin_socket, "space-docs", &url, Some("token-1")).await;
        // Both members read the stream live from here on.
        snapshot_events(&mut grace_socket).await;
        snapshot_events(&mut lin_socket).await;

        // Grace attaches her own credential to the Space's server. The event
        // names her credential change, not the server's scope, so only she
        // receives it: the other member cannot learn who attached what.
        command(
            &mut grace_socket,
            Id::new(),
            CommandCall::McpConnect {
                server,
                credential: Some("grace-token".to_owned()),
            },
        )
        .await;
        let grace_events = live_events(&mut grace_socket).await;
        let lin_events = live_events(&mut lin_socket).await;
        assert!(
            grace_events.iter().any(|event| {
                event.kind == EventKind::McpServerUpdated
                    && event.data["server"] == json!(server)
                    && event.data["scope"] == json!("user")
                    && event.data["by"] == json!(grace.id)
            }),
            "the attaching member sees their own credential's change"
        );
        assert!(
            lin_events
                .iter()
                .all(|event| event.data.get("server") != Some(&json!(server))),
            "the other member receives no event for the personal attach"
        );

        // The Admin attaching the Space's credential records a Space change,
        // which every member receives.
        command(
            &mut admin_socket,
            Id::new(),
            CommandCall::McpAdd(neha::surface::McpAdd {
                name: "space-docs".to_owned(),
                url: url.clone(),
                scope: neha::record::McpScope::Space,
                credential: Some("space-token".to_owned()),
            }),
        )
        .await;
        let lin_events = live_events(&mut lin_socket).await;
        assert!(
            lin_events.iter().any(|event| {
                event.kind == EventKind::McpServerUpdated
                    && event.data["server"] == json!(server)
                    && event.data["scope"] == json!("space")
            }),
            "the Admin's Space credential change broadcasts to the member"
        );

        // Neither change touched the server the Space connected: the event's
        // scope describes the credential change, and the projection ignores
        // it for the server's own scope and user.
        {
            let state = running.server.state.lock().unwrap();
            let view = state.mcp(server).expect("the server stays connected");
            assert_eq!(
                view.scope,
                neha::record::McpScope::Space,
                "an attachment never changes the server's scope"
            );
            assert_eq!(view.user, None, "a Space server has no User");
        }

        grace_socket.close(None).await.unwrap();
        lin_socket.close(None).await.unwrap();
        admin_socket.close(None).await.unwrap();
        mcp_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_refreshes_when_a_package_is_removed() {
        let (_dir, running, invite) = running().await;
        let mut admin_socket = admin_socket(&running, &invite).await;
        let admin = running
            .server
            .identity
            .admin_account()
            .await
            .unwrap()
            .expect("the Space has an Admin");
        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let member_token = sign_in(&running, "Grace").await;
        let mut member_socket = open_socket(&running, &member_token).await;
        snapshot_events(&mut admin_socket).await;
        snapshot_events(&mut member_socket).await;

        // The Admin installs a package and then removes it. The installation
        // names the package, its source, and its digest and stays the Admin's;
        // the removal carries only the id, so every account learns the package
        // left and can refresh what it offered.
        let plugin = Id::new();
        for (name, draft) in [
            (
                "plugin-installed",
                EventDraft::new(
                    EventKind::PluginInstalled,
                    Author::User(admin.id),
                    json!({
                        "plugin": plugin,
                        "name": "reports",
                        "digest": "a".repeat(64),
                        "source": "https://example.com/reports",
                    }),
                ),
            ),
            (
                "plugin-removed",
                EventDraft::new(
                    EventKind::PluginRemoved,
                    Author::User(admin.id),
                    json!({"plugin": plugin}),
                ),
            ),
        ] {
            let appended = running
                .server
                .store
                .append(AppendRequest::new(Id::new(), name.to_owned(), vec![draft]))
                .await
                .unwrap();
            super::broadcast(&running.server, &appended.events);
        }

        let events = live_events(&mut member_socket).await;
        assert!(
            events.iter().any(|event| {
                event.kind == EventKind::PluginRemoved && event.data["plugin"] == json!(plugin)
            }),
            "a member receives the package removal and refreshes"
        );
        assert!(
            events
                .iter()
                .all(|event| event.kind != EventKind::PluginInstalled),
            "the installation's source, commit, and digest stay the Admin's"
        );

        // The catch-up path applies the same rule: a member that subscribes
        // after the removal reads it from the record, and still not the
        // installation.
        let mut late_socket = open_socket(&running, &member_token).await;
        let (_cursor, caught_up) = snapshot_events(&mut late_socket).await;
        assert!(
            caught_up.iter().any(|event| {
                event.kind == EventKind::PluginRemoved && event.data["plugin"] == json!(plugin)
            }),
            "the catch-up path delivers the package removal"
        );
        assert!(
            caught_up
                .iter()
                .all(|event| event.kind != EventKind::PluginInstalled),
            "the catch-up path withholds the installation"
        );

        late_socket.close(None).await.unwrap();
        member_socket.close(None).await.unwrap();
        admin_socket.close(None).await.unwrap();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_browser_flow_leaves_the_newer_flows_cookie_alone() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut socket = open_socket(&running, token).await;
        let session_cookie = browser_session(&running, "Ada").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let callback = sign_in_callback(&running, &authorization_server);
        let bind_url = format!("{}/api/mcp/signin/bind", running.browser_url());

        // The account starts one browser flow and binds it, then a command
        // line flow (which owns no browser cookie and stays pending), then a
        // second browser flow. The Space keeps one pending browser flow per
        // account, so the second replaces the first; each flow's binding
        // cookie has its own name.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("first".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let first: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (first_cookie, _first_nonce, first_authorize) =
            bind_sign_in(&client, &running, first, Some(&session_cookie)).await;
        let first_state =
            query_parameter(&first_authorize, "state").expect("the URL carries a state");

        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("command-line".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: Some("http://127.0.0.1:4714".to_owned()),
            }),
        )
        .await;
        let command_line: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("second".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let second: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();

        let replaced = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: first },
        )
        .await;
        assert_eq!(replaced["state"], json!("failed"));
        assert_eq!(
            replaced["reason"],
            json!(super::SIGN_IN_REPLACED),
            "the replaced flow reads the fixed sentence"
        );
        let untouched = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus {
                sign_in: command_line,
            },
        )
        .await;
        assert_eq!(
            untouched["state"],
            json!("pending"),
            "a command line flow is not replaced"
        );

        // A replaced flow cannot bind again: a terminal flow's bind reads the
        // fixed failure and writes no cookie, so it cannot overwrite the newer
        // flow's binding.
        let stale_bind = client
            .get(&bind_url)
            .query(&[("sign_in", first.to_string())])
            .header("cookie", &session_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(stale_bind.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            stale_bind.headers().get("set-cookie").is_none(),
            "a terminal flow's bind sets no cookie"
        );
        let body = stale_bind.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&first.to_string()),
            "the page echoes no sign-in id: {body}"
        );

        // The newer flow binds under its own cookie name, so both answers can
        // reach one browser without either overwriting the other's binding.
        let (second_cookie, _second_nonce, second_authorize) =
            bind_sign_in(&client, &running, second, Some(&session_cookie)).await;
        assert_ne!(
            first_cookie.split('=').next(),
            second_cookie.split('=').next(),
            "each flow's binding has its own cookie name"
        );
        let second_state =
            query_parameter(&second_authorize, "state").expect("the URL carries a state");

        // The browser holds both binding cookies now. The older flow's
        // callback arrives after the newer bind; its state names no flow any
        // more, so the fixed failure must clear neither cookie.
        let stale = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", first_state.as_str())])
            .header("cookie", format!("{first_cookie}; {second_cookie}"))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            stale.headers().get("set-cookie").is_none(),
            "a spent state's failure must not clear the newer flow's cookie"
        );
        let body = stale.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert!(
            !body.contains(&first_state) && !body.contains("fake-code"),
            "the page echoes neither the state nor the code: {body}"
        );
        assert!(
            forms.lock().unwrap().is_empty(),
            "a spent state's callback exchanges no code"
        );

        // The newer flow still finishes with both cookies in the browser, and
        // its answer clears only its own binding.
        let page = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", second_state.as_str())])
            .header("cookie", format!("{first_cookie}; {second_cookie}"))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        let cleared = page
            .headers()
            .get("set-cookie")
            .expect("the finished flow clears its binding")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            cleared.starts_with(&format!("{}=", super::sign_in_cookie_name(second))),
            "the flow that finished clears its own binding cookie: {cleared}"
        );
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(
            !cleared.contains(&super::sign_in_cookie_name(first)),
            "the answer clears no other flow's cookie: {cleared}"
        );
        let body = page.text().await.unwrap();
        assert!(body.contains("finished"), "{body}");
        let finished = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: second },
        )
        .await;
        assert_eq!(finished["state"], json!("finished"));
        assert_eq!(forms.lock().unwrap().len(), 1, "one code was exchanged");

        // A finished flow cannot bind: it already has its browser.
        let finished_bind = client
            .get(&bind_url)
            .query(&[("sign_in", second.to_string())])
            .header("cookie", &session_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(finished_bind.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            finished_bind.headers().get("set-cookie").is_none(),
            "a finished flow's bind sets no cookie"
        );

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_browsers_bind_cannot_overwrite_the_newer_flows_cookie() {
        let (
            mcp_url,
            authorization_server,
            _tokens,
            _forms,
            _calls,
            _registrations,
            mcp_task,
            as_task,
        ) = fake_sign_in_landscape().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut socket = open_socket(&running, token).await;
        let session_cookie = browser_session(&running, "Ada").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let callback = sign_in_callback(&running, &authorization_server);
        let bind_url = format!("{}/api/mcp/signin/bind", running.browser_url());

        // Flow A starts and binds. Its bind answer is the one an in-progress
        // request would write; the browser receives it only after B's answer.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("first".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let first: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (first_cookie, _first_nonce, first_authorize) =
            bind_sign_in(&client, &running, first, Some(&session_cookie)).await;
        let first_state =
            query_parameter(&first_authorize, "state").expect("the URL carries a state");

        // Flow B for the same account replaces A and binds. Both answers write
        // their own cookie names, so the older one cannot overwrite B's.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("second".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let second: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (second_cookie, _second_nonce, second_authorize) =
            bind_sign_in(&client, &running, second, Some(&session_cookie)).await;
        let second_state =
            query_parameter(&second_authorize, "state").expect("the URL carries a state");
        assert_ne!(
            first_cookie.split('=').next(),
            second_cookie.split('=').next(),
            "each flow's binding has its own cookie name"
        );

        // A's bind request that was in flight when B replaced A completes: the
        // handler re-checks the replaced flow under the lock and refuses it,
        // setting no cookie, so B's binding is untouched.
        let stale_bind = client
            .get(&bind_url)
            .query(&[("sign_in", first.to_string())])
            .header("cookie", &session_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(stale_bind.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            stale_bind.headers().get("set-cookie").is_none(),
            "a replaced flow's bind sets no cookie"
        );

        // The callback reads the cookie its matched flow names: B's state with
        // only A's cookie is no binding, and its refusal clears nothing.
        let crossed = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", second_state.as_str())])
            .header("cookie", &first_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(crossed.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            crossed.headers().get("set-cookie").is_none(),
            "another flow's cookie completes nothing and clears nothing"
        );

        // The browser holds both binding cookies now. A's state names no flow
        // any more, so its callback clears neither.
        let both = format!("{first_cookie}; {second_cookie}");
        let stale = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", first_state.as_str())])
            .header("cookie", &both)
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            stale.headers().get("set-cookie").is_none(),
            "a spent state's failure clears no binding"
        );

        // B's callback succeeds although A's cookie is in the browser too, and
        // it clears only its own binding.
        let page = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", second_state.as_str())])
            .header("cookie", &both)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        let cleared = page
            .headers()
            .get("set-cookie")
            .expect("the finished flow clears its binding")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            cleared.starts_with(&format!("{}=", super::sign_in_cookie_name(second))),
            "the flow that finished clears its own binding cookie: {cleared}"
        );
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(
            !cleared.contains(&super::sign_in_cookie_name(first)),
            "the answer clears no other flow's cookie: {cleared}"
        );
        let body = page.text().await.unwrap();
        assert!(body.contains("finished"), "{body}");
        let finished = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: second },
        )
        .await;
        assert_eq!(finished["state"], json!("finished"));

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_flow_discards_the_token_its_exchange_returns() {
        let (
            (
                mcp_url,
                authorization_server,
                _tokens,
                forms,
                _calls,
                _registrations,
                mcp_task,
                as_task,
            ),
            delay,
        ) = fake_sign_in_landscape_delayed().await;
        let (_dir, running, invite) = running().await;
        let session = join(&running, &invite, "Ada", "correct horse battery").await;
        let token = session["token"].as_str().unwrap();
        let mut socket = open_socket(&running, token).await;
        let session_cookie = browser_session(&running, "Ada").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let callback = sign_in_callback(&running, &authorization_server);

        // Flow A starts and binds, then its callback claims the flow and its
        // token request stops inside the fake authorization server.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("first".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let first: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let (first_cookie, _first_nonce, first_authorize) =
            bind_sign_in(&client, &running, first, Some(&session_cookie)).await;
        let first_state =
            query_parameter(&first_authorize, "state").expect("the URL carries a state");
        let first_callback = tokio::spawn({
            let client = client.clone();
            let callback = callback.clone();
            let state = first_state.clone();
            let cookie = first_cookie.clone();
            async move {
                client
                    .get(&callback)
                    .query(&[("code", "fake-code"), ("state", state.as_str())])
                    .header("cookie", cookie)
                    .send()
                    .await
                    .unwrap()
            }
        });
        // The exchange is in flight: the flow is admitted but not yet
        // committed, so a poll still reads pending.
        delay.arrived.notified().await;
        let during = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: first },
        )
        .await;
        assert_eq!(during["state"], json!("pending"));

        // Flow B for the same account replaces A. Its start finds A
        // executing and writes the terminal sentence A's delivery must not
        // overwrite.
        let started = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStart(neha::surface::McpSignInStart {
                server: None,
                name: Some("second".to_owned()),
                url: Some(mcp_url.clone()),
                scope: None,
                redirect_uri: None,
            }),
        )
        .await;
        let second: Id = serde_json::from_value(started["sign_in"].clone()).unwrap();
        let replaced = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: first },
        )
        .await;
        assert_eq!(replaced["state"], json!("failed"));
        assert_eq!(replaced["reason"], json!(super::SIGN_IN_REPLACED));

        // Flow B binds and finishes while A's token request is still held.
        let (second_cookie, _second_nonce, second_authorize) =
            bind_sign_in(&client, &running, second, Some(&session_cookie)).await;
        let second_state =
            query_parameter(&second_authorize, "state").expect("the URL carries a state");
        let page = client
            .get(&callback)
            .query(&[("code", "fake-code"), ("state", second_state.as_str())])
            .header("cookie", &second_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::OK);
        assert!(page.text().await.unwrap().contains("finished"));
        let finished = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: second },
        )
        .await;
        assert_eq!(finished["state"], json!("finished"));
        let server: Id = serde_json::from_value(finished["server"].clone()).unwrap();
        // B's exchange was the second one: its numbered token is the newer
        // credential, and the Vault holds it now.
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("token-2"),
            "the winning flow's credential is in the Vault"
        );
        let cursor = running.server.store.last_seq().await.unwrap();

        // Release A's token. Its flow was replaced, so the token is discarded:
        // no Vault write, no event, and the terminal sentence stays.
        delay.release.notify_one();
        let stale = first_callback.await.unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            stale.headers().get("set-cookie").is_none(),
            "a replaced flow's answer clears no newer binding"
        );
        let body = stale.text().await.unwrap();
        assert!(body.contains("failed"), "{body}");
        assert_eq!(
            running.server.store.last_seq().await.unwrap(),
            cursor,
            "a replaced flow's late exchange appends no event"
        );
        assert_eq!(
            running
                .server
                .secrets
                .mcp_credential(server, None)
                .unwrap()
                .as_deref(),
            Some("token-2"),
            "a replaced flow's late token does not overwrite the newer credential"
        );
        let stale_status = command(
            &mut socket,
            Id::new(),
            CommandCall::McpSignInStatus { sign_in: first },
        )
        .await;
        assert_eq!(stale_status["state"], json!("failed"));
        assert_eq!(
            stale_status["reason"],
            json!(super::SIGN_IN_REPLACED),
            "the replacement's terminal sentence stays"
        );
        let events = running
            .server
            .store
            .events_from(Seq::ZERO, 100)
            .await
            .unwrap()
            .events;
        let added: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::McpServerAdded)
            .collect();
        assert_eq!(added.len(), 1, "only the winning flow records a connection");
        assert_eq!(added[0].data["server"], json!(server));
        assert_eq!(
            forms.lock().unwrap().len(),
            2,
            "both exchanges reached the authority"
        );

        socket.close(None).await.unwrap();
        mcp_task.abort();
        as_task.abort();
        running.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn historical_events_without_a_scope_stay_their_authors() {
        let (_dir, running, invite) = running().await;
        let mut admin_socket = admin_socket(&running, &invite).await;
        let admin = running
            .server
            .identity
            .admin_account()
            .await
            .unwrap()
            .expect("the Space has an Admin");
        running
            .server
            .identity
            .add_account_for_test("Grace", "correct horse battery")
            .await
            .unwrap();
        let member_token = sign_in(&running, "Grace").await;
        let mut member_socket = open_socket(&running, &member_token).await;

        // A record written before the events carried a scope or a library: it
        // says nothing about the Space, so a member must not read it.
        let server = Id::new();
        let skill = Id::new();
        let plugin = Id::new();
        running
            .server
            .store
            .append(AppendRequest::new(
                Id::new(),
                "old-record".to_owned(),
                vec![
                    EventDraft::new(
                        EventKind::McpServerUpdated,
                        Author::User(admin.id),
                        json!({
                            "server": server,
                            "url": null,
                            "credential": true,
                            "by": admin.id,
                        }),
                    ),
                    EventDraft::new(
                        EventKind::McpServerRemoved,
                        Author::User(admin.id),
                        json!({"server": server}),
                    ),
                    EventDraft::new(
                        EventKind::SkillRemoved,
                        Author::User(admin.id),
                        json!({"skill": skill}),
                    ),
                    EventDraft::new(
                        EventKind::PluginInstalled,
                        Author::User(admin.id),
                        json!({
                            "plugin": plugin,
                            "name": "reports",
                            "digest": "a".repeat(64),
                            "source": "https://example.com/reports",
                        }),
                    ),
                ],
            ))
            .await
            .unwrap();

        // The author reads the old shaped events back.
        let (_cursor, admin_events) = snapshot_events(&mut admin_socket).await;
        assert!(admin_events.iter().any(|event| {
            event.kind == EventKind::McpServerUpdated
                && event.data.get("scope").is_none()
                && event.data["server"] == json!(server)
        }));
        assert!(admin_events.iter().any(|event| {
            event.kind == EventKind::McpServerRemoved
                && event.data.get("scope").is_none()
                && event.data["server"] == json!(server)
        }));
        assert!(admin_events.iter().any(|event| {
            event.kind == EventKind::SkillRemoved
                && event.data.get("library").is_none()
                && event.data["skill"] == json!(skill)
        }));
        assert!(
            admin_events
                .iter()
                .any(|event| event.kind == EventKind::PluginInstalled)
        );

        // A member receives none of them, and still receives what belongs to
        // the Space itself.
        let (_cursor, member_events) = snapshot_events(&mut member_socket).await;
        assert!(
            member_events.iter().all(|event| {
                event.data.get("server") != Some(&json!(server))
                    && event.data.get("skill") != Some(&json!(skill))
                    && event.data.get("plugin") != Some(&json!(plugin))
            }),
            "the historical events stay their author's"
        );
        assert!(
            member_events
                .iter()
                .any(|event| event.kind == EventKind::UserJoined),
            "a Space-wide event still reaches a member"
        );

        member_socket.close(None).await.unwrap();
        admin_socket.close(None).await.unwrap();
        running.shutdown().await;
    }
}
