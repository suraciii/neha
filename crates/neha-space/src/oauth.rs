//! The OAuth 2.1 sign-in an MCP server asks for.
//!
//! A Plugin's MCP server may answer the first unauthenticated request with
//! `401 Unauthorized` and name the protected resource metadata that says which
//! authorization server speaks for it. This module walks one sign-in along that
//! path: discover the metadata, register a public client, build the browser's
//! authorization URL with PKCE, and exchange the callback's code for a bearer
//! token. It owns no callback server and keeps nothing between calls except the
//! public client it registered, so the caller holds the [`SignInPlan`] across
//! the two browser hops and decides where the token lives. The redirect URI it
//! plans carries a stable key derived from the discovered issuer, so a client
//! that serves one callback base for several issuers still binds every
//! authorization response to the issuer it was discovered from.
//!
//! One rule governs every address: an endpoint is HTTPS, or HTTP on a loopback
//! address, and carries no user information. A metadata request never follows a
//! redirect. PKCE is S256, and the authorization code is bound to the MCP
//! server's canonical URL as the RFC 8707 resource. A failure is a fixed
//! message: no token, verifier, state, authorization code, or URL carrying a
//! query or user information enters one.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::mcp::{PROTOCOL_VERSION, header_class};

/// How long one metadata request may wait for its answer.
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one sign-in step may wait for every answer it needs.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest answer this client reads for one request.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// The random bytes one `state` or PKCE verifier starts with.
const RANDOM_BYTES: usize = 32;

/// The most public clients one [`OAuthClient`] remembers.
///
/// A sign-in registers at most one client, so the bound is a memory bound on a
/// long-lived client rather than a security one: the oldest registration is
/// evicted when the cache is full.
const MAX_REGISTRATIONS: usize = 64;

/// The token endpoint's OAuth error codes a refusal may quote.
///
/// The authorization server's answer is provider text. Only a code from this
/// fixed list is ever repeated to a caller; any other text is dropped and the
/// refusal keeps its fixed message.
const KNOWN_REFUSALS: [&str; 7] = [
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "invalid_target",
];

/// The stable key that binds one authorization server's redirect URI to its
/// issuer.
///
/// The key is the first 16 lowercase hexadecimal characters of the SHA-256
/// digest of the exact issuer string. It is a public, deterministic path
/// segment: a callback route checks the `{issuer_key}` segment against the
/// plan's issuer, so a client with several issuers cannot hand one issuer's
/// authorization response to another. Hashing the exact issuer (rather than a
/// normalized URL) keeps two strings that differ in any byte distinct, which is
/// what the mix-up defense needs.
#[must_use]
pub fn issuer_key(issuer: &str) -> String {
    /// The first eight digest bytes, which are sixteen hexadecimal characters.
    const KEY_BYTES: usize = 8;
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let digest = Sha256::digest(issuer.as_bytes());
    let mut key = String::with_capacity(KEY_BYTES * 2);
    for byte in &digest[..KEY_BYTES] {
        key.push(char::from(HEX[usize::from(byte >> 4)]));
        key.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    key
}

/// One signed-in browser flow's server-side parameters.
#[derive(Clone)]
pub struct SignInPlan {
    /// The authorization server URL the browser opens.
    pub authorize_url: reqwest::Url,
    /// The opaque value the callback must return unchanged.
    pub state: String,
    /// The PKCE verifier whose S256 digest the browser carried.
    pub code_verifier: String,
    /// The token endpoint the authorization code is exchanged at.
    pub token_endpoint: reqwest::Url,
    /// The public client registered for this flow.
    pub client_id: String,
    /// The redirect URI the browser returns to: the caller's base with
    /// `/{issuer_key}/callback` appended. The `{issuer_key}` path segment is
    /// checked against this plan's [`issuer`](SignInPlan::issuer) by the
    /// callback route before the code is exchanged, so one client's several
    /// issuers keep their authorization responses apart.
    pub redirect_uri: String,
    /// The canonical MCP server URL every request binds the token to.
    pub resource: String,
    /// The validated issuer identifier: the authorization server the metadata
    /// named, byte for byte. A callback's `iss` parameter must equal it.
    pub issuer: String,
    /// Whether the callback must require the RFC 9207 `iss` parameter.
    ///
    /// True when the authorization server's RFC 8414 metadata advertised
    /// `authorization_response_iss_parameter_supported`. An authorization
    /// response from such a server always carries `iss`, so the callback/finish
    /// layers enforce it as a requirement: the parameter must be present and
    /// equal [`issuer`](SignInPlan::issuer), and an answer that omits it is
    /// refused. A false value only means the callback cannot require the
    /// parameter; a present `iss` still has to equal the issuer.
    pub iss_required: bool,
}

impl std::fmt::Debug for SignInPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The authorize URL carries the state and the challenge, and the
        // verifier stands in for the token during the flow: a plan that is
        // printed for a log must not print either.
        formatter
            .debug_struct("SignInPlan")
            .field("authorize_url", &"<redacted>")
            .field("state", &"<redacted>")
            .field("code_verifier", &"<redacted>")
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("iss_required", &self.iss_required)
            .finish()
    }
}

/// An OAuth 2.1 client for the MCP servers a Space reaches.
///
/// The client holds one connection pool and the public clients it registered,
/// so one OAuth client may serve many sign-ins. The registrations are the only
/// state kept: [`OAuthClient::begin`] starts a flow and hands every value the
/// callback needs back to the caller in the [`SignInPlan`].
pub struct OAuthClient {
    /// The one connection pool every request of a sign-in shares.
    http: reqwest::Client,
    /// The public clients registered for one issuer and redirect URI.
    registrations: Mutex<RegistrationCache>,
}

/// The public clients one [`OAuthClient`] remembers, oldest first.
///
/// A registration is keyed by issuer and redirect URI. The redirect URI carries
/// the issuer's key, so two issuers that share a redirect base still keep
/// separate registrations. The cache is bounded: the oldest registration is
/// evicted before a new one beyond the bound is added, so a client that signs in
/// to many issuers keeps a fixed memory.
struct RegistrationCache {
    /// The registered client ids, with the order they were added.
    entries: Vec<((String, String), String)>,
}

impl RegistrationCache {
    /// An empty cache.
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The client id registered for one issuer and redirect URI.
    fn get(&self, issuer: &str, redirect_uri: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|((registered_issuer, registered_redirect), _)| {
                registered_issuer == issuer && registered_redirect == redirect_uri
            })
            .map(|(_, client_id)| client_id.clone())
    }

    /// Remembers one registration, evicting the oldest when the cache is full.
    fn insert(&mut self, issuer: &str, redirect_uri: &str, client_id: &str) {
        if let Some(((_, _), registered)) =
            self.entries
                .iter_mut()
                .find(|((registered_issuer, registered_redirect), _)| {
                    registered_issuer == issuer && registered_redirect == redirect_uri
                })
        {
            *registered = client_id.to_owned();
            return;
        }
        if self.entries.len() >= MAX_REGISTRATIONS {
            // `entries` is ordered by insertion, so the first entry is the
            // oldest registration.
            self.entries.remove(0);
        }
        self.entries.push((
            (issuer.to_owned(), redirect_uri.to_owned()),
            client_id.to_owned(),
        ));
    }
}

impl OAuthClient {
    /// Builds a client for one or more sign-ins.
    ///
    /// An address is HTTPS, or HTTP on a loopback address; a redirect is never
    /// followed, so a metadata or token answer cannot move the request to an
    /// address the rule did not admit. A metadata request may wait ten seconds,
    /// and one sign-in step thirty.
    #[must_use]
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            // A redirect could leave the address rule behind; a server answers
            // where it was asked.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(METADATA_TIMEOUT)
            .timeout(STEP_TIMEOUT)
            .build()
            .expect("the OAuth HTTP client could not be built");

