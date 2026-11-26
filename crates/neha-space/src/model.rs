//! The OpenAI Chat Completions adapter.
//!
//! The Space speaks one protocol to a Provider, streams the reply, and
//! records the usage the Provider reports in one normalized form.

use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(10);

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use neha::error::ErrorCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use neha::usage::Usage;

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

/// How a step retries a failed call.
#[derive(Clone, Copy, Debug)]
struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            base_delay: BASE_DELAY,
            max_delay: MAX_DELAY,
            jitter: true,
        }
    }
}

/// A client for OpenAI Chat Completions providers.
pub struct ModelClient {
    http: reqwest::Client,
    policy: RetryPolicy,
}

impl ModelClient {
    /// Builds the client.
    pub fn new() -> Result<Self, ModelError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ModelError::Transport)?;

        Ok(Self {
            http,
            policy: RetryPolicy::default(),
        })
    }

    #[cfg(test)]
    fn fast_retry() -> Result<Self, ModelError> {
        let mut client = Self::new()?;
        client.policy = RetryPolicy {
            max_attempts: MAX_ATTEMPTS,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter: false,
        };

        Ok(client)
    }

    /// Sends a request and returns the complete reply.
    ///
    /// A step is retried as a whole: rate limits, provider failures, empty
    /// replies, and transport failures try again up to five times with
    /// jittered exponential backoff, honoring `Retry-After` when it is in
    /// range. A rejected or oversized request is not retried.
    pub async fn chat(&self, request: ChatRequest<'_>) -> Result<ChatReply, ModelError> {
        let mut attempt = 0_u32;
        loop {
            match self.attempt(&request).await {
                Ok(reply) => return Ok(reply),
                Err(error) if error.retryable() && attempt + 1 < self.policy.max_attempts => {
                    let delay = error.retry_after().unwrap_or_else(|| self.backoff(attempt));
                    tokio::time::sleep(delay.min(self.policy.max_delay)).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let base = self
            .policy
            .base_delay
            .saturating_mul(1_u32 << attempt.min(5));
        let factor = if self.policy.jitter {
            0.5 + rand::random::<f64>()
        } else {
            1.0
        };
        base.mul_f64(factor).min(self.policy.max_delay)
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
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, &text));
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

fn map_status(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    body: &str,
) -> ModelError {
    match status.as_u16() {
        401 | 403 => ModelError::Unauthorized,
        429 => ModelError::RateLimited { retry_after },
        413 => ModelError::ContextTooLarge,
        500..=599 => ModelError::Server { retry_after },
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
    RateLimited {
        /// The delay the Provider named.
        retry_after: Option<Duration>,
    },
    /// The request is larger than the Model's context.
    #[error("the request is larger than the Model's context")]
    ContextTooLarge,
    /// The Provider failed.
    #[error("the Provider failed")]
    Server {
        /// The delay the Provider named.
        retry_after: Option<Duration>,
    },
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
        matches!(
            self,
            Self::RateLimited { .. } | Self::Server { .. } | Self::Transport | Self::Empty
        )
    }

    /// Returns the delay the Provider asked for, when it named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Server { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Returns the command-surface code for this failure.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized => ErrorCode::Unauthorized,
            Self::RateLimited { .. } => ErrorCode::RateLimited,
            Self::Server { .. } | Self::Transport => ErrorCode::Unavailable,
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
        let client = ModelClient::fast_retry().unwrap();

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
        let client = ModelClient::fast_retry().unwrap();

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
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.text, "retried");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn an_empty_reply_is_retried() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    let attempt = seen.fetch_add(1, Ordering::SeqCst);
                    let body = if attempt == 0 {
                        "data: [DONE]\n\n"
                    } else {
                        sse
                    };
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.text, "second");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn a_five_hundred_with_retry_after_is_retried_immediately() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [(axum::http::header::RETRY_AFTER, "0")],
                            "later",
                        )
                            .into_response();
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
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.text, "recovered");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn a_rate_limit_stops_after_five_attempts() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(axum::http::header::RETRY_AFTER, "0")],
                        "slow down",
                    )
                        .into_response()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let error = client.chat(request(&base, "sk-test")).await.unwrap_err();

        assert!(matches!(
            error,
            ModelError::RateLimited {
                retry_after: Some(_)
            }
        ));
        assert_eq!(error.code(), neha::error::ErrorCode::RateLimited);
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
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
        let client = Arc::new(ModelClient::fast_retry().unwrap());

        let error = client.chat(request(&base, "sk-test")).await;

        assert!(matches!(error, Err(ModelError::Empty)));
        task.abort();
    }
}
