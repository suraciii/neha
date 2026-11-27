//! The Model protocol adapters.
//!
//! The Space speaks one protocol to a Provider, streams the reply, and
//! records the usage the Provider reports in one normalized form.

use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(10);

/// The Anthropic API version this adapter speaks.
const ANTHROPIC_VERSION: &str = "2023-06-01";

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use neha::error::ErrorCode;
use neha::surface::{ProviderProtocol, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    /// A tool result.
    Tool,
}

/// A tool call on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatToolCall {
    /// The identifier the Model gave the call.
    pub id: String,
    /// The tool's name.
    pub name: String,
    /// The raw JSON arguments.
    pub arguments: String,
}

/// One Message in a Model request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    /// The role.
    pub role: ChatRole,
    /// The text.
    pub content: Option<String>,
    /// The tool calls an assistant Message requested.
    pub tool_calls: Vec<ChatToolCall>,
    /// The call a tool Message answers.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Builds a Message.
    #[must_use]
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Builds an assistant Message that requested tools.
    #[must_use]
    pub fn assistant_calls(content: Option<String>, tool_calls: Vec<ChatToolCall>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Builds a tool result Message.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool offered to a Model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatTool {
    /// Its name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// Its JSON schema.
    pub parameters: serde_json::Value,
}

/// A reply from a Model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatReply {
    /// The reply text.
    pub text: String,
    /// The tool calls the Model requested.
    pub tool_calls: Vec<ChatToolCall>,
    /// The usage the Provider reported.
    pub usage: Usage,
    /// Provider-native state the next step may need.
    pub continuation: Option<neha_loop::Continuation>,
}

/// A request to a Provider.
pub struct ChatRequest<'a> {
    /// The Provider's base address.
    pub base_url: &'a str,
    /// The Provider's API key.
    pub api_key: &'a str,
    /// The Model's name at the Provider.
    pub model: &'a str,
    /// The Provider's identifier, so native state can be matched to it.
    pub provider: &'a str,
    /// The Messages, oldest first.
    pub messages: Vec<ChatMessage>,
    /// The tools the Model may call.
    pub tools: Vec<ChatTool>,
    /// The largest reply in tokens.
    pub max_output: u32,
    /// The protocol the Provider speaks.
    pub protocol: ProviderProtocol,
    /// How much thought the Model should give this reply.
    pub thinking: Option<ThinkingLevel>,
    /// Provider-native state from the previous step.
    pub continuation: Option<neha_loop::Continuation>,
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
        let mut reported = Usage::default();
        loop {
            match self.attempt(&request).await {
                Ok(mut reply) => {
                    reply.usage += reported;
                    return Ok(reply);
                }
                Err(error) if error.retryable() && attempt + 1 < self.policy.max_attempts => {
                    reported += error.usage().unwrap_or_default();
                    let delay = error.retry_after().unwrap_or_else(|| self.backoff(attempt));
                    tokio::time::sleep(delay.min(self.policy.max_delay)).await;
                    attempt += 1;
                }
                Err(error) => {
                    if reported == Usage::default() {
                        return Err(error);
                    }
                    return Err(ModelError::Retried {
                        usage: reported + error.usage().unwrap_or_default(),
                        source: Box::new(error),
                    });
                }
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
        match request.protocol {
            ProviderProtocol::ChatCompletions => self.chat_completions(request).await,
            ProviderProtocol::OpenAiResponses => self.openai_responses(request).await,
            ProviderProtocol::AnthropicMessages => self.anthropic_messages(request).await,
        }
    }

    async fn chat_completions(&self, request: &ChatRequest<'_>) -> Result<ChatReply, ModelError> {
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
                    content: message.content.as_deref(),
                    tool_calls: if message.tool_calls.is_empty() {
                        None
                    } else {
                        Some(message.tool_calls.iter().map(WireToolCall::from).collect())
                    },
                    tool_call_id: message.tool_call_id.as_deref(),
                })
                .collect(),
            tools: request
                .tools
                .iter()
                .map(|tool| WireTool {
                    kind: "function",
                    function: WireToolDefinition {
                        name: &tool.name,
                        description: &tool.description,
                        parameters: &tool.parameters,
                    },
                })
                .collect(),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            max_tokens: request.max_output,
            reasoning_effort: request.thinking.map(reasoning_effort),
        };
        let response = send(self.http.post(url).bearer_auth(request.api_key).json(&body)).await?;

        let mut events = response.bytes_stream().eventsource();
        let mut text = String::new();
        let mut calls: std::collections::BTreeMap<u32, ChatToolCall> =
            std::collections::BTreeMap::new();
        let mut usage = Usage::default();
        let mut finished = false;
        let mut finish_failure: Option<ModelError> = None;
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
                finished = true;
                break;
            }
            let chunk: Chunk = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
            if let Some(choice) = chunk.choices.first() {
                if let Some(reason) = choice.finish_reason.as_deref() {
                    match reason {
                        "stop" | "tool_calls" | "function_call" => finished = true,
                        "length" => {
                            finished = true;
                            finish_failure = Some(ModelError::Truncated { usage });
                        }
                        "content_filter" => {
                            finished = true;
                            finish_failure = Some(ModelError::Filtered { usage });
                        }
                        _ => return Err(ModelError::Protocol),
                    }
                }
                if finish_failure.is_none() {
                    if let Some(delta) = &choice.delta.content {
                        text.push_str(delta);
                    }
                    for delta in &choice.delta.tool_calls {
                        let call = calls.entry(delta.index).or_default();
                        if let Some(id) = &delta.id
                            && !id.is_empty()
                        {
                            call.id.clone_from(id);
                        }
                        if let Some(function) = &delta.function {
                            if let Some(name) = &function.name {
                                call.name.push_str(name);
                            }
                            if let Some(arguments) = &function.arguments {
                                call.arguments.push_str(arguments);
                            }
                        }
                    }
                }
            }
            if let Some(reported) = chunk.usage {
                usage = reported.normalize();
            }
        }
        if let Some(mut failure) = finish_failure {
            match &mut failure {
                ModelError::Truncated { usage: reported }
                | ModelError::Filtered { usage: reported } => *reported = usage,
                _ => {}
            }
            return Err(failure);
        }
        if !finished {
            return Err(ModelError::StreamEnded { usage });
        }
        let tool_calls: Vec<ChatToolCall> = calls.into_values().collect();
        if text.is_empty() && tool_calls.is_empty() {
            return Err(ModelError::Empty);
        }

        Ok(ChatReply {
            text,
            tool_calls,
            usage,
            continuation: None,
        })
    }

    async fn openai_responses(&self, request: &ChatRequest<'_>) -> Result<ChatReply, ModelError> {
        let url = format!("{}/responses", request.base_url.trim_end_matches('/'));
        let body = responses_body(request);
        let response = send(self.http.post(url).bearer_auth(request.api_key).json(&body)).await?;

        read_responses_stream(response, request).await
    }

    async fn anthropic_messages(&self, request: &ChatRequest<'_>) -> Result<ChatReply, ModelError> {
        let url = format!("{}/messages", request.base_url.trim_end_matches('/'));
        let body = anthropic_body(request);
        let response = send(
            self.http
                .post(url)
                .header("x-api-key", request.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body),
        )
        .await?;

        read_anthropic_stream(response, request).await
    }
}

