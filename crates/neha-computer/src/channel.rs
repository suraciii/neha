//! The wire protocol between a Space and a real Computer.
//!
//! One frame is one JSON object. Both ends speak [`CHANNEL_VERSION`] and
//! refuse a frame larger than [`MAX_CHANNEL_FRAME_BYTES`]. A field that
//! carries bytes holds them as base64 in the URL-safe alphabet without
//! padding (RFC 4648 §5), so a frame stays plain JSON text.
//!
//! The Space sends [`HostFrame`] and the Computer sends [`DeviceFrame`]. An
//! action is a [`WireAction`], the wire spelling of [`Action`]; [`HostFrame`]
//! never carries the domain type, and [`ToolOutput`] and [`ActionStatus`] are
//! shared verbatim.

use serde::{Deserialize, Serialize};

use crate::action::{Action, ActionStatus, ToolOutput};

/// The protocol version both ends speak.
pub const CHANNEL_VERSION: u32 = 1;

/// The largest frame either end accepts, 64 MiB.
pub const MAX_CHANNEL_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The file access a Bot has on its Computer.
///
/// `write_roots` are extra folders the owner granted; the Bot's own folder is
/// always writable. A full-access Bot runs without the write fence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessFrame {
    /// Writes anything the machine user can write; the shell is not fenced.
    #[serde(default)]
    pub full_access: bool,
    /// Extra folders the owner granted for writing, as absolute paths.
    #[serde(default)]
    pub write_roots: Vec<String>,
}

impl AccessFrame {
    fn is_default(&self) -> bool {
        !self.full_access && self.write_roots.is_empty()
    }
}

/// A frame the Space sends to a Computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// Runs one tool action in the Bot's folder.
    Action {
        /// The id the Space assigned this request.
        id: u64,
        /// The Bot whose folder the action uses.
        bot: String,
        /// The action to run.
        action: WireAction,
        /// The directory this run works in; absent means the Bot's folder.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// The owner-granted write access this run carries.
        #[serde(default, skip_serializing_if = "AccessFrame::is_default")]
        access: AccessFrame,
        /// Environment entries the run adds to shell commands. A Space issues
        /// its short-lived Bot token here.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<(String, String)>,
    },
    /// Reads a file's bytes from the machine (a File take).
    Read {
        /// The id the Space assigned this request.
        id: u64,
        /// The Bot whose folder the path uses.
        bot: String,
        /// The path to read.
        path: String,
        /// The directory this run works in; absent means the Bot's folder.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
    },
    /// Writes a file's bytes on the machine (a File delivery).
    Write {
        /// The id the Space assigned this request.
        id: u64,
        /// The Bot whose folder the path uses.
        bot: String,
        /// The path to write.
        path: String,
        /// The bytes, as URL-safe base64 without padding.
        bytes: String,
    },
    /// Keeps the connection alive.
    Ping,
}

/// A frame a Computer sends to the Space.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceFrame {
    /// Opens the channel and names this Computer and its workspace.
    Hello {
        /// The protocol version this Computer speaks.
        version: u32,
        /// The Computer this connection serves.
        computer: String,
        /// The machine's workspace directory.
        workspace: String,
        /// Whether commands run under a sandbox.
        sandbox: bool,
    },
    /// Returns the result of an action.
    Result {
        /// The id of the request this answers.
        id: u64,
        /// How the action ended.
        status: ActionStatus,
        /// What the action produced.
        output: ToolOutput,
    },
    /// Returns a file's bytes (a File take response).
    Bytes {
        /// The id of the request this answers.
        id: u64,
        /// The bytes, as URL-safe base64 without padding.
        bytes: String,
    },
    /// Refuses a request that produced no result.
    Failed {
        /// The id of the request this answers.
        id: u64,
        /// Why the request failed.
        message: String,
    },
    /// Answers [`HostFrame::Ping`].
    Pong,
}

/// An action's wire spelling, one-to-one with [`Action`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireAction {
    /// Runs a POSIX shell command in the Bot's folder.
    Shell {
        /// The command line.
        command: String,
    },
    /// Reads a file.
    ReadFile {
        /// The path, relative to the Bot's folder or absolute.
        path: String,
    },
    /// Writes a file.
    WriteFile {
        /// The path, relative to the Bot's folder or absolute.
        path: String,
        /// The content.
        content: String,
    },
    /// Lists a directory.
    ListFiles {
        /// The path, relative to the Bot's folder or absolute.
        path: String,
    },
}

impl From<Action> for WireAction {
    fn from(action: Action) -> Self {
        match action {
            Action::Shell { command } => Self::Shell { command },
            Action::ReadFile { path } => Self::ReadFile { path },
            Action::WriteFile { path, content } => Self::WriteFile { path, content },
            Action::ListFiles { path } => Self::ListFiles { path },
        }
    }
}

