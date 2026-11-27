//! The Model Context Protocol client.
//!
//! The Space calls a Plugin's MCP server over the Streamable HTTP transport:
//! one JSON-RPC 2.0 message per POST, answered with a JSON document or an
//! event stream. This module owns that wire protocol and nothing else; it
//! starts no program and holds no server state. A server the Space cannot
//! reach, or whose answer does not fit the protocol, becomes an [`McpError`]
//! that the Space reports, and the credential never enters an error message.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use futures_util::StreamExt as _;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};

/// The MCP protocol revision this client speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The header a server names the session it made for the client.
const SESSION_ID_HEADER: &str = "Mcp-Session-Id";

/// The header a client names the protocol revision it speaks.
const PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";

/// How long one request may wait for its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest answer this client reads for one request.
const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// The most pages one tool listing may follow.
///
/// A server that keeps naming fresh cursors would otherwise spend the run's
/// whole budget before the Model sees a single tool.
const MAX_TOOL_PAGES: usize = 16;

/// The most tools this client accepts from one server.
const MAX_TOOLS: usize = 256;

/// A tool one server offers.
#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    /// The tool's original name.
    pub name: String,
    /// What the tool does; empty when the server declares none.
    pub description: String,
    /// The tool's JSON input schema; empty when the server declares none.
    pub input_schema: Value,
}

/// One content block of a tool result.
#[derive(Clone, Debug, PartialEq)]
pub enum McpContent {
    /// Text the Model reads as text.
    Text(String),
    /// An image the Space keeps as a File.
    Image {
        /// The image's media type.
        media_type: String,
        /// The image's bytes, decoded from the server's base64.
        data: Vec<u8>,
    },
    /// A block this client does not model, as the server sent it.
    Other(Value),
}

/// What one tool call answered.
#[derive(Clone, Debug, PartialEq)]
pub struct McpAnswer {
    /// The answer's content blocks, in the order the server sent them.
    pub content: Vec<McpContent>,
    /// Whether the server reported the call as failed.
    pub is_error: bool,
}

/// One MCP server the Space may call.
///
/// A client is one use: [`McpClient::open`] performs the handshake, and
/// [`McpClient::tools`] and [`McpClient::call`] work only after it.
pub struct McpClient {
    /// The server's address: HTTPS anywhere, or HTTP on a loopback address.
    url: reqwest::Url,
    /// The bearer token the server requires, when the Space has one.
    credential: Option<String>,
    /// The one connection pool every request of this use shares.
    http: reqwest::Client,
    /// The next JSON-RPC request id.
    next_id: u64,
    /// The session the server named in its `initialize` answer.
    session_id: Option<String>,
    /// Whether the handshake settled the protocol revision.
    negotiated: bool,
    /// Whether the handshake completed.
    opened: bool,
    /// How long one request may wait for its answer.
    timeout: Duration,
}

