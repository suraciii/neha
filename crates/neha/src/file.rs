//! Files.
//!
//! A File belongs to the Chat or Group it was sent in, and the Space holds its
//! bytes. A Bot takes a File into its Workspace or delivers one from it.

/// The largest File a Space accepts.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Whether a digest is a lowercase hexadecimal SHA-256.
#[must_use]
pub fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{is_sha256, MAX_FILE_BYTES};

    #[test]
    fn a_digest_is_sixty_four_lowercase_hex() {
        assert!(is_sha256(&"ab".repeat(32)));
        assert!(!is_sha256(&"AB".repeat(32)));
        assert!(!is_sha256(&"ab".repeat(31)));
        assert!(!is_sha256("not a digest"));
    }

    #[test]
    fn the_limit_is_a_power_of_two_within_reach() {
        assert_eq!(MAX_FILE_BYTES, 32 * 1024 * 1024);
    }
}
