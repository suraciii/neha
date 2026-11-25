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