impl McpClient {
    /// Builds a client for one server address and an optional bearer token.
    ///
    /// The address is HTTPS, or HTTP on a loopback address: a server on the
    /// Space's own machine needs no certificate, and any other server is
    /// called over HTTPS.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Protocol`] when the address is empty, invalid, or
    /// does not meet that rule.
    pub fn new(url: &str, credential: Option<&str>) -> Result<Self, McpError> {
        let url = reqwest::Url::parse(url.trim())
            .map_err(|_| protocol_error("the MCP server URL is not valid"))?;
        let allowed = match url.scheme() {
            "https" => url.has_host(),
            "http" => is_loopback(&url),
            _ => false,
        };
        if !allowed
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(protocol_error(
                "the MCP server URL must be https://, or http:// on a loopback address, and carry no credentials or fragment",
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // A redirect could leave the address rule behind; a server answers
            // where it was connected.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unreachable_error("the HTTP client could not be built"))?;

        Ok(Self {
            url,
            credential: credential
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            http,
            next_id: 1,
            session_id: None,
            negotiated: false,
            opened: false,
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Uses another request timeout, for a caller that shows a status rather
    /// than waiting for an answer.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Performs the `initialize` handshake and the `notifications/initialized`
    /// notification.
    ///
    /// Call it once, before [`McpClient::tools`] or [`McpClient::call`].
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Unreachable`] when the server cannot be reached,
    /// [`McpError::NotConnected`] when it refuses the credential, and
    /// [`McpError::Protocol`] when its answer does not finish the handshake.
    pub async fn open(&mut self) -> Result<(), McpError> {
        if self.opened {
            return Ok(());
        }
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "neha",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let answer = self.rpc("initialize", Some(params)).await?;
        let negotiated = answer
            .result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                protocol_error("the MCP server's handshake named no protocol version")
            })?;
        if negotiated != PROTOCOL_VERSION {
            return Err(protocol_error(
                "the MCP server speaks a protocol version this Space does not",
            ));
        }
        self.session_id = answer.session_id;
        self.negotiated = true;
        self.notify("notifications/initialized").await?;
        self.opened = true;
        Ok(())
    }

    /// Lists every tool the server offers, following `nextCursor` pagination.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Protocol`] when the client is not open or the
    /// server's answer does not fit the protocol, and the transport failures
    /// [`McpClient::open`] reports.
    pub async fn tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        self.require_open()?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0_usize;
        loop {
            pages = pages.saturating_add(1);
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let answer = self.rpc("tools/list", Some(params)).await?;
            let page = answer.result.as_object().ok_or_else(malformed)?;
            let listed = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(malformed)?;
            for tool in listed {
                if tools.len() >= MAX_TOOLS {
                    return Err(protocol_error("the MCP server lists too many tools"));
                }
                tools.push(tool_from_json(tool)?);
            }
            if pages >= MAX_TOOL_PAGES {
                return Err(protocol_error("the MCP server lists too many pages"));
            }
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            let Some(next) = next else {
                return Ok(tools);
            };
            // A server that repeats a cursor would page forever.
            if !seen.insert(next.clone()) {
                return Err(protocol_error("the MCP server repeated a page cursor"));
            }
            cursor = Some(next);
        }
    }

    /// Calls one tool by its original name.
    ///
    /// A tool result with `isError: true` is a normal call: it returns an
    /// [`McpAnswer`] whose `is_error` is true.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Protocol`] when the client is not open or the
    /// server's answer does not fit the protocol, and the transport failures
    /// [`McpClient::open`] reports.
    pub async fn call(&mut self, tool: &str, arguments: &Value) -> Result<McpAnswer, McpError> {
        self.require_open()?;
        let params = json!({"name": tool, "arguments": arguments});
        let answer = self.rpc("tools/call", Some(params)).await?;
        let result = answer.result.as_object().ok_or_else(malformed)?;
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?
            .iter()
            .map(content_from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let is_error = match result.get("isError") {
            None => false,
            Some(Value::Bool(is_error)) => *is_error,
            Some(_) => return Err(malformed()),
        };

        Ok(McpAnswer { content, is_error })
    }

    /// Rejects a call that comes before the handshake.
    fn require_open(&self) -> Result<(), McpError> {
        if self.opened {
            Ok(())
        } else {
            Err(protocol_error("the MCP client has not been opened"))
        }
    }

    /// Sends one JSON-RPC request and returns the answer to its id.
    async fn rpc(&mut self, method: &str, params: Option<Value>) -> Result<Reply, McpError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }

        match tokio::time::timeout(self.timeout, self.exchange(&message, id)).await {
            Ok(answer) => answer,
            Err(_) => Err(unreachable_error("the server did not answer in time")),
        }
    }

    /// Sends one JSON-RPC notification and accepts any success status.
    async fn notify(&self, method: &str) -> Result<(), McpError> {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match tokio::time::timeout(self.timeout, self.send(&message)).await {
            Ok(response) => ensure_success(response?.status().as_u16()),
            Err(_) => Err(unreachable_error("the server did not answer in time")),
        }
    }

    /// Sends one JSON-RPC message.
    async fn send(&self, message: &Value) -> Result<reqwest::Response, McpError> {
        self.start(message)
            .send()
            .await
            .map_err(|_| unreachable_error("the MCP server could not be reached"))
    }

    /// Builds the POST for one JSON-RPC message.
    fn start(&self, message: &Value) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream");
        if self.negotiated {
            request = request.header(PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION);
        }
        if let Some(session) = &self.session_id {
            request = request.header(SESSION_ID_HEADER, session.as_str());
        }
        if let Some(credential) = &self.credential {
            request = request.bearer_auth(credential.as_str());
        }

        request.json(message)
    }

    /// Sends one request and reads the answer to its id.
    async fn exchange(&self, message: &Value, id: u64) -> Result<Reply, McpError> {
        let response = self.send(message).await?;
        ensure_success(response.status().as_u16())?;
        // Read the headers before the body consumes the response.
        let session_id = header(&response, SESSION_ID_HEADER);
        let content_type = header(&response, CONTENT_TYPE);
        let result = self
            .read_answer(response, content_type.as_deref(), id)
            .await?;

        Ok(Reply { session_id, result })
    }

    /// Reads the JSON-RPC answer carried by one HTTP response.
    async fn read_answer(
        &self,
        response: reqwest::Response,
        content_type: Option<&str>,
        id: u64,
    ) -> Result<Value, McpError> {
        // A server may name its media type with parameters or in another case.
        let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            self.read_event_stream(response, id).await
        } else if content_type.contains("json") {
            let body = read_answer_body(response).await?;
            let answer = serde_json::from_slice(&body)
                .map_err(|_| protocol_error("the MCP server's answer is not JSON"))?;
            let message = matching_message(answer, id).ok_or_else(|| {
                protocol_error("the MCP server's answer did not match the request")
            })?;
            self.response_result(&message)
        } else {
            Err(protocol_error(
                "the MCP server's answer has an unknown content type",
            ))
        }
    }