impl From<WireAction> for Action {
    fn from(action: WireAction) -> Self {
        match action {
            WireAction::Shell { command } => Self::Shell { command },
            WireAction::ReadFile { path } => Self::ReadFile { path },
            WireAction::WriteFile { path, content } => Self::WriteFile { path, content },
            WireAction::ListFiles { path } => Self::ListFiles { path },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CHANNEL_VERSION, DeviceFrame, HostFrame, MAX_CHANNEL_FRAME_BYTES, WireAction};
    use crate::action::{Action, ActionResult, ActionStatus, ToolOutput};

    fn output() -> ToolOutput {
        ToolOutput::full("done")
    }

    #[test]
    fn both_ends_are_pinned_to_version_one_and_a_64_mib_frame() {
        assert_eq!(CHANNEL_VERSION, 1);
        assert_eq!(MAX_CHANNEL_FRAME_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn every_host_frame_round_trips_as_its_pinned_json() {
        let cases = [
            (
                HostFrame::Action {
                    id: 7,
                    bot: "bot-1".to_owned(),
                    action: WireAction::Shell {
                        command: "pwd".to_owned(),
                    },
                    working_dir: None,
                    access: super::AccessFrame::default(),
                    env: Vec::new(),
                },
                json!({
                    "type": "action",
                    "id": 7,
                    "bot": "bot-1",
                    "action": { "kind": "shell", "command": "pwd" },
                }),
            ),
            (
                HostFrame::Action {
                    id: 8,
                    bot: "bot-1".to_owned(),
                    action: WireAction::ReadFile {
                        path: "notes.txt".to_owned(),
                    },
                    working_dir: None,
                    access: super::AccessFrame::default(),
                    env: Vec::new(),
                },
                json!({
                    "type": "action",
                    "id": 8,
                    "bot": "bot-1",
                    "action": { "kind": "read_file", "path": "notes.txt" },
                }),
            ),
            (
                HostFrame::Action {
                    id: 9,
                    bot: "bot-1".to_owned(),
                    action: WireAction::WriteFile {
                        path: "notes.txt".to_owned(),
                        content: "hello".to_owned(),
                    },
                    working_dir: None,
                    access: super::AccessFrame::default(),
                    env: Vec::new(),
                },
                json!({
                    "type": "action",
                    "id": 9,
                    "bot": "bot-1",
                    "action": { "kind": "write_file", "path": "notes.txt", "content": "hello" },
                }),
            ),
            (
                HostFrame::Action {
                    id: 10,
                    bot: "bot-1".to_owned(),
                    action: WireAction::ListFiles {
                        path: ".".to_owned(),
                    },
                    working_dir: None,
                    access: super::AccessFrame::default(),
                    env: Vec::new(),
                },
                json!({
                    "type": "action",
                    "id": 10,
                    "bot": "bot-1",
                    "action": { "kind": "list_files", "path": "." },
                }),
            ),
            (
                HostFrame::Read {
                    id: 11,
                    bot: "bot-1".to_owned(),
                    path: "/home/me/report.pdf".to_owned(),
                    working_dir: None,
                },
                json!({
                    "type": "read",
                    "id": 11,
                    "bot": "bot-1",
                    "path": "/home/me/report.pdf",
                }),
            ),
            (
                HostFrame::Write {
                    id: 12,
                    bot: "bot-1".to_owned(),
                    path: "out/report.bin".to_owned(),
                    bytes: "aGVsbG8".to_owned(),
                },
                json!({
                    "type": "write",
                    "id": 12,
                    "bot": "bot-1",
                    "path": "out/report.bin",
                    "bytes": "aGVsbG8",
                }),
            ),
            (HostFrame::Ping, json!({ "type": "ping" })),
        ];

        for (frame, expected) in cases {
            assert_eq!(serde_json::to_value(&frame).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<HostFrame>(expected.clone()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn every_device_frame_round_trips_as_its_pinned_json() {
        let cases = [
            (
                DeviceFrame::Hello {
                    version: 1,
                    computer: "computer-1".to_owned(),
                    workspace: "/home/me/.neha/workspace".to_owned(),
                    sandbox: true,
                },
                json!({
                    "type": "hello",
                    "version": 1,
                    "computer": "computer-1",
                    "workspace": "/home/me/.neha/workspace",
                    "sandbox": true,
                }),
            ),
            (
                DeviceFrame::Result {
                    id: 7,
                    status: ActionStatus::Succeeded,
                    output: output(),
                },
                json!({
                    "type": "result",
                    "id": 7,
                    "status": "succeeded",
                    "output": { "text": "done", "truncated": false, "omitted_bytes": 0 },
                }),
            ),
            (
                DeviceFrame::Bytes {
                    id: 8,
                    bytes: "AAEC/w".to_owned(),
                },
                json!({ "type": "bytes", "id": 8, "bytes": "AAEC/w" }),
            ),
            (
                DeviceFrame::Failed {
                    id: 9,
                    message: "the Computer went offline".to_owned(),
                },
                json!({
                    "type": "failed",
                    "id": 9,
                    "message": "the Computer went offline",
                }),
            ),
            (DeviceFrame::Pong, json!({ "type": "pong" })),
        ];

        for (frame, expected) in cases {
            assert_eq!(serde_json::to_value(&frame).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<DeviceFrame>(expected.clone()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn a_frame_with_an_unknown_type_or_kind_is_refused() {
        assert!(serde_json::from_value::<HostFrame>(json!({ "type": "reboot" })).is_err());
        assert!(
            serde_json::from_value::<WireAction>(
                json!({ "kind": "delete_everything", "path": "x" })
            )
            .is_err()
        );
    }

    #[test]
    fn wire_actions_convert_to_and_from_actions() {
        let actions = [
            Action::Shell {
                command: "pwd".to_owned(),
            },
            Action::ReadFile {
                path: "notes.txt".to_owned(),
            },
            Action::WriteFile {
                path: "notes.txt".to_owned(),
                content: "hello".to_owned(),
            },
            Action::ListFiles {
                path: ".".to_owned(),
            },
        ];

        for action in actions {
            let wire = WireAction::from(action.clone());
            assert_eq!(Action::from(wire), action);
        }
    }

    #[test]
    fn actions_and_results_round_trip_through_serde() {
        let action = Action::WriteFile {
            path: "notes.txt".to_owned(),
            content: "hello".to_owned(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);

        let result = ActionResult {
            status: ActionStatus::Succeeded,
            output: output(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::from_str::<ActionResult>(&json).unwrap(), result);
    }
}