/// Sends a request, mapping a failed status to the shared error vocabulary.
async fn send(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, ModelError> {
    let response = builder.send().await.map_err(|_| ModelError::Transport)?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let text = response.text().await.unwrap_or_default();
    Err(map_status(status, retry_after, &text))
}

/// The delay an HTTP response names, when it names one this client honors.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The wire spelling of a thinking level.
fn reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Max => "max",
    }
}

/// The wire name of a Provider protocol, exactly as the surface serializes it.
fn protocol_wire(protocol: ProviderProtocol) -> String {
    match serde_json::to_value(protocol) {
        Ok(Value::String(wire)) => wire,
        _ => String::new(),
    }
}

/// The message a protocol's error payload carries.
fn provider_error(value: &Value, fallback: &str) -> String {
    value
        .pointer("/error/message")
        .or_else(|| value.pointer("/response/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

/// The request body of one OpenAI Responses call.
#[derive(Serialize)]
struct ResponsesBody<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool<'a>>,
    stream: bool,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
}

/// A tool as OpenAI Responses declares it.
#[derive(Serialize)]
struct ResponsesTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

/// How much thought OpenAI Responses should give the reply.
#[derive(Serialize)]
struct ResponsesReasoning {
    effort: &'static str,
}

/// The usage OpenAI Responses reports.
#[derive(Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponsesOutputDetails>,
}

/// The cached portion of the input OpenAI Responses reports.
#[derive(Deserialize, Default)]
struct ResponsesInputDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

/// The reasoning portion of the output OpenAI Responses reports.
#[derive(Deserialize, Default)]
struct ResponsesOutputDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl ResponsesUsage {
    fn normalize(self) -> Usage {
        let input = self.input_tokens_details.unwrap_or_default();
        let output = self.output_tokens_details.unwrap_or_default();
        Usage {
            uncached_input: self.input_tokens.saturating_sub(input.cached_tokens),
            cache_read: input.cached_tokens,
            cache_write: input.cache_write_tokens,
            output: self.output_tokens,
            reasoning: output.reasoning_tokens,
        }
    }
}

/// Builds the body of one OpenAI Responses call.
fn responses_body<'a>(request: &'a ChatRequest<'_>) -> ResponsesBody<'a> {
    let mut system_texts = Vec::new();
    let mut normalized: Vec<Vec<Value>> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let items = match message.role {
            ChatRole::System => {
                if let Some(text) = message.content.as_deref()
                    && !text.is_empty()
                {
                    system_texts.push(text);
                }
                Vec::new()
            }
            ChatRole::User => vec![serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": message.content.as_deref().unwrap_or_default(),
                }],
            })],
            ChatRole::Assistant => {
                let text = message.content.as_deref().unwrap_or_default();
                let mut items = Vec::new();
                if !text.is_empty() || message.tool_calls.is_empty() {
                    items.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in &message.tool_calls {
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
                items
            }
            ChatRole::Tool => vec![serde_json::json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "output": message.content.as_deref().unwrap_or_default(),
            })],
        };
        normalized.push(items);
    }

    let last_assistant = request
        .messages
        .iter()
        .rposition(|message| message.role == ChatRole::Assistant);
    if let Some(index) = last_assistant
        && index + 1 < request.messages.len()
        && request.messages[index + 1..]
            .iter()
            .all(|message| message.role == ChatRole::Tool)
        && let Some(items) = responses_continuation(request)
    {
        normalized[index] = items;
    }

    let system = system_texts.join("\n\n");
    let tools = request
        .tools
        .iter()
        .map(|tool| ResponsesTool {
            kind: "function",
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
        })
        .collect();
    ResponsesBody {
        model: request.model,
        instructions: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        input: normalized.into_iter().flatten().collect(),
        tools,
        stream: true,
        max_output_tokens: request.max_output,
        reasoning: request.thinking.map(|level| ResponsesReasoning {
            effort: reasoning_effort(level),
        }),
    }
}

/// The raw OpenAI Responses items to replay, when this Provider supplies them.
fn responses_continuation(request: &ChatRequest<'_>) -> Option<Vec<Value>> {
    let continuation = request.continuation.as_ref()?;
    if continuation.protocol != protocol_wire(ProviderProtocol::OpenAiResponses)
        || continuation.provider != request.provider
        || continuation.model != request.model
    {
        return None;
    }
    continuation.state.get("items")?.as_array().cloned()
}

