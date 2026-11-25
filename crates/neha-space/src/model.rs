//! The OpenAI Chat Completions adapter.
//!
//! The Space speaks one protocol to a Provider, streams the reply, and
//! records the usage the Provider reports in one normalized form.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use neha::error::ErrorCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The role of a Message sent to a Model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    /// The Space's instructions.
    System,
    /// A person's Message.
    User,
    /// A Bot's earlier Message.
    Assistant,
}

/// One Message in a Model request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    /// The role.
    pub role: ChatRole,
    /// The text.
    pub content: String,
}

impl ChatMessage {
    /// Builds a Message.
    #[must_use]
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// The normalized usage of one Model call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens that were not cached.
    pub uncached_input: u64,
    /// Input tokens read from the provider's cache.
    pub cache_read: u64,
    /// Input tokens written to the provider's cache.
    pub cache_write: u64,
    /// Output tokens.
    pub output: u64,
    /// Output tokens that were reasoning.
    pub reasoning: u64,
}

impl Usage {
    /// Returns the tokens a usage budget counts.
    #[must_use]
    pub fn budgeted(self) -> u64 {
        self.uncached_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }
}

/// A reply from a Model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatReply {
    /// The reply text.
    pub text: String,
    /// The usage the Provider reported.
    pub usage: Usage,
}

/// A request to a Provider.
pub struct ChatRequest<'a> {
    /// The Provider's base address.
    pub base_url: &'a str,
    /// The Provider's API key.
    pub api_key: &'a str,
    /// The Model's name at the Provider.
    pub model: &'a str,
    /// The Messages, oldest first.
    pub messages: Vec<ChatMessage>,
    /// The largest reply in tokens.
    pub max_output: u32,
}

/// A client for OpenAI Chat Completions providers.
pub struct ModelClient {
    http: reqwest::Client,
}

impl ModelClient {
    /// Builds the client.
    pub fn new() -> Result<Self, ModelError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ModelError::Transport)?;

        Ok(Self { http })
    }

    /// Sends a request and returns the complete reply.
    ///
    /// A step is retried as a whole: transport and provider failures try
    /// again at most twice, while a rejected or oversized request does not.
    pub async fn chat(&self, request: ChatRequest<'_>) -> Result<ChatReply, ModelError> {
        let mut attempt = 0_u32;
        loop {
            match self.attempt(&request).await {
                Ok(reply) => return Ok(reply),
                Err(error) if error.retryable() && attempt < 2 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn attempt(&self, request: &ChatRequest<'_>) -> Result<ChatReply, ModelError> {
        let url = format!(
            "{}/chat/completions",
            request.base_url.trim_end_matches('/')
        );
        let body = RequestBody {
            model: request.model,
            messages: request
                .messages
                .iter()
                .map(|message| WireMessage {
                    role: message.role,
                    content: &message.content,
                })
                .collect(),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            max_tokens: request.max_output,
        };
        let response = self
            .http
            .post(url)
            .bearer_auth(request.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| ModelError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status, &text));
        }

        let mut events = response.bytes_stream().eventsource();
        let mut text = String::new();
        let mut usage = Usage::default();
        while let Some(event) = events.next().await {
            let event = event.map_err(|_| ModelError::Transport)?;
            if event.event == "error" {
                return Err(ModelError::Provider(event.data));
            }
            let data = event.data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let chunk: Chunk = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
            if let Some(choice) = chunk.choices.first()
                && let Some(delta) = &choice.delta.content
            {
                text.push_str(delta);
            }
            if let Some(reported) = chunk.usage {
                usage = reported.normalize();
            }
        }
        if text.is_empty() {
            return Err(ModelError::Empty);
        }

        Ok(ChatReply { text, usage })
    }
}

