//! Record sequence numbers.
//!
//! The Space assigns a global, monotonically increasing sequence number to
//! every record event. It crosses the wire as a decimal string and serves as
//! a cursor for reading the record.

use std::fmt;
use std::num::ParseIntError;
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
        let value: i64 = text.parse()?;

        Self::from_i64(value).ok_or(SeqError::Negative(value))
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

/// A sequence number that is not a non-negative decimal integer.
#[derive(Debug, thiserror::Error)]
pub enum SeqError {
    /// The text is not a decimal integer.
    #[error("sequence number is not a decimal integer: {0}")]
    NotDecimal(#[from] ParseIntError),
    /// The text is a negative value.
    #[error("sequence number is negative: {0}")]
    Negative(i64),
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
    fn negative_and_malformed_values_are_rejected() {
        assert!("-1".parse::<Seq>().is_err());
        assert!("abc".parse::<Seq>().is_err());
    }

    #[test]
    fn next_moves_one_step_and_stops_at_the_end_of_the_range() {
        assert_eq!(Seq::ZERO.next().unwrap().as_i64(), 1);
        assert!(Seq::from_i64(i64::MAX).unwrap().next().is_none());
    }
}