        Self {
            http,
            registrations: Mutex::new(RegistrationCache::new()),
        }
    }

    /// Starts one sign-in and returns the parameters the browser flow needs.
    ///
    /// The method asks the MCP server for its protected resource metadata,
    /// selects its authorization server, registers a public client for the
    /// issuer-bound redirect URI when one is not already known, and builds the
    /// authorization URL with a fresh `state` and PKCE verifier. The plan also
    /// carries whether the authorization server's metadata requires the RFC
    /// 9207 `iss` parameter in the authorization response.
    ///
    /// `redirect_base` is the public address the callback is served under,
    /// without the issuer-bound tail: the plan's
    /// [`redirect_uri`](SignInPlan::redirect_uri) is this base with
    /// `/{issuer_key}/callback` appended after trailing slashes are trimmed.
    /// The base must carry no query or fragment. The caller's callback route
    /// checks the path's `{issuer_key}` segment against the plan's issuer.
    ///
    /// `headers` are the fixed headers a package configures for its MCP
    /// server. Only a header [`header_class`] leaves unclassified is sent on
    /// the unauthenticated `initialize` request: a header the client owns or
    /// the credential class covers is dropped, and the client's own framing
    /// headers are written last.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::Unreachable`] when a server cannot be reached in
    /// time, and [`OAuthError::Protocol`] when an answer does not meet the
    /// metadata, transport, PKCE, or client-registration rules.
    pub async fn begin(
        &self,
        mcp_url: &str,
        redirect_base: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<SignInPlan, OAuthError> {
        let target = Target::parse(mcp_url)?;
        validate_redirect_base(redirect_base)?;

        match tokio::time::timeout(
            STEP_TIMEOUT,
            self.begin_flow(&target, redirect_base, headers),
        )
        .await
        {
            Ok(plan) => plan,
            Err(_) => Err(unreachable_error("the sign-in did not finish in time")),
        }
    }

    /// Exchanges one callback's authorization code for an access token.
    ///
    /// The form carries the PKCE verifier and the resource the plan bound the
    /// flow to, and no client authentication: a dynamic client is public. The
    /// token endpoint's transport rule is checked again here, and the plan's
    /// [`issuer`](SignInPlan::issuer) is not needed: the caller compares the
    /// callback's `iss` parameter with it before this exchange.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::Unreachable`] when the token endpoint cannot be
    /// reached in time, [`OAuthError::Refused`] when it answers with an OAuth
    /// error, and [`OAuthError::Protocol`] when its answer names no bearer
    /// access token.
    pub async fn finish(&self, plan: &SignInPlan, code: &str) -> Result<String, OAuthError> {
        match tokio::time::timeout(STEP_TIMEOUT, self.finish_flow(plan, code)).await {
            Ok(token) => token,
            Err(_) => Err(unreachable_error(
                "the token request did not finish in time",
            )),
        }
    }

    /// Performs one sign-in's discovery, registration, and URL construction.
    async fn begin_flow(
        &self,
        target: &Target,
        redirect_base: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<SignInPlan, OAuthError> {
        let challenge = self.initialize(&target.url, headers).await?;
        let metadata_url = match challenge.resource_metadata {
            Some(address) => address,
            None => protected_resource_metadata_url(&target.url)?,
        };
        let protected = self
            .metadata(&metadata_url, "the protected resource metadata")
            .await?;

        let resource = protected
            .get("resource")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol_error("the protected resource metadata names no resource"))?;
        if resource != target.resource {
            return Err(protocol_error(
                "the protected resource metadata names another resource",
            ));
        }

        let issuer_text = protected
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(Value::as_str)
            .ok_or_else(|| {
                protocol_error("the protected resource metadata names no authorization server")
            })?;
        let issuer = reqwest::Url::parse(issuer_text)
            .map_err(|_| protocol_error("the authorization server address is not valid"))?;
        require_identifier(&issuer, "the authorization server address")?;

        let metadata = self
            .authorization_server_metadata(&issuer, issuer_text)
            .await?;
        let authorization_endpoint = endpoint(&metadata, "authorization_endpoint")?;
        let token_endpoint = endpoint(&metadata, "token_endpoint")?;
        if !advertises_s256(&metadata) {
            return Err(protocol_error(
                "the authorization server does not advertise PKCE S256",
            ));
        }
        // RFC 9207: the metadata says whether every authorization response
        // carries `iss`. The plan carries the requirement to the callback
        // layers, which enforce it; a document that names another type for the
        // field is not read.
        let iss_required = authorization_response_iss_parameter_supported(&metadata)?;
        let registration_endpoint = registration_endpoint(&metadata)?;
        // Bind the redirect to the issuer before any client registration: the
        // redirect URI is issuer-specific, so a registration for another
        // issuer's callback can never receive this flow's authorization
        // response.
        let redirect_uri = redirect_uri_for(redirect_base, issuer_text);

        let client_id = match self.cached_client(issuer_text, &redirect_uri) {
            Some(client_id) => client_id,
            None => {
                let Some(registration_endpoint) = registration_endpoint else {
                    return Err(protocol_error(
                        "the authorization server does not support dynamic registration",
                    ));
                };
                let client_id = self.register(&registration_endpoint, &redirect_uri).await?;
                self.remember_client(issuer_text, &redirect_uri, &client_id);
                client_id
            }
        };

        let state = random_value();
        let code_verifier = random_value();
        let code_challenge = challenge_for(&code_verifier);
        // The MCP server's challenge names the scope it needs; the protected
        // resource metadata's list is the fallback.
        let scope = challenge
            .scope
            .filter(|scope| !scope.is_empty())
            .or_else(|| requested_scope(&protected));
        let mut authorize_url = authorization_endpoint;
        {
            let mut parameters = authorize_url.query_pairs_mut();
            parameters.append_pair("response_type", "code");
            parameters.append_pair("client_id", &client_id);
            parameters.append_pair("redirect_uri", &redirect_uri);
            parameters.append_pair("state", &state);
            parameters.append_pair("code_challenge", &code_challenge);
            parameters.append_pair("code_challenge_method", "S256");
            parameters.append_pair("resource", &target.resource);
            if let Some(scope) = &scope {
                parameters.append_pair("scope", scope);
            }
        }

        Ok(SignInPlan {
            authorize_url,
            state,
            code_verifier,
            token_endpoint,
            client_id,
            redirect_uri,
            resource: target.resource.clone(),
            issuer: issuer_text.to_owned(),
            iss_required,
        })
    }

    /// Exchanges one code for the access token the plan's flow was for.
    async fn finish_flow(&self, plan: &SignInPlan, code: &str) -> Result<String, OAuthError> {
        if code.is_empty() {
            return Err(protocol_error("no authorization code was given"));
        }
        require_secure_url(&plan.token_endpoint, "the token endpoint")?;

        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", plan.code_verifier.as_str()),
            ("redirect_uri", plan.redirect_uri.as_str()),
            ("client_id", plan.client_id.as_str()),
            ("resource", plan.resource.as_str()),
        ];
        let response = self
            .http
            .post(plan.token_endpoint.clone())
            .form(&form)
            .send()
            .await
            .map_err(|_| unreachable_error("the authorization server could not be reached"))?;
        let status = response.status();
        let body = read_body(response).await?;
        let answer: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

        if !status.is_success() {
            let code = answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(refused_error(code));
        }

        bearer_access_token(&answer)
    }

    /// Posts the uncleared `initialize` request and reads the challenge it
    /// earns.
    ///
    /// The package's fixed headers travel with the request, filtered by
    /// [`configured_headers`]. The answer may name the protected resource
    /// metadata address and the scope to ask for; a challenge that names no
    /// address leaves the RFC 9728 well-known address to be derived from the
    /// MCP server URL instead.
    async fn initialize(
        &self,
        url: &reqwest::Url,
        headers: &BTreeMap<String, String>,
    ) -> Result<Challenge, OAuthError> {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "neha",
                    "version": "1",
                },
            },
        });
        let response = self
            .http
            .post(url.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .headers(configured_headers(headers))
            .json(&message)
            .send()
            .await
            .map_err(|_| unreachable_error("the MCP server could not be reached"))?;
        let status = response.status();
        if status != reqwest::StatusCode::UNAUTHORIZED {
            return Err(protocol_error(format!(
                "the MCP server did not answer with 401 Unauthorized (HTTP {})",
                status.as_u16()
            )));
        }

        // A header may carry several challenges: the bearer one is the one
        // this flow reads, whatever scheme precedes it.
        let challenge = response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(bearer_challenge)
            .unwrap_or_default();
        let BearerChallenge {
            resource_metadata,
            scope,
        } = challenge;
        let Some(address) = resource_metadata else {
            return Ok(Challenge {
                resource_metadata: None,
                scope,
            });
        };
        let address = reqwest::Url::parse(&address)
            .map_err(|_| protocol_error("the protected resource metadata address is not valid"))?;
        require_secure_url(&address, "the protected resource metadata address")?;
        Ok(Challenge {
            resource_metadata: Some(address),
            scope,
        })
    }

    /// Fetches one JSON metadata document without following a redirect.
    async fn metadata(&self, url: &reqwest::Url, what: &str) -> Result<Value, OAuthError> {
        require_secure_url(url, what)?;
        let answer = tokio::time::timeout(METADATA_TIMEOUT, async {
            let response = self
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(|_| unreachable_error(format!("{what} could not be reached")))?;
            let status = response.status();
            let body = read_body(response).await?;
            Ok::<_, OAuthError>((status, body))
        })
        .await;
        let (status, body) = match answer {
            Ok(answer) => answer?,
            Err(_) => return Err(unreachable_error(format!("{what} did not answer in time"))),
        };
        if !status.is_success() {
            return Err(protocol_error(format!(
                "{what} answered with HTTP {}",
                status.as_u16()
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|_| protocol_error(format!("{what} is not valid JSON")))
    }

    /// Registers one public client for the redirect URI.
    async fn register(
        &self,
        endpoint: &reqwest::Url,
        redirect_uri: &str,
    ) -> Result<String, OAuthError> {
        require_secure_url(endpoint, "the client registration endpoint")?;
        let request = json!({
            "redirect_uris": [redirect_uri],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "client_name": "Neha",
        });
        let response = self
            .http
            .post(endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(|_| {
                unreachable_error("the client registration endpoint could not be reached")
            })?;
        let status = response.status();
        let body = read_body(response).await?;
        if !status.is_success() {
            return Err(protocol_error(format!(
                "the client registration answered with HTTP {}",
                status.as_u16()
            )));
        }
        let answer: Value = serde_json::from_slice(&body)
            .map_err(|_| protocol_error("the client registration is not valid JSON"))?;
        let client_id = answer
            .get("client_id")
            .and_then(Value::as_str)
            .filter(|client_id| !client_id.is_empty())
            .ok_or_else(|| protocol_error("the authorization server registered no client id"))?;
        // The answer must describe the public client this flow registered:
        // the exact redirect URI it will return to, no client authentication,
        // and no secret. Anything else is a client this flow cannot use.
        let redirect_matches = answer
            .get("redirect_uris")
            .and_then(Value::as_array)
            .is_some_and(|uris| uris.iter().any(|uri| uri.as_str() == Some(redirect_uri)));
        if !redirect_matches {
            return Err(protocol_error(
                "the client registration does not name the redirect URI it was given",
            ));
        }
        if let Some(method) = answer.get("token_endpoint_auth_method")
            && method.as_str() != Some("none")
        {
            return Err(protocol_error(
                "the authorization server registered a client that authenticates another way",
            ));
        }
        // A public client keeps no secret; an answer that names one describes
        // a credential this flow cannot carry.
        let has_secret = answer
            .get("client_secret")
            .and_then(Value::as_str)
            .is_some_and(|secret| !secret.is_empty());
        if has_secret {
            return Err(protocol_error(
                "the authorization server registered a confidential client, which needs a client secret",
            ));
        }

        Ok(client_id.to_owned())
    }

    /// Fetches authorization server metadata, falling back to the OpenID
    /// Connect configuration only when the RFC 8414 document was not obtained.
    ///
    /// A document the RFC 8414 address published is the answer: when it names
    /// another `issuer`, the sign-in ends instead of asking a second address,
    /// because the issuer check is what binds the metadata to the authorization
    /// server (RFC 8414 section 3.3).
    async fn authorization_server_metadata(
        &self,
        issuer: &reqwest::Url,
        issuer_text: &str,
    ) -> Result<Value, OAuthError> {
        let rfc8414 = metadata_url(issuer, "/.well-known/oauth-authorization-server")?;
        match self
            .metadata(&rfc8414, "the authorization server metadata")
            .await
        {
            Ok(metadata) => {
                validate_issuer(&metadata, issuer_text)?;
                Ok(metadata)
            }
            Err(_) => {
                let openid = openid_configuration_url(issuer)?;
                let metadata = self
                    .metadata(&openid, "the authorization server metadata")
                    .await?;
                validate_issuer(&metadata, issuer_text)?;
                Ok(metadata)
            }
        }
    }

    /// Returns the public client registered for one issuer and redirect URI.
    fn cached_client(&self, issuer: &str, redirect_uri: &str) -> Option<String> {
        let registrations = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registrations.get(issuer, redirect_uri)
    }

    /// Keeps the public client one issuer registered for a redirect URI.
    fn remember_client(&self, issuer: &str, redirect_uri: &str, client_id: &str) {
        let mut registrations = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registrations.insert(issuer, redirect_uri, client_id);
    }
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

/// An OAuth sign-in failed.
///
/// Every message is locally defined: it names no token, verifier, state,
/// authorization code, or URL carrying a query or user information. A refusal
/// may quote one known OAuth error code; the
/// authorization server's own error text never enters a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthError {
    /// A server could not be reached, or did not answer in time.
    Unreachable(String),
    /// A server's answer did not meet the OAuth rules.
    Protocol(String),
    /// The token endpoint answered with an OAuth error code.
    Refused(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(message) | Self::Protocol(message) => formatter.write_str(message),
            // The refusal's payload comes from the authorization server, so
            // only a code from the fixed list is ever interpolated; every other
            // value (including a provider's own text) keeps the plain message.
            Self::Refused(code) if KNOWN_REFUSALS.contains(&code.as_str()) => write!(
                formatter,
                "the authorization server refused the token request ({code})"
            ),
            Self::Refused(_) => {
                formatter.write_str("the authorization server refused the token request")
            }
        }
    }
}

impl std::error::Error for OAuthError {}

/// One MCP server's address and the resource identifier its sign-in uses.
struct Target {
    /// Where the uncleared `initialize` request is sent.
    url: reqwest::Url,
    /// The canonical resource identifier: scheme, host, port, and path, with no
    /// query, fragment, user information, or trailing slash the caller did not
    /// write.
    resource: String,
}

impl Target {
    /// Parses the MCP server URL, refusing one outside the transport rule.
    fn parse(mcp_url: &str) -> Result<Self, OAuthError> {
        let text = mcp_url.trim();
        let url = reqwest::Url::parse(text)
            .map_err(|_| protocol_error("the MCP server URL is not valid"))?;
        require_secure_url(&url, "the MCP server URL")?;
        let resource = canonical_identifier(text, &url);

        Ok(Self { url, resource })
    }
}

/// The canonical text of one URL: lowercase scheme and host, no default port,
/// and no trailing slash the caller did not write.
fn canonical_identifier(text: &str, url: &reqwest::Url) -> String {
    let mut canonical = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        canonical.push(':');
        canonical.push_str(&port.to_string());
    }
    if url.path() != "/" {
        canonical.push_str(url.path());
    } else if wrote_path(text) {
        canonical.push('/');
    }

    canonical
}

/// Whether the URL wrote a path component after its authority.
fn wrote_path(text: &str) -> bool {
    text.split_once("://").is_some_and(|(_, rest)| {
        rest.find(['/', '?', '#'])
            .is_some_and(|index| rest[index..].starts_with('/'))
    })
}

/// The RFC 9728 metadata URL for one resource.
fn protected_resource_metadata_url(resource: &reqwest::Url) -> Result<reqwest::Url, OAuthError> {
    let mut base = resource.clone();
    base.set_query(None);
    base.set_fragment(None);
    let path = base.path();
    let path = if path == "/" { "" } else { path };
    base.join(&format!("/.well-known/oauth-protected-resource{path}"))
        .map_err(|_| protocol_error("the protected resource metadata address is not valid"))
}

/// The RFC 8414 metadata URL for one issuer: the well-known suffix sits between
/// the host and the issuer's path, whose terminating slash is removed first.
fn metadata_url(issuer: &reqwest::Url, suffix: &str) -> Result<reqwest::Url, OAuthError> {
    let mut base = issuer.clone();
    base.set_query(None);
    base.set_fragment(None);
    let path = base.path().trim_end_matches('/');
    base.join(&format!("{suffix}{path}"))
        .map_err(|_| protocol_error("the authorization server metadata address is not valid"))
}

/// The OpenID Connect configuration URL: the well-known suffix follows the
/// issuer's path.
fn openid_configuration_url(issuer: &reqwest::Url) -> Result<reqwest::Url, OAuthError> {
    let mut base = issuer.clone();
    base.set_query(None);
    base.set_fragment(None);
    let path = base.path().trim_end_matches('/');
    base.join(&format!("{path}/.well-known/openid-configuration"))
        .map_err(|_| protocol_error("the OpenID configuration address is not valid"))
}

/// Whether the address is one the sign-in may call: HTTPS anywhere, or HTTP on
/// a loopback address, with no user information.
fn secure(url: &reqwest::Url) -> bool {
    let transport = match url.scheme() {
        "https" => url.has_host(),
        "http" => is_loopback(url),
        _ => false,
    };

    transport && url.username().is_empty() && url.password().is_none()
}

/// Whether one fetched address meets the transport rule.
fn require_secure_url(url: &reqwest::Url, what: &str) -> Result<(), OAuthError> {
    if secure(url) {
        return Ok(());
    }

    Err(protocol_error(format!(
        "{what} must be an https:// address, or http:// on a loopback address, with no credentials"
    )))
}

/// Whether one identifier meets the transport rule and carries no query or
/// fragment.
fn require_identifier(url: &reqwest::Url, what: &str) -> Result<(), OAuthError> {
    require_secure_url(url, what)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(protocol_error(format!(
            "{what} must carry no query or fragment"
        )));
    }

    Ok(())
}

