//! The Model Context Protocol client.
//!
//! The Space calls a Plugin's MCP server over the Streamable HTTP transport:
//! one JSON-RPC 2.0 message per POST, answered with a JSON document or an
//! event stream. This module owns that wire protocol and nothing else; it
//! starts no program and holds no server state. A server the Space cannot
//! reach, or whose answer does not fit the protocol, becomes an [`McpError`]
//! that the Space reports, and the credential never enters an error message.

use std::collections::HashSet;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use futures_util::StreamExt as _;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tracing::Instrument as _;

/// The MCP protocol revision this client speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP extension that lets a tool answer with an interactive Card.
pub const UI_EXTENSION: &str = "io.modelcontextprotocol/ui";

/// The media type an MCP App's HTML carries.
pub const APP_MIME_TYPE: &str = "text/html;profile=mcp-app";

/// The policy a client applies when an app's resource declares none.
pub const DEFAULT_APP_CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' data:; connect-src 'none'; object-src 'none'";

/// The longest `ui://` resource URI this client accepts from one declaration.
const MAX_UI_RESOURCE_URI: usize = 2048;

/// The header a server names the session it made for the client.
const SESSION_ID_HEADER: &str = "Mcp-Session-Id";

/// The header a client names the protocol revision it speaks.
const PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";

/// How a configured header name is treated.
///
/// A package's headers are visible data, not a credential. Two names the Space
/// decides for itself are told apart so a caller can report one policy apart
/// from the other: a header that could carry a credential keeps a server out of
/// the record, and a header the client owns is dropped from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderClass {
    /// A header whose name conventionally carries a secret. The Space never
    /// sends one a package configures, and never stores it.
    Credential,
    /// A header the client owns for the protocol, the request's framing, or a
    /// proxy. A configured value never replaces or duplicates it.
    Generated,
}

/// Classifies a configured header name under the policy the Space applies.
///
/// The comparison ignores case, as header names do. [`HeaderClass::Credential`]
/// wins when a name matches both rules, so `Proxy-Authorization` is refused
/// rather than sent. `None` means a header the Space neither supplies nor
/// treats as a secret: a package's own data, sent as configured.
#[must_use]
pub fn header_class(name: &str) -> Option<HeaderClass> {
    let name = name.trim().to_ascii_lowercase();
    // A name that merely contains one of these words is treated as a
    // credential: a header convention is not something a package's data can
    // prove, and refusing one visible header costs only a server's entry.
    if matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || ["auth", "token", "key", "secret", "password", "credential"]
        .iter()
        .any(|part| name.contains(part))
    {
        return Some(HeaderClass::Credential);
    }
    if name.starts_with("proxy-")
        || matches!(
            name.as_str(),
            "accept"
                | "content-type"
                | "mcp-session-id"
                | "mcp-protocol-version"
                | "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "accept-encoding"
                | "user-agent"
        )
    {
        return Some(HeaderClass::Generated);
    }
    None
}

/// How long one request may wait for its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest answer this client reads for one request.
pub(crate) const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// The most pages one tool listing may follow.
///
/// A server that keeps naming fresh cursors would otherwise spend the run's
/// whole budget before the Model sees a single tool.
pub(crate) const MAX_TOOL_PAGES: usize = 16;

/// The most tools this client accepts from one server.
pub(crate) const MAX_TOOLS: usize = 256;

/// A tool one server offers.
#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    /// The tool's original name.
    pub name: String,
    /// What the tool does; empty when the server declares none.
    pub description: String,
    /// The tool's JSON input schema; empty when the server declares none.
    pub input_schema: Value,
    /// The `ui://` resource that renders this tool's answer, when it names one.
    pub ui_resource: Option<String>,
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
    /// An audio clip the Model reads as JSON text.
    ///
    /// The wire carries the clip's bytes as standard base64, so a credential
    /// they hide is invisible to the literal redaction: the decoded bytes are
    /// searched instead.
    Audio {
        /// The clip's bytes as standard base64, as the server sent them.
        data: String,
        /// The clip's media type, when the server declares one.
        mime_type: Option<String>,
        /// The whole content block, as the server sent it.
        ///
        /// The Model reads the server's own JSON block, so fields this client
        /// does not parse (annotations, `_meta`, a future addition) survive.
        raw: Value,
    },
    /// An embedded resource, as the server sent it.
    Resource {
        /// The resource's URI.
        uri: String,
        /// The resource's media type, when the server declares one.
        mime_type: Option<String>,
        /// The resource's text, when it carries text.
        text: Option<String>,
        /// The resource's bytes as standard base64, when it carries a blob.
        blob: Option<String>,
        /// The whole content block, as the server sent it.
        ///
        /// The Model reads the server's own JSON block, so fields this client
        /// does not parse (annotations, `_meta`, a future addition) survive.
        raw: Value,
    },
    /// A block this client does not model, as the server sent it.
    Other(Value),
}

impl McpContent {
    /// Whether this block hides the credential in bytes the record would keep.
    ///
    /// A text block is redacted as text where it enters the answer, so a
    /// literal occurrence is replaced. An image's payload, an audio block's
    /// payload, and a resource's `blob` arrive as base64 and are opaque once
    /// decoded: a literal redaction cannot see the credential in them, so the
    /// decoded bytes are searched instead. A resource's `blob` is decoded again
    /// here, under the answer bound [`content_from_json`] already enforces.
    pub(crate) fn carries_credential(&self, credential: &str) -> bool {
        match self {
            Self::Image { media_type, data } => {
                media_type.contains(credential) || bytes_carry_credential(data, credential)
            }
            Self::Resource {
                blob: Some(blob), ..
            } => decode_base64(blob).is_some_and(|data| bytes_carry_credential(&data, credential)),
            Self::Audio {
                data, mime_type, ..
            } => {
                mime_type
                    .as_deref()
                    .is_some_and(|mime_type| mime_type.contains(credential))
                    || base64_carries_credential(data, credential)
            }
            Self::Text(_) | Self::Resource { blob: None, .. } | Self::Other(_) => false,
        }
    }
}

/// What one tool call answered.
#[derive(Clone, Debug, PartialEq)]
pub struct McpAnswer {
    /// The answer's content blocks, in the order the server sent them.
    pub content: Vec<McpContent>,
    /// Whether the server reported the call as failed.
    pub is_error: bool,
    /// The answer's `result` object, as the server sent it.
    pub raw: Value,
}

/// One resource an MCP server answered.
#[derive(Clone, Debug, PartialEq)]
pub struct McpResource {
    /// The resource's URI, as the server answered it.
    pub uri: String,
    /// The resource's media type, when the server declares one.
    pub mime_type: Option<String>,
    /// The resource's bytes: its text, or its decoded base64 blob.
    pub data: Vec<u8>,
    /// The domains the resource's `_meta.ui.csp` declares, when it does.
    csp: Option<UiCsp>,
}

