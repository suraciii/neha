//! Entity identifiers.
//!
//! An identifier is a UUID version 7 that crosses the wire as a lowercase
//! hyphenated string. Generating one puts the current time first, so the
//! values sort by creation time.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An entity identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(Uuid);

impl Id {
    /// Creates an identifier from the current time and random bits.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing UUID.
    #[must_use]
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
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
    type Err = uuid::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::Id;

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
    fn malformed_text_is_rejected() {
        assert!("not-an-id".parse::<Id>().is_err());
        assert!(serde_json::from_str::<Id>("\"not-an-id\"").is_err());
    }
}
