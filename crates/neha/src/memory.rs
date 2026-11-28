//! A Bot's memory.
//!
//! A memory entry is a lasting instruction or a note a Bot keeps for itself.
//! Its kind decides how much room it takes and who may have written it; its
//! scope decides where it applies. The record carries the entries as
//! `neha.memory.added` and `neha.memory.removed` events; this module owns the
//! vocabulary those events, the command surface, and the loop share.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The largest `rule` text one Bot keeps, in UTF-8 bytes.
pub const MAX_RULE_BYTES: usize = 4 * 1024;
/// The largest `note` text one Bot keeps, in UTF-8 bytes.
pub const MAX_NOTE_BYTES: usize = 16 * 1024;

/// What a memory entry is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A lasting instruction; only a User's words start one.
    Rule,
    /// What the Bot noticed, kept as data for the Bot.
    Note,
}

impl MemoryKind {
    /// Every kind, in the order a prompt lists them.
    pub const ALL: [Self; 2] = [Self::Rule, Self::Note];

    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Note => "note",
        }
    }

    /// Returns the bytes of this kind one Bot may hold.
    #[must_use]
    pub const fn limit(self) -> usize {
        match self {
            Self::Rule => MAX_RULE_BYTES,
            Self::Note => MAX_NOTE_BYTES,
        }
    }
}

impl FromStr for MemoryKind {
    type Err = UnknownMemoryKind;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "rule" => Ok(Self::Rule),
            "note" => Ok(Self::Note),
            _ => Err(UnknownMemoryKind(text.to_owned())),
        }
    }
}

/// Where a memory entry applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Every Chat of the Bot's owner.
    Owner,
    /// The Chat where it was written.
    User,
    /// The Group where it was written.
    Group,
}

impl MemoryScope {
    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::User => "user",
            Self::Group => "group",
        }
    }
}

impl FromStr for MemoryScope {
    type Err = UnknownMemoryScope;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "owner" => Ok(Self::Owner),
            "user" => Ok(Self::User),
            "group" => Ok(Self::Group),
            _ => Err(UnknownMemoryScope(text.to_owned())),
        }
    }
}

/// What wrote a memory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceKind {
    /// A Message wrote it.
    Message,
    /// A run wrote it.
    Run,
    /// A command wrote it.
    Command,
}

impl MemorySourceKind {
    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Run => "run",
            Self::Command => "command",
        }
    }
}

impl FromStr for MemorySourceKind {
    type Err = UnknownMemorySourceKind;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "message" => Ok(Self::Message),
            "run" => Ok(Self::Run),
            "command" => Ok(Self::Command),
            _ => Err(UnknownMemorySourceKind(text.to_owned())),
        }
    }
}

/// A memory kind this version does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown memory kind: {0}")]
pub struct UnknownMemoryKind(pub String);

/// A memory scope this version does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown memory scope: {0}")]
pub struct UnknownMemoryScope(pub String);

/// A memory source this version does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown memory source: {0}")]
pub struct UnknownMemorySourceKind(pub String);

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{MAX_NOTE_BYTES, MAX_RULE_BYTES, MemoryKind, MemoryScope, MemorySourceKind};

    #[test]
    fn memory_enums_round_trip_on_the_wire() {
        for (kind, wire) in [(MemoryKind::Rule, "rule"), (MemoryKind::Note, "note")] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(wire));
            assert_eq!(MemoryKind::from_str(wire).unwrap(), kind);
            assert_eq!(kind.as_str(), wire);
        }
        for (scope, wire) in [
            (MemoryScope::Owner, "owner"),
            (MemoryScope::User, "user"),
            (MemoryScope::Group, "group"),
        ] {
            assert_eq!(
                serde_json::to_value(scope).unwrap(),
                serde_json::json!(wire)
            );
            assert_eq!(MemoryScope::from_str(wire).unwrap(), scope);
            assert_eq!(scope.as_str(), wire);
        }
        for (source, wire) in [
            (MemorySourceKind::Message, "message"),
            (MemorySourceKind::Run, "run"),
            (MemorySourceKind::Command, "command"),
        ] {
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::json!(wire)
            );
            assert_eq!(MemorySourceKind::from_str(wire).unwrap(), source);
            assert_eq!(source.as_str(), wire);
        }
    }

    #[test]
    fn an_unknown_name_is_refused() {
        assert!(MemoryKind::from_str("fact").is_err());
        assert!(MemoryScope::from_str("global").is_err());
        assert!(MemorySourceKind::from_str("chat").is_err());
    }

    #[test]
    fn each_kind_has_its_own_limit() {
        assert_eq!(MemoryKind::Rule.limit(), MAX_RULE_BYTES);
        assert_eq!(MemoryKind::Note.limit(), MAX_NOTE_BYTES);
        assert_eq!(MAX_RULE_BYTES, 4096);
        assert_eq!(MAX_NOTE_BYTES, 16384);
    }
}