    /// Reads the first event of an event stream that answers `id`.
    async fn read_event_stream(
        &self,
        response: reqwest::Response,
        id: u64,
    ) -> Result<Value, McpError> {
        let mut chunks = response.bytes_stream();
        let mut pending = Vec::new();
        let mut received = 0_usize;
        // Where the next search for an event end starts: rescanning a whole
        // buffer on every chunk would be quadratic within the body limit.
        let mut scanned = 0_usize;
        loop {
            while let Some((end, separator)) = event_end(&pending, scanned) {
                let event = pending[..end].to_vec();
                pending.drain(..end + separator);
                scanned = 0;
                if let Some(message) = matching_event(&event, id) {
                    return self.response_result(&message);
                }
            }
            // A separator can start up to three bytes before the last byte.
            scanned = pending.len().saturating_sub(3);
            match chunks.next().await {
                Some(Ok(chunk)) => {
                    received = received.saturating_add(chunk.len());
                    if received > MAX_ANSWER_BYTES {
                        return Err(protocol_error("the server's answer is too large"));
                    }
                    pending.extend_from_slice(&chunk);
                }
                Some(Err(_)) => {
                    return Err(unreachable_error("the MCP server closed the connection"));
                }
                None => {
                    return Err(protocol_error(
                        "the MCP server closed the stream without answering",
                    ));
                }
            }
        }
    }

    /// Returns the result of one JSON-RPC answer, or the error it reported.
    fn response_result(&self, message: &Value) -> Result<Value, McpError> {
        let object = message.as_object().ok_or_else(malformed)?;
        if let Some(error) = object.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the server reported an error");
            let message = self.redact(message);
            return Err(protocol_error(&format!(
                "the MCP server reported an error: {message}"
            )));
        }

        object.get("result").cloned().ok_or_else(malformed)
    }

    /// Removes the credential from text a server supplied.
    fn redact(&self, text: &str) -> String {
        match &self.credential {
            Some(credential) => text.replace(credential.as_str(), "[redacted]"),
            None => text.to_owned(),
        }
    }
}

/// The answer to one JSON-RPC request.
struct Reply {
    /// The session the server named, when this answer named one.
    session_id: Option<String>,
    /// The answer's `result`.
    result: Value,
}

/// An MCP call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpError {
    /// The server could not be reached, or did not answer in time.
    Unreachable(String),
    /// The server refused the Space's credential.
    NotConnected {
        /// The HTTP status the server answered.
        status: u16,
    },
    /// The server's answer did not fit the protocol.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(message) | Self::Protocol(message) => formatter.write_str(message),
            Self::NotConnected { status } => {
                write!(formatter, "the MCP server is not connected (HTTP {status})")
            }
        }
    }
}

impl std::error::Error for McpError {}

/// Whether a URL's host is the Space's own machine.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

/// The first text header of a response, under its canonical name.
fn header<K: reqwest::header::AsHeaderName>(
    response: &reqwest::Response,
    name: K,
) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Rejects an HTTP status that is not a success.
fn ensure_success(status: u16) -> Result<(), McpError> {
    if (200..300).contains(&status) {
        Ok(())
    } else if matches!(status, 401 | 403) {
        Err(McpError::NotConnected { status })
    } else {
        Err(protocol_error(&format!(
            "the MCP server answered with HTTP {status}"
        )))
    }
}

