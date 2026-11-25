//! Server-assigned timestamps.
//!
//! The Space assigns every record event a timestamp in UTC with microsecond
//! precision. It crosses the wire as an RFC 3339 string; clients render it in
//! their own time zone.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An instant in UTC at microsecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(jiff::Timestamp);

impl Timestamp {
    /// Returns the current instant, truncated to microseconds.
    #[must_use]
    pub fn now() -> Self {
        let now = jiff::Timestamp::now();

        Self::from_microseconds(now.as_microsecond()).expect("a timestamp is a valid instant")
    }

    /// Builds an instant from microseconds since the Unix epoch.
    #[must_use]
    pub fn from_microseconds(value: i64) -> Option<Self> {
        jiff::Timestamp::from_microsecond(value).ok().map(Self)
    }

    /// Returns microseconds since the Unix epoch.
    #[must_use]
    pub fn as_microseconds(self) -> i64 {
        self.0.as_microsecond()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Timestamp {
    type Err = jiff::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse::<jiff::Timestamp>().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;

    const GOLDEN: &str = "2025-11-25T23:18:24.123456Z";
    const GOLDEN_MICROSECONDS: i64 = 1_764_112_704_123_456;

    #[test]
    fn crosses_the_wire_as_rfc_3339_utc() {
        let timestamp = Timestamp::from_microseconds(GOLDEN_MICROSECONDS).unwrap();

        assert_eq!(timestamp.to_string(), GOLDEN);
        assert_eq!(
            serde_json::to_string(&timestamp).unwrap(),
            format!("\"{GOLDEN}\"")
        );
        assert_eq!(
            serde_json::from_str::<Timestamp>(&format!("\"{GOLDEN}\""))
                .unwrap()
                .as_microseconds(),
            GOLDEN_MICROSECONDS
        );
    }

    #[test]
    fn keeps_microsecond_precision() {
        let timestamp = Timestamp::from_microseconds(1).unwrap();

        assert_eq!(timestamp.as_microseconds(), 1);
        assert_eq!(timestamp.to_string(), "1970-01-01T00:00:00.000001Z");
    }

    #[test]
    fn now_is_an_instant_after_the_epoch() {
        assert!(Timestamp::now().as_microseconds() > 0);
    }

    #[test]
    fn malformed_text_is_rejected() {
        assert!("2025-11-25 23:18:24".parse::<Timestamp>().is_err());
        assert!(serde_json::from_str::<Timestamp>("\"not-a-time\"").is_err());
    }
}
