//! The bytes of Files.
//!
//! A File's bytes live under their SHA-256, so the same bytes are stored once
//! however many Files carry them. The record names a File by that digest.
//!
//! A blob appears under `<root>/<first two hex digits>/<digest>` only once it
//! is complete: the store writes beside it and renames it into place.

use std::path::Path;

use neha::file::is_sha256;
use opendal::services::Fs;
use opendal::{ErrorKind, Operator};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A Space's content-addressed File bytes.
pub struct Blobs {
    operator: Operator,
}

impl Blobs {
    /// Opens the store under `root`, creating it when missing.
    pub fn open(root: &Path) -> Result<Self, BlobsError> {
        let scratch = root.join("tmp");
        // A scratch file belongs to a write that never finished: a complete
        // blob is renamed into place before the record names it.
        let _ = std::fs::remove_dir_all(&scratch);
        let operator = Operator::new(
            Fs::default()
                .root(&root.to_string_lossy())
                .atomic_write_dir(&scratch.to_string_lossy()),
        )
        .map_err(BlobsError::Open)?;

        Ok(Self { operator })
    }

    /// Stores the bytes and returns their lowercase hex SHA-256.
    ///
    /// Bytes that are already stored are not written again.
    pub async fn put(&self, bytes: &[u8]) -> Result<String, BlobsError> {
        let sha256 = digest(bytes);
        let key = key(&sha256)?;
        match self.operator.stat(&key).await {
            Ok(_) => return Ok(sha256),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(BlobsError::Backend(error)),
        }
        self.operator
            .write(&key, bytes.to_vec())
            .await
            .map_err(BlobsError::Backend)?;

        Ok(sha256)
    }

    /// Reads the bytes stored under one digest of the record.
    pub async fn get(&self, sha256: &str) -> Result<Vec<u8>, BlobsError> {
        self.operator
            .read(&key(sha256)?)
            .await
            .map(|buffer| buffer.to_vec())
            .map_err(BlobsError::Backend)
    }
}

/// Returns where a digest's bytes live: two hex digits, then the digest.
fn key(sha256: &str) -> Result<String, BlobsError> {
    if !is_sha256(sha256) {
        return Err(BlobsError::MalformedDigest(sha256.to_owned()));
    }

    Ok(format!("{}/{}", &sha256[..2], sha256))
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The store cannot be used.
#[derive(Debug, Error)]
pub enum BlobsError {
    /// The store cannot be opened.
    #[error("the blob store cannot be opened: {0}")]
    Open(#[source] opendal::Error),
    /// A read or write failed.
    #[error("the blob store failed: {0}")]
    Backend(#[source] opendal::Error),
    /// A digest of the record is not a SHA-256.
    #[error("a File's digest is not a SHA-256: {0}")]
    MalformedDigest(String),
}

#[cfg(test)]
mod tests {
    use super::{Blobs, BlobsError};

    // The SHA-256 of `abc`, from FIPS 180-2.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[tokio::test]
    async fn bytes_are_stored_once_under_their_digest() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        std::fs::write(root.path().join("tmp").join("interrupted"), b"half").unwrap();
        let blobs = Blobs::open(root.path()).unwrap();
        assert!(
            std::fs::read_dir(root.path().join("tmp"))
                .unwrap()
                .next()
                .is_none(),
            "an interrupted write leaves nothing behind"
        );

        let first = blobs.put(b"abc").await.unwrap();
        let second = blobs.put(b"abc").await.unwrap();

        assert_eq!(first, ABC);
        assert_eq!(second, ABC);
        assert_eq!(blobs.get(ABC).await.unwrap(), b"abc");
        let objects: Vec<_> = std::fs::read_dir(root.path().join(&ABC[..2]))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(objects, [std::ffi::OsStr::new(ABC)]);
        assert!(
            std::fs::read_dir(root.path().join("tmp"))
                .unwrap()
                .next()
                .is_none(),
            "a stored blob leaves no scratch file"
        );
        assert!(blobs.get(&"00".repeat(32)).await.is_err());
    }

    #[tokio::test]
    async fn a_malformed_digest_is_an_error_not_a_panic() {
        let root = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(root.path()).unwrap();

        for digest in [
            String::new(),
            "ab".to_owned(),
            "ab".repeat(31),
            "AB".repeat(32),
        ] {
            assert!(
                matches!(
                    blobs.get(&digest).await,
                    Err(BlobsError::MalformedDigest(_))
                ),
                "{digest:?} must be refused, not sliced"
            );
        }
    }
}