/// Reads one OpenAI Responses stream into the shared reply vocabulary.
async fn read_responses_stream(
    response: reqwest::Response,
    request: &ChatRequest<'_>,
) -> Result<ChatReply, ModelError> {
    let mut events = response.bytes_stream().eventsource();
    let mut text = String::new();
    let mut items = Vec::new();
    let mut arguments: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut usage = Usage::default();
    let mut completed = false;
    let mut incomplete: Option<String> = None;
    let mut output: Option<Vec<Value>> = None;

    while let Some(event) = events.next().await {
        let event = event.map_err(|_| ModelError::Transport)?;
        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let value: Value = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.event.as_str());
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            "response.function_call_arguments.delta" => {
                if let (Some(item_id), Some(delta)) = (
                    value.get("item_id").and_then(Value::as_str),
                    value.get("delta").and_then(Value::as_str),
                ) {
                    arguments
                        .entry(item_id.to_owned())
                        .or_default()
                        .push_str(delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item")
                    && !item.is_null()
                {
                    items.push(item.clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                completed = true;
                let Some(report) = value.get("response") else {
                    return Err(ModelError::Protocol);
                };
                if let Some(reported) = report.get("usage") {
                    usage = serde_json::from_value::<ResponsesUsage>(reported.clone())
                        .map_err(|_| ModelError::Protocol)?
                        .normalize();
                }
                if let Some(raw) = report.get("output").and_then(Value::as_array) {
                    output = Some(raw.clone());
                }
                if let Some(reason) = report
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                {
                    incomplete = Some(reason.to_owned());
                }
            }
            "response.failed" | "error" => {
                return Err(ModelError::ProviderStream {
                    message: provider_error(&value, data),
                    usage,
                });
            }
            _ => {}
        }
    }

    if let Some(reason) = incomplete {
        return Err(match reason.as_str() {
            "max_output_tokens" => ModelError::Truncated { usage },
            "content_filter" => ModelError::Filtered { usage },
            _ => ModelError::Protocol,
        });
    }
    if !completed {
        return Err(ModelError::StreamEnded { usage });
    }

    let output = output.unwrap_or(items);
    let tool_calls = responses_tool_calls(&output, &arguments);
    if text.is_empty() && tool_calls.is_empty() {
        return Err(ModelError::Empty);
    }
    let continuation = if tool_calls.is_empty() {
        None
    } else {
        Some(neha_loop::Continuation {
            protocol: protocol_wire(ProviderProtocol::OpenAiResponses),
            provider: request.provider.to_owned(),
            model: request.model.to_owned(),
            state: serde_json::json!({"items": output}),
        })
    };

    Ok(ChatReply {
        text,
        tool_calls,
        usage,
        continuation,
    })
}

/// The tool calls among the final OpenAI Responses output items, in order.
fn responses_tool_calls(
    items: &[Value],
    arguments: &std::collections::BTreeMap<String, String>,
) -> Vec<ChatToolCall> {
    let mut calls = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let mut call_arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if call_arguments.is_empty()
            && let Some(item_id) = item.get("id").and_then(Value::as_str)
            && let Some(accumulated) = arguments.get(item_id)
        {
            call_arguments.clone_from(accumulated);
        }
        calls.push(ChatToolCall {
            id: item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: call_arguments,
        });
    }
    calls
}

/// The request body of one Anthropic Messages call.
#[derive(Serialize)]
struct AnthropicBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

/// A tool as Anthropic Messages declares it.
#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

/// How much thought Anthropic Messages should give the reply.
#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

/// The usage Anthropic Messages reports.
#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<AnthropicOutputDetails>,
}

/// The thinking portion of the output Anthropic Messages reports.
#[derive(Deserialize, Default)]
struct AnthropicOutputDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

impl AnthropicUsage {
    fn apply(self, usage: &mut Usage) {
        if let Some(tokens) = self.input_tokens {
            usage.uncached_input = tokens;
        }
        if let Some(tokens) = self.output_tokens {
            usage.output = tokens;
        }
        if let Some(tokens) = self.cache_read_input_tokens {
            usage.cache_read = tokens;
        }
        if let Some(tokens) = self.cache_creation_input_tokens {
            usage.cache_write = tokens;
        }
        if let Some(details) = self.output_tokens_details {
            usage.reasoning = details.thinking_tokens;
        }
    }
}

/// One content block of an Anthropic Messages reply, as it streams.
#[derive(Default)]
struct AnthropicBlock {
    kind: String,
    text: String,
    thinking: String,
    signature: String,
    data: String,
    id: String,
    name: String,
    input: Value,
    partial_json: String,
}

impl AnthropicBlock {
    fn start(block: &Value) -> Self {
        Self {
            kind: block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            thinking: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            input: block
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
            signature: String::new(),
            data: block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            partial_json: String::new(),
        }
    }

    fn push_delta(&mut self, delta: &Value) {
        match delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text_delta" => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    self.text.push_str(text);
                }
            }
            "thinking_delta" => {
                if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                    self.thinking.push_str(thinking);
                }
            }
            "signature_delta" => {
                if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                    self.signature.push_str(signature);
                }
            }
            "input_json_delta" => {
                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                    self.partial_json.push_str(partial);
                }
            }
            _ => {}
        }
    }

    fn tool_input(&self) -> Value {
        serde_json::from_str::<Value>(&self.partial_json)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| self.input.clone())
    }

    /// The raw arguments, even when they are not valid JSON: the loop, not
    /// the adapter, decides whether a call can run.
    fn tool_arguments(&self) -> String {
        if self.partial_json.is_empty() {
            self.input.to_string()
        } else {
            self.partial_json.clone()
        }
    }

    fn finish(&self) -> Option<Value> {
        match self.kind.as_str() {
            "text" => Some(serde_json::json!({"type": "text", "text": self.text})),
            "thinking" => {
                let mut block = serde_json::json!({"type": "thinking", "thinking": self.thinking});
                if !self.signature.is_empty() {
                    block["signature"] = Value::String(self.signature.clone());
                }
                Some(block)
            }
            "tool_use" => Some(serde_json::json!({
                "type": "tool_use",
                "id": self.id,
                "name": self.name,
                "input": self.tool_input(),
            })),
            "redacted_thinking" => Some(serde_json::json!({
                "type": "redacted_thinking",
                "data": self.data,
            })),
            _ => None,
        }
    }
}