/// Whether the URL's host is the Space's own machine.
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

/// One required endpoint of the authorization server metadata.
fn endpoint(metadata: &Value, field: &str) -> Result<reqwest::Url, OAuthError> {
    let text = metadata.get(field).and_then(Value::as_str).ok_or_else(|| {
        protocol_error(format!(
            "the authorization server metadata names no `{field}`"
        ))
    })?;
    let url = reqwest::Url::parse(text).map_err(|_| {
        protocol_error(format!(
            "the authorization server metadata's `{field}` is not valid"
        ))
    })?;
    require_secure_url(&url, &format!("the `{field}`"))?;

    Ok(url)
}

/// The optional client registration endpoint of the authorization server
/// metadata.
fn registration_endpoint(metadata: &Value) -> Result<Option<reqwest::Url>, OAuthError> {
    let Some(value) = metadata.get("registration_endpoint") else {
        return Ok(None);
    };
    let text = value.as_str().ok_or_else(|| {
        protocol_error("the authorization server metadata's `registration_endpoint` is not text")
    })?;
    let url = reqwest::Url::parse(text).map_err(|_| {
        protocol_error("the authorization server metadata's `registration_endpoint` is not valid")
    })?;
    require_secure_url(&url, "the `registration_endpoint`")?;

    Ok(Some(url))
}

