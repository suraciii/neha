//! Entity identifiers.
//!
//! An identifier is a UUID version 7 written as a lowercase hyphenated string.
//! The values a generator creates sort by their millisecond time prefix.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// An entity identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(Uuid);

impl Id {
    /// Creates an identifier from the current time and random bits.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps a UUID version 7.
    #[must_use]
    pub fn from_uuid(value: Uuid) -> Option<Self> {
        (value.get_version_num() == 7).then_some(Self(value))
    }

    /// Returns the wrapped UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Id {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let value = Uuid::parse_str(text).map_err(IdError::InvalidUuid)?;
        if text != value.hyphenated().to_string() {
            return Err(IdError::NotCanonical);
        }

        Self::from_uuid(value).ok_or(IdError::NotVersion7)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;

        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Text that is not a canonical UUID version 7.
#[derive(Debug, thiserror::Error)]
pub enum IdError {
    /// The text is not a UUID.
    #[error("identifier is not a UUID: {0}")]
    InvalidUuid(#[from] uuid::Error),
    /// The text is not lowercase and hyphenated.
    #[error("identifier is not in lowercase hyphenated form")]
    NotCanonical,
    /// The UUID is not version 7.
    #[error("identifier is not a UUID version 7")]
    NotVersion7,
}

#[cfg(test)]
mod tests {
    use super::Id;
    use uuid::Uuid;

    const GOLDEN: &str = "01890a5d-ac96-774b-bcce-b302099a8057";

    #[test]
    fn new_generates_version_7() {
        assert_eq!(Id::new().as_uuid().get_version_num(), 7);
    }

    #[test]
    fn text_is_the_golden_lowercase_hyphenated_form() {
        let id: Id = GOLDEN.parse().expect("golden id parses");

        assert_eq!(id.to_string(), GOLDEN);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{GOLDEN}\""));
        assert_eq!(
            serde_json::from_str::<Id>(&format!("\"{GOLDEN}\"")).unwrap(),
            id
        );
    }

    #[test]
    fn a_version_4_uuid_is_rejected() {
        assert!(
            Id::from_uuid(Uuid::parse_str("01890a5d-ac96-474b-bcce-b302099a8057").unwrap())
                .is_none()
        );
        assert!(
            "01890a5d-ac96-474b-bcce-b302099a8057"
                .parse::<Id>()
                .is_err()
        );
    }

    #[test]
    fn non_canonical_text_is_rejected() {
        assert!(
            "01890A5D-AC96-774B-BCCE-B302099A8057"
                .parse::<Id>()
                .is_err()
        );
        assert!(
            "{01890a5d-ac96-774b-bcce-b302099a8057}"
                .parse::<Id>()
                .is_err()
        );
        assert!(
            "urn:uuid:01890a5d-ac96-774b-bcce-b302099a8057"
                .parse::<Id>()
                .is_err()
        );
        assert!("not-an-id".parse::<Id>().is_err());
    }
}