/// Builds the body of one Anthropic Messages call.
fn anthropic_body<'a>(request: &'a ChatRequest<'_>) -> AnthropicBody<'a> {
    let native_assistant = request
        .messages
        .iter()
        .rposition(|message| message.role == ChatRole::Assistant);
    let needs_native_turn = native_assistant.is_some_and(|index| {
        index + 1 < request.messages.len()
            && request.messages[index + 1..]
                .iter()
                .all(|message| message.role == ChatRole::Tool)
    });
    let native_blocks = native_assistant
        .filter(|index| {
            *index + 1 < request.messages.len()
                && request.messages[*index + 1..]
                    .iter()
                    .all(|message| message.role == ChatRole::Tool)
        })
        .and_then(|_| anthropic_continuation(request));

    let mut system_texts = Vec::new();
    let mut messages = Vec::new();
    let mut tool_results = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            ChatRole::System => {
                if let Some(text) = message.content.as_deref()
                    && !text.is_empty()
                {
                    system_texts.push(text);
                }
            }
            ChatRole::User => {
                flush_tool_results(&mut messages, &mut tool_results);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": message.content.as_deref().unwrap_or_default(),
                    }],
                }));
            }
            ChatRole::Assistant => {
                flush_tool_results(&mut messages, &mut tool_results);
                let blocks = if Some(index) == native_assistant {
                    native_blocks
                        .as_ref()
                        .map_or_else(|| anthropic_content_blocks(message), Clone::clone)
                } else {
                    anthropic_content_blocks(message)
                };
                if !blocks.is_empty() {
                    messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
                }
            }
            ChatRole::Tool => {
                tool_results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or_default(),
                    "content": message.content.as_deref().unwrap_or_default(),
                }));
            }
        }
    }
    flush_tool_results(&mut messages, &mut tool_results);

    let system = system_texts.join("\n\n");
    let tools = request
        .tools
        .iter()
        .map(|tool| AnthropicTool {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.parameters,
        })
        .collect();
    AnthropicBody {
        model: request.model,
        max_tokens: request.max_output,
        system: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        messages,
        tools,
        stream: true,
        // A tool round needs its signed native turn. If this run lost it
        // (an old record, or a mismatched Provider/Model), sending thinking
        // without that turn would make Anthropic reject the request; continue
        // without extended thinking instead.
        thinking: if needs_native_turn && native_blocks.is_none() {
            None
        } else {
            request
                .thinking
                .and_then(|level| anthropic_thinking(level, request.max_output))
        },
    }
}

/// Groups the tool results that arrived since the last other Message.
fn flush_tool_results(messages: &mut Vec<Value>, tool_results: &mut Vec<Value>) {
    if !tool_results.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "content": std::mem::take(tool_results),
        }));
    }
}

/// The content blocks an assistant Message contributes to the history.
fn anthropic_content_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(text) = message.content.as_deref()
        && !text.is_empty()
    {
        blocks.push(serde_json::json!({"type": "text", "text": text}));
    }
    for call in &message.tool_calls {
        let input = serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    blocks
}

/// The raw Anthropic content blocks to replay, when this Provider supplies them.
fn anthropic_continuation(request: &ChatRequest<'_>) -> Option<Vec<Value>> {
    let continuation = request.continuation.as_ref()?;
    if continuation.protocol != protocol_wire(ProviderProtocol::AnthropicMessages)
        || continuation.provider != request.provider
        || continuation.model != request.model
    {
        return None;
    }
    continuation.state.get("blocks")?.as_array().cloned()
}

/// The thinking budget Anthropic Messages accepts for a level.
fn anthropic_thinking(level: ThinkingLevel, max_output: u32) -> Option<AnthropicThinking> {
    if max_output < 1025 {
        return None;
    }
    let mut budget = match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 4096,
        ThinkingLevel::High => 8192,
        ThinkingLevel::Max => 16384,
    };
    if max_output <= budget {
        budget = max_output.saturating_sub(1);
    }
    if budget < 1024 {
        return None;
    }
    Some(AnthropicThinking {
        kind: "enabled",
        budget_tokens: budget,
    })
}

/// Reads one Anthropic Messages stream into the shared reply vocabulary.
async fn read_anthropic_stream(
    response: reqwest::Response,
    request: &ChatRequest<'_>,
) -> Result<ChatReply, ModelError> {
    let mut events = response.bytes_stream().eventsource();
    let mut blocks: std::collections::BTreeMap<u64, AnthropicBlock> =
        std::collections::BTreeMap::new();
    let mut finished_blocks: std::collections::BTreeMap<u64, Value> =
        std::collections::BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<String> = None;
    let mut finished = false;

    while let Some(event) = events.next().await {
        let event = event.map_err(|_| ModelError::Transport)?;
        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.event.as_str());
        match kind {
            "message_start" => {
                let Some(message) = value.get("message") else {
                    return Err(ModelError::Protocol);
                };
                if let Some(reported) = message.get("usage") {
                    let reported: AnthropicUsage = serde_json::from_value(reported.clone())
                        .map_err(|_| ModelError::Protocol)?;
                    reported.apply(&mut usage);
                }
            }
            "content_block_start" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(ModelError::Protocol)?;
                let Some(block) = value.get("content_block") else {
                    return Err(ModelError::Protocol);
                };
                blocks.insert(index, AnthropicBlock::start(block));
            }
            "content_block_delta" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(ModelError::Protocol)?;
                let Some(delta) = value.get("delta") else {
                    return Err(ModelError::Protocol);
                };
                blocks.entry(index).or_default().push_delta(delta);
            }
            "content_block_stop" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(ModelError::Protocol)?;
                if let Some(block) = blocks.get(&index)
                    && let Some(finished) = block.finish()
                {
                    finished_blocks.insert(index, finished);
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    stop_reason = Some(reason.to_owned());
                }
                if let Some(reported) = value.get("usage") {
                    let reported: AnthropicUsage = serde_json::from_value(reported.clone())
                        .map_err(|_| ModelError::Protocol)?;
                    reported.apply(&mut usage);
                }
            }
            "message_stop" => finished = true,
            "error" => {
                return Err(ModelError::ProviderStream {
                    message: provider_error(&value, data),
                    usage,
                });
            }
            _ => {}
        }
    }

    if let Some(reason) = stop_reason {
        match reason.as_str() {
            "max_tokens" => return Err(ModelError::Truncated { usage }),
            "refusal" => return Err(ModelError::Filtered { usage }),
            _ => {}
        }
    }
    if !finished {
        return Err(ModelError::StreamEnded { usage });
    }

    let text: String = blocks
        .values()
        .filter(|block| block.kind == "text")
        .map(|block| block.text.as_str())
        .collect();
    let tool_calls: Vec<ChatToolCall> = blocks
        .values()
        .filter(|block| block.kind == "tool_use")
        .map(|block| ChatToolCall {
            id: block.id.clone(),
            name: block.name.clone(),
            arguments: block.tool_arguments(),
        })
        .collect();
    if text.is_empty() && tool_calls.is_empty() {
        return Err(ModelError::Empty);
    }
    let continuation = if tool_calls.is_empty() {
        None
    } else {
        Some(neha_loop::Continuation {
            protocol: protocol_wire(ProviderProtocol::AnthropicMessages),
            provider: request.provider.to_owned(),
            model: request.model.to_owned(),
            state: serde_json::json!({
                "blocks": finished_blocks.into_values().collect::<Vec<_>>(),
            }),
        })
    };

    Ok(ChatReply {
        text,
        tool_calls,
        usage,
        continuation,
    })
}