impl McpResource {
    /// The Content Security Policy a client applies to the app this resource
    /// holds.
    ///
    /// A resource that declares no policy gets the restrictive default. A
    /// policy names only the origins this Space accepts: the declared domains
    /// widen the directives the MCP Apps extension assigns them, and every
    /// directive it does not assign stays locked down.
    ///
    /// A declared source that names the credential, or whose origin would, is
    /// dropped: the policy is stored in the record, and a credential never is.
    /// The policy is built under the bound a Card accepts, so a declaration
    /// too large for a Card falls back to the restrictive default instead of
    /// becoming unrecordable. The collection stops as soon as the declaration
    /// is already past that bound, so a server cannot make the Space serialize
    /// a policy it will refuse.
    #[must_use]
    pub fn content_security_policy(&self, credential: Option<&str>) -> String {
        let Some(csp) = &self.csp else {
            return DEFAULT_APP_CSP.to_owned();
        };
        // A resource domain is named by five directives; every other domain
        // is named by one.
        let mut budget = CspBudget::default();
        let Some(resources) = csp_origins(&csp.resource_domains, credential, 5, &mut budget) else {
            return DEFAULT_APP_CSP.to_owned();
        };
        let Some(connect) = csp_origins(&csp.connect_domains, credential, 1, &mut budget) else {
            return DEFAULT_APP_CSP.to_owned();
        };
        let Some(frames) = csp_origins(&csp.frame_domains, credential, 1, &mut budget) else {
            return DEFAULT_APP_CSP.to_owned();
        };
        let Some(base) = csp_origins(&csp.base_uri_domains, credential, 1, &mut budget) else {
            return DEFAULT_APP_CSP.to_owned();
        };
        let directives = [
            "default-src 'none'".to_owned(),
            with_sources("script-src 'self' 'unsafe-inline'", &resources),
            with_sources("style-src 'self' 'unsafe-inline'", &resources),
            with_sources("img-src 'self' data:", &resources),
            with_sources("media-src 'self' data:", &resources),
            with_sources("font-src 'self'", &resources),
            if connect.is_empty() {
                "connect-src 'none'".to_owned()
            } else {
                with_sources("connect-src", &connect)
            },
            if frames.is_empty() {
                "frame-src 'none'".to_owned()
            } else {
                with_sources("frame-src", &frames)
            },
            if base.is_empty() {
                "base-uri 'self'".to_owned()
            } else {
                with_sources("base-uri", &base)
            },
            "object-src 'none'".to_owned(),
        ];
        let mut policy = String::new();
        for directive in directives {
            // The bound is enforced as the policy is built: a declared policy
            // that would outgrow a Card never becomes one.
            if !append_directive(&mut policy, &directive) {
                return DEFAULT_APP_CSP.to_owned();
            }
        }

        policy
    }
}

/// The Content Security Policy domains one resource declares.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct UiCsp {
    /// Origins the app may connect to.
    connect_domains: Vec<String>,
    /// Origins the app may load scripts, styles, images, fonts, and media from.
    resource_domains: Vec<String>,
    /// Origins the app may frame.
    frame_domains: Vec<String>,
    /// Origins the app's documents may use as their base.
    base_uri_domains: Vec<String>,
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
    /// Fixed headers a package configures for this server.
    headers: reqwest::header::HeaderMap,
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
            headers: reqwest::header::HeaderMap::new(),
        })
    }

    /// Adds the fixed headers a package configures.
    ///
    /// The headers are visible package data. Only a header the Space neither
    /// owns nor treats as a credential is kept: a credential still comes only
    /// from the Vault, and a name the client generates is written last, so a
    /// configured value can neither duplicate nor override it.
    #[must_use]
    pub fn with_headers(mut self, headers: &std::collections::BTreeMap<String, String>) -> Self {
        for (name, value) in headers {
            if header_class(name).is_some() {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                self.headers.insert(name, value);
            }
        }
        self
    }

    /// The headers of one request.
    ///
    /// The configured headers are visible package data; every header this
    /// client sets for the framing, the protocol, the session, or the
    /// credential is inserted last: an insert replaces a configured header of
    /// the same name instead of appending a second value, so a configured
    /// value cannot duplicate or override one of them.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if self.negotiated {
            headers.insert(
                PROTOCOL_VERSION_HEADER,
                HeaderValue::from_static(PROTOCOL_VERSION),
            );
        }
        if let Some(session) = &self.session_id
            && let Ok(value) = HeaderValue::from_str(session)
        {
            headers.insert(SESSION_ID_HEADER, value);
        }
        if let Some(credential) = &self.credential
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {credential}"))
        {
            headers.insert(AUTHORIZATION, value);
        }
        crate::diagnostics::inject_headers(&mut headers);
        headers
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
        // The extension tells a server this host renders an app's HTML; a
        // server that offers none still answers the handshake.
        let mut extensions = serde_json::Map::new();
        extensions.insert(
            UI_EXTENSION.to_owned(),
            json!({"mimeTypes": [APP_MIME_TYPE]}),
        );
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"extensions": extensions},
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
        self.tools_inner().await
    }

    async fn tools_inner(&mut self) -> Result<Vec<McpTool>, McpError> {
        self.require_open()?;
        let mut tools = Vec::new();
        let mut declared = 0_usize;
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
                if declared >= MAX_TOOLS {
                    return Err(protocol_error("the MCP server lists too many tools"));
                }
                declared = declared.saturating_add(1);
                if let Some(tool) = tool_from_json(tool)? {
                    tools.push(tool);
                }
            }
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            let Some(next) = next else {
                return Ok(tools);
            };
            if pages >= MAX_TOOL_PAGES {
                return Err(protocol_error("the MCP server lists too many pages"));
            }
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
        answer_from_json(&answer.result)
    }

    /// Reads one resource by its URI.
    ///
    /// The answer's `contents` may hold more than one resource; only the
    /// entries that name the requested URI count, and exactly one must.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Protocol`] when the client is not open, the server's
    /// answer does not fit the protocol, or the resource it answers is larger
    /// than this client reads.
    pub async fn read_resource(&mut self, uri: &str) -> Result<McpResource, McpError> {
        self.require_open()?;
        let params = json!({"uri": uri});
        let answer = self.rpc("resources/read", Some(params)).await?;
        resource_from_answer(uri, &answer.result)
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
        let span = tracing::info_span!(
            target: "neha::mcp",
            "mcp.call",
            method,
            transport = "http",
            otel.kind = "client",
        );
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(mut params) = params {
            span.in_scope(|| crate::diagnostics::inject_json_meta(&mut params));
            message["params"] = params;
        }

        let result = match tokio::time::timeout(self.timeout, self.exchange(&message, id))
            .instrument(span.clone())
            .await
        {
            Ok(answer) => answer,
            Err(_) => Err(unreachable_error("the server did not answer in time")),
        };
        crate::metrics::install().mcp_call(if result.is_ok() { "ok" } else { "failed" });
        span.in_scope(|| match &result {
            Ok(_) => tracing::debug!(target: "neha::mcp", event = "mcp.call", outcome = "ok", "an MCP call answered"),
            Err(error) => tracing::warn!(target: "neha::mcp", event = "mcp.call", outcome = "failed", error_kind = mcp_error_kind(error), "an MCP call failed"),
        });
        result
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
        self.http
            .post(self.url.clone())
            .headers(self.request_headers())
            .json(message)
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

/// Names an MCP failure without its display text.
fn mcp_error_kind(error: &McpError) -> &'static str {
    match error {
        McpError::Unreachable(_) => "unreachable",
        McpError::NotConnected { .. } => "not_connected",
        McpError::Protocol(_) => "protocol",
    }
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
    // An event's field lines end with CR, LF, or CRLF.
    for line in text.split(['\r', '\n']) {
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
///
/// A tool that names its visibility without the model is not offered to the
/// model at all; a tool that names a `ui://` resource carries it so a call can
/// show its answer as a Card. A stdio client reads a declaration with this
/// same rule.
pub(crate) fn tool_from_json(value: &Value) -> Result<Option<McpTool>, McpError> {
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
    let meta = object.get("_meta").and_then(Value::as_object);
    let ui = meta
        .and_then(|meta| meta.get("ui"))
        .and_then(Value::as_object);
    if let Some(visibility) = ui
        .and_then(|ui| ui.get("visibility"))
        .and_then(Value::as_array)
        && !visibility
            .iter()
            .any(|entry| entry.as_str() == Some("model"))
    {
        return Ok(None);
    }
    // The nested form is the current one; the flat key is the deprecated form
    // an older server still writes.
    let resource = ui
        .and_then(|ui| ui.get("resourceUri"))
        .and_then(Value::as_str)
        .or_else(|| {
            meta.and_then(|meta| meta.get("ui/resourceUri"))
                .and_then(Value::as_str)
        });

    Ok(Some(McpTool {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        ui_resource: resource.and_then(ui_resource),
    }))
}

/// The `ui://` resource a tool declaration names.
///
/// A resource of another scheme is no UI resource this client reads, and a
/// server that names one keeps a tool the model sees without a Card.
fn ui_resource(uri: &str) -> Option<String> {
    let uri = uri.trim();
    (!uri.is_empty() && uri.len() <= MAX_UI_RESOURCE_URI && uri.starts_with("ui://"))
        .then(|| uri.to_owned())
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
        Some("audio") => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            // The wire carries standard base64. A payload this client cannot
            // decode is refused here, like an image's, so the bytes the
            // credential check searches are exactly the bytes a record would
            // expose.
            decode_base64(data)
                .ok_or_else(|| protocol_error("the MCP server's audio is not base64"))?;
            let mime_type = match block.get("mimeType") {
                None => None,
                Some(Value::String(mime_type)) => Some(mime_type.clone()),
                Some(_) => return Err(malformed()),
            };
            Ok(McpContent::Audio {
                data: data.to_owned(),
                mime_type,
                raw: value.clone(),
            })
        }
        Some("resource") => {
            let resource = block
                .get("resource")
                .and_then(Value::as_object)
                .ok_or_else(malformed)?;
            let uri = resource
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let mime_type = match resource.get("mimeType") {
                None => None,
                Some(Value::String(mime_type)) => Some(mime_type.clone()),
                Some(_) => return Err(malformed()),
            };
            let text = match resource.get("text") {
                None => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => return Err(malformed()),
            };
            let blob = match resource.get("blob") {
                None => None,
                Some(Value::String(blob)) => Some(blob.clone()),
                Some(_) => return Err(malformed()),
            };
            // A blob's size is bounded after decoding, so its base64 padding
            // cannot buy a server more than the answer limit.
            if let Some(blob) = &blob {
                let decoded = decode_base64(blob)
                    .ok_or_else(|| protocol_error("the MCP server's resource is not base64"))?;
                if decoded.len() > MAX_ANSWER_BYTES {
                    return Err(protocol_error("the server's answer is too large"));
                }
            }
            if text.is_some() == blob.is_some() {
                return Err(malformed());
            }
            Ok(McpContent::Resource {
                uri: uri.to_owned(),
                mime_type,
                text,
                blob,
                raw: value.clone(),
            })
        }
        _ => Ok(McpContent::Other(value.clone())),
    }
}