/// Whether the authorization server advertises PKCE S256.
fn advertises_s256(metadata: &Value) -> bool {
    metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|method| method.as_str() == Some("S256")))
}

/// Whether the authorization server's metadata requires the RFC 9207 `iss`
/// parameter.
///
/// RFC 8414 defines `authorization_response_iss_parameter_supported` as an
/// optional boolean that defaults to `false`. A present value of another type
/// is not a document this client reads: the requirement can only be required or
/// not, so a closed validation refuses the answer instead of guessing.
fn authorization_response_iss_parameter_supported(metadata: &Value) -> Result<bool, OAuthError> {
    match metadata.get("authorization_response_iss_parameter_supported") {
        None => Ok(false),
        Some(Value::Bool(supported)) => Ok(*supported),
        Some(_) => Err(protocol_error(
            "the authorization server metadata's `authorization_response_iss_parameter_supported` is not a boolean",
        )),
    }
}

/// The scope the protected resource metadata asks for, space-joined.
fn requested_scope(metadata: &Value) -> Option<String> {
    let scopes = metadata.get("scopes_supported")?.as_array()?;
    let scopes: Vec<&str> = scopes.iter().filter_map(Value::as_str).collect();
    if scopes.is_empty() {
        return None;
    }

    Some(scopes.join(" "))
}

/// The bearer access token one token answer names.
fn bearer_access_token(answer: &Value) -> Result<String, OAuthError> {
    let token_type = answer
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(protocol_error(
            "the authorization server did not return a bearer token",
        ));
    }
    let token = answer
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| protocol_error("the authorization server did not return an access token"))?;

    Ok(token.to_owned())
}

/// The redirect URI one issuer's callback is served at.
///
/// The base's trailing slashes are trimmed so a base written with or without
/// one yields exactly one `/callback` segment, and the issuer key makes the
/// URI specific to the discovered issuer.
fn redirect_uri_for(redirect_base: &str, issuer: &str) -> String {
    format!(
        "{}/{}/callback",
        redirect_base.trim_end_matches('/'),
        issuer_key(issuer)
    )
}

/// Validates the redirect base the caller will serve the callback under.
///
/// The issuer-keyed callback path is appended to this base, so a base that is
/// not a URL, or that carries a query or fragment the appended path would
/// displace, is refused.
fn validate_redirect_base(redirect_base: &str) -> Result<(), OAuthError> {
    let url = reqwest::Url::parse(redirect_base)
        .map_err(|_| protocol_error("the redirect base is not valid"))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(protocol_error(
            "the redirect base must carry no query or fragment",
        ));
    }

    Ok(())
}

/// A fresh `state` or PKCE verifier: 32 random bytes, base64url without
/// padding.
fn random_value() -> String {
    let mut bytes = [0_u8; RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 challenge of one PKCE verifier.
fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Reads one answer body, refusing to buffer more than [`MAX_ANSWER_BYTES`].
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, OAuthError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| protocol_error("the answer could not be read"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_ANSWER_BYTES {
            return Err(protocol_error("the answer is too large"));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// What the MCP server's `initialize` challenge tells the sign-in.
#[derive(Debug, Default)]
struct Challenge {
    /// The protected resource metadata address, when the challenge named one.
    resource_metadata: Option<reqwest::Url>,
    /// The scope the challenge asks for, when it named one.
    scope: Option<String>,
}

/// What one `WWW-Authenticate` header value's bearer challenge asks for.
#[derive(Debug, Default)]
struct BearerChallenge {
    /// The protected resource metadata address the challenge names.
    resource_metadata: Option<String>,
    /// The scope the challenge asks for.
    scope: Option<String>,
}

/// One tokenized piece of a `WWW-Authenticate` header value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// A bare token: a scheme, or a parameter's name or value.
    Token(String),
    /// A quoted-string value, with its escape pairs resolved.
    Quoted(String),
    /// The `=` between a parameter's name and its value.
    Equals,
    /// The comma between parameters or challenges.
    Comma,
}

/// Selects the first `Bearer` challenge in one header value.
///
/// A header value may carry several comma-separated challenges, and another
/// scheme may come first. Schemes and parameter names compare without regard to
/// case; a parameter is a token followed by `=`, so a token followed by
/// anything else starts the next challenge.
fn bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let mut pieces = challenge_pieces(header).into_iter().peekable();
    loop {
        while matches!(pieces.peek(), Some(Piece::Comma)) {
            pieces.next();
        }
        let Some(Piece::Token(scheme)) = pieces.next() else {
            return None;
        };
        let bearer = scheme.eq_ignore_ascii_case("bearer");
        let mut challenge = BearerChallenge::default();
        loop {
            match pieces.next() {
                None => break,
                Some(Piece::Comma) => {
                    // Only a following token that is not a parameter's name (a
                    // token followed by `=`) begins another challenge.
                    let mut look = pieces.clone();
                    while matches!(look.peek(), Some(Piece::Comma)) {
                        look.next();
                    }
                    let new_challenge = match look.next() {
                        Some(Piece::Token(_)) => !matches!(look.next(), Some(Piece::Equals)),
                        Some(Piece::Quoted(_) | Piece::Equals | Piece::Comma) | None => false,
                    };
                    if new_challenge {
                        break;
                    }
                }
                Some(Piece::Token(name)) => {
                    if !matches!(pieces.peek(), Some(Piece::Equals)) {
                        // A bare token, such as a scheme's token68, names no
                        // parameter.
                        continue;
                    }
                    pieces.next();
                    let Some(Piece::Token(value) | Piece::Quoted(value)) = pieces.next() else {
                        break;
                    };
                    if !bearer {
                        continue;
                    }
                    if name.eq_ignore_ascii_case("resource_metadata") {
                        challenge.resource_metadata = Some(value);
                    } else if name.eq_ignore_ascii_case("scope") {
                        challenge.scope = Some(value);
                    }
                }
                Some(Piece::Quoted(_) | Piece::Equals) => {}
            }
        }
        if bearer {
            return Some(challenge);
        }
    }
}

/// Tokenizes one `WWW-Authenticate` header value.
fn challenge_pieces(header: &str) -> Vec<Piece> {
    let bytes = header.as_bytes();
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' => index += 1,
            b',' => {
                pieces.push(Piece::Comma);
                index += 1;
            }
            b'=' => {
                pieces.push(Piece::Equals);
                index += 1;
            }
            b'"' => {
                index += 1;
                let mut value = String::new();
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' if index + 1 < bytes.len() => {
                            value.push(char::from(bytes[index + 1]));
                            index += 2;
                        }
                        b'"' => {
                            index += 1;
                            break;
                        }
                        byte => {
                            value.push(char::from(byte));
                            index += 1;
                        }
                    }
                }
                pieces.push(Piece::Quoted(value));
            }
            byte if is_token_byte(byte) => {
                let start = index;
                while index < bytes.len() && is_token_byte(bytes[index]) {
                    index += 1;
                }
                pieces.push(Piece::Token(header[start..index].to_owned()));
            }
            _ => index += 1,
        }
    }

    pieces
}

/// Whether one byte is an RFC 7230 token character.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The fixed headers a package configures for its MCP server, filtered by the
/// policy [`header_class`] applies.
///
/// Only a name the Space leaves unclassified is sent: a header the client owns
/// or the credential class covers is dropped, and a name or value that is not a
/// valid header is dropped too.
fn configured_headers(headers: &BTreeMap<String, String>) -> HeaderMap {
    let mut configured = HeaderMap::new();
    for (name, value) in headers {
        if header_class(name).is_some() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            configured.insert(name, value);
        }
    }

    configured
}