/// Reads one response body, refusing to buffer more than [`MAX_ANSWER_BYTES`].
async fn read_answer_body(response: reqwest::Response) -> Result<Vec<u8>, McpError> {
    let mut body = Vec::new();
    if let Some(length) = response.content_length() {
        if length > MAX_ANSWER_BYTES as u64 {
            return Err(protocol_error("the server's answer is too large"));
        }
        body.reserve(length as usize);
    }
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| unreachable_error("the MCP server closed the connection"))?;
        if body.len().saturating_add(chunk.len()) > MAX_ANSWER_BYTES {
            return Err(protocol_error("the server's answer is too large"));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// The end of the first complete event in a stream buffer at or after `from`,
/// and the length of the blank line that ends it.
fn event_end(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..buffer.len()).find_map(|start| {
        let rest = &buffer[start..];
        let length = if rest.starts_with(b"\r\n\r\n") {
            4
        } else if rest.starts_with(b"\n\n") || rest.starts_with(b"\r\r") {
            2
        } else {
            return None;
        };
        Some((start, length))
    })
}

/// The first JSON-RPC message in one event whose id is the request's.
fn matching_event(event: &[u8], id: u64) -> Option<Value> {
    let text = std::str::from_utf8(event).ok()?;
    let mut data = String::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if data.is_empty() {
        return None;
    }

    matching_message(serde_json::from_str(&data).ok()?, id)
}

/// The JSON-RPC message in an answer that carries the request's id.
fn matching_message(answer: Value, id: u64) -> Option<Value> {
    let expected = Value::from(id);
    // A server-initiated request may carry any id, so only an answer with a
    // result or an error answers this client.
    let is_answer = |message: &Value| {
        message.get("id") == Some(&expected)
            && (message.get("result").is_some() || message.get("error").is_some())
    };
    match answer {
        Value::Array(messages) => messages.into_iter().find(|message| is_answer(message)),
        message if is_answer(&message) => Some(message),
        _ => None,
    }
}

/// One tool as the server declared it.
fn tool_from_json(value: &Value) -> Result<McpTool, McpError> {
    let object = value.as_object().ok_or_else(malformed)?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_schema = object
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({}));

    Ok(McpTool {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
    })
}

/// One content block of a tool result.
fn content_from_json(value: &Value) -> Result<McpContent, McpError> {
    let block = value.as_object().ok_or_else(malformed)?;
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            Ok(McpContent::Text(text.to_owned()))
        }
        Some("image") => {
            let media_type = block
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let data = decode_base64(data)
                .ok_or_else(|| protocol_error("the MCP server's image is not base64"))?;
            Ok(McpContent::Image {
                media_type: media_type.to_owned(),
                data,
            })
        }
        _ => Ok(McpContent::Other(value.clone())),
    }
}

/// Decodes standard-alphabet base64, with or without the final padding.
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    STANDARD_NO_PAD.decode(data.trim_end_matches('=')).ok()
}

/// The failure every shape of malformed answer reports.
fn malformed() -> McpError {
    protocol_error("the MCP server's answer did not fit the protocol")
}

/// Builds a [`McpError::Protocol`].
fn protocol_error(message: &str) -> McpError {
    McpError::Protocol(message.to_owned())
}