/// One tool result, as the server sent it.
///
/// The shape of a result belongs to the protocol, not to one transport, so a
/// stdio client parses an answer with this same rule.
pub(crate) fn answer_from_json(result: &Value) -> Result<McpAnswer, McpError> {
    let object = result.as_object().ok_or_else(malformed)?;
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?
        .iter()
        .map(content_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let is_error = match object.get("isError") {
        None => false,
        Some(Value::Bool(is_error)) => *is_error,
        Some(_) => return Err(malformed()),
    };

    Ok(McpAnswer {
        content,
        is_error,
        raw: result.clone(),
    })
}

/// One resource answer, as the server sent it.
///
/// The answer's `contents` may hold more than one resource; only the entries
/// that name the requested URI count, and exactly one must. A stdio client
/// reads a resource with this same rule.
pub(crate) fn resource_from_answer(uri: &str, result: &Value) -> Result<McpResource, McpError> {
    let object = result.as_object().ok_or_else(malformed)?;
    let contents = object
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let mut found = None;
    for content in contents {
        let content = content.as_object().ok_or_else(malformed)?;
        if content.get("uri").and_then(Value::as_str) != Some(uri) {
            continue;
        }
        if found.is_some() {
            return Err(protocol_error(
                "the MCP server answered one resource more than once",
            ));
        }
        found = Some(content);
    }
    let content = found
        .ok_or_else(|| protocol_error("the MCP server answered no resource it was asked for"))?;
    resource_from_json(uri, content)
}

/// One content entry of a `resources/read` answer.
fn resource_from_json(
    uri: &str,
    content: &serde_json::Map<String, Value>,
) -> Result<McpResource, McpError> {
    let mime_type = match content.get("mimeType") {
        None => None,
        Some(Value::String(mime_type)) => Some(mime_type.clone()),
        Some(_) => return Err(malformed()),
    };
    let text = match content.get("text") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(malformed()),
    };
    let blob = match content.get("blob") {
        None => None,
        Some(Value::String(blob)) => Some(blob.clone()),
        Some(_) => return Err(malformed()),
    };
    let data = match (text, blob) {
        (Some(text), None) => text.into_bytes(),
        (None, Some(blob)) => decode_base64(&blob)
            .ok_or_else(|| protocol_error("the MCP server's resource is not base64"))?,
        _ => return Err(malformed()),
    };
    if data.len() > MAX_ANSWER_BYTES {
        return Err(protocol_error("the server's answer is too large"));
    }

    Ok(McpResource {
        uri: uri.to_owned(),
        mime_type,
        data,
        csp: ui_csp(content.get("_meta")),
    })
}

/// Decodes standard-alphabet base64, with or without the final padding.
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    STANDARD_NO_PAD.decode(data.trim_end_matches('=')).ok()
}

/// Whether one standard-base64 payload decodes to bytes that spell the
/// credential.
///
/// The MCP wire carries standard base64, so a payload is read exactly as
/// [`decode_base64`] reads one. A string that is not base64 is no payload this
/// client can recover, so it hides nothing behind an encoding.
pub(crate) fn base64_carries_credential(payload: &str, credential: &str) -> bool {
    decode_base64(payload).is_some_and(|bytes| bytes_carry_credential(&bytes, credential))
}

/// Whether decoded bytes spell the credential anywhere in them.
///
/// The bytes need not be UTF-8, so the search is over bytes; an empty
/// credential is no credential and matches nothing.
pub(crate) fn bytes_carry_credential(bytes: &[u8], credential: &str) -> bool {
    !credential.is_empty()
        && bytes
            .windows(credential.len())
            .any(|window| window == credential.as_bytes())
}

/// The Content Security Policy domains a resource's `_meta` declares.
///
/// A metadata document of another shape is no declaration: the resource gets
/// the restrictive default rather than a guessed policy.
fn ui_csp(meta: Option<&Value>) -> Option<UiCsp> {
    let csp = meta?
        .as_object()?
        .get("ui")?
        .as_object()?
        .get("csp")?
        .as_object()?;
    Some(UiCsp {
        connect_domains: declared_domains(csp.get("connectDomains")),
        resource_domains: declared_domains(csp.get("resourceDomains")),
        frame_domains: declared_domains(csp.get("frameDomains")),
        base_uri_domains: declared_domains(csp.get("baseUriDomains")),
    })
}

