//! The Space's HTTP and WebSocket server.
//!
//! A client joins with the Space's invitation and password, then reaches
//! every capability through one WebSocket. The server listens on loopback
//! until a remote Space is configured.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::rand_core::RngCore as _;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Request, State};
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use neha::error::ErrorCode;
use neha::id::Id;
use neha::record::{Author, EventDraft, EventKind};
use neha::surface::{ClientFrame, CommandCall, MAX_FRAME_BYTES, PROTOCOL_VERSION, ServerFrame};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::commands;
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
    invite: Mutex<Option<Invite>>,
    login: Mutex<LoginLimiter>,
    state: Mutex<SpaceState>,
    secrets: Secrets,
    model: ModelClient,
    events: broadcast::Sender<neha::record::Event>,
    tool_timeout: Duration,
    chat_runs: Mutex<std::collections::HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
}

impl SpaceServer {
    /// Opens a home and prepares its invitation when it has no account.
    pub async fn open(home: Home) -> Result<Self, ServerError> {
        Self::open_with(home, neha_computer::local::DEFAULT_TIMEOUT).await
    }

    async fn open_with(home: Home, tool_timeout: Duration) -> Result<Self, ServerError> {
        let store = Store::open(&home).await?;
        let identity = Identity::open(&home).await?;
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
            invite: Mutex::new(invite),
            login: Mutex::new(LoginLimiter::default()),
            state: Mutex::new(state),
            secrets,
            model,
            events,
            tool_timeout,
            chat_runs: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Returns the lock that serializes runs in one Chat.
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
        Router::new()
            .route("/healthz", get(health))
            .route("/invite", get(invite_page))
            .route("/api/join", post(join))
            .route("/api/login", post(login))
            .route("/api/me", get(me))
            .route("/api/socket", get(socket))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .layer(middleware::from_fn(guard))
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
    home: Home,
    task: JoinHandle<()>,
}

impl Running {
    /// Returns the base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Returns the Space's home.
    #[must_use]
    pub fn server_home(&self) -> std::path::PathBuf {
        self.home.root().to_path_buf()
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

/// Binds a home on loopback and starts serving it with a tool time limit.
pub async fn start_with_tool_timeout(
    home: Home,
    port: u16,
    tool_timeout: Duration,
) -> Result<Running, ServerError> {
    let server = Arc::new(SpaceServer::open_with(home, tool_timeout).await?);
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
        home: server.home().clone(),
        task,
    })
}

async fn guard(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    if !host.is_some_and(loopback_host) {
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
    if origin.is_some() && !bearer {
        return ApiError::forbidden("a browser Origin requires a Bearer token").into_response();
    }

    next.run(request).await
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

    matches!(host, "127.0.0.1" | "localhost" | "::1")
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

async fn health() -> &'static str {
    "ok"
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
    Json(request): Json<JoinRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
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

    Ok(Json(SessionResponse {
        token,
        account: account.into(),
    }))
}

async fn login(
    State(server): State<Arc<SpaceServer>>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    Json(request): Json<LoginRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
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
        Ok((account, token)) => Ok(Json(SessionResponse {
            token,
            account: account.into(),
        })),
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

async fn me(Authed { account }: Authed) -> Json<AccountBody> {
    Json(account.into())
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
                                        match commands::finish_run(&server.store, &server.state, &server.home, &server.model, &server.events, server.tool_timeout, pending).await {
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
        if complete < page_size {
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
        let token = bearer(&parts.headers).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "a Bearer token is required",
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
    /// The model client cannot be built.
    #[error("model error: {0}")]
    Model(#[from] ModelError),
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{Running, SpaceServer, StreamError, catch_up, start};
    use crate::home::Home;
    use crate::store::{AppendRequest, Store};

    async fn running() -> (tempfile::TempDir, Running, String) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let running = start(home, 0).await.unwrap();
        let invite = running.invite.clone().expect("a fresh Space invites");

        (dir, running, invite)
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
    use neha::id::Id;
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
        Tool { name: String, arguments: Value },
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
            }),
        )
        .await;
        command(
            &mut socket,
            Id::new(),
            CommandCall::MessageSend(MessageSend {
                chat,
                text: "again".to_owned(),
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

        let first = catch_up(&reader, &out, neha::seq::Seq::ZERO, None)
            .await
            .unwrap();
        let second = catch_up(&reader, &out, first, None).await;

        assert_eq!(first, neha::seq::Seq::from_i64(1).unwrap());
        assert!(matches!(second, Err(StreamError::Oversized(_))));
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
}
