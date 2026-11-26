//! The command surface's wire vocabulary.
//!
//! A client opens one WebSocket, sends `hello`, then commands and
//! subscriptions. Each command is accepted, committed to the record, and
//! confirmed with its result. Events follow the cursor of the record.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::id::Id;
use crate::record::{Author, Event};
use crate::seq::Seq;
use crate::time::Timestamp;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 1;

/// The largest frame a client or the Space accepts.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A frame a client sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Opens the session and states the protocol version.
    Hello {
        /// The protocol version the client speaks.
        version: u16,
        /// Optional capabilities the client understands.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// Runs one command.
    Command {
        /// The client's identifier for the command.
        id: Id,
        /// The command.
        command: CommandCall,
    },
    /// Reads the record after a cursor.
    Subscribe {
        /// The client's identifier for the subscription.
        id: Id,
        /// The cursor to resume from; the beginning when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<Seq>,
        /// The page size the client asks for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// A command and its parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "args", rename_all = "snake_case")]
pub enum CommandCall {
    /// Connects a Provider to the Space.
    ProviderAdd(ProviderAdd),
    /// Lists the Space's Providers.
    ProviderList,
    /// Lists the Space's Models.
    ModelList,
    /// Adds a real Computer the User just paired.
    ComputerAdd {
        /// The pairing code the machine showed.
        code: String,
    },
    /// Lists the caller's Computers.
    ComputerList,
    /// Creates a Bot with its Computer and Chat.
    BotCreate(BotCreate),
    /// Lists the caller's Bots.
    BotList,
    /// Shows one Bot.
    BotShow {
        /// The Bot.
        bot: Id,
    },
    /// Lists the caller's Chats.
    ChatList,
    /// Lists a Chat's Files.
    FileList {
        /// The Chat.
        chat: Id,
    },
    /// Reads a Chat's Messages.
    ChatRead {
        /// The Chat.
        chat: Id,
        /// Starts after this cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Seq>,
    },
    /// Sends a Message to a Chat.
    MessageSend(MessageSend),
    /// Shows one run.
    RunShow {
        /// The run.
        run: Id,
    },
}

/// The parameters of `provider_add`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderAdd {
    /// The provider protocol spoken.
    pub protocol: ProviderProtocol,
    /// The base address of the provider.
    pub base_url: String,
    /// The API key, which the Space stores encrypted.
    pub api_key: String,
    /// The Models this Provider offers.
    pub models: Vec<ModelSpec>,
    /// The Model the Space uses by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// A provider protocol the Space can speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    /// OpenAI Chat Completions.
    ChatCompletions,
}

/// A Model as the Admin declares it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// The model name the provider expects.
    pub name: String,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// Whether the Model reads images.
    pub reads_images: bool,
}

/// The parameters of `bot_create`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BotCreate {
    /// The Bot's name.
    pub name: String,
    /// The real Computer the Bot works on; a managed Computer when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<Id>,
}

/// The parameters of `message_send`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSend {
    /// The Chat to post in.
    pub chat: Id,
    /// The Message text, empty only when the Message carries a File.
    pub text: String,
    /// The File the Message carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Id>,
}