/// The domains one policy field declares.
fn declared_domains(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The most unique origins one policy collects before it falls back.
///
/// The fixed directives already spend a few hundred bytes of the policy
/// budget, and a resource origin is named by five of them, so a declaration
/// keeping more origins than this is not worth building: it falls back from
/// the collection instead of spending work on source lists that are then
/// thrown away.
const MAX_CSP_ORIGINS: usize = 64;

/// The bounds a policy's declared sources are collected under.
///
/// Every accepted origin is counted and its serialized text estimated as an
/// upper bound: one space follows it in each directive that names it. A
/// declaration past either bound falls back, so it is refused while it is still
/// being read and no directive is built.
#[derive(Default)]
struct CspBudget {
    /// The unique origins kept across every directive.
    origins: usize,
    /// An upper bound on the bytes those origins serialize into.
    bytes: usize,
}

impl CspBudget {
    /// Adds one kept origin that `copies` directives will name.
    ///
    /// Returns false once the collected sources already doom the policy: too
    /// many origins to build a small policy, or source text past the Card's
    /// whole policy budget. The byte estimate over-counts rather than
    /// under-counts, so the authoritative bound is still checked when the
    /// directives are appended.
    fn add(&mut self, origin: &str, copies: usize) -> bool {
        self.origins += 1;
        self.bytes = self
            .bytes
            .saturating_add(origin.len().saturating_add(1).saturating_mul(copies));
        self.origins <= MAX_CSP_ORIGINS && self.bytes <= neha::record::MAX_CARD_CSP_BYTES
    }
}

/// The declared sources this Space accepts, as origins, in order, once each.
///
/// A source is checked against the credential because the policy is stored in
/// the record; a duplicate is skipped in constant time. Each kept origin is
/// counted into `budget` before it is kept, and `None` means the declaration
/// outgrew the budget: the caller falls back without building a directive.
fn csp_origins(
    declared: &[String],
    credential: Option<&str>,
    copies: usize,
    budget: &mut CspBudget,
) -> Option<Vec<String>> {
    let mut seen = HashSet::new();
    let mut origins = Vec::new();
    for source in declared {
        if let Some(origin) = csp_origin(source, credential)
            && seen.insert(origin.clone())
        {
            if !budget.add(&origin, copies) {
                return None;
            }
            origins.push(origin);
        }
    }
    Some(origins)
}

/// Appends one directive to a policy, if the policy still fits a Card.
fn append_directive(policy: &mut String, directive: &str) -> bool {
    let separator = if policy.is_empty() { 0 } else { 2 };
    if policy
        .len()
        .saturating_add(separator)
        .saturating_add(directive.len())
        > neha::record::MAX_CARD_CSP_BYTES
    {
        return false;
    }
    if !policy.is_empty() {
        policy.push_str("; ");
    }
    policy.push_str(directive);

    true
}

/// Appends origins to a directive, when there is one to append.
fn with_sources(directive: &str, sources: &[String]) -> String {
    if sources.is_empty() {
        directive.to_owned()
    } else {
        format!("{directive} {}", sources.join(" "))
    }
}

/// Returns one declared source as an origin this Space accepts.
///
/// A source must be exactly an origin: `https://` anywhere, or `http://` on
/// the Space's own machine (`127.0.0.1` or `localhost`), a host of lowercase
/// ASCII letters, digits, dots, and hyphens, and an optional numeric port. A
/// path, a query, a fragment, a credential, userinfo, whitespace, a separator,
/// a wildcard, or another scheme is no origin: dropping it can only narrow the
/// policy, never widen it. An origin whose text names the credential is
/// dropped too, because the policy is stored in the record.
fn csp_origin(source: &str, credential: Option<&str>) -> Option<String> {
    let source = source.trim();
    if source.is_empty() || source.len() > 255 || names_credential(source, credential) {
        return None;
    }
    let (scheme, authority) = source.split_once("://")?;
    if !matches!(scheme, "https" | "http") {
        return None;
    }
    // Only what an origin's authority may carry: letters and digits, a dot, a
    // hyphen, and the colon before a port. Anything else (a path, a query, a
    // fragment, userinfo, a separator, a quote, a wildcard, whitespace) is no
    // source this policy stores.
    if !authority.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b':')
    }) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            port.parse::<u16>().ok()?;
            (host, Some(port))
        }
        None => (authority, None),
    };
    if host.is_empty()
        || host.len() > 253
        || !host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
    {
        return None;
    }
    if scheme == "http" && !is_loopback_host(host) {
        return None;
    }
    let mut origin = format!("{scheme}://{host}");
    if let Some(port) = port {
        origin.push(':');
        origin.push_str(port);
    }
    // The host is lowercase already, but a port can also spell the credential.
    if names_credential(&origin, credential) {
        return None;
    }

    Some(origin)
}

/// Whether a serialized origin contains the credential.
fn names_credential(text: &str, credential: Option<&str>) -> bool {
    credential.is_some_and(|credential| !credential.is_empty() && text.contains(credential))
}

/// Whether a host is the Space's own machine, by the name a policy uses.
///
/// Only the two names the proxy's inherited policy can express are accepted:
/// `127.0.0.1` and `localhost`. Every address in 127.0.0.0/8 is loopback too,
/// but a policy cannot name one, so accepting it would store a declaration the
/// proxy could never make effective.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1")
}

/// The most frame sources the sandbox proxy names in its response header.
///
/// The Card's own policy is narrower; this is the proxy's own bound, so a
/// policy this route is handed cannot turn one page's header into an unbounded
/// source list.
pub(crate) const MAX_FRAME_SOURCES: usize = 32;

/// The frame sources one Card policy admits to the sandbox proxy.
///
/// The proxy's response header carries a `frame-src` directive built from the
/// Card's policy, which reaches the route as a query parameter, so it is
/// untrusted: the page embedding the proxy chooses it. Only the `frame-src`
/// directive is read; every other directive is ignored. Every source must be
/// exactly an origin this Space accepts (see [`csp_origin`]), and the policy's
/// directive boundaries are the semicolons the Space's own policy format
/// separates with, so a source carrying one is malformed rather than a
/// directive boundary to trust. A policy that names no frame source, names a
/// source that is no origin, or outgrows the bounds falls back to no source
/// rather than to a directive built from text this Space cannot vouch for; a
/// fallback only ever narrows where the app's frame may navigate.
pub(crate) fn sandbox_frame_sources(policy: Option<&str>) -> Vec<String> {
    let Some(policy) = policy else {
        return Vec::new();
    };
    if policy.len() > neha::record::MAX_CARD_CSP_BYTES {
        return Vec::new();
    }
    let tokens: Vec<&str> = policy.split_whitespace().collect();
    let Some(start) = tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case("frame-src"))
    else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for token in &tokens[start + 1..] {
        let (source, ends_directive) = match token.strip_suffix(';') {
            Some(source) => (source, true),
            None => (*token, false),
        };
        // A semicolon inside a source is neither a source nor a boundary this
        // parser may trust: the policy format separates directives with `"; "`,
        // so such a token means the policy is malformed.
        if source.contains(';') {
            return Vec::new();
        }
        if !source.is_empty() {
            if sources.len() == MAX_FRAME_SOURCES {
                return Vec::new();
            }
            let Some(origin) = csp_origin(source, None) else {
                return Vec::new();
            };
            sources.push(origin);
        }
        if ends_directive {
            break;
        }
    }

    sources
}

