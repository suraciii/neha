//! Event classification.
//!
//! A reader of the record must know whether an event changes authority. It can
//! skip an event it does not understand only when that event is presentation.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How a reader treats an event it does not understand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventClass {
    /// The event changes only what a client presents, so a reader can skip it.
    Presentation,
    /// The event changes authority, so a reader that does not know it must stop.
    Authority,
}

impl EventClass {
    /// Returns the wire slug.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Presentation => "presentation",
            Self::Authority => "authority",
        }
    }
}

impl fmt::Display for EventClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::EventClass;

    #[test]
    fn crosses_the_wire_as_a_slug() {
        assert_eq!(
            serde_json::to_string(&EventClass::Presentation).unwrap(),
            "\"presentation\""
        );
        assert_eq!(
            serde_json::to_string(&EventClass::Authority).unwrap(),
            "\"authority\""
        );
        assert_eq!(
            serde_json::from_str::<EventClass>("\"authority\"").unwrap(),
            EventClass::Authority
        );
    }

    #[test]
    fn an_unknown_slug_is_rejected() {
        assert!(serde_json::from_str::<EventClass>("\"progress\"").is_err());
    }
}
