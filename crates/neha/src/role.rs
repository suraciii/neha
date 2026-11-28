//! The role a User holds in one Space.
//!
//! The Space has one Owner, who may make Admins. The Owner and the Admins
//! administer the Space; a Member uses it without administering it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What a User may do in a Space.
///
/// The Space has one Owner, who may make Admins. The Owner and the Admins
/// administer the Space; a Member uses it without administering it.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS,
)]
#[ts(export_to = "identity.ts")]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The User who owns the Space: makes Admins, transfers the Space, and
    /// deletes it.
    Owner,
    /// A User the Owner makes an Admin: invites Members, connects Providers,
    /// and configures the Space.
    Admin,
    /// A User who uses the Space without administering it.
    #[default]
    Member,
}

impl Role {
    /// Whether this role administers the Space.
    #[must_use]
    pub const fn administers(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// Whether this role owns the Space.
    #[must_use]
    pub const fn owns(self) -> bool {
        matches!(self, Self::Owner)
    }

    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A role name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown role: {0}")]
pub struct UnknownRole(pub String);

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err(UnknownRole(text.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn wire_names_round_trip() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
            assert_eq!(serde_json::to_value(role).unwrap(), role.as_str());
        }
    }

    #[test]
    fn only_owner_and_admin_administrate() {
        assert!(Role::Owner.administers());
        assert!(Role::Admin.administers());
        assert!(!Role::Member.administers());
        assert!(Role::Owner.owns());
        assert!(!Role::Admin.owns());
        assert!(!Role::Member.owns());
    }

    #[test]
    fn an_unknown_role_is_rejected() {
        assert!("superuser".parse::<Role>().is_err());
    }

    #[test]
    fn a_missing_role_reads_as_member() {
        assert_eq!(Role::default(), Role::Member);
    }
}
