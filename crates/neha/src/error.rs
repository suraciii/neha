//! Stable error codes.
//!
//! Every command failure carries one of these codes. Clients branch on the
//! code and never on the message.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A stable command-surface error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A parameter is missing or malformed.
    InvalidArgument,
    /// The caller is not authenticated.
    Unauthorized,
    /// The caller is authenticated but lacks the authority.
    Forbidden,
    /// The object does not exist, or the caller may not know that it exists.
    NotFound,
    /// The command conflicts with the object's current state.
    Conflict,
    /// A command identifier was reused with different parameters.
    IdempotencyConflict,
    /// The cursor is older than the record the Space can serve.
    CursorExpired,
    /// The caller and the Space do not share a version.
    UnsupportedVersion,
    /// The caller sent too many requests.
    RateLimited,
    /// The work was cancelled.
    Cancelled,
    /// The Space failed in a way it does not expose.
    Internal,
    /// The Space or a dependency is temporarily unavailable.
    Unavailable,
}

impl ErrorCode {
    /// Every code the command surface can return.
    pub const ALL: [Self; 12] = [
        Self::InvalidArgument,
        Self::Unauthorized,
        Self::Forbidden,
        Self::NotFound,
        Self::Conflict,
        Self::IdempotencyConflict,
        Self::CursorExpired,
        Self::UnsupportedVersion,
        Self::RateLimited,
        Self::Cancelled,
        Self::Internal,
        Self::Unavailable,
    ];

    /// Returns the wire slug.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::CursorExpired => "cursor_expired",
            Self::UnsupportedVersion => "unsupported_version",
            Self::RateLimited => "rate_limited",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    const GOLDEN: [&str; 12] = [
        "invalid_argument",
        "unauthorized",
        "forbidden",
        "not_found",
        "conflict",
        "idempotency_conflict",
        "cursor_expired",
        "unsupported_version",
        "rate_limited",
        "cancelled",
        "internal",
        "unavailable",
    ];

    #[test]
    fn every_code_has_its_golden_slug() {
        let slugs: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();

        assert_eq!(slugs, GOLDEN);
        for (code, slug) in ErrorCode::ALL.iter().zip(GOLDEN) {
            assert_eq!(serde_json::to_string(code).unwrap(), format!("\"{slug}\""));
            assert_eq!(
                serde_json::from_str::<ErrorCode>(&format!("\"{slug}\"")).unwrap(),
                *code
            );
        }
    }

    #[test]
    fn an_unknown_slug_is_rejected() {
        assert!(serde_json::from_str::<ErrorCode>("\"quota_exceeded\"").is_err());
    }
}