fn map_status(status: reqwest::StatusCode, body: &str) -> ModelError {
    match status.as_u16() {
        401 | 403 => ModelError::Unauthorized,
        429 => ModelError::RateLimited,
        413 => ModelError::ContextTooLarge,
        500..=599 => ModelError::Server,
        _ if body.to_lowercase().contains("context") => ModelError::ContextTooLarge,
        _ => ModelError::Request(body.to_owned()),
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u32,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: ChatRole,
    content: &'a str,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ReportedUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ReportedUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
    #[serde(default)]
    completion_tokens_details: CompletionDetails,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl ReportedUsage {
    fn normalize(self) -> Usage {
        let cache_read = self.prompt_tokens_details.cached_tokens;
        Usage {
            uncached_input: self.prompt_tokens.saturating_sub(cache_read),
            cache_read,
            cache_write: 0,
            output: self.completion_tokens,
            reasoning: self.completion_tokens_details.reasoning_tokens,
        }
    }
}

/// A Model call failed.
#[derive(Debug, Error)]
pub enum ModelError {
    /// The Provider's credential is not accepted.
    #[error("the Provider rejected the credential")]
    Unauthorized,
    /// The Provider asked the Space to slow down.
    #[error("the Provider rate-limited the call")]
    RateLimited,
    /// The request is larger than the Model's context.
    #[error("the request is larger than the Model's context")]
    ContextTooLarge,
    /// The Provider failed.
    #[error("the Provider failed")]
    Server,
    /// The connection failed.
    #[error("the Provider could not be reached")]
    Transport,
    /// The reply was empty.
    #[error("the Model returned an empty reply")]
    Empty,
    /// The Provider's stream did not fit the protocol.
    #[error("the Provider's reply did not fit the protocol")]
    Protocol,
    /// The Provider reported an error in the stream.
    #[error("the Provider reported an error: {0}")]
    Provider(String),
    /// The request itself was rejected.
    #[error("the Provider rejected the request: {0}")]
    Request(String),
}

impl ModelError {
    /// Returns whether retrying the step can help.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Self::RateLimited | Self::Server | Self::Transport)
    }

    /// Returns the command-surface code for this failure.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized => ErrorCode::Unauthorized,
            Self::RateLimited => ErrorCode::RateLimited,
            Self::Server | Self::Transport => ErrorCode::Unavailable,
            Self::ContextTooLarge
            | Self::Empty
            | Self::Protocol
            | Self::Provider(_)
            | Self::Request(_) => ErrorCode::InvalidArgument,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use tokio::net::TcpListener;

    use super::{ChatMessage, ChatRequest, ChatRole, ModelClient, ModelError, Usage};

    async fn provider(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1"), task)
    }

    fn request(base_url: &str, api_key: &'static str) -> ChatRequest<'static> {
        let base: &'static str = Box::leak(base_url.to_owned().into_boxed_str());

        ChatRequest {
            base_url: base,
            api_key,
            model: "test-model",
            messages: vec![
                ChatMessage::new(ChatRole::System, "be brief"),
                ChatMessage::new(ChatRole::User, "hello"),
            ],
            max_output: 128,
        }
    }

    #[tokio::test]
    async fn streams_a_reply_and_normalizes_usage() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,",
            "\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap| async move {
                assert_eq!(
                    headers.get("authorization").unwrap().to_str().unwrap(),
                    "Bearer sk-test"
                );
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::new().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.text, "Hello");
        assert_eq!(
            reply.usage,
            Usage {
                uncached_input: 6,
                cache_read: 4,
                cache_write: 0,
                output: 2,
                reasoning: 0,
            }
        );
        assert_eq!(reply.usage.budgeted(), 12);
        task.abort();
    }

    #[tokio::test]
    async fn maps_an_unauthorized_reply() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { (StatusCode::UNAUTHORIZED, "no") }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::new().unwrap();

        let error = client.chat(request(&base, "sk-test")).await.unwrap_err();

        assert!(matches!(error, ModelError::Unauthorized));
        assert_eq!(error.code(), neha::error::ErrorCode::Unauthorized);
        task.abort();
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"retried\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (StatusCode::INTERNAL_SERVER_ERROR, "later").into_response();
                    }
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(sse))
                        .unwrap()
                        .into_response()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::new().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.text, "retried");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn an_empty_stream_is_an_empty_reply() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .body(Body::from("data: [DONE]\n\n"))
                    .unwrap()
            }),
        );
        let (base, task) = provider(app).await;
        let client = Arc::new(ModelClient::new().unwrap());

        let error = client.chat(request(&base, "sk-test")).await;

        assert!(matches!(error, Err(ModelError::Empty)));
        task.abort();
    }
}
