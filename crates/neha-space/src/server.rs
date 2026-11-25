//! The Space's HTTP and WebSocket server.
//!
//! A client joins with the Space's invitation and password, then reaches
//! every capability through one WebSocket. The server listens on loopback
//! until a remote Space is configured.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::rand_core::RngCore as _;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::error::ErrorCode;
use neha::id::Id;
use neha::record::{Author, EventDraft, EventKind};
use neha::surface::{ClientFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION, ServerFrame};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::home::Home;
use crate::identity::{Account, Identity, IdentityError};
use crate::store::{AppendRequest, Store, StoreError};

const LOGIN_ATTEMPTS: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const HEARTBEAT: Duration = Duration::from_secs(30);

/// A Space ready to answer clients.
pub struct SpaceServer {
    home: Home,
    store: Store,
    identity: Identity,
    invite: Mutex<Option<Invite>>,
    login: Mutex<LoginLimiter>,
}

impl SpaceServer {
    /// Opens a home and prepares its invitation when it has no account.
    pub async fn open(home: Home) -> Result<Self, ServerError> {
        let store = Store::open(&home).await?;
        let identity = Identity::open(&home).await?;
        let invite = if identity.has_accounts().await? {
            None
        } else {
            Some(Invite::generate())
        };

        Ok(Self {
            home,
            store,
            identity,
            invite: Mutex::new(invite),
            login: Mutex::new(LoginLimiter::default()),
        })
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
    task: JoinHandle<()>,
}

impl Running {
    /// Returns the base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops the server.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Binds a home on loopback and starts serving it.
pub async fn start(home: Home, port: u16) -> Result<Running, ServerError> {
    let server = Arc::new(SpaceServer::open(home).await?);
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let invite = server
        .invite_secret()
        .map(|secret| format!("http://{addr}/invite#{secret}"));
    let router = server.router();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Ok(Running { addr, invite, task })
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
    Json(request): Json<LoginRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    if !server
        .login
        .lock()
        .expect("login lock")
        .allows(&request.display_name)
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
                .record(&request.display_name);
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

async fn handle_socket(mut socket: WebSocket, server: Arc<SpaceServer>, _account: Account) {
    let Some(Ok(Message::Text(text))) = socket.recv().await else {
        return;
    };
    let hello = match serde_json::from_str::<ClientFrame>(text.as_str()) {
        Ok(ClientFrame::Hello { version, .. }) => version,
        _ => {
            let _ = send_frame(
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
        let _ = send_frame(
            &mut socket,
            ServerFrame::ProtocolError {
                message: format!("this Space speaks protocol version {PROTOCOL_VERSION}"),
            },
        )
        .await;
        return;
    }
    if send_frame(
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

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { return; };
                let Message::Text(text) = message else { continue; };
                match serde_json::from_str::<ClientFrame>(text.as_str()) {
                    Ok(ClientFrame::Hello { .. }) => {
                        if send_frame(&mut socket, ServerFrame::ProtocolError { message: "hello was already sent".to_owned() }).await.is_err() {
                            return;
                        }
                    }
                    Ok(ClientFrame::Subscribe { id: _, cursor, limit }) => {
                        let cursor = cursor.unwrap_or(neha::seq::Seq::ZERO);
                        match server.store.events_from(cursor, limit.unwrap_or(256)).await {
                            Ok(page) => {
                                if send_frame(&mut socket, ServerFrame::Snapshot { cursor: page.next, events: page.events }).await.is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                if send_frame(&mut socket, ServerFrame::Error { id: None, code: ErrorCode::Internal, message: error.to_string() }).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Ok(ClientFrame::Command { id, .. }) => {
                        if send_frame(&mut socket, ServerFrame::Error { id: Some(id), code: ErrorCode::Unavailable, message: "this command is not served yet".to_owned() }).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        if send_frame(&mut socket, ServerFrame::ProtocolError { message: "the frame is not valid JSON".to_owned() }).await.is_err() {
                            return;
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                let cursor = server.store.last_seq().await.unwrap_or(neha::seq::Seq::ZERO);
                if send_frame(&mut socket, ServerFrame::Heartbeat { cursor }).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn send_frame(socket: &mut WebSocket, frame: ServerFrame) -> Result<(), ()> {
    let text = serde_json::to_string(&frame).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
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
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            message,
        )
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
}

impl LoginLimiter {
    fn allows(&mut self, display_name: &str) -> bool {
        let now = Instant::now();
        let failures = self.failures.entry(display_name.to_owned()).or_default();
        failures.retain(|at| now.duration_since(*at) < LOGIN_WINDOW);

        failures.len() < LOGIN_ATTEMPTS
    }

    fn record(&mut self, display_name: &str) {
        self.failures
            .entry(display_name.to_owned())
            .or_default()
            .push(Instant::now());
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
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{Running, start};
    use crate::home::Home;

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
}
