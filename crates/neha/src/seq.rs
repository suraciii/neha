//! Record sequence numbers.
//!
//! The Space assigns a global, monotonically increasing sequence number to
//! every record event. It crosses the wire as a canonical decimal string and
//! serves as a cursor for reading the record.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A Space-wide record cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Seq(i64);

impl Seq {
    /// The cursor before the first event.
    pub const ZERO: Self = Self(0);

    /// Wraps a non-negative value.
    #[must_use]
    pub fn from_i64(value: i64) -> Option<Self> {
        (value >= 0).then_some(Self(value))
    }

    /// Returns the value.
    #[must_use]
    pub fn as_i64(self) -> i64 {
        self.0
    }

    /// Returns the next cursor, or `None` at the end of the range.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).and_then(Self::from_i64)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Seq {
    type Err = SeqError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(SeqError::Empty);
        }
        if !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SeqError::NotDecimal);
        }
        if text.len() > 1 && text.starts_with('0') {
            return Err(SeqError::LeadingZero);
        }

        text.parse::<i64>()
            .map(Self)
            .map_err(|_| SeqError::NotDecimal)
    }
}

impl Serialize for Seq {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;

        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Text that is not a canonical non-negative decimal integer.
#[derive(Debug, thiserror::Error)]
pub enum SeqError {
    /// The text is empty.
    #[error("sequence number is empty")]
    Empty,
    /// The text is not a decimal integer, or it is out of range.
    #[error("sequence number is not a decimal integer")]
    NotDecimal,
    /// The text has a leading zero.
    #[error("sequence number has a leading zero")]
    LeadingZero,
}

#[cfg(test)]
mod tests {
    use super::Seq;

    #[test]
    fn crosses_the_wire_as_a_decimal_string() {
        assert_eq!(serde_json::to_string(&Seq::ZERO).unwrap(), "\"0\"");
        assert_eq!(
            serde_json::to_string(&Seq::from_i64(4812).unwrap()).unwrap(),
            "\"4812\""
        );
        assert_eq!(
            serde_json::from_str::<Seq>("\"4812\"").unwrap().as_i64(),
            4812
        );
    }

    #[test]
    fn a_number_is_not_the_wire_form() {
        assert!(serde_json::from_str::<Seq>("4812").is_err());
    }

    #[test]
    fn non_canonical_decimal_text_is_rejected() {
        for text in [
            "",
            "+1",
            "007",
            "-0",
            " 1",
            "1 ",
            "abc",
            "9223372036854775808",
        ] {
            assert!(text.parse::<Seq>().is_err(), "{text:?} must be rejected");
        }
        assert_eq!("0".parse::<Seq>().unwrap(), Seq::ZERO);
        assert_eq!("4812".parse::<Seq>().unwrap().as_i64(), 4812);
    }

    #[test]
    fn next_moves_one_step_and_stops_at_the_end_of_the_range() {
        assert_eq!(Seq::ZERO.next().unwrap().as_i64(), 1);
        assert!(Seq::from_i64(i64::MAX).unwrap().next().is_none());
    }
}
