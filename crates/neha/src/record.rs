//! Record events.
//!
//! Every change to a Space enters the record as an event. An event carries a
//! global sequence number, the actor that wrote it, and the data of the kind
//! it names.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::event::EventClass;
use crate::id::Id;
use crate::seq::Seq;
use crate::time::Timestamp;
use crate::usage::Usage;

/// Who wrote an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Author {
    /// A User.
    User(Id),
    /// A Bot.
    Bot(Id),
}

/// The kinds of event the record carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// A User joined the Space.
    #[serde(rename = "neha.user.joined")]
    UserJoined,
    /// The Admin connected a Provider.
    #[serde(rename = "neha.provider.added")]
    ProviderAdded,
    /// A Bot was created.
    #[serde(rename = "neha.bot.created")]
    BotCreated,
    /// A Computer was created.
    #[serde(rename = "neha.computer.created")]
    ComputerCreated,
    /// A Chat was created.
    #[serde(rename = "neha.chat.created")]
    ChatCreated,
    /// A Message was added to a Chat or Group.
    #[serde(rename = "neha.message.added")]
    MessageAdded,
    /// A run started.
    #[serde(rename = "neha.run.started")]
    RunStarted,
    /// A run finished.
    #[serde(rename = "neha.run.finished")]
    RunFinished,
}

impl EventKind {
    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserJoined => "neha.user.joined",
            Self::ProviderAdded => "neha.provider.added",
            Self::BotCreated => "neha.bot.created",
            Self::ComputerCreated => "neha.computer.created",
            Self::ChatCreated => "neha.chat.created",
            Self::MessageAdded => "neha.message.added",
            Self::RunStarted => "neha.run.started",
            Self::RunFinished => "neha.run.finished",
        }
    }

    /// Returns how a reader that does not know this kind must treat it.
    #[must_use]
    pub const fn class(self) -> EventClass {
        match self {
            Self::MessageAdded | Self::RunStarted | Self::RunFinished => EventClass::Presentation,
            Self::UserJoined
            | Self::ProviderAdded
            | Self::BotCreated
            | Self::ComputerCreated
            | Self::ChatCreated => EventClass::Authority,
        }
    }
}

impl FromStr for EventKind {
    type Err = UnknownEventKind;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "neha.user.joined" => Ok(Self::UserJoined),
            "neha.provider.added" => Ok(Self::ProviderAdded),
            "neha.bot.created" => Ok(Self::BotCreated),
            "neha.computer.created" => Ok(Self::ComputerCreated),
            "neha.chat.created" => Ok(Self::ChatCreated),
            "neha.message.added" => Ok(Self::MessageAdded),
            "neha.run.started" => Ok(Self::RunStarted),
            "neha.run.finished" => Ok(Self::RunFinished),
            _ => Err(UnknownEventKind(text.to_owned())),
        }
    }
}

/// An event name this version does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown event kind: {0}")]
pub struct UnknownEventKind(pub String);

/// An event before the record assigns its sequence number and time.
#[derive(Clone, Debug, PartialEq)]
pub struct EventDraft {
    /// The kind.
    pub kind: EventKind,
    /// The kind's version.
    pub version: u16,
    /// Who wrote it.
    pub author: Author,
    /// The User a Bot acts for, when the author is a Bot.
    pub acting: Option<Id>,
    /// The data of its kind.
    pub data: serde_json::Value,
}

impl EventDraft {
    /// Builds a draft of the first version of a kind.
    #[must_use]
    pub fn new(kind: EventKind, author: Author, data: serde_json::Value) -> Self {
        Self {
            kind,
            version: 1,
            author,
            acting: None,
            data,
        }
    }

    /// Records the User a Bot acts for.
    #[must_use]
    pub fn acting(mut self, user: Id) -> Self {
        self.acting = Some(user);
        self
    }
}

/// A recorded event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The cursor of the event.
    pub seq: Seq,
    /// The kind.
    pub kind: EventKind,
    /// The kind's version.
    pub version: u16,
    /// When the Space recorded it.
    pub at: Timestamp,
    /// Who wrote it.
    pub author: Author,
    /// The User a Bot acts for, when the author is a Bot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acting: Option<Id>,
    /// The command that wrote it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Id>,
    /// The data of its kind.
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::EventDraft;
    use super::EventKind;
    use crate::event::EventClass;

    #[test]
    fn wire_names_round_trip() {
        let kinds = [
            EventKind::UserJoined,
            EventKind::ProviderAdded,
            EventKind::BotCreated,
            EventKind::ComputerCreated,
            EventKind::ChatCreated,
            EventKind::MessageAdded,
            EventKind::RunStarted,
            EventKind::RunFinished,
        ];

        for kind in kinds {
            assert_eq!(kind.as_str().parse::<EventKind>().unwrap(), kind);
        }
    }

    #[test]
    fn message_and_run_events_are_presentation() {
        assert_eq!(EventKind::MessageAdded.class(), EventClass::Presentation);
        assert_eq!(EventKind::RunStarted.class(), EventClass::Presentation);
        assert_eq!(EventKind::BotCreated.class(), EventClass::Authority);
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        assert!("neha.future.thing".parse::<EventKind>().is_err());
    }

    #[test]
    fn draft_keeps_the_acting_user() {
        let user = crate::id::Id::new();
        let draft = EventDraft::new(
            EventKind::MessageAdded,
            super::Author::Bot(crate::id::Id::new()),
            serde_json::json!({}),
        )
        .acting(user);

        assert_eq!(draft.acting, Some(user));
    }
}

/// How a finished run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The run is still working.
    Running,
    /// The run produced a reply.
    Succeeded,
    /// The run failed.
    Failed,
}