/// A frame the Space sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Answers `hello`.
    Hello {
        /// The protocol version the Space speaks.
        version: u16,
        /// Capabilities the Space offers.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// The Space accepted a command and will run it.
    Accepted {
        /// The client's command identifier.
        id: Id,
    },
    /// The record holds the command's events.
    Committed {
        /// The client's command identifier.
        id: Id,
        /// The cursor of the command's last event.
        seq: Seq,
    },
    /// The command's work is done.
    Confirmed {
        /// The client's command identifier.
        id: Id,
        /// The command's result.
        result: serde_json::Value,
    },
    /// A command failed.
    Error {
        /// The client's command identifier, when the failure has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        /// The stable error code.
        code: ErrorCode,
        /// A message for the caller.
        message: String,
    },
    /// The record advanced.
    Event {
        /// The event.
        event: Event,
    },
    /// A page of the record for a subscription.
    Snapshot {
        /// The cursor after the page.
        cursor: Seq,
        /// The events in order.
        events: Vec<Event>,
    },
    /// The connection is alive; the cursor only moves.
    Heartbeat {
        /// The cursor the Space has reached.
        cursor: Seq,
    },
    /// The client's request was not understood.
    ProtocolError {
        /// What the client should do differently.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        BotCreate, ClientFrame, CommandCall, MessageSend, ModelSpec, PROTOCOL_VERSION, ProviderAdd,
        ProviderProtocol, ServerFrame,
    };
    use crate::error::ErrorCode;
    use crate::id::Id;
    use crate::record::{Author, EventKind};
    use crate::seq::Seq;
    use crate::time::Timestamp;

    #[test]
    fn a_command_frame_round_trips() {
        let frame = ClientFrame::Command {
            id: Id::new(),
            command: CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,
            }),
        };

        let text = serde_json::to_string(&frame).unwrap();
        assert_eq!(serde_json::from_str::<ClientFrame>(&text).unwrap(), frame);
    }

    #[test]
    fn computer_commands_round_trip() {
        let add = CommandCall::ComputerAdd {
            code: "ABCD2345".to_owned(),
        };
        let text = serde_json::to_string(&add).unwrap();
        assert!(text.contains("\"name\":\"computer_add\""));
        assert!(text.contains("\"code\":\"ABCD2345\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), add);

        let list = CommandCall::ComputerList;
        let text = serde_json::to_string(&list).unwrap();
        assert_eq!(text, "{\"name\":\"computer_list\"}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), list);
    }

    #[test]
    fn bot_create_keeps_its_old_shape_without_a_computer() {
        let command = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: None,
        });
        let text = serde_json::to_string(&command).unwrap();
        assert!(
            !text.contains("\"computer\""),
            "an absent Computer is omitted"
        );

        let old: CommandCall =
            serde_json::from_str("{\"name\":\"bot_create\",\"args\":{\"name\":\"Scout\"}}")
                .unwrap();
        assert_eq!(old, command);
    }

    #[test]
    fn bot_create_carries_a_real_computer() {
        let computer = Id::new();
        let command = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: Some(computer),
        });
        let text = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn provider_add_carries_its_models() {
        let command = CommandCall::ProviderAdd(ProviderAdd {
            protocol: ProviderProtocol::ChatCompletions,
            base_url: "http://127.0.0.1:9000/v1".to_owned(),
            api_key: "secret".to_owned(),
            models: vec![ModelSpec {
                name: "test-model".to_owned(),
                context_limit: 128_000,
                max_output: 8_192,
                reads_images: false,
            }],
            default_model: Some("test-model".to_owned()),
        });

        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"name\":\"provider_add\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn a_message_send_round_trips() {
        let chat = Id::new();
        let plain = CommandCall::MessageSend(MessageSend {
            chat,
            text: "hello".to_owned(),
            file: None,
        });
        let text = serde_json::to_string(&plain).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), plain);
        assert!(
            !text.contains("\"file\""),
            "a Message without a File omits it"
        );

        let file = CommandCall::MessageSend(MessageSend {
            chat,
            text: String::new(),
            file: Some(Id::new()),
        });
        let text = serde_json::to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), file);
    }

    #[test]
    fn frames_are_tagged_by_type() {
        let frame = ServerFrame::Hello {
            version: PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            "{\"type\":\"hello\",\"version\":1}"
        );

        let error = ServerFrame::Error {
            id: None,
            code: ErrorCode::Unauthorized,
            message: "sign in".to_owned(),
        };
        let text = serde_json::to_string(&error).unwrap();
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"unauthorized\""));
    }

    #[test]
    fn an_event_frame_carries_a_record_event() {
        let event = crate::record::Event {
            seq: Seq::from_i64(7).unwrap(),
            kind: EventKind::MessageAdded,
            version: 1,
            at: Timestamp::from_microseconds(1).unwrap(),
            author: Author::User(Id::new()),
            acting: None,
            command: Some(Id::new()),
            data: serde_json::json!({"text": "hi"}),
        };
        let frame = ServerFrame::Event {
            event: event.clone(),
        };
        let text = serde_json::to_string(&frame).unwrap();

        assert!(text.contains("\"type\":\"event\""));
        assert!(text.contains("\"kind\":\"neha.message.added\""));
    }
}

/// One File as a client reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileSummary {
    /// The File.
    pub id: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Its name.
    pub name: String,
    /// Its media type.
    pub media_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its lowercase hex SHA-256.
    pub sha256: String,
    /// Who sent it.
    pub author: Author,
    /// The User a Bot acted for, when a Bot sent it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acting: Option<Id>,
    /// When the record holds it.
    pub at: Timestamp,
    /// The cursor of the event that recorded it.
    pub seq: Seq,
}