/// The loop's Model seam, backed by this adapter.
pub struct SpaceModel<'a> {
    client: &'a ModelClient,
    base_url: &'a str,
    api_key: &'a str,
    model: &'a str,
    provider: &'a str,
    protocol: ProviderProtocol,
}

impl<'a> SpaceModel<'a> {
    /// Builds the adapter for one Provider and Model.
    #[must_use]
    pub fn new(
        client: &'a ModelClient,
        base_url: &'a str,
        api_key: &'a str,
        model: &'a str,
        provider: &'a str,
        protocol: ProviderProtocol,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key,
            model,
            provider,
            protocol,
        }
    }
}

impl neha_loop::Model for SpaceModel<'_> {
    // The trait promises a `Send` future; `async fn` would not state that.
    #[allow(clippy::manual_async_fn)]
    fn complete(
        &self,
        request: neha_loop::ModelRequest,
    ) -> impl std::future::Future<Output = Result<neha_loop::ModelReply, neha_loop::ModelFailure>> + Send
    {
        async move {
            let messages = request
                .messages
                .into_iter()
                .map(|message| {
                    let calls = message
                        .tool_calls
                        .into_iter()
                        .map(|call| ChatToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                        })
                        .collect();
                    let role = match message.role {
                        neha_loop::Role::System => ChatRole::System,
                        neha_loop::Role::User => ChatRole::User,
                        neha_loop::Role::Assistant => ChatRole::Assistant,
                        neha_loop::Role::Tool => ChatRole::Tool,
                    };
                    ChatMessage {
                        role,
                        content: message.content,
                        tool_calls: calls,
                        tool_call_id: message.tool_call_id,
                    }
                })
                .collect();
            let tools = request
                .tools
                .into_iter()
                .map(|tool| ChatTool {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect();
            let reply = self
                .client
                .chat(ChatRequest {
                    base_url: self.base_url,
                    api_key: self.api_key,
                    model: self.model,
                    provider: self.provider,
                    messages,
                    tools,
                    max_output: request.max_output,
                    protocol: self.protocol,
                    thinking: request.thinking,
                    continuation: request.continuation,
                })
                .await
                .map_err(|error| neha_loop::ModelFailure {
                    code: error.code(),
                    detail: error.to_string(),
                    usage: error.usage().unwrap_or_default(),
                })?;

            Ok(neha_loop::ModelReply {
                content: if reply.text.is_empty() {
                    None
                } else {
                    Some(reply.text)
                },
                tool_calls: reply
                    .tool_calls
                    .into_iter()
                    .map(|call| neha_loop::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    })
                    .collect(),
                usage: reply.usage,
                continuation: reply.continuation,
            })
        }
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

impl<'a> From<&'a ChatToolCall> for WireToolCall<'a> {
    fn from(call: &'a ChatToolCall) -> Self {
        Self {
            id: &call.id,
            kind: "function",
            function: WireFunction {
                name: &call.name,
                arguments: &call.arguments,
            },
        }
    }
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolDefinition<'a>,
}

