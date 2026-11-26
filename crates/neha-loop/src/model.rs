//! The provider-neutral Model vocabulary the loop speaks.

use neha::error::ErrorCode;
use neha::usage::Usage;
use serde::{Deserialize, Serialize};

/// The role of a Message sent to a Model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The Space's instructions.
    System,
    /// A person's Message.
    User,
    /// A Bot's Message.
    Assistant,
    /// A tool's result.
    Tool,
}

/// A tool call a Model requested.
///
/// The arguments stay the raw JSON string the Model produced until the
/// Computer validates them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The identifier the Model gave the call.
    pub id: String,
    /// The tool's name.
    pub name: String,
    /// The raw JSON arguments.
    pub arguments: String,
}

/// One Message in a Model request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// The role.
    pub role: Role,
    /// The text.
    pub content: Option<String>,
    /// The tool calls an assistant Message requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a tool Message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Builds a Message of a role.
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Builds a system Message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// Builds a User Message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// Builds an assistant Message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    /// Builds an assistant Message that requested tools.
    #[must_use]
    pub fn assistant_with_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Builds a tool result Message.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool the Model may call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool's name.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// Its JSON schema.
    pub parameters: serde_json::Value,
}

/// One request to a Model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    /// The Messages, oldest first.
    pub messages: Vec<Message>,
    /// The tools the Model may call.
    pub tools: Vec<ToolSpec>,
    /// The largest reply in tokens.
    pub max_output: u32,
}

/// One reply from a Model.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelReply {
    /// The text, when the Model wrote one.
    pub content: Option<String>,
    /// The tools the Model requested.
    pub tool_calls: Vec<ToolCall>,
    /// The usage the Provider reported.
    pub usage: Usage,
}

/// A Model call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFailure {
    /// The stable error code.
    pub code: ErrorCode,
    /// A message for the Bot to repeat.
    pub detail: String,
}

/// The Model seam the loop calls.
pub trait Model {
    /// Runs one Model call.
    fn complete(
        &self,
        request: ModelRequest,
    ) -> impl std::future::Future<Output = Result<ModelReply, ModelFailure>> + Send;
}
