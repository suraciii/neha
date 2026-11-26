//! Files.
//!
//! A File belongs to the Chat or Group it was sent in, and the Space holds its
//! bytes. A Bot takes a File into its Workspace or delivers one from it.

/// The largest File a Space accepts.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The media type a File's name suggests.
///
/// A sender that knows the type states it; this is the default a name alone
/// implies, and the Space uses it for a File a Bot delivers.
#[must_use]
pub fn media_type_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" | "md" | "rs" | "ts" | "js" | "py" | "toml" | "yml" | "yaml" | "csv" => {
            "text/plain"
        }
        "json" => "application/json",
        "html" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

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
    use super::{MAX_FILE_BYTES, is_sha256};

    #[test]
    fn a_name_suggests_a_media_type() {
        use super::media_type_for;
        assert_eq!(media_type_for("notes.txt"), "text/plain");
        assert_eq!(media_type_for("REPORT.CSV"), "text/plain");
        assert_eq!(media_type_for("data.json"), "application/json");
        assert_eq!(media_type_for("scan.PNG"), "image/png");
        assert_eq!(media_type_for("archive"), "application/octet-stream");
        assert_eq!(media_type_for("no.extension."), "application/octet-stream");
    }

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