/// Requires one metadata document to name the issuer it was discovered for.
///
/// RFC 8414 section 3.3 compares the two values as strings: a document whose
/// `issuer` differs in any byte belongs to another authorization server.
fn validate_issuer(metadata: &Value, issuer: &str) -> Result<(), OAuthError> {
    let named = metadata
        .get("issuer")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("the authorization server metadata names no issuer"))?;
    if named != issuer {
        return Err(protocol_error(
            "the authorization server metadata names another issuer",
        ));
    }

    Ok(())
}

/// Builds the refusal of one token request, quoting only a known error code.
fn refused_error(code: &str) -> OAuthError {
    if KNOWN_REFUSALS.contains(&code) {
        OAuthError::Refused(code.to_owned())
    } else {
        OAuthError::Refused(String::new())
    }
}

/// Builds the fixed message of one protocol failure.
fn protocol_error(message: impl Into<String>) -> OAuthError {
    OAuthError::Protocol(message.into())
}

/// Builds the fixed message of one unreachable failure.
fn unreachable_error(message: impl Into<String>) -> OAuthError {
    OAuthError::Unreachable(message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::{Form, Path, State};
    use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
    use axum::response::Response;
    use axum::routing::{get, post};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio::net::TcpListener;

    use super::{MAX_REGISTRATIONS, OAuthClient, OAuthError, RegistrationCache, issuer_key};

    /// The redirect base every test signs in with.
    const REDIRECT_BASE: &str = "http://127.0.0.1:41111";

    /// The headers a package configures when a test configures none.
    fn headers() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// The redirect URI one issuer's flow returns to.
    ///
    /// The formula is repeated here rather than read from the implementation,
    /// so the test proves the planned URI rather than restating it.
    fn expected_redirect_for(redirect_base: &str, issuer: &str) -> String {
        format!(
            "{}/{}/callback",
            redirect_base.trim_end_matches('/'),
            issuer_key(issuer)
        )
    }

    /// How the fake token endpoint answers.
    #[derive(Clone, Copy)]
    enum TokenAnswer {
        /// A bearer answer.
        Bearer,
        /// A token type that is not bearer.
        NotBearer,
        /// A bearer answer that names no access token.
        NoAccessToken,
        /// An OAuth error answer that carries provider text.
        Error { code: &'static str },
    }

    /// How the fake registration endpoint answers.
    #[derive(Clone, Copy)]
    enum Registration {
        /// A public client registered for the requested redirect URI.
        Public,
        /// A public client that names another redirect URI.
        OtherRedirectUri,
        /// A registration that names no redirect URI.
        NoRedirectUri,
        /// A public client that authenticates with a secret.
        OtherAuthMethod,
        /// A client that needs a secret.
        Confidential,
        /// No registration endpoint at all.
        Unsupported,
    }

    /// One fake MCP server and authorization server deployment.
    struct Fake {
        /// The deployment's origin.
        base: String,
        /// The `WWW-Authenticate` challenge the MCP server answers with.
        challenge: String,
        /// The `resource` the protected resource metadata names.
        resource: String,
        /// The `issuer` the authorization server metadata names.
        issuer: String,
        /// The issuer the RFC 8414 document names, when it differs.
        rfc8414_issuer: Option<String>,
        /// A raw body the RFC 8414 address answers with, when it is not JSON.
        rfc8414_body: Option<String>,
        /// The authorization servers the protected resource metadata names.
        authorization_servers: Vec<String>,
        /// The scopes the protected resource metadata names.
        scopes: Vec<String>,
        /// The code challenge methods the authorization server metadata names.
        pkce_methods: Vec<String>,
        /// The `authorization_response_iss_parameter_supported` value the
        /// authorization server metadata names, when it names one.
        iss_parameter_supported: Option<Value>,
        /// How the registration endpoint answers.
        registration: Registration,
        /// How the token endpoint answers.
        token: TokenAnswer,
        /// Whether the RFC 8414 metadata is served.
        rfc8414: bool,
        /// Whether the OpenID configuration is served.
        openid: bool,
        /// The `initialize` requests that arrived.
        initialize: Mutex<Vec<(HeaderMap, Value)>>,
        /// The client registration requests that arrived.
        registrations: Mutex<Vec<Value>>,
        /// The token requests that arrived.
        token_requests: Mutex<Vec<(HeaderMap, HashMap<String, String>)>>,
        /// How many protected resource metadata requests the challenge named.
        challenged_metadata: AtomicUsize,
        /// How many protected resource metadata requests the well-known URL
        /// named.
        well_known_metadata: AtomicUsize,
        /// How many OpenID configuration requests arrived.
        openid_fetches: AtomicUsize,
    }

    impl Fake {
        /// A deployment whose MCP server lives at `/mcp`.
        fn new(base: &str) -> Self {
            Self {
                base: base.to_owned(),
                challenge: format!("Bearer realm=\"fake\", resource_metadata=\"{base}/prm\""),
                resource: format!("{base}/mcp"),
                issuer: base.to_owned(),
                rfc8414_issuer: None,
                rfc8414_body: None,
                authorization_servers: vec![base.to_owned()],
                scopes: vec!["read".to_owned(), "write".to_owned()],
                pkce_methods: vec!["S256".to_owned()],
                iss_parameter_supported: None,
                registration: Registration::Public,
                token: TokenAnswer::Bearer,
                rfc8414: true,
                openid: true,
                initialize: Mutex::new(Vec::new()),
                registrations: Mutex::new(Vec::new()),
                token_requests: Mutex::new(Vec::new()),
                challenged_metadata: AtomicUsize::new(0),
                well_known_metadata: AtomicUsize::new(0),
                openid_fetches: AtomicUsize::new(0),
            }
        }

        /// The authorization server metadata this deployment publishes.
        fn metadata(&self) -> Value {
            self.metadata_for(&self.issuer)
        }

        /// The authorization server metadata that names one issuer.
        fn metadata_for(&self, issuer: &str) -> Value {
            let mut metadata = json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{}/authorize", self.base),
                "token_endpoint": format!("{}/token", self.base),
                "code_challenge_methods_supported": self.pkce_methods.clone(),
            });
            if !matches!(self.registration, Registration::Unsupported) {
                metadata["registration_endpoint"] =
                    Value::String(format!("{}/register", self.base));
            }
            if let Some(supported) = &self.iss_parameter_supported {
                metadata["authorization_response_iss_parameter_supported"] = supported.clone();
            }
            metadata
        }
    }

    /// Answers one JSON document.
    fn json_answer(value: &Value) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    /// Answers one status without a body.
    fn status_answer(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    }

    /// Answers one body labelled JSON, without parsing it.
    fn raw_answer(body: String) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    /// The fake MCP server's `initialize`: a challenge that names the metadata.
    async fn challenged_initialize(
        State(fake): State<Arc<Fake>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        fake.initialize.lock().unwrap().push((headers, message));
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", fake.challenge.clone())
            .body(Body::empty())
            .unwrap()
    }

    /// The fake MCP server's `initialize`: a challenge that names no metadata.
    async fn bare_initialize(State(_fake): State<Arc<Fake>>) -> Response {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Bearer")
            .body(Body::empty())
            .unwrap()
    }

    /// The protected resource metadata the challenge names.
    async fn challenged_metadata(State(fake): State<Arc<Fake>>) -> Response {
        fake.challenged_metadata.fetch_add(1, Ordering::SeqCst);
        json_answer(&protected_resource(&fake))
    }

    /// The protected resource metadata the RFC 9728 well-known URL names.
    async fn well_known_metadata(State(fake): State<Arc<Fake>>) -> Response {
        fake.well_known_metadata.fetch_add(1, Ordering::SeqCst);
        json_answer(&protected_resource(&fake))
    }

    /// The protected resource metadata document for one deployment.
    fn protected_resource(fake: &Fake) -> Value {
        json!({
            "resource": fake.resource.clone(),
            "authorization_servers": fake.authorization_servers.clone(),
            "scopes_supported": fake.scopes.clone(),
        })
    }

    /// The RFC 8414 authorization server metadata.
    async fn authorization_server_metadata(State(fake): State<Arc<Fake>>) -> Response {
        if !fake.rfc8414 {
            return status_answer(StatusCode::NOT_FOUND);
        }
        if let Some(body) = &fake.rfc8414_body {
            return raw_answer(body.clone());
        }
        let issuer = fake.rfc8414_issuer.as_deref().unwrap_or(&fake.issuer);
        json_answer(&fake.metadata_for(issuer))
    }

    /// The OpenID Connect configuration.
    async fn openid_configuration(State(fake): State<Arc<Fake>>) -> Response {
        fake.openid_fetches.fetch_add(1, Ordering::SeqCst);
        if !fake.openid {
            return status_answer(StatusCode::NOT_FOUND);
        }
        json_answer(&fake.metadata())
    }

    /// One tenant's MCP server `initialize`: a challenge that names the tenant's
    /// protected resource metadata.
    async fn tenant_initialize(
        State(fake): State<Arc<Fake>>,
        Path(tenant): Path<String>,
    ) -> Response {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "www-authenticate",
                format!(
                    "Bearer realm=\"fake\", resource_metadata=\"{}/tenants/{tenant}/prm\"",
                    fake.base
                ),
            )
            .body(Body::empty())
            .unwrap()
    }

    /// One tenant's protected resource metadata.
    async fn tenant_protected(
        State(fake): State<Arc<Fake>>,
        Path(tenant): Path<String>,
    ) -> Response {
        json_answer(&json!({
            "resource": format!("{}/tenants/{tenant}/mcp", fake.base),
            "authorization_servers": [format!("{}/tenants/{tenant}", fake.base)],
            "scopes_supported": ["read"],
        }))
    }

    /// One tenant's authorization server metadata.
    ///
    /// The endpoint fields are the same for every tenant: these issuers share
    /// one authorization server deployment but are distinct issuers.
    async fn tenant_authorization_server_metadata(
        State(fake): State<Arc<Fake>>,
        Path(tenant): Path<String>,
    ) -> Response {
        json_answer(&json!({
            "issuer": format!("{}/tenants/{tenant}", fake.base),
            "authorization_endpoint": format!("{}/authorize", fake.base),
            "token_endpoint": format!("{}/token", fake.base),
            "registration_endpoint": format!("{}/register", fake.base),
            "code_challenge_methods_supported": ["S256"],
        }))
    }

    /// The dynamic client registration endpoint.
    async fn register(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        // A faithful registration answers with the redirect URIs it was sent:
        // the issuer-bound callback the client planned.
        let redirect_uris = request.get("redirect_uris").cloned().unwrap_or(Value::Null);
        fake.registrations.lock().unwrap().push(request);
        match fake.registration {
            Registration::Public => json_answer(&json!({
                "client_id": "client-1",
                "redirect_uris": redirect_uris,
                "token_endpoint_auth_method": "none",
            })),
            Registration::OtherRedirectUri => json_answer(&json!({
                "client_id": "client-1",
                "redirect_uris": ["http://127.0.0.1:41112/other"],
                "token_endpoint_auth_method": "none",
            })),
            Registration::NoRedirectUri => json_answer(&json!({
                "client_id": "client-1",
                "token_endpoint_auth_method": "none",
            })),
            Registration::OtherAuthMethod => json_answer(&json!({
                "client_id": "client-1",
                "redirect_uris": redirect_uris,
                "token_endpoint_auth_method": "client_secret_basic",
            })),
            Registration::Confidential => json_answer(&json!({
                "client_id": "client-1",
                "redirect_uris": redirect_uris,
                "client_secret": "secret-1",
            })),
            Registration::Unsupported => status_answer(StatusCode::NOT_FOUND),
        }
    }

    /// The token endpoint.
    async fn token(
        State(fake): State<Arc<Fake>>,
        headers: HeaderMap,
        Form(form): Form<HashMap<String, String>>,
    ) -> Response {
        fake.token_requests.lock().unwrap().push((headers, form));
        match fake.token {
            TokenAnswer::Bearer => json_answer(&json!({
                "access_token": "access-1",
                "token_type": "Bearer",
            })),
            TokenAnswer::NotBearer => json_answer(&json!({
                "access_token": "access-1",
                "token_type": "mac",
            })),
            TokenAnswer::NoAccessToken => json_answer(&json!({"token_type": "Bearer"})),
            TokenAnswer::Error { code: failure } => {
                let mut response = json_answer(&json!({
                    "error": failure,
                    // Provider text a client must never echo: a description
                    // that reads like a secret, and a token-shaped value.
                    "error_description": "secret code",
                    "access_token": "secret-token",
                }));
                *response.status_mut() = StatusCode::BAD_REQUEST;
                response
            }
        }
    }

    /// Runs one fake MCP and authorization server deployment on a loopback
    /// port.
    async fn serve(build: impl FnOnce(&str) -> Fake) -> (String, Arc<Fake>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(build(&base));
        let app = Router::new()
            .route("/mcp", post(challenged_initialize))
            .route("/nochallenge", post(bare_initialize))
            .route("/prm", get(challenged_metadata))
            .route(
                "/.well-known/oauth-protected-resource/nochallenge",
                get(well_known_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(authorization_server_metadata),
            )
            .route(
                "/.well-known/openid-configuration",
                get(openid_configuration),
            )
            .route("/tenants/{tenant}/mcp", post(tenant_initialize))
            .route("/tenants/{tenant}/prm", get(tenant_protected))
            .route(
                "/.well-known/oauth-authorization-server/tenants/{tenant}",
                get(tenant_authorization_server_metadata),
            )
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(fake.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (base, fake)
    }

    /// The query parameters of one URL.
    fn parameters(url: &reqwest::Url) -> HashMap<String, String> {
        url.query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }

    /// The S256 challenge of one verifier, recomputed from the specification.
    fn s256(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    #[tokio::test]
    async fn begin_carries_pkce_the_resource_the_state_and_the_scope() {
        let (base, fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        let plan = client
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(plan.token_endpoint.as_str(), format!("{base}/token"));
        assert_eq!(plan.client_id, "client-1");
        assert_eq!(
            plan.redirect_uri,
            expected_redirect_for(REDIRECT_BASE, &plan.issuer)
        );
        assert_eq!(plan.resource, format!("{base}/mcp"));
        assert_eq!(plan.issuer, base);
        assert_eq!(plan.state.len(), 43);
        assert_eq!(plan.code_verifier.len(), 43);
        assert_ne!(plan.state, plan.code_verifier);

        let query = parameters(&plan.authorize_url);
        assert_eq!(plan.authorize_url.path(), "/authorize");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "client-1");
        assert_eq!(query["redirect_uri"], plan.redirect_uri);
        assert_eq!(query["state"], plan.state);
        assert_eq!(query["code_challenge"], s256(&plan.code_verifier));
        assert_eq!(query["code_challenge"].len(), 43);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["resource"], plan.resource);
        assert_eq!(query["scope"], "read write");

        let initialize = fake.initialize.lock().unwrap();
        assert_eq!(initialize.len(), 1);
        let (headers, message) = &initialize[0];
        assert_eq!(message["jsonrpc"], "2.0");
        assert_eq!(message["id"], 1);
        assert_eq!(message["method"], "initialize");
        assert_eq!(message["params"]["protocolVersion"], "2025-06-18");
        assert_eq!(message["params"]["capabilities"], json!({}));
        assert_eq!(
            message["params"]["clientInfo"],
            json!({"name": "neha", "version": "1"})
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/json"
        );
        assert!(
            headers
                .get("accept")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("text/event-stream")
        );
        assert!(headers.get("authorization").is_none());

        let registrations = fake.registrations.lock().unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(
            registrations[0]["redirect_uris"],
            json!([plan.redirect_uri])
        );
        assert_eq!(registrations[0]["token_endpoint_auth_method"], "none");
        assert_eq!(
            registrations[0]["grant_types"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(registrations[0]["response_types"], json!(["code"]));
        assert_eq!(registrations[0]["client_name"], "Neha");
        assert_eq!(fake.challenged_metadata.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn issuer_key_is_stable_sixteen_lowercase_hex_and_distinguishes_issuers() {
        let issuer = "https://auth.example";
        let key = issuer_key(issuer);
        let digest: String = Sha256::digest(issuer.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        assert_eq!(key.as_str(), &digest[..16]);
        assert_eq!(key.len(), 16);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{key} is not lowercase hexadecimal"
        );
        assert_eq!(key, issuer_key(issuer), "the key is stable");
        assert_ne!(
            key,
            issuer_key("https://auth.example/"),
            "a trailing slash names another issuer"
        );
        assert_ne!(key, issuer_key("https://auth.example:443"));
    }

    #[tokio::test]
    async fn begin_appends_one_keyed_callback_segment_to_any_redirect_base() {
        let (base, _fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        for redirect_base in [
            REDIRECT_BASE.to_owned(),
            format!("{REDIRECT_BASE}/"),
            format!("{REDIRECT_BASE}/api/mcp/signin"),
            format!("{REDIRECT_BASE}/api/mcp/signin/"),
            "https://space.example/api/mcp/signin".to_owned(),
            "https://space.example/api/mcp/signin//".to_owned(),
        ] {
            let plan = client
                .begin(&format!("{base}/mcp"), &redirect_base, &headers())
                .await
                .unwrap();
            let expected = expected_redirect_for(&redirect_base, &plan.issuer);
            assert_eq!(plan.redirect_uri, expected, "{redirect_base}");

            let url = reqwest::Url::parse(&plan.redirect_uri).unwrap();
            assert_eq!(
                url.path().matches("/callback").count(),
                1,
                "exactly one callback segment: {}",
                plan.redirect_uri
            );
            let after_scheme = plan.redirect_uri.split_once("://").unwrap().1;
            assert!(
                !after_scheme.contains("//"),
                "no double slash: {}",
                plan.redirect_uri
            );
            let segments: Vec<_> = url.path_segments().unwrap().collect();
            assert_eq!(segments.last().copied(), Some("callback"));
            let key = segments[segments.len() - 2];
            assert_eq!(key, issuer_key(&plan.issuer));
            assert_eq!(key.len(), 16);
            assert!(
                key.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "the key needs no percent-encoding: {}",
                plan.redirect_uri
            );
            assert!(
                !plan.redirect_uri.contains('%'),
                "no percent-encoding is needed: {}",
                plan.redirect_uri
            );
        }
    }

    #[tokio::test]
    async fn issuers_that_share_metadata_endpoints_get_different_redirect_uris() {
        let (base, fake) = serve(Fake::new).await;
        let redirect_base = "https://space.example/api/mcp/signin";
        let client = OAuthClient::new();

        let first = client
            .begin(
                &format!("{base}/tenants/one/mcp"),
                redirect_base,
                &headers(),
            )
            .await
            .unwrap();
        let second = client
            .begin(
                &format!("{base}/tenants/two/mcp"),
                redirect_base,
                &headers(),
            )
            .await
            .unwrap();

        assert_ne!(first.issuer, second.issuer);
        assert_eq!(
            first.redirect_uri,
            expected_redirect_for(redirect_base, &first.issuer)
        );
        assert_eq!(
            second.redirect_uri,
            expected_redirect_for(redirect_base, &second.issuer)
        );
        assert_ne!(
            first.redirect_uri, second.redirect_uri,
            "one redirect base must not serve two issuers with one URI"
        );
        // The issuers share the authorization server's endpoints; only the
        // issuer values differ.
        assert_eq!(first.token_endpoint, second.token_endpoint);
        assert_eq!(first.authorize_url.origin(), second.authorize_url.origin());
        assert_eq!(first.authorize_url.path(), second.authorize_url.path());

        // The DCR request and the authorization request both carry the keyed
        // callback.
        let registrations = fake.registrations.lock().unwrap();
        assert_eq!(registrations.len(), 2);
        assert_eq!(
            registrations[0]["redirect_uris"],
            json!([first.redirect_uri])
        );
        assert_eq!(
            registrations[1]["redirect_uris"],
            json!([second.redirect_uri])
        );
        assert_eq!(
            parameters(&first.authorize_url)["redirect_uri"],
            first.redirect_uri
        );
        assert_eq!(
            parameters(&second.authorize_url)["redirect_uri"],
            second.redirect_uri
        );
    }

    #[tokio::test]
    async fn the_registration_cache_keys_the_issuer_specific_redirect() {
        let (base, fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        let url = format!("{base}/mcp");
        let first = client
            .begin(&url, "http://127.0.0.1:41111", &headers())
            .await
            .unwrap();
        let second = client
            .begin(&url, "http://127.0.0.1:42222", &headers())
            .await
            .unwrap();

        assert_eq!(first.issuer, second.issuer);
        assert_ne!(first.redirect_uri, second.redirect_uri);
        assert_eq!(
            fake.registrations.lock().unwrap().len(),
            2,
            "a different issuer-bound redirect is a different registration"
        );

        let replay = client
            .begin(&url, "http://127.0.0.1:41111", &headers())
            .await
            .unwrap();
        assert_eq!(replay.client_id, first.client_id);
        assert_eq!(
            fake.registrations.lock().unwrap().len(),
            2,
            "the same issuer and redirect reuse one registration"
        );
    }

    #[tokio::test]
    async fn begin_refuses_a_redirect_base_with_a_query_or_fragment() {
        let client = OAuthClient::new();
        for redirect_base in [
            "https://space.example/api/mcp/signin?tenant=one",
            "https://space.example/api/mcp/signin#fragment",
        ] {
            let error = client
                .begin("https://example.com/mcp", redirect_base, &headers())
                .await
                .unwrap_err();

            assert!(
                matches!(error, OAuthError::Protocol(_)),
                "{redirect_base}: {error}"
            );
            assert!(
                error.to_string().contains("query or fragment"),
                "{redirect_base}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn finish_sends_the_verifier_the_code_binds_to_the_resource() {
        let (base, fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        let plan = client
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();
        let token = client.finish(&plan, "authorization-code").await.unwrap();
        assert_eq!(token, "access-1");

        let requests = fake.token_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let (headers, form) = &requests[0];
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "authorization-code");
        assert_eq!(form["code_verifier"], plan.code_verifier);
        assert_eq!(form["redirect_uri"], plan.redirect_uri);
        assert_eq!(form["client_id"], "client-1");
        assert_eq!(form["resource"], plan.resource);
        // The browser's challenge is the verifier's S256 digest.
        assert_eq!(
            s256(&form["code_verifier"]),
            parameters(&plan.authorize_url)["code_challenge"]
        );
        assert!(headers.get("authorization").is_none());
        assert!(
            headers
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("application/x-www-form-urlencoded")
        );
    }

    #[tokio::test]
    async fn a_second_begin_for_one_issuer_reuses_the_registered_client() {
        let (base, fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        let url = format!("{base}/mcp");
        let first = client.begin(&url, REDIRECT_BASE, &headers()).await.unwrap();
        let second = client.begin(&url, REDIRECT_BASE, &headers()).await.unwrap();

        assert_eq!(first.client_id, second.client_id);
        assert_eq!(fake.registrations.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_challenge_without_resource_metadata_falls_back_to_the_well_known_url() {
        let (base, fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.resource = format!("{base}/nochallenge");
            fake
        })
        .await;
        let client = OAuthClient::new();
        let plan = client
            .begin(&format!("{base}/nochallenge"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(plan.resource, format!("{base}/nochallenge"));
        assert_eq!(fake.well_known_metadata.load(Ordering::SeqCst), 1);
        assert_eq!(fake.challenged_metadata.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn metadata_outside_the_transport_rule_is_refused() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.authorization_servers = vec!["http://example.com".to_owned()];
            fake
        })
        .await;
        let error = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("https://"), "{error}");
    }

    #[tokio::test]
    async fn begin_refuses_an_address_outside_the_transport_rule() {
        let client = OAuthClient::new();
        for url in [
            "http://example.com/mcp",
            "ftp://example.com/mcp",
            "https://user:password@example.com/mcp",
        ] {
            let error = client
                .begin(url, REDIRECT_BASE, &headers())
                .await
                .unwrap_err();
            assert!(matches!(error, OAuthError::Protocol(_)), "{url}: {error}");
        }
    }

    #[tokio::test]
    async fn the_resource_the_metadata_names_must_match_the_mcp_server() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.resource = format!("{base}/other");
            fake
        })
        .await;
        let error = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("another resource"), "{error}");
    }

    #[tokio::test]
    async fn metadata_that_does_not_advertise_pkce_s256_is_refused() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.pkce_methods = vec!["plain".to_owned()];
            fake
        })
        .await;
        let error = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("does not advertise PKCE S256"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn begin_carries_the_rfc9207_iss_requirement_the_metadata_advertises() {
        // RFC 8414: the field is optional and defaults to false. The plan keeps
        // the issuer either way; only the callback's requirement changes.
        for (advertised, required) in [
            (None, false),
            (Some(json!(false)), false),
            (Some(json!(true)), true),
        ] {
            let label = format!("{advertised:?}");
            let (base, _fake) = serve(move |base| {
                let mut fake = Fake::new(base);
                fake.iss_parameter_supported = advertised.clone();
                fake
            })
            .await;
            let plan = OAuthClient::new()
                .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
                .await
                .unwrap();

            assert_eq!(plan.iss_required, required, "{base}: {label}");
            assert_eq!(plan.issuer, base, "the flag does not change the issuer");
        }
    }

    #[tokio::test]
    async fn metadata_with_a_non_boolean_rfc9207_flag_is_refused() {
        for advertised in [json!("true"), json!(1), json!(null), json!([true])] {
            let label = advertised.to_string();
            let (base, fake) = serve(move |base| {
                let mut fake = Fake::new(base);
                fake.iss_parameter_supported = Some(advertised.clone());
                fake
            })
            .await;
            let error = OAuthClient::new()
                .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
                .await
                .unwrap_err();

            assert!(matches!(error, OAuthError::Protocol(_)), "{label}: {error}");
            assert!(
                error
                    .to_string()
                    .contains("authorization_response_iss_parameter_supported"),
                "{label}: {error}"
            );
            assert!(
                fake.registrations.lock().unwrap().is_empty(),
                "{label}: metadata the client cannot read registers no client"
            );
        }
    }

    #[tokio::test]
    async fn the_openid_configuration_is_the_fallback_when_rfc8414_metadata_is_missing() {
        let (base, fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.rfc8414 = false;
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(plan.token_endpoint.as_str(), format!("{base}/token"));
        assert_eq!(plan.issuer, base);
        assert_eq!(fake.openid_fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_openid_configuration_is_the_fallback_when_rfc8414_metadata_is_not_json() {
        let (base, fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.rfc8414_body = Some("this is not a metadata document".to_owned());
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(plan.issuer, base);
        assert_eq!(fake.openid_fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_rfc8414_document_naming_another_issuer_never_falls_back_to_openid() {
        for trailing_slash in [false, true] {
            let (base, fake) = serve(move |base| {
                let mut fake = Fake::new(base);
                // The RFC 8414 document belongs to a different issuer: once
                // outright, once by one trailing slash. The OpenID
                // configuration would name the expected issuer.
                fake.rfc8414_issuer = Some(if trailing_slash {
                    format!("{base}/")
                } else {
                    "https://another.example".to_owned()
                });
                fake
            })
            .await;
            let error = OAuthClient::new()
                .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
                .await
                .unwrap_err();

            assert!(matches!(error, OAuthError::Protocol(_)), "{error}");
            assert!(
                error.to_string().contains("another issuer"),
                "{base}: {error}"
            );
            assert_eq!(
                fake.openid_fetches.load(Ordering::SeqCst),
                0,
                "a published metadata document must not be retried at the OpenID address"
            );
        }
    }

    #[tokio::test]
    async fn an_authorization_server_without_dynamic_registration_is_refused() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.registration = Registration::Unsupported;
            fake
        })
        .await;
        let error = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not support dynamic registration"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_confidential_client_registration_is_refused() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.registration = Registration::Confidential;
            fake
        })
        .await;
        let error = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("confidential client"), "{error}");
    }

    #[tokio::test]
    async fn a_token_answer_without_a_bearer_access_token_is_refused() {
        for (token, expected) in [
            (TokenAnswer::NotBearer, "bearer token"),
            (TokenAnswer::NoAccessToken, "access token"),
        ] {
            let (base, _fake) = serve(move |base| {
                let mut fake = Fake::new(base);
                fake.token = token;
                fake
            })
            .await;
            let client = OAuthClient::new();
            let plan = client
                .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
                .await
                .unwrap();
            let error = client.finish(&plan, "the-code").await.unwrap_err();

            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn a_refused_token_request_names_only_a_known_oauth_error_code() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.token = TokenAnswer::Error {
                code: "invalid_grant",
            };
            fake
        })
        .await;
        let client = OAuthClient::new();
        let plan = client
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();
        let code = "secret-authorization-code";
        let error = client.finish(&plan, code).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("invalid_grant"), "{message}");
        assert!(!message.contains(code), "{message}");
        assert!(!message.contains(&plan.code_verifier), "{message}");
        assert!(!message.contains(&plan.state), "{message}");
        assert!(!message.contains("access-1"), "{message}");
        // The fake endpoint echoes provider text that reads like a secret, and
        // a token-shaped value; neither may enter the error.
        assert!(!message.contains("secret code"), "{message}");
        assert!(!message.contains("secret-token"), "{message}");
        assert!(!message.contains(&plan.issuer), "{message}");
        assert!(matches!(error, OAuthError::Refused(_)));
    }

    #[tokio::test]
    async fn a_refused_token_request_with_an_unknown_code_names_no_code() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.token = TokenAnswer::Error {
                code: "provider_specific_failure",
            };
            fake
        })
        .await;
        let client = OAuthClient::new();
        let plan = client
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();
        let error = client.finish(&plan, "the-code").await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "the authorization server refused the token request"
        );
        assert!(matches!(error, OAuthError::Refused(_)));
    }

    #[tokio::test]
    async fn finish_refuses_a_token_endpoint_outside_the_transport_rule() {
        let (base, fake) = serve(Fake::new).await;
        let client = OAuthClient::new();
        let mut plan = client
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();
        plan.token_endpoint = reqwest::Url::parse("http://example.com/token").unwrap();
        let error = client.finish(&plan, "the-code").await.unwrap_err();

        assert!(matches!(error, OAuthError::Protocol(_)), "{error}");
        assert!(error.to_string().contains("https://"), "{error}");
        assert!(
            fake.token_requests.lock().unwrap().is_empty(),
            "the refused endpoint must not be called"
        );
    }

    #[tokio::test]
    async fn begin_sends_only_the_packages_unclassified_headers_to_initialize() {
        let (base, fake) = serve(Fake::new).await;
        let mut configured = headers();
        configured.insert("X-Tenant".to_owned(), "acme".to_owned());
        configured.insert("X-Request-Tag".to_owned(), "visible".to_owned());
        configured.insert("Accept".to_owned(), "text/plain".to_owned());
        configured.insert("Mcp-Session-Id".to_owned(), "forged".to_owned());
        configured.insert("Authorization".to_owned(), "Bearer forged".to_owned());
        configured.insert("X-Api-Key".to_owned(), "forged".to_owned());

        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &configured)
            .await
            .unwrap();
        assert_eq!(plan.client_id, "client-1");

        let initialize = fake.initialize.lock().unwrap();
        assert_eq!(initialize.len(), 1);
        let (headers, _) = &initialize[0];
        assert_eq!(
            headers
                .get("x-tenant")
                .and_then(|value| value.to_str().ok()),
            Some("acme"),
            "a package's own header reaches the MCP server"
        );
        assert_eq!(
            headers
                .get("x-request-tag")
                .and_then(|value| value.to_str().ok()),
            Some("visible")
        );
        // A credential-class name is never sent, even when a package sets it.
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
        // A name the client generates is never replaced by a package's value.
        assert!(headers.get("mcp-session-id").is_none());
        assert_eq!(
            headers.get("accept").and_then(|value| value.to_str().ok()),
            Some("application/json, text/event-stream")
        );
    }

    #[tokio::test]
    async fn begin_selects_the_bearer_challenge_after_another_scheme() {
        let (base, fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.challenge = format!(
                "Basic realm=\"legacy\", bEaReR resource_metadata=\"{base}/prm\", scope=\"a b\""
            );
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(plan.issuer, base);
        assert_eq!(parameters(&plan.authorize_url)["scope"], "a b");
        assert_eq!(fake.challenged_metadata.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn begin_uses_the_challenge_scope_before_the_metadata_scopes() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.challenge =
                format!("Bearer resource_metadata=\"{base}/prm\", scope=\"profile email\"");
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        // The protected resource metadata names `read write`; the challenge
        // names the scope the server actually asks for.
        assert_eq!(parameters(&plan.authorize_url)["scope"], "profile email");
    }

    #[tokio::test]
    async fn begin_uses_the_metadata_scope_when_the_challenge_names_none() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.challenge = format!("Bearer resource_metadata=\"{base}/prm\"");
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert_eq!(parameters(&plan.authorize_url)["scope"], "read write");
    }

    #[tokio::test]
    async fn begin_omits_the_scope_when_neither_names_one() {
        let (base, _fake) = serve(|base| {
            let mut fake = Fake::new(base);
            fake.challenge = format!("Bearer resource_metadata=\"{base}/prm\"");
            fake.scopes = Vec::new();
            fake
        })
        .await;
        let plan = OAuthClient::new()
            .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
            .await
            .unwrap();

        assert!(!parameters(&plan.authorize_url).contains_key("scope"));
    }

    #[tokio::test]
    async fn a_registration_that_describes_another_client_is_refused() {
        for (registration, expected) in [
            (Registration::OtherRedirectUri, "redirect URI"),
            (Registration::NoRedirectUri, "redirect URI"),
            (Registration::OtherAuthMethod, "authenticates another way"),
        ] {
            let (base, _fake) = serve(move |base| {
                let mut fake = Fake::new(base);
                fake.registration = registration;
                fake
            })
            .await;
            let error = OAuthClient::new()
                .begin(&format!("{base}/mcp"), REDIRECT_BASE, &headers())
                .await
                .unwrap_err();

            assert!(matches!(error, OAuthError::Protocol(_)), "{error}");
            assert!(error.to_string().contains(expected), "{base}: {error}");
        }
    }

    #[test]
    fn the_registration_cache_keeps_a_bounded_number_of_clients() {
        let mut cache = RegistrationCache::new();
        for index in 0..=MAX_REGISTRATIONS {
            cache.insert(
                "https://auth.example",
                &format!("http://127.0.0.1:{}/callback", index + 1),
                &format!("client-{index}"),
            );
        }

        assert_eq!(cache.entries.len(), MAX_REGISTRATIONS);
        assert!(
            cache
                .get("https://auth.example", "http://127.0.0.1:1/callback")
                .is_none(),
            "the oldest registration is evicted"
        );
        assert_eq!(
            cache.get("https://auth.example", "http://127.0.0.1:2/callback"),
            Some("client-1".to_owned())
        );
        assert_eq!(
            cache.get(
                "https://auth.example",
                &format!("http://127.0.0.1:{}/callback", MAX_REGISTRATIONS + 1)
            ),
            Some(format!("client-{MAX_REGISTRATIONS}"))
        );
    }
}