/// The data each event kind carries.
#[derive(Clone, Debug, PartialEq)]
pub enum EventData {
    /// A User joined the Space.
    UserJoined(UserJoined),
    /// A Provider was connected.
    ProviderAdded(ProviderAdded),
    /// A Bot was created.
    BotCreated(BotCreated),
    /// A Computer was created.
    ComputerCreated(ComputerCreated),
    /// A Chat was created.
    ChatCreated(ChatCreated),
    /// A Message was added.
    MessageAdded(MessageAdded),
    /// A run started.
    RunStarted(RunStarted),
    /// A run finished.
    RunFinished(RunFinished),
}

/// The first version of every event kind.
pub const KIND_VERSION: u16 = 1;

impl EventKind {
    /// Returns the version of this kind this build writes.
    #[must_use]
    pub const fn supported_version(self) -> u16 {
        KIND_VERSION
    }

    /// Decodes and validates a kind's data.
    pub fn decode(self, data: &serde_json::Value) -> Result<EventData, EventDataError> {
        let decoded = match self {
            Self::UserJoined => EventData::UserJoined(decode(data)?),
            Self::ProviderAdded => EventData::ProviderAdded(decode(data)?),
            Self::BotCreated => EventData::BotCreated(decode(data)?),
            Self::ComputerCreated => EventData::ComputerCreated(decode(data)?),
            Self::ChatCreated => EventData::ChatCreated(decode(data)?),
            Self::MessageAdded => EventData::MessageAdded(decode(data)?),
            Self::RunStarted => EventData::RunStarted(decode(data)?),
            Self::RunFinished => EventData::RunFinished(decode(data)?),
        };

        Ok(decoded)
    }

    /// Checks that the author and acting User fit the kind.
    pub fn validate_author(self, author: Author, acting: Option<Id>) -> Result<(), EventDataError> {
        match self {
            Self::UserJoined
            | Self::ProviderAdded
            | Self::BotCreated
            | Self::ComputerCreated
            | Self::ChatCreated => {
                if !matches!(author, Author::User(_)) || acting.is_some() {
                    return Err(EventDataError::Author);
                }
            }
            Self::MessageAdded => {}
            Self::RunStarted | Self::RunFinished => {
                if !matches!(author, Author::Bot(_)) || acting.is_none() {
                    return Err(EventDataError::Author);
                }
            }
        }

        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(data: &serde_json::Value) -> Result<T, EventDataError> {
    serde_json::from_value(data.clone()).map_err(|error| EventDataError::Data(error.to_string()))
}

impl EventDraft {
    /// Checks the kind, version, author, and data before the record holds it.
    pub fn validate(&self) -> Result<(), EventDataError> {
        if self.version != self.kind.supported_version() {
            return Err(EventDataError::Version(self.version));
        }
        self.kind.validate_author(self.author, self.acting)?;
        self.kind.decode(&self.data)?;

        Ok(())
    }
}

/// An event does not fit the schema of its kind.
#[derive(Debug, thiserror::Error)]
pub enum EventDataError {
    /// The kind's data does not match its schema.
    #[error("event data does not match its kind: {0}")]
    Data(String),
    /// The author or acting User does not fit the kind.
    #[error("the event's author does not fit its kind")]
    Author,
    /// The version is not one this build writes.
    #[error("event version {0} is not supported")]
    Version(u16),
}

/// The data of `neha.user.joined`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserJoined {
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// The data of `neha.provider.added`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAdded {
    /// The Provider identifier.
    pub provider: Id,
    /// The protocol it speaks.
    pub protocol: String,
    /// Its base address.
    pub base_url: String,
    /// The Models it offers.
    pub models: Vec<ModelData>,
    /// The default Model.
    #[serde(default)]
    pub default_model: Option<Id>,
}

/// One Model a Provider offers.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelData {
    /// The Model identifier.
    pub id: Id,
    /// The name the Provider expects.
    pub name: String,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// Whether it reads images.
    pub reads_images: bool,
}

/// The data of `neha.bot.created`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotCreated {
    /// The Bot identifier.
    pub bot: Id,
    /// The User who owns it.
    pub owner: Id,
    /// Its name.
    pub name: String,
    /// Its Computer.
    pub computer: Id,
    /// Its Chat with the owner.
    pub chat: Id,
}

/// The data of `neha.computer.created`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerCreated {
    /// The Computer identifier.
    pub computer: Id,
    /// The User who owns it.
    pub owner: Id,
    /// `managed` or `real`.
    pub kind: String,
    /// The Computer's workspace directory.
    pub workspace: String,
}

/// The data of `neha.chat.created`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCreated {
    /// The Chat identifier.
    pub chat: Id,
    /// The User who owns it.
    pub owner: Id,
    /// The Bot the Chat is with.
    pub bot: Id,
}

/// The data of `neha.message.added`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageAdded {
    /// The Message identifier.
    pub message: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Its text.
    pub text: String,
    /// The run it belongs to.
    #[serde(default)]
    pub run: Option<Id>,
}

/// The data of `neha.run.started`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStarted {
    /// The run.
    pub run: Id,
    /// The Chat.
    pub chat: Id,
    /// The Bot.
    pub bot: Id,
    /// The command that started it.
    pub command: Id,
    /// The Message that started it.
    pub message: Id,
}

/// The data of `neha.run.finished`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFinished {
    /// The run.
    pub run: Id,
    /// How it ended.
    pub status: RunStatus,
    /// The Bot's reply Message.
    #[serde(default)]
    pub reply: Option<Id>,
    /// The usage the Model reported.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The failure.
    #[serde(default)]
    pub error: Option<String>,
}