/// The failure every shape of malformed answer reports.
///
/// Both transports parse an answer with the same rules, so both report the
/// same failure.
pub(crate) fn malformed() -> McpError {
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

    use super::{
        APP_MIME_TYPE, DEFAULT_APP_CSP, HeaderClass, MAX_ANSWER_BYTES, MAX_CSP_ORIGINS,
        MAX_FRAME_SOURCES, McpClient, McpContent, McpError, McpResource, PROTOCOL_VERSION, UiCsp,
        header_class,
    };

    #[test]
    fn a_header_name_is_classified_by_what_it_carries() {
        for name in [
            "Authorization",
            "proxy-authorization",
            "Cookie",
            "Set-Cookie",
            "X-Api-Key",
            "X-Auth-Token",
            "X-Secret",
            "X-Password",
            "X-Credential",
            "Tenant-Token",
        ] {
            assert_eq!(header_class(name), Some(HeaderClass::Credential), "{name}");
        }
        for name in [
            "Accept",
            "Content-Type",
            "Mcp-Session-Id",
            "MCP-Protocol-Version",
            "Host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Proxy-Connection",
            "Accept-Encoding",
            "User-Agent",
        ] {
            assert_eq!(header_class(name), Some(HeaderClass::Generated), "{name}");
        }
        for name in ["X-Tenant", "X-Trace-Id", ""] {
            assert_eq!(header_class(name), None, "{name}");
        }
        assert_eq!(
            header_class("proxy-authorization"),
            Some(HeaderClass::Credential),
            "a proxy credential is a credential before it is a proxy header"
        );
    }

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

    /// How many values one header carries.
    fn values(headers: &HeaderMap, name: &str) -> usize {
        headers.get_all(name).iter().count()
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
        assert_eq!(
            initialize.message["params"]["capabilities"],
            json!({
                "extensions": {
                    "io.modelcontextprotocol/ui": {
                        "mimeTypes": ["text/html;profile=mcp-app"],
                    },
                },
            }),
            "the handshake names the UI extension this host renders"
        );
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
    async fn a_configured_header_never_replaces_or_duplicates_the_clients_own() {
        let log = log();
        let (url, task) = server(
            log.clone(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                handshake(message).expect("only the handshake messages arrive")
            }),
        )
        .await;
        let configured: std::collections::BTreeMap<String, String> = [
            ("Authorization", "Bearer planted"),
            ("Proxy-Authorization", "Basic planted"),
            ("Cookie", "session=planted"),
            ("X-Api-Key", "planted"),
            ("Content-Type", "text/plain"),
            ("Accept", "*/*"),
            ("Mcp-Session-Id", "planted"),
            ("MCP-Protocol-Version", "1999-01-01"),
            ("X-Tenant", "acme"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        let mut client = McpClient::new(&url, Some("secret-token"))
            .unwrap()
            .with_headers(&configured);

        client.open().await.unwrap();

        let requests = received(&log);
        let initialize = &requests[0];
        assert_eq!(
            header(&initialize.headers, "authorization"),
            Some("Bearer secret-token"),
            "the credential is the client's"
        );
        assert_eq!(
            header(&initialize.headers, "proxy-authorization"),
            None,
            "a proxy credential is never a package header"
        );
        assert_eq!(
            header(&initialize.headers, "cookie"),
            None,
            "a cookie is a credential, so it is never a package header"
        );
        assert_eq!(
            header(&initialize.headers, "x-api-key"),
            None,
            "a header whose name says key is a credential"
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
            header(&initialize.headers, "mcp-session-id"),
            None,
            "the client sets no session before it has one"
        );
        assert_eq!(
            header(&initialize.headers, "mcp-protocol-version"),
            None,
            "the protocol is named only once negotiated"
        );
        assert_eq!(
            header(&initialize.headers, "x-tenant"),
            Some("acme"),
            "a header the client does not own still reaches the server"
        );
        for name in ["authorization", "content-type", "accept", "x-tenant"] {
            assert_eq!(values(&initialize.headers, name), 1, "{name}");
        }
        assert_eq!(
            values(&initialize.headers, "proxy-authorization"),
            0,
            "a proxy credential never reaches the server"
        );

        let notification = &requests[1];
        assert_eq!(
            header(&notification.headers, "mcp-session-id"),
            Some(SESSION),
            "the session the server named replaces the configured one"
        );
        assert_eq!(
            header(&notification.headers, "mcp-protocol-version"),
            Some(PROTOCOL_VERSION),
            "the negotiated protocol replaces the configured one"
        );
        assert_eq!(values(&notification.headers, "mcp-session-id"), 1);
        assert_eq!(values(&notification.headers, "mcp-protocol-version"), 1);
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
                                    "resource": {
                                        "uri": "file:///report.txt",
                                        "mimeType": "text/plain",
                                        "text": "report body",
                                    },
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
                McpContent::Resource {
                    uri: "file:///report.txt".to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    text: Some("report body".to_owned()),
                    blob: None,
                    raw: json!({
                        "type": "resource",
                        "resource": {
                            "uri": "file:///report.txt",
                            "mimeType": "text/plain",
                            "text": "report body",
                        },
                    }),
                },
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
                // A CR-only stream may carry other fields before the data.
                event_stream(format!("event: message\rdata: {answer}\r\r"))
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

    #[tokio::test]
    async fn the_sixteenth_page_may_be_the_last() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                assert_eq!(message["method"], "tools/list");
                let cursor = message["params"]["cursor"].as_str();
                let page: u32 = cursor
                    .and_then(|cursor| cursor.strip_prefix("page-"))
                    .and_then(|page| page.parse().ok())
                    .unwrap_or(1);
                let mut result = json!({
                    "tools": [{
                        "name": format!("tool-{page}"),
                        "inputSchema": {"type": "object"},
                    }],
                });
                if page < 16 {
                    result["nextCursor"] = json!(format!("page-{}", page + 1));
                }
                rpc_result(&message["id"], result)
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();
        assert_eq!(tools.len(), 16);
        assert_eq!(tools[15].name, "tool-16");
        task.abort();
    }

    #[tokio::test]
    async fn a_tool_declaration_names_the_app_that_renders_it() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                assert_eq!(message["method"], "tools/list");
                rpc_result(
                    &message["id"],
                    json!({
                        "tools": [
                            {
                                "name": "chart",
                                "description": "Plot a chart.",
                                "inputSchema": {"type": "object"},
                                "_meta": {"ui": {"resourceUri": "ui://charts/bar"}},
                            },
                            {
                                "name": "flat",
                                "inputSchema": {},
                                "_meta": {"ui/resourceUri": "ui://charts/flat"},
                            },
                            {
                                "name": "hidden",
                                "inputSchema": {},
                                "_meta": {"ui": {
                                    "visibility": ["app"],
                                    "resourceUri": "ui://charts/hidden",
                                }},
                            },
                            {
                                "name": "both",
                                "inputSchema": {},
                                "_meta": {"ui": {"visibility": ["model", "app"]}},
                            },
                            {
                                "name": "elsewhere",
                                "inputSchema": {},
                                "_meta": {"ui": {"resourceUri": "file:///etc/passwd"}},
                            },
                        ],
                    }),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let tools = client.tools().await.unwrap();

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["chart", "flat", "both", "elsewhere"],
            "a tool only an app sees is not offered to the model"
        );
        assert_eq!(tools[0].ui_resource.as_deref(), Some("ui://charts/bar"));
        assert_eq!(
            tools[1].ui_resource.as_deref(),
            Some("ui://charts/flat"),
            "the deprecated flat key still names the app"
        );
        assert_eq!(tools[2].ui_resource, None);
        assert_eq!(
            tools[3].ui_resource, None,
            "a resource of another scheme is no app this host renders"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_resource_is_read_by_its_uri() {
        let (url, log) = {
            let log = log();
            let (url, task) = server(
                log.clone(),
                Arc::new(|message: &Value, _: &HeaderMap| {
                    if let Some(response) = handshake(message) {
                        return response;
                    }
                    assert_eq!(message["method"], "resources/read");
                    rpc_result(
                        &message["id"],
                        json!({
                            "contents": [
                                {"uri": "ui://other/view", "text": "not this one"},
                                {
                                    "uri": "ui://charts/bar",
                                    "mimeType": APP_MIME_TYPE,
                                    "text": "<html></html>",
                                    "_meta": {"ui": {"csp": {
                                        "connectDomains": ["https://api.example.com"],
                                        "resourceDomains": ["https://cdn.example.com"],
                                        "frameDomains": ["https://frames.example.com"],
                                        "baseUriDomains": ["https://base.example.com"],
                                    }}},
                                },
                            ],
                        }),
                    )
                }),
            )
            .await;
            (url, (log, task))
        };
        let (log, task) = log;
        let mut client = McpClient::new(&url, Some("secret-token")).unwrap();
        client.open().await.unwrap();

        let resource = client.read_resource("ui://charts/bar").await.unwrap();

        assert_eq!(resource.uri, "ui://charts/bar");
        assert_eq!(resource.mime_type.as_deref(), Some(APP_MIME_TYPE));
        assert_eq!(resource.data, b"<html></html>");
        assert_eq!(
            resource.csp,
            Some(UiCsp {
                connect_domains: vec!["https://api.example.com".to_owned()],
                resource_domains: vec!["https://cdn.example.com".to_owned()],
                frame_domains: vec!["https://frames.example.com".to_owned()],
                base_uri_domains: vec!["https://base.example.com".to_owned()],
            })
        );
        let read = received(&log);
        let request = read.last().unwrap();
        assert_eq!(request.message["method"], "resources/read");
        assert_eq!(request.message["params"], json!({"uri": "ui://charts/bar"}));
        assert_eq!(
            header(&request.headers, "mcp-session-id"),
            Some(SESSION),
            "the read uses the session the handshake settled"
        );
        assert_eq!(
            header(&request.headers, "authorization"),
            Some("Bearer secret-token"),
            "the read uses the credential the call uses"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_blob_resource_is_decoded() {
        let encoded = STANDARD.encode(b"report bytes");
        let (url, task) = server(
            log(),
            Arc::new(move |message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({"contents": [{
                        "uri": "ui://charts/blob",
                        "mimeType": APP_MIME_TYPE,
                        "blob": encoded,
                    }]}),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let resource = client.read_resource("ui://charts/blob").await.unwrap();

        assert_eq!(resource.data, b"report bytes");
        assert_eq!(resource.mime_type.as_deref(), Some(APP_MIME_TYPE));
        task.abort();
    }

    #[tokio::test]
    async fn a_resource_read_reports_its_media_type() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({"contents": [{
                        "uri": "ui://charts/bar",
                        "mimeType": "text/html",
                        "text": "<html></html>",
                    }]}),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        // The client reports what the server answered; the Space decides
        // whether that type is an app it renders.
        let resource = client.read_resource("ui://charts/bar").await.unwrap();

        assert_eq!(resource.mime_type.as_deref(), Some("text/html"));
        task.abort();
    }

    #[tokio::test]
    async fn a_resource_read_answers_only_the_uri_it_was_asked_for() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({"contents": [{"uri": "ui://other/view", "text": "no"}]}),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let error = client.read_resource("ui://charts/bar").await.unwrap_err();
        assert!(
            matches!(error, McpError::Protocol(ref message) if message == "the MCP server answered no resource it was asked for"),
            "{error}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn an_unknown_resource_metadata_is_no_policy() {
        let (url, task) = server(
            log(),
            Arc::new(|message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({"contents": [{
                        "uri": "ui://charts/bar",
                        "mimeType": APP_MIME_TYPE,
                        "text": "<html></html>",
                        "_meta": "not a metadata document",
                    }]}),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let resource = client.read_resource("ui://charts/bar").await.unwrap();

        assert_eq!(resource.csp, None, "an unknown shape declares nothing");
        assert_eq!(resource.content_security_policy(None), DEFAULT_APP_CSP);
        task.abort();
    }

    #[tokio::test]
    async fn a_resource_larger_than_the_limit_is_refused() {
        let padding = "a".repeat(MAX_ANSWER_BYTES);
        let (url, task) = server(
            log(),
            Arc::new(move |message: &Value, _: &HeaderMap| {
                if let Some(response) = handshake(message) {
                    return response;
                }
                rpc_result(
                    &message["id"],
                    json!({"contents": [{"uri": "ui://charts/bar", "text": padding}]}),
                )
            }),
        )
        .await;
        let mut client = McpClient::new(&url, None).unwrap();
        client.open().await.unwrap();

        let error = client.read_resource("ui://charts/bar").await.unwrap_err();
        assert!(
            matches!(error, McpError::Protocol(ref message) if message == "the server's answer is too large"),
            "{error}"
        );
        task.abort();
    }

    fn app_resource(csp: Option<UiCsp>) -> McpResource {
        McpResource {
            uri: "ui://charts/bar".to_owned(),
            mime_type: Some(APP_MIME_TYPE.to_owned()),
            data: b"<html></html>".to_vec(),
            csp,
        }
    }

    #[test]
    fn an_apps_policy_names_only_declared_origins() {
        assert_eq!(
            app_resource(None).content_security_policy(None),
            DEFAULT_APP_CSP,
            "a resource that declares no policy gets the restrictive default"
        );

        let declared = UiCsp {
            connect_domains: vec![
                "https://api.example.com".to_owned(),
                "http://127.0.0.1:8080".to_owned(),
                "http://localhost:3000".to_owned(),
                "http://example.com".to_owned(),
                "ftp://files.example.com".to_owned(),
                "https://api.example.com/path".to_owned(),
                "https://user@api.example.com".to_owned(),
            ],
            resource_domains: vec![
                "https://cdn.example.com".to_owned(),
                "https://cdn.example.com".to_owned(),
                "https://cdn.example.com/path".to_owned(),
            ],
            frame_domains: vec!["https://frames.example.com".to_owned()],
            base_uri_domains: Vec::new(),
        };
        let policy = app_resource(Some(declared)).content_security_policy(None);

        assert!(policy.contains("default-src 'none'"), "{policy}");
        assert!(policy.contains("object-src 'none'"), "{policy}");
        assert!(
            policy.contains("script-src 'self' 'unsafe-inline' https://cdn.example.com;"),
            "{policy}"
        );
        assert!(
            policy.contains("style-src 'self' 'unsafe-inline' https://cdn.example.com;"),
            "{policy}"
        );
        assert!(
            policy.contains("img-src 'self' data: https://cdn.example.com;"),
            "{policy}"
        );
        assert!(
            policy.contains("media-src 'self' data: https://cdn.example.com;"),
            "{policy}"
        );
        assert!(
            policy.contains("font-src 'self' https://cdn.example.com;"),
            "{policy}"
        );
        assert!(
            policy.contains(
                "connect-src https://api.example.com http://127.0.0.1:8080 http://localhost:3000;"
            ),
            "a declared origin widens only its directive: {policy}"
        );
        assert!(
            policy.contains("frame-src https://frames.example.com;"),
            "{policy}"
        );
        assert!(policy.contains("base-uri 'self';"), "{policy}");
        for refused in [
            "http://example.com",
            "ftp://",
            "https://api.example.com/path",
            "https://user@",
            "https://cdn.example.com/path",
        ] {
            assert!(
                !policy.contains(refused),
                "{refused} is no origin this Space accepts: {policy}"
            );
        }
    }

    #[test]
    fn an_apps_policy_that_would_outgrow_a_card_is_restricted() {
        let many: Vec<String> = (0..40)
            .map(|index| format!("https://cdn{index}.example.com"))
            .collect();
        let declared = UiCsp {
            connect_domains: Vec::new(),
            resource_domains: many,
            frame_domains: Vec::new(),
            base_uri_domains: Vec::new(),
        };

        assert_eq!(
            app_resource(Some(declared)).content_security_policy(None),
            DEFAULT_APP_CSP,
            "a policy a Card cannot hold falls back instead of failing the record"
        );
    }

    #[test]
    fn an_apps_policy_takes_only_strict_origins() {
        for (source, expected) in [
            ("https://good.example", Some("https://good.example")),
            (
                "https://good.example:8443",
                Some("https://good.example:8443"),
            ),
            ("http://127.0.0.1:8123", Some("http://127.0.0.1:8123")),
            ("http://localhost", Some("http://localhost")),
        ] {
            assert_eq!(
                super::csp_origin(source, None),
                expected.map(str::to_owned),
                "{source}"
            );
        }
        for source in [
            "https://a;script-src *",
            "https://x.example/\"",
            "https://x.example/<script>",
            "*",
            "https://*.example",
            "https://good.example/path",
            "https://good.example?query=1",
            "https://good.example#fragment",
            "https://user@good.example",
            "ftp://good.example",
            "HTTP://good.example",
            "https://Good.example",
            "http://good.example",
            // Only the loopback names the proxy's own policy can express are
            // origins this Space accepts: another 127.0.0.0/8 address or the
            // IPv6 loopback would be a declaration no policy could honor.
            "http://127.0.0.2:8123",
            "http://127.0.0.2",
            "http://127.1.2.3:8080",
            "http://[::1]:8123",
            "https://",
            "https://good.example:abc",
            "https://good.example:70000",
            "https://good.example:80/x",
        ] {
            assert_eq!(super::csp_origin(source, None), None, "{source}");
        }
        // A source whose text names the credential is no source, and neither
        // is one whose serialized origin does: the policy is stored.
        assert_eq!(
            super::csp_origin("https://token-1.example", Some("token-1")),
            None
        );
        assert_eq!(
            super::csp_origin("https://good.example:8443", Some("8443")),
            None
        );
    }

    #[test]
    fn an_apps_policy_drops_the_credential_from_a_declared_origin() {
        let declared = UiCsp {
            connect_domains: vec![
                "https://token-1.example".to_owned(),
                "https://good.example".to_owned(),
            ],
            ..UiCsp::default()
        };

        let policy = app_resource(Some(declared)).content_security_policy(Some("token-1"));

        assert!(
            policy.contains("connect-src https://good.example;"),
            "{policy}"
        );
        assert!(!policy.contains("token-1"), "{policy}");
        assert!(policy.len() <= neha::record::MAX_CARD_CSP_BYTES);
    }

    #[test]
    fn an_apps_policy_dedupes_and_keeps_its_bound_while_building() {
        // A source declared a thousand times is kept once and does not inflate
        // the policy.
        let declared = UiCsp {
            resource_domains: vec!["https://cdn.example.com".to_owned(); 1_000],
            ..UiCsp::default()
        };
        let policy = app_resource(Some(declared)).content_security_policy(None);
        assert_eq!(
            policy.matches("https://cdn.example.com").count(),
            5,
            "one per resource directive: {policy}"
        );
        assert!(policy.len() <= neha::record::MAX_CARD_CSP_BYTES);

        // Many declared domains would outgrow a Card: the restrictive default.
        let many: Vec<String> = (0..1_000)
            .map(|index| format!("https://cdn{index}.example.com"))
            .collect();
        let declared = UiCsp {
            resource_domains: many,
            ..UiCsp::default()
        };
        assert_eq!(
            app_resource(Some(declared)).content_security_policy(None),
            DEFAULT_APP_CSP,
            "a policy built past the Card bound falls back instead of failing"
        );
    }

    #[test]
    fn an_apps_policy_stops_collecting_a_declaration_that_cannot_fit() {
        // Thousands of declared domains are refused from the collection bound:
        // no directive string is built for a policy that cannot fit a Card.
        // The test completes without relying on the size of the declaration.
        let many: Vec<String> = (0..10_000)
            .map(|index| format!("https://cdn{index}.example.com"))
            .collect();
        let declared = UiCsp {
            connect_domains: many.clone(),
            resource_domains: many.clone(),
            frame_domains: many.clone(),
            base_uri_domains: many,
        };

        assert_eq!(
            app_resource(Some(declared)).content_security_policy(None),
            DEFAULT_APP_CSP,
            "a declaration past the collection bound falls back"
        );
    }

    #[test]
    fn an_apps_policy_counts_origins_while_it_collects_them() {
        // More unique origins than one policy may collect fall back even when
        // their text alone would fit: the count bound is checked while the
        // declaration is read, not after every directive is built.
        let many: Vec<String> = (0..MAX_CSP_ORIGINS + 1)
            .map(|index| format!("https://host{index}.example"))
            .collect();
        let declared = UiCsp {
            connect_domains: many,
            ..UiCsp::default()
        };

        assert_eq!(
            app_resource(Some(declared)).content_security_policy(None),
            DEFAULT_APP_CSP
        );
    }

    #[test]
    fn the_sandbox_proxy_names_only_a_cards_frame_sources() {
        // The proxy's header rebuilds the Card's frame-src and nothing else:
        // the policy itself is never re-emitted, so every other directive is
        // ignored rather than copied into a header.
        assert_eq!(
            super::sandbox_frame_sources(Some(
                "default-src 'none'; frame-src https://frames.example; object-src 'none'"
            )),
            ["https://frames.example".to_owned()]
        );
        assert_eq!(
            super::sandbox_frame_sources(Some("frame-src http://127.0.0.1:8080 http://localhost")),
            [
                "http://127.0.0.1:8080".to_owned(),
                "http://localhost".to_owned()
            ]
        );

        // A policy with no frame-src, or no frames, names no source: the
        // header falls back to 'none'.
        assert!(super::sandbox_frame_sources(None).is_empty());
        assert!(super::sandbox_frame_sources(Some("")).is_empty());
        assert!(super::sandbox_frame_sources(Some("frame-src 'none'")).is_empty());
        assert!(
            super::sandbox_frame_sources(Some("default-src 'none'; object-src 'none'")).is_empty()
        );
    }

    #[test]
    fn the_sandbox_proxy_refuses_a_frame_source_that_is_no_origin() {
        // A query parameter is untrusted text: only an origin this Space
        // accepts may become a source of the response's frame-src, so a
        // keyword, a wildcard, a path, userinfo, another scheme, a bare host,
        // or a non-loopback http origin falls back to naming no source.
        for policy in [
            "frame-src javascript:alert(1)",
            "frame-src *",
            "frame-src 127.0.0.2",
            "frame-src http://example.com",
            "frame-src https://frames.example/path",
            "frame-src https://user@frames.example",
            "frame-src https://UPPER.example",
            "frame-src https://",
            "frame-src https://a;b",
            "frame-src 'self'",
            "frame-src https://frames.example 'self'",
        ] {
            assert!(
                super::sandbox_frame_sources(Some(policy)).is_empty(),
                "{policy}"
            );
        }
        // One bad entry makes the whole source list unusable: a header built
        // from part of a list is not the policy the Card declared.
        assert!(
            super::sandbox_frame_sources(Some("frame-src https://frames.example *")).is_empty()
        );
    }

    #[test]
    fn the_sandbox_proxy_bounds_how_many_frame_sources_it_names() {
        let policy = |count: usize| {
            let sources: Vec<String> = (0..count)
                .map(|index| format!("https://frames{index}.example"))
                .collect();
            format!(
                "default-src 'none'; frame-src {}; object-src 'none'",
                sources.join(" ")
            )
        };

        assert_eq!(
            super::sandbox_frame_sources(Some(&policy(MAX_FRAME_SOURCES))).len(),
            MAX_FRAME_SOURCES,
            "the bound admits as many sources as it names"
        );
        assert!(
            super::sandbox_frame_sources(Some(&policy(MAX_FRAME_SOURCES + 1))).is_empty(),
            "one source past the bound falls back"
        );

        // The whole policy is read under the Card's own bound too: the route
        // does not decode an unbounded query parameter.
        let oversized = format!(
            "frame-src https://frames.example {}",
            "a".repeat(neha::record::MAX_CARD_CSP_BYTES)
        );
        assert!(super::sandbox_frame_sources(Some(&oversized)).is_empty());
    }

    #[test]
    fn an_embedded_block_hides_the_credential_only_in_decoded_bytes() {
        let resource = |value: Value| super::content_from_json(&value).unwrap();

        // A resource's text is plain text: redaction reaches it, so it is no
        // hidden credential.
        let text = resource(json!({
            "type": "resource",
            "resource": {
                "uri": "file:///report.txt",
                "mimeType": "text/plain",
                "text": "the answer is token-1",
            },
        }));
        assert!(!text.carries_credential("token-1"));

        // A resource's blob is base64 on the wire: its decoded bytes carry the
        // credential even though the payload does not spell it.
        let blob = resource(json!({
            "type": "resource",
            "resource": {
                "uri": "file:///data.bin",
                "mimeType": "application/octet-stream",
                "blob": STANDARD.encode(b"prefix token-1"),
            },
        }));
        assert!(
            blob.carries_credential("token-1"),
            "a resource blob that decodes to the credential is found"
        );
        let harmless = resource(json!({
            "type": "resource",
            "resource": {
                "uri": "file:///data.bin",
                "mimeType": "application/octet-stream",
                "blob": STANDARD.encode(b"harmless bytes"),
            },
        }));
        assert!(!harmless.carries_credential("token-1"));

        // An image's decoded bytes are searched the same way.
        let image = resource(json!({
            "type": "image",
            "mimeType": "image/png",
            "data": STANDARD.encode(b"token-1"),
        }));
        assert!(image.carries_credential("token-1"));

        // One payload helper serves every string a Card or a block carries.
        assert!(super::base64_carries_credential(
            &STANDARD.encode(b"token-1"),
            "token-1"
        ));
        assert!(!super::base64_carries_credential("not base64!", "token-1"));
    }

    #[test]
    fn an_audio_block_hides_the_credential_in_its_decoded_bytes() {
        let audio = |value: &[u8]| {
            super::content_from_json(&json!({
                "type": "audio",
                "mimeType": "audio/wav",
                "data": STANDARD.encode(value),
            }))
            .expect("a base64 audio block is a modelled block")
        };

        // The base64 payload spells nothing the literal redaction could see,
        // but its decoded bytes are the credential: only decoding finds it.
        let hidden = audio(b"token-1");
        assert!(
            hidden.carries_credential("token-1"),
            "an audio payload that decodes to the credential is found"
        );
        let prefixed = audio(b"prefix token-1");
        assert!(prefixed.carries_credential("token-1"));

        // A clean clip carries nothing, and the block is modelled as audio
        // rather than as an unmodelled block.
        let clean = audio(b"clean sound");
        assert!(!clean.carries_credential("token-1"));
        assert!(
            matches!(&clean, McpContent::Audio { mime_type, .. } if mime_type.as_deref() == Some("audio/wav"))
        );

        // A payload the client cannot decode is no audio block: it is refused
        // at parse time, before any output is built.
        assert!(
            super::content_from_json(&json!({
                "type": "audio",
                "mimeType": "audio/wav",
                "data": "not base64!",
            }))
            .is_err()
        );
    }

    #[test]
    fn an_audio_block_keeps_the_block_the_server_sent() {
        // An answer may annotate a block, and the Model reads the server's own
        // block: a field this client does not model survives on the Audio
        // variant instead of being dropped with a rebuilt block.
        let block = json!({
            "type": "audio",
            "mimeType": "audio/wav",
            "data": STANDARD.encode(b"clean sound"),
            "_meta": {"ui": {"annotations": {"audience": ["user"]}}},
            "annotations": [{"audience": ["user"], "priority": 0.5}],
        });

        let McpContent::Audio { raw, .. } = super::content_from_json(&block).unwrap() else {
            panic!("expected an audio block");
        };

        assert_eq!(raw, block, "the Model reads the server's own block");
    }

    #[test]
    fn an_embedded_resource_keeps_the_block_the_server_sent() {
        let block = json!({
            "type": "resource",
            "resource": {
                "uri": "file:///report.txt",
                "mimeType": "text/plain",
                "text": "report body",
                "_meta": {"ui": {"annotations": {"audience": ["user"]}}},
                "annotations": [{"audience": ["user"], "priority": 0.5}],
            },
        });

        let McpContent::Resource { raw, .. } = super::content_from_json(&block).unwrap() else {
            panic!("expected a resource");
        };

        assert_eq!(raw, block, "the Model reads the server's own block");
    }

    #[test]
    fn a_server_request_does_not_answer_the_client() {
        let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        assert!(super::matching_message(request, 1).is_none());
        let answer = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert!(super::matching_message(answer, 1).is_some());
    }

    #[test]
    fn one_call_carries_one_traceparent_in_header_and_meta() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("mcp-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = McpClient::new("http://127.0.0.1:1", Some("secret-token")).unwrap();
        let span = tracing::info_span!("mcp.call");
        span.in_scope(|| {
            // The header and the JSON-RPC field are built from the same
            // current span, so a server sees one parent context.
            let headers = client.request_headers();
            let mut params = json!({});
            crate::diagnostics::inject_json_meta(&mut params);
            let header = header(&headers, "traceparent").expect("a traceparent header");
            let meta = params["_meta"]["traceparent"]
                .as_str()
                .expect("a traceparent in _meta");
            assert_eq!(header, meta, "one call has one parent context");
            assert!(header.starts_with("00-") && header.len() == 55);
        });
    }
}
