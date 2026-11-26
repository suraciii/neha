//! The actions a Computer can run and what they return.

use serde::{Deserialize, Serialize};

/// One action a Bot asks its Computer to run.
///
/// The channel's wire spelling is [`WireAction`](crate::channel::WireAction);
/// this type's own Serde shape is for tests and local transports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Runs a POSIX shell command in the workspace.
    Shell {
        /// The command line.
        command: String,
    },
    /// Reads a file in the workspace.
    ReadFile {
        /// The path, relative to the workspace or absolute inside it.
        path: String,
    },
    /// Writes a file in the workspace.
    WriteFile {
        /// The path, relative to the workspace or absolute inside it.
        path: String,
        /// The content.
        content: String,
    },
    /// Lists a directory in the workspace.
    ListFiles {
        /// The path, relative to the workspace or absolute inside it.
        path: String,
    },
}

/// How an action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// The action did what it was asked.
    Succeeded,
    /// The action returned an error.
    Failed,
    /// The action ran past its time limit and was killed.
    TimedOut,
    /// The action's result never returned.
    Unknown,
}

/// The text an action produced, with its truncation details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The text the Model reads.
    pub text: String,
    /// Whether bytes were dropped from the middle.
    pub truncated: bool,
    /// How many bytes were dropped.
    pub omitted_bytes: u64,
}

impl ToolOutput {
    /// Builds an output that was not truncated.
    #[must_use]
    pub fn full(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            truncated: false,
            omitted_bytes: 0,
        }
    }
}

/// What an action returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// How the action ended.
    pub status: ActionStatus,
    /// What the action printed.
    pub output: ToolOutput,
    /// The failure, when the action failed or timed out.
    pub error: Option<String>,
}

impl ActionResult {
    /// Builds a successful result.
    #[must_use]
    pub fn succeeded(output: ToolOutput) -> Self {
        Self {
            status: ActionStatus::Succeeded,
            output,
            error: None,
        }
    }

    /// Builds a failed result.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            status: ActionStatus::Failed,
            output: ToolOutput::full(message.clone()),
            error: Some(message),
        }
    }
}

/// The seam every Computer implementation provides.
pub trait ComputerRuntime {
    /// Runs one action.
    fn execute(&self, action: Action) -> impl std::future::Future<Output = ActionResult> + Send;

    /// Reads a file's bytes from the workspace.
    fn read_bytes(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send;

    /// Writes bytes to a file in the workspace, creating its parent directories.
    fn write_bytes(
        &self,
        path: &str,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}