#[derive(Serialize)]
struct WireToolDefinition<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
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
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Deserialize, Default)]
struct DeltaToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize, Default)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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
    /// The provider stream ended before it said the reply was complete.
    #[error("the Provider closed the stream before the reply was complete")]
    StreamEnded {
        /// The usage it reported before it ended.
        usage: Usage,
    },
    /// The provider cut the reply at its output limit.
    #[error("the Provider cut the reply at its output limit")]
    Truncated {
        /// The usage it reported.
        usage: Usage,
    },
    /// The provider filtered the reply.
    #[error("the Provider filtered the reply")]
    Filtered {
        /// The usage it reported.
        usage: Usage,
    },
    /// The Provider reported a failure inside the stream.
    #[error("the Provider reported an error: {message}")]
    ProviderStream {
        /// What the Provider said.
        message: String,
        /// The usage it reported before the failure.
        usage: Usage,
    },
    /// The call failed after earlier attempts reported usage.
    #[error("{source}")]
    Retried {
        /// The usage every attempt reported.
        usage: Usage,
        /// The failure that ended the call.
        source: Box<ModelError>,
    },
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
            Self::RateLimited { .. }
                | Self::Server { .. }
                | Self::Transport
                | Self::Empty
                | Self::StreamEnded { .. }
                | Self::ProviderStream { .. }
        )
    }

    /// Returns the delay the Provider asked for, when it named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Server { retry_after } => *retry_after,
            Self::Retried { source, .. } => source.retry_after(),
            _ => None,
        }
    }

    /// Returns the usage the Provider reported for a failed call.
    #[must_use]
    pub fn usage(&self) -> Option<Usage> {
        match self {
            Self::StreamEnded { usage }
            | Self::Truncated { usage }
            | Self::Filtered { usage }
            | Self::ProviderStream { usage, .. }
            | Self::Retried { usage, .. } => Some(*usage),
            _ => None,
        }
    }

    /// Returns the command-surface code for this failure.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized => ErrorCode::Unauthorized,
            Self::RateLimited { .. } => ErrorCode::RateLimited,
            Self::Server { .. }
            | Self::Transport
            | Self::StreamEnded { .. }
            | Self::ProviderStream { .. } => ErrorCode::Unavailable,
            Self::Retried { source, .. } => source.code(),
            Self::ContextTooLarge
            | Self::Empty
            | Self::Protocol
            | Self::Truncated { .. }
            | Self::Filtered { .. }
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
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::{
        ChatMessage, ChatRequest, ChatRole, ChatTool, ChatToolCall, ModelClient, ModelError,
        ProviderProtocol, ThinkingLevel, Usage,
    };

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
            provider: "test-provider",
            tools: Vec::new(),
            messages: vec![
                ChatMessage::new(ChatRole::System, "be brief"),
                ChatMessage::new(ChatRole::User, "hello"),
            ],
            max_output: 128,
            protocol: ProviderProtocol::ChatCompletions,
            thinking: None,
            continuation: None,
        }
    }

    type Recorded = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    fn recorder() -> Recorded {
        Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    fn record(recorded: &Recorded, body: &[u8]) {
        recorded
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
    }

    fn bodies(recorded: &Recorded) -> Vec<serde_json::Value> {
        recorded.lock().unwrap().clone()
    }

    fn sse(events: &[serde_json::Value]) -> String {
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        body
    }

    fn named_sse(events: &[(&str, serde_json::Value)]) -> String {
        let mut body = String::new();
        for (name, event) in events {
            body.push_str("event: ");
            body.push_str(name);
            body.push_str("\ndata: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        body
    }

    fn wire(protocol: ProviderProtocol) -> String {
        serde_json::to_value(protocol)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn event_stream(body: impl Into<String>) -> Response {
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body.into()))
            .unwrap()
    }

    fn responses_request(base_url: &str) -> ChatRequest<'static> {
        let mut request = request(base_url, "sk-test");
        request.protocol = ProviderProtocol::OpenAiResponses;
        request.thinking = Some(ThinkingLevel::High);
        request.max_output = 256;
        request.tools = vec![ChatTool {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
            }),
        }];
        request
    }

    fn anthropic_request(base_url: &str) -> ChatRequest<'static> {
        let mut request = request(base_url, "sk-test");
        request.protocol = ProviderProtocol::AnthropicMessages;
        request.max_output = 2000;
        request.thinking = Some(ThinkingLevel::Low);
        request.messages = vec![
            ChatMessage::new(ChatRole::System, "be brief"),
            ChatMessage::new(ChatRole::System, "and kind"),
            ChatMessage::new(ChatRole::User, "hello"),
        ];
        request.tools = vec![ChatTool {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        request
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
    async fn streams_a_tool_call_and_offers_tools() {
        use axum::body::Bytes;

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"id\":\"call_1\",\"type\":\"function\",\"function\":",
            "{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: Bytes| async move {
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["tools"][0]["type"], "function");
                assert_eq!(body["tools"][0]["function"]["name"], "read_file");
                assert_eq!(
                    body["messages"][0]["role"], "system",
                    "the adapter sends roles"
                );
                assert!(
                    body["messages"][0].get("tool_calls").is_none(),
                    "a plain message must not carry an empty tool_calls field"
                );
                assert!(body["messages"][1].get("tool_calls").is_none());
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();
        let mut request = request(&base, "sk-test");
        request.tools = vec![super::ChatTool {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let reply = client.chat(request).await.unwrap();

        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_1");
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
        assert!(reply.text.is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn parses_two_tool_calls_from_one_chunk() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[\
            {\"index\":0,\"id\":\"a\",\"type\":\"function\",\"function\":\
            {\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}},\
            {\"index\":1,\"id\":\"b\",\"type\":\"function\",\"function\":\
            {\"name\":\"list_files\",\"arguments\":\"{\\\"path\\\":\\\".\\\"}\"}}]}}]}\n\n\
            data: [DONE]\n\n";
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(request(&base, "sk-test")).await.unwrap();

        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].id, "a");
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[1].id, "b");
        assert_eq!(reply.tool_calls[1].name, "list_files");
        assert_eq!(reply.tool_calls[1].arguments, "{\"path\":\".\"}");
        task.abort();
    }

    #[tokio::test]
    async fn a_stream_that_ends_early_is_retried_and_never_released() {
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
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let error = client.chat(request(&base, "sk-test")).await;

        assert!(matches!(error, Err(ModelError::StreamEnded { .. })));
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
        task.abort();
    }

    #[tokio::test]
    async fn a_length_finish_releases_no_tool_call() {
        let chunk = serde_json::json!({"choices":[{
            "delta": {"tool_calls":[{
                "index": 0,
                "id": "call-1",
                "type": "function",
                "function": {
                    "name": "write_file",
                    "arguments": "{\"path\":\"a.txt\",\"content\":\"a\"}",
                },
            }]},
            "finish_reason": "length",
        }]});
        let usage = serde_json::json!({"choices":[],"usage":{
            "prompt_tokens": 40,
            "completion_tokens": 12,
        }});
        let sse = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let sse = sse.clone();
                async move {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(sse))
                        .unwrap()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let error = client.chat(request(&base, "sk-test")).await;

        match error {
            Err(ModelError::Truncated { usage }) => {
                assert_eq!(usage.budgeted(), 52);
            }
            other => panic!("expected a truncated stream, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn a_retry_keeps_the_usage_of_every_attempt() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    let attempt = seen.fetch_add(1, Ordering::SeqCst);
                    let body = if attempt == 0 {
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":40,\"completion_tokens\":12}}\n\n",
                        )
                    } else {
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"}}]}\n\n",
                            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2}}\n\n",
                            "data: [DONE]\n\n",
                        )
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

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(reply.text, "complete");
        assert_eq!(reply.usage.uncached_input, 48);
        assert_eq!(reply.usage.output, 14);
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

    #[tokio::test]
    async fn openai_responses_streams_text_and_tool_calls_with_reasoning() {
        use axum::body::Bytes;
        use axum::extract::OriginalUri;

        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let stream = sse(&[
            json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "rs_1", "summary": []},
            }),
            json!({"type": "response.output_text.delta", "delta": "Done"}),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"path\":\"a.txt\"}",
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"a.txt\"}",
                },
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {"type": "reasoning", "id": "rs_1", "summary": []},
                        {
                            "type": "message",
                            "id": "msg_1",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "Done"}],
                        },
                        {
                            "type": "function_call",
                            "id": "fc_1",
                            "call_id": "call_1",
                            "name": "read_file",
                            "arguments": "{\"path\":\"a.txt\"}",
                        },
                    ],
                    "usage": {
                        "input_tokens": 10,
                        "input_tokens_details": {"cached_tokens": 4, "cache_write_tokens": 2},
                        "output_tokens": 2,
                        "output_tokens_details": {"reasoning_tokens": 1},
                    },
                },
            }),
        ]);
        let app = Router::new().route(
            "/v1/responses",
            post(move |uri: OriginalUri, headers: HeaderMap, body: Bytes| {
                let seen = Arc::clone(&seen);
                let stream = stream.clone();
                async move {
                    assert_eq!(uri.path(), "/v1/responses");
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer sk-test"
                    );
                    record(&seen, &body);
                    event_stream(stream)
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(responses_request(&base)).await.unwrap();

        assert_eq!(reply.text, "Done");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_1");
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
        assert_eq!(
            reply.usage,
            Usage {
                uncached_input: 6,
                cache_read: 4,
                cache_write: 2,
                output: 2,
                reasoning: 1,
            }
        );

        let body = bodies(&recorded).pop().unwrap();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"].as_u64(), Some(256));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["instructions"], "be brief");
        assert!(body.get("messages").is_none());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["description"], "Read a file.");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");

        let continuation = reply
            .continuation
            .expect("a tool call keeps the native state");
        assert_eq!(
            continuation.protocol,
            wire(ProviderProtocol::OpenAiResponses)
        );
        assert_eq!(continuation.provider, "test-provider");
        assert_eq!(continuation.model, "test-model");
        let items = continuation.state["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        task.abort();
    }

    #[tokio::test]
    async fn openai_responses_continuation_replays_the_native_assistant_items() {
        use axum::body::Bytes;

        let native = sse(&[
            json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "rs_1", "summary": []},
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"a.txt\"}",
                },
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {"type": "reasoning", "id": "rs_1", "summary": []},
                        {
                            "type": "function_call",
                            "id": "fc_1",
                            "call_id": "call_1",
                            "name": "read_file",
                            "arguments": "{\"path\":\"a.txt\"}",
                        },
                    ],
                    "usage": {"input_tokens": 8, "output_tokens": 3},
                },
            }),
        ]);
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let native = native.clone();
                async move { event_stream(native) }
            }),
        );
        let (first_base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let first = client.chat(responses_request(&first_base)).await.unwrap();
        assert!(first.text.is_empty());
        assert_eq!(first.tool_calls.len(), 1);
        let continuation = first
            .continuation
            .expect("a tool call keeps the native state");
        task.abort();

        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let second = sse(&[
            json!({"type": "response.output_text.delta", "delta": "ok"}),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {"input_tokens": 5, "output_tokens": 1}},
            }),
        ]);
        let app = Router::new().route(
            "/v1/responses",
            post(move |body: Bytes| {
                let seen = Arc::clone(&seen);
                let second = second.clone();
                async move {
                    record(&seen, &body);
                    event_stream(second)
                }
            }),
        );
        let (base, task) = provider(app).await;
        let mut request = responses_request(&base);
        request.messages = vec![
            ChatMessage::new(ChatRole::System, "be brief"),
            ChatMessage::new(ChatRole::User, "hello"),
            ChatMessage::assistant_calls(
                Some("Calling read_file.".to_owned()),
                vec![ChatToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.txt\"}".to_owned(),
                }],
            ),
            ChatMessage::tool_result("call_1", "file contents"),
        ];
        request.continuation = Some(continuation);

        let reply = client.chat(request).await.unwrap();

        assert_eq!(reply.text, "ok");
        let body = bodies(&recorded).pop().unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            4,
            "the native items replace the generated assistant message"
        );
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "file contents");
        assert!(
            input.iter().all(|item| item["role"] != "assistant"),
            "no generated assistant message survives: {input:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn anthropic_messages_streams_thinking_text_and_tool_calls() {
        use axum::body::Bytes;

        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let stream = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "test-model",
                    "usage": {
                        "input_tokens": 10,
                        "cache_read_input_tokens": 4,
                        "cache_creation_input_tokens": 2,
                    },
                }}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "Let me think."},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-abc"},
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {"type": "text", "text": ""},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {"type": "text_delta", "text": "Hi"},
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 2,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read_file",
                        "input": {},
                    },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"path\":\"a.txt\"}",
                    },
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 2}),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {
                        "output_tokens": 15,
                        "output_tokens_details": {"thinking_tokens": 5},
                    },
                }),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: Bytes| {
                let seen = Arc::clone(&seen);
                let stream = stream.clone();
                async move {
                    assert_eq!(
                        headers.get("x-api-key").unwrap().to_str().unwrap(),
                        "sk-test"
                    );
                    assert_eq!(
                        headers.get("anthropic-version").unwrap().to_str().unwrap(),
                        "2023-06-01"
                    );
                    assert!(
                        headers.get("authorization").is_none(),
                        "Anthropic authenticates with x-api-key"
                    );
                    record(&seen, &body);
                    event_stream(stream)
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(anthropic_request(&base)).await.unwrap();

        assert_eq!(reply.text, "Hi");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "toolu_1");
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
        assert_eq!(
            reply.usage,
            Usage {
                uncached_input: 10,
                cache_read: 4,
                cache_write: 2,
                output: 15,
                reasoning: 5,
            }
        );

        let body = bodies(&recorded).pop().unwrap();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"].as_u64(), Some(2000));
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "be brief\n\nand kind");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"].as_u64(), Some(1999));
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["description"], "Read a file.");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "hello");

        let continuation = reply
            .continuation
            .expect("a tool call keeps the native state");
        assert_eq!(
            continuation.protocol,
            wire(ProviderProtocol::AnthropicMessages)
        );
        assert_eq!(continuation.provider, "test-provider");
        assert_eq!(continuation.model, "test-model");
        let blocks = continuation.state["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "Let me think.");
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "Hi");
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[2]["id"], "toolu_1");
        assert_eq!(blocks[2]["input"]["path"], "a.txt");
        task.abort();
    }

    #[tokio::test]
    async fn anthropic_continuation_replays_the_native_thinking_blocks() {
        use axum::body::Bytes;

        let native = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "usage": {"input_tokens": 10},
                }}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "Let me think."},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-abc"},
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read_file",
                        "input": {},
                    },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"path\":\"a.txt\"}",
                    },
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {
                        "output_tokens": 15,
                        "output_tokens_details": {"thinking_tokens": 5},
                    },
                }),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let native = native.clone();
                async move { event_stream(native) }
            }),
        );
        let (first_base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let first = client.chat(anthropic_request(&first_base)).await.unwrap();
        assert!(first.text.is_empty());
        assert_eq!(first.tool_calls.len(), 1);
        let continuation = first
            .continuation
            .expect("a tool call keeps the native state");
        task.abort();

        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let second = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_2",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "usage": {"input_tokens": 5},
                }}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "done"},
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 2},
                }),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move |body: Bytes| {
                let seen = Arc::clone(&seen);
                let second = second.clone();
                async move {
                    record(&seen, &body);
                    event_stream(second)
                }
            }),
        );
        let (base, task) = provider(app).await;
        let mut request = anthropic_request(&base);
        request.messages = vec![
            ChatMessage::new(ChatRole::System, "be brief"),
            ChatMessage::new(ChatRole::User, "hello"),
            ChatMessage::assistant_calls(
                Some("Thinking about it.".to_owned()),
                vec![ChatToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.txt\"}".to_owned(),
                }],
            ),
            ChatMessage::tool_result("toolu_1", "file contents"),
        ];
        request.continuation = Some(continuation);

        let reply = client.chat(request).await.unwrap();

        assert_eq!(reply.text, "done");
        let body = bodies(&recorded).pop().unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(
            content.len(),
            2,
            "the native blocks replace the generated assistant content"
        );
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "Let me think.");
        assert_eq!(content[0]["signature"], "sig-abc");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "toolu_1");
        assert_eq!(content[1]["input"]["path"], "a.txt");
        assert!(
            content
                .iter()
                .all(|block| block["text"] != "Thinking about it."),
            "the generated assistant text is not replayed: {content:?}"
        );
        let tool_results = messages[2]["content"].as_array().unwrap();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0]["type"], "tool_result");
        assert_eq!(tool_results[0]["tool_use_id"], "toolu_1");
        assert_eq!(tool_results[0]["content"], "file contents");
        task.abort();
    }

    #[tokio::test]
    async fn anthropic_keeps_redacted_thinking_blocks() {
        use axum::body::Bytes;

        let native = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "usage": {"input_tokens": 10},
                }}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "redacted_thinking", "data": "opaque"},
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read_file",
                        "input": {"path": "a.txt"},
                    },
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 2},
                }),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let native = native.clone();
                async move { event_stream(native) }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();
        let request = anthropic_request(&base);
        let reply = client.chat(request).await.unwrap();
        let continuation = reply.continuation.expect("a tool call keeps native state");
        assert_eq!(continuation.state["blocks"][0]["type"], "redacted_thinking");
        assert_eq!(continuation.state["blocks"][0]["data"], "opaque");
        task.abort();

        // The same request without its native turn must not ask Anthropic for
        // thinking, which would be rejected without the signed block.
        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let text = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_2", "type": "message", "role": "assistant",
                    "content": [], "usage": {"input_tokens": 1},
                }}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "done"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 1}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move |body: Bytes| {
                let seen = Arc::clone(&seen);
                let text = text.clone();
                async move {
                    record(&seen, &body);
                    event_stream(text)
                }
            }),
        );
        let (base, task) = provider(app).await;
        let mut request = anthropic_request(&base);
        request.messages = vec![
            ChatMessage::new(ChatRole::System, "be brief"),
            ChatMessage::new(ChatRole::User, "hello"),
            ChatMessage::assistant_calls(
                None,
                vec![ChatToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.txt\"}".to_owned(),
                }],
            ),
            ChatMessage::tool_result("toolu_1", "file contents"),
        ];
        request.thinking = Some(ThinkingLevel::Low);
        request.continuation = None;
        client.chat(request).await.unwrap();
        let body = bodies(&recorded).pop().unwrap();
        assert!(
            body.get("thinking").is_none(),
            "a tool round without its native turn must not ask for thinking: {body}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_failed_responses_call_is_retried_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let stream = sse(&[
            json!({"type": "response.output_text.delta", "delta": "retried"}),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {"input_tokens": 1, "output_tokens": 1}},
            }),
        ]);
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let seen = Arc::clone(&seen);
                let stream = stream.clone();
                async move {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (StatusCode::INTERNAL_SERVER_ERROR, "later").into_response();
                    }
                    event_stream(stream).into_response()
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();

        let reply = client.chat(responses_request(&base)).await.unwrap();

        assert_eq!(reply.text, "retried");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn an_anthropic_stream_error_is_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let error = named_sse(&[(
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
        )]);
        let ok = named_sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_2", "type": "message", "role": "assistant",
                    "content": [], "usage": {"input_tokens": 1},
                }}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "retried"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 1}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let seen = Arc::clone(&seen);
                let error = error.clone();
                let ok = ok.clone();
                async move {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        event_stream(error)
                    } else {
                        event_stream(ok)
                    }
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();
        let reply = client.chat(anthropic_request(&base)).await.unwrap();
        assert_eq!(reply.text, "retried");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn chat_completions_sends_reasoning_effort_only_when_thinking() {
        use axum::body::Bytes;

        let recorded = recorder();
        let seen = Arc::clone(&recorded);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    record(&seen, &body);
                    event_stream(concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                        "data: [DONE]\n\n",
                    ))
                }
            }),
        );
        let (base, task) = provider(app).await;
        let client = ModelClient::fast_retry().unwrap();
        let levels = [
            (ThinkingLevel::Minimal, "minimal"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Max, "max"),
        ];
        for (level, _) in levels {
            let mut request = request(&base, "sk-test");
            request.thinking = Some(level);
            client.chat(request).await.unwrap();
        }
        let mut plain = request(&base, "sk-test");
        plain.thinking = None;
        client.chat(plain).await.unwrap();

        let bodies = bodies(&recorded);
        assert_eq!(bodies.len(), 6);
        for (index, (_, wire)) in levels.iter().enumerate() {
            assert_eq!(bodies[index]["reasoning_effort"], *wire);
        }
        assert!(
            bodies[5].get("reasoning_effort").is_none(),
            "thinking None sends no reasoning_effort"
        );
        task.abort();
    }
}