/// Builds a [`McpError::Unreachable`].
fn unreachable_error(message: &str) -> McpError {
    McpError::Unreachable(message.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
    use axum::response::Response;
    use axum::routing::post;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::{MAX_ANSWER_BYTES, McpClient, McpContent, McpError, PROTOCOL_VERSION};

    /// The session id the fake server names in its handshake.
    const SESSION: &str = "session-1";

    /// One request the fake server received.
    #[derive(Clone)]
    struct Received {
        message: Value,
        headers: HeaderMap,
    }

    type Log = Arc<Mutex<Vec<Received>>>;

    /// What the fake server answers one request.
    type Answer = Arc<dyn Fn(&Value, &HeaderMap) -> Response + Send + Sync>;

    fn log() -> Log {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn received(log: &Log) -> Vec<Received> {
        log.lock().unwrap().clone()
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Runs a fake MCP server on a loopback port.
    async fn server(log: Log, answer: Answer) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: Bytes| {
                let log = log.clone();
                let answer = answer.clone();
                async move {
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(Received {
                        message: message.clone(),
                        headers: headers.clone(),
                    });
                    answer(&message, &headers)
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{address}"), task)
    }

    fn json_response(body: impl Into<String>) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.into()))
            .unwrap()
    }

    fn rpc_result(id: &Value, result: Value) -> Response {
        json_response(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
    }

    fn rpc_error(id: &Value, message: &str) -> Response {
        json_response(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
                .to_string(),
        )
    }

    fn empty(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    }

    fn event_stream(body: String) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    fn sse(messages: &[Value]) -> String {
        messages
            .iter()
            .map(|message| format!("data: {message}\n\n"))
            .collect()
    }

    fn initialize(id: &Value) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .header("mcp-session-id", SESSION)
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": {"name": "fake", "version": "1.0.0"},
                    },
                })
                .to_string(),
            ))
            .unwrap()
    }

    /// The handshake answers, when the message belongs to the handshake.
    fn handshake(message: &Value) -> Option<Response> {
        match message.get("method").and_then(Value::as_str)? {
            "initialize" => Some(initialize(&message["id"])),
            "notifications/initialized" => Some(empty(StatusCode::ACCEPTED)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn performs_the_handshake_and_echoes_the_session() {
        let log = log();
        let (url, task) = server(
            log.clone(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                handshake(message).expect("only the handshake messages arrive")
            }),
        )
        .await;
        let mut client = McpClient::new(&url, Some("secret-token")).unwrap();

        client.open().await.unwrap();

        let requests = received(&log);
        assert_eq!(
            requests.len(),
            2,
            "initialize and the initialized notification"
        );
        let initialize = &requests[0];
        assert_eq!(initialize.message["jsonrpc"], "2.0");
        assert_eq!(initialize.message["method"], "initialize");
        assert_eq!(initialize.message["id"], 1);
        assert_eq!(
            initialize.message["params"]["protocolVersion"],
            PROTOCOL_VERSION
        );
        assert_eq!(initialize.message["params"]["capabilities"], json!({}));
        assert_eq!(initialize.message["params"]["clientInfo"]["name"], "neha");
        assert_eq!(
            initialize.message["params"]["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            header(&initialize.headers, "content-type"),
            Some("application/json")
        );
        assert_eq!(
            header(&initialize.headers, "accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(
            header(&initialize.headers, "authorization"),
            Some("Bearer secret-token")
        );
        assert_eq!(header(&initialize.headers, "mcp-session-id"), None);
        assert_eq!(header(&initialize.headers, "mcp-protocol-version"), None);

        let notification = &requests[1];
        assert_eq!(notification.message["jsonrpc"], "2.0");
        assert_eq!(notification.message["method"], "notifications/initialized");
        assert!(
            notification.message.get("id").is_none(),
            "a notification carries no id"
        );
        assert_eq!(
            header(&notification.headers, "mcp-session-id"),
            Some(SESSION)
        );
        assert_eq!(
            header(&notification.headers, "mcp-protocol-version"),
            Some(PROTOCOL_VERSION)
        );
        assert_eq!(
            header(&notification.headers, "authorization"),
            Some("Bearer secret-token")
        );
        task.abort();
    }

    #[tokio::test]
    async fn lists_tools_across_pages() {
        let log = log();
        let (url, task) = server(
            log.clone(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                match message["params"].get("cursor").and_then(Value::as_str) {
                    None => rpc_result(
                        &message["id"],
                        json!({
                            "tools": [{
                                "name": "search",
                                "description": "Search the web.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                },
                            }],
                            "nextCursor": "page-2",
                        }),
                    ),
                    Some("page-2") => {
                        rpc_result(&message["id"], json!({"tools": [{"name": "fetch"}]}))
                    }
                    Some(other) => panic!("the client sent an unexpected cursor: {other}"),
                }
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "Search the web.");
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(
            tools[0].input_schema["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(tools[1].name, "fetch");
        assert_eq!(tools[1].description, "", "a missing description is empty");
        assert_eq!(
            tools[1].input_schema,
            json!({}),
            "a missing schema is empty"
        );

        let requests = received(&log);
        assert_eq!(requests.len(), 4, "the handshake and two pages");
        assert_eq!(requests[2].message["method"], "tools/list");
        assert_eq!(requests[2].message["id"], 2);
        assert_eq!(requests[2].message["params"], json!({}));
        assert_eq!(
            header(&requests[2].headers, "mcp-session-id"),
            Some(SESSION)
        );
        assert_eq!(
            header(&requests[2].headers, "mcp-protocol-version"),
            Some(PROTOCOL_VERSION)
        );
        assert_eq!(requests[3].message["method"], "tools/list");
        assert_eq!(requests[3].message["id"], 3);
        assert_eq!(requests[3].message["params"], json!({"cursor": "page-2"}));
        task.abort();
    }

    #[tokio::test]
    async fn a_repeated_page_cursor_is_a_protocol_error() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(&message["id"], json!({"tools": [], "nextCursor": "again"}))
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let error = client.tools().await.unwrap_err();

        assert!(
            matches!(error, McpError::Protocol(ref message) if message == "the MCP server repeated a page cursor"),
            "{error}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn calls_a_tool_and_decodes_its_content() {
        let log = log();
        let padded = STANDARD.encode(b"pixel-bytes");
        let unpadded = STANDARD.encode(b"other-bytes");
        let unpadded = unpadded.trim_end_matches('=').to_owned();
        let (url, task) = server(
            log.clone(),
            Arc::new(move |message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                match message["params"]["name"].as_str() {
                    Some("render") => rpc_result(
                        &message["id"],
                        json!({
                            "content": [
                                {"type": "text", "text": "hello"},
                                {"type": "image", "data": padded, "mimeType": "image/png"},
                                {"type": "image", "data": unpadded, "mimeType": "image/jpeg"},
                                {
                                    "type": "resource",
                                    "resource": {"uri": "file:///report.txt"},
                                },
                            ],
                        }),
                    ),
                    Some("broken") => rpc_result(
                        &message["id"],
                        json!({
                            "content": [{
                                "type": "image",
                                "data": "not-base64!",
                                "mimeType": "image/png",
                            }],
                        }),
                    ),
                    other => panic!("the client called an unexpected tool: {other:?}"),
                }
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let answer = client.call("render", &json!({"count": 3})).await.unwrap();

        assert!(!answer.is_error);
        assert_eq!(
            answer.content,
            vec![
                McpContent::Text("hello".to_owned()),
                McpContent::Image {
                    media_type: "image/png".to_owned(),
                    data: b"pixel-bytes".to_vec(),
                },
                McpContent::Image {
                    media_type: "image/jpeg".to_owned(),
                    data: b"other-bytes".to_vec(),
                },
                McpContent::Other(json!({
                    "type": "resource",
                    "resource": {"uri": "file:///report.txt"},
                })),
            ]
        );

        let requests = received(&log);
        let call = requests.last().unwrap();
        assert_eq!(call.message["method"], "tools/call");
        assert_eq!(
            call.message["params"],
            json!({"name": "render", "arguments": {"count": 3}})
        );

        let error = client.call("broken", &json!({})).await.unwrap_err();
        assert!(
            matches!(error, McpError::Protocol(ref message) if message == "the MCP server's image is not base64"),
            "{error}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_tool_error_is_an_answer() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({
                        "content": [{"type": "text", "text": "the file is missing"}],
                        "isError": true,
                    }),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let answer = client.call("read", &json!({})).await.unwrap();

        assert!(answer.is_error);
        assert_eq!(
            answer.content,
            vec![McpContent::Text("the file is missing".to_owned())]
        );
        task.abort();
    }

    #[tokio::test]
    async fn reads_the_answer_from_an_event_stream() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                let progress = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progress": 1},
                });
                let other = json!({"jsonrpc": "2.0", "id": 4096, "result": {"tools": []}});
                let answer = json!({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"tools": [{
                        "name": "ping",
                        "description": "Ping.",
                        "inputSchema": {"type": "object"},
                    }]},
                });
                event_stream(sse(&[progress, other, answer]))
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
        task.abort();
    }

    #[tokio::test]
    async fn reads_the_matching_answer_from_a_batch() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                let progress = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progress": 1},
                });
                let answer = json!({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"tools": [{
                        "name": "batch",
                        "description": "From a batch.",
                        "inputSchema": {"type": "object"},
                    }]},
                });
                json_response(json!([progress, answer]).to_string())
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "batch");
        task.abort();
    }

    #[tokio::test]
    async fn an_event_stream_without_the_answer_is_a_protocol_error() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                event_stream(sse(&[json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progress": 1},
                })]))
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let error = client.tools().await.unwrap_err();

        assert!(matches!(error, McpError::Protocol(_)), "{error}");
        task.abort();
    }

    #[tokio::test]
    async fn a_malformed_answer_is_a_protocol_error() {
        let (url, task) = server(
            log(),
            Arc::new(|_: &Value, _: &HeaderMap| json_response("not json")),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();

        let error = client.open().await.unwrap_err();

        assert!(matches!(error, McpError::Protocol(_)), "{error}");
        task.abort();
    }

    #[tokio::test]
    async fn unauthorized_or_forbidden_is_not_connected() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let (url, task) = server(
                log(),
                Arc::new(move |_: &Value, _: &HeaderMap| empty(status)),
            )
            .await;
            let mut client = McpClient::new(&url, None).unwrap();

            let error = client.open().await.unwrap_err();

            assert!(
                matches!(error, McpError::NotConnected { status: code } if code == status.as_u16()),
                "{error}"
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_server_failure_is_a_protocol_error() {
        let (url, task) = server(
            log(),
            Arc::new(|_: &Value, _: &HeaderMap| empty(StatusCode::INTERNAL_SERVER_ERROR)),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();

        let error = client.open().await.unwrap_err();

        assert!(matches!(error, McpError::Protocol(_)), "{error}");
        task.abort();
    }

    #[tokio::test]
    async fn a_json_rpc_error_is_a_protocol_error_without_the_credential() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                rpc_error(&message["id"], "the token secret-token is not accepted")
            }),
        )
        .await;
        let mut client = McpClient::new(&url, Some("secret-token")).unwrap();

        let error = client.open().await.unwrap_err();

        assert!(matches!(error, McpError::Protocol(_)), "{error}");
        assert!(
            !error.to_string().contains("secret-token"),
            "an error never carries the credential: {error}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn an_unreachable_server_is_unreachable() {
        // A port this test opened and closed has no listener for the client.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut client = McpClient::new(&format!("http://{address}"), None).unwrap();

        let error = client.open().await.unwrap_err();

        assert!(matches!(error, McpError::Unreachable(_)), "{error}");
    }

    #[tokio::test]
    async fn new_rejects_a_url_that_is_not_https_or_loopback_http() {
        assert!(matches!(
            McpClient::new("", None),
            Err(McpError::Protocol(_))
        ));
        assert!(matches!(
            McpClient::new("not a url", None),
            Err(McpError::Protocol(_))
        ));
        assert!(matches!(
            McpClient::new("http://example.com/mcp", None),
            Err(McpError::Protocol(_))
        ));
        assert!(matches!(
            McpClient::new("ftp://127.0.0.1/mcp", None),
            Err(McpError::Protocol(_))
        ));
        assert!(McpClient::new("https://example.com/mcp", None).is_ok());
        assert!(McpClient::new("http://127.0.0.1:8080/mcp", None).is_ok());
        assert!(McpClient::new("http://localhost:8080/mcp", None).is_ok());
        assert!(McpClient::new("http://[::1]:8080/mcp", None).is_ok());
    }

    #[tokio::test]
    async fn an_answer_larger_than_the_limit_is_a_protocol_error() {
        let padding = "a".repeat(MAX_ANSWER_BYTES);
        let (url, task) = server(
            log(),
            Arc::new(move |message: &Value, _: &HeaderMap| {
                rpc_result(&message["id"], json!({"padding": padding}))
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();

        let error = client.open().await.unwrap_err();

        assert!(
            matches!(error, McpError::Protocol(ref message) if message == "the server's answer is too large"),
            "{error}"
        );
        task.abort();
    }
    #[tokio::test]
    async fn a_handshake_version_this_client_does_not_speak_is_refused() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => json_response(
                        json!({
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": {},
                                "serverInfo": {"name": "fake", "version": "1.0.0"},
                            },
                        })
                        .to_string(),
                    ),
                    _ => empty(StatusCode::ACCEPTED),
                }
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();

        let error = client.open().await.unwrap_err();
        assert!(matches!(error, McpError::Protocol(_)), "{error}");
        task.abort();
    }

    #[tokio::test]
    async fn an_event_stream_may_end_events_with_carriage_returns() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                assert_eq!(message["method"], "tools/list");
                let answer = json!({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"tools": [{ "name": "search", "inputSchema": {} }]},
                });
                event_stream(format!("data: {answer}\r\r"))
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        task.abort();
    }

    #[test]
    fn a_server_request_does_not_answer_the_client() {
        let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        assert!(super::matching_message(request, 1).is_none());
        let answer = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert!(super::matching_message(answer, 1).is_some());
    }
}
