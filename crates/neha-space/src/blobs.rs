//! The bytes of Files.
//!
//! A File's bytes live under their SHA-256, so the same bytes are stored once
//! however many Files carry them. The record names a File by that digest.
//!
//! The bytes live where the Space's configuration points: a directory under
//! the home, or a prefix of an S3-compatible bucket. Both backends write the
//! same way: an object appears under `<prefix>/<first two hex digits>/<digest>`
//! only once it is complete, and a write never replaces an object that exists.

use std::path::{Path, PathBuf};

use neha::file::is_sha256;
use opendal::services::{Fs, S3};
use opendal::{ErrorKind, Operator};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;

use crate::config::{S3Storage, Storage};
use crate::home::Home;

/// A Space's content-addressed File bytes.
pub struct Blobs {
    operator: Operator,
    backend: Backend,
}

/// Where the bytes live.
enum Backend {
    /// A directory under the home, published onto with hard links.
    Local {
        /// The directory that holds the objects.
        root: PathBuf,
        /// The directory that holds writes still in progress.
        scratch: PathBuf,
    },
    /// An S3-compatible service.
    Remote,
}

impl Blobs {
    /// Opens the store the configuration names.
    pub fn open(home: &Home, storage: &Storage) -> Result<Self, BlobsError> {
        match storage {
            Storage::Local => {
                let root = home.blobs_dir();
                let scratch = root.join("tmp");
                // A scratch file belongs to a write that never finished: a
                // complete blob is published before the record names it.
                let _ = std::fs::remove_dir_all(&scratch);
                let operator = local_operator(&root)?;
                Ok(Self {
                    operator,
                    backend: Backend::Local { root, scratch },
                })
            }
            Storage::S3(storage) => Ok(Self {
                operator: s3_operator(storage)?,
                backend: Backend::Remote,
            }),
        }
    }

    /// Checks once that the configured store can be used.
    ///
    /// A local directory is prepared by the Space itself, so only an
    /// S3-compatible store is reached. Listing one object proves that the
    /// credentials and the bucket work; an empty prefix still answers.
    pub async fn check(&self) -> Result<(), BlobsError> {
        if matches!(self.backend, Backend::Local { .. }) {
            return Ok(());
        }

        self.operator
            .list_with("")
            .limit(1)
            .await
            .map(|_| ())
            .map_err(BlobsError::Backend)
    }

    /// Opens the store with credentials of its own, for tests that must not
    /// depend on the process environment.
    #[cfg(test)]
    pub(crate) fn open_with_credentials(
        storage: &S3Storage,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self, BlobsError> {
        install_http_transport();
        let builder = s3_builder(storage)
            .access_key_id(access_key)
            .secret_access_key(secret_key);
        let operator = Operator::new(builder).map_err(BlobsError::Open)?;

        Ok(Self {
            operator,
            backend: Backend::Remote,
        })
    }

    /// Opens the store with the config file and the EC2 metadata disabled, for
    /// the test of a store the Space cannot sign for. It does not promise that
    /// every credential source of the default chain is empty.
    #[cfg(test)]
    pub(crate) fn open_without_credentials(storage: &S3Storage) -> Result<Self, BlobsError> {
        install_http_transport();
        let builder = s3_builder(storage)
            .disable_config_load()
            .disable_ec2_metadata();
        let operator = Operator::new(builder).map_err(BlobsError::Open)?;

        Ok(Self {
            operator,
            backend: Backend::Remote,
        })
    }

    /// Stores the bytes and returns their lowercase hex SHA-256.
    ///
    /// Bytes that are already stored are not written again: neither backend
    /// replaces an object, and content addressing makes the existing object
    /// the same bytes by definition.
    pub async fn put(&self, bytes: &[u8]) -> Result<String, BlobsError> {
        let sha256 = digest(bytes);
        let key = key(&sha256)?;
        match &self.backend {
            Backend::Local { root, scratch } => local_put(root, scratch, &key, bytes).await?,
            Backend::Remote => {
                let written = self
                    .operator
                    .write_with(&key, bytes.to_vec())
                    .if_not_exists(true)
                    .await;
                match written {
                    Ok(_) => {}
                    // Another writer stored these bytes first; nothing to do.
                    Err(error) if error.kind() == ErrorKind::ConditionNotMatch => {}
                    Err(error) => return Err(BlobsError::Backend(error)),
                }
            }
        }

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

/// Opens the local directory that holds the bytes.
fn local_operator(root: &Path) -> Result<Operator, BlobsError> {
    Operator::new(Fs::default().root(&root.to_string_lossy())).map_err(BlobsError::Open)
}

/// Writes the bytes beside their place, then publishes them under the digest.
///
/// A hard link is the publish step: it fails when the object already exists,
/// so a concurrent writer can never replace complete bytes. The scratch file
/// is this write's own, so it is removed whatever happens.
async fn local_put(root: &Path, scratch: &Path, key: &str, bytes: &[u8]) -> Result<(), BlobsError> {
    tokio::fs::create_dir_all(scratch).await?;
    let temp = scratch.join(neha::id::Id::new().to_string());
    let written = write_complete(&temp, bytes).await;
    let published = match written {
        Ok(()) => publish(&temp, &root.join(key)).await,
        Err(error) => Err(error),
    };
    let _ = tokio::fs::remove_file(&temp).await;

    published
}

/// Writes all bytes of one blob and waits until they are on the device.
async fn write_complete(path: &Path, bytes: &[u8]) -> Result<(), BlobsError> {
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;

    Ok(())
}

/// Publishes a complete scratch file under its final name.
async fn publish(temp: &Path, target: &Path) -> Result<(), BlobsError> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::hard_link(temp, target).await {
        Ok(()) => Ok(()),
        // The object is already published, and content addressing makes it
        // the same bytes by definition.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Installs opendal's process-wide HTTP transport.
///
/// opendal is built without default features, so its reqwest transport must be
/// installed before the first S3 request; the first caller wins and the rest
/// do nothing.
fn install_http_transport() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(opendal::install_default);
}

/// Opens the S3-compatible bucket and prefix that hold the bytes.
fn s3_operator(storage: &S3Storage) -> Result<Operator, BlobsError> {
    install_http_transport();

    Operator::new(s3_builder(storage)).map_err(BlobsError::Open)
}

/// Builds the S3 client for the configured bucket, prefix, and options.
fn s3_builder(storage: &S3Storage) -> S3 {
    let mut builder = S3::default()
        .endpoint(&storage.endpoint)
        .bucket(&storage.bucket)
        .region(&storage.region)
        .root(&storage.prefix);
    if let Some(profile) = &storage.profile {
        builder = builder.profile(profile);
    }
    if storage.virtual_host {
        builder = builder.enable_virtual_host_style();
    }
    if let Some(key_id) = &storage.sse_kms_key_id {
        builder = builder.server_side_encryption_with_customer_managed_kms_key(key_id);
    }

    builder
}

/// Returns where a digest's bytes live: two hex digits, then the digest.
fn key(sha256: &str) -> Result<String, BlobsError> {
    if !is_sha256(sha256) {
        return Err(BlobsError::MalformedDigest(sha256.to_owned()));
    }

    Ok(format!("{}/{}", &sha256[..2], sha256))
}

/// Returns the digest the File store names one byte sequence by.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
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
    /// A local blob could not be written.
    #[error("the blob store failed: {0}")]
    Local(#[from] std::io::Error),
    /// A digest of the record is not a SHA-256.
    #[error("a File's digest is not a SHA-256: {0}")]
    MalformedDigest(String),
}

impl BlobsError {
    /// Returns whether the dependency may recover: a temporary network or
    /// service failure or a rate limit answers `503 Unavailable`, while a
    /// configuration error or bytes missing under a recorded digest answers
    /// `500 Internal Server Error`.
    pub fn is_unavailable(&self) -> bool {
        let error = match self {
            Self::Open(error) | Self::Backend(error) => error,
            Self::Local(_) | Self::MalformedDigest(_) => return false,
        };

        error.is_temporary() || matches!(error.kind(), ErrorKind::RateLimited)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_s3 {
    //! The S3 test environment.
    //!
    //! The S3 tests run only when `NEHA_TEST_S3` is set. The endpoint, bucket,
    //! and region come from `NEHA_TEST_S3_*`; the credentials come from the
    //! process's `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, which is
    //! where opendal's default credential chain reads them.

    use std::sync::OnceLock;

    use crate::config::S3Storage;

    /// Runs one S3 test on the single runtime the process keeps for them all.
    ///
    /// opendal installs one process-wide HTTP transport, and its reqwest client
    /// pool reuses connections that belong to the runtime which drove them. The
    /// test runner gives every `#[tokio::test]` its own runtime, so a
    /// connection from a finished test makes the next one fail with "dispatch
    /// task is gone". The S3 tests run their async bodies on this runtime
    /// instead.
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("the S3 test runtime builds")
            })
            .block_on(future)
    }

    /// Returns the store the S3 tests run against, or `None` when they are off.
    ///
    /// `NEHA_TEST_S3` opens the tests explicitly. Once it is set, a missing
    /// variable refuses to skip quietly: the test fails and names what is
    /// missing.
    pub fn storage() -> Option<S3Storage> {
        std::env::var_os("NEHA_TEST_S3")?;
        require_credentials();

        Some(S3Storage {
            bucket: required("NEHA_TEST_S3_BUCKET"),
            region: std::env::var("NEHA_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned()),
            prefix: unique_prefix(),
            endpoint: required("NEHA_TEST_S3_ENDPOINT"),
            profile: None,
            virtual_host: false,
            sse_kms_key_id: None,
        })
    }

    /// Returns a prefix no other test run shares.
    pub fn unique_prefix() -> String {
        format!("spaces/test-{}", neha::id::Id::new())
    }

    /// Writes one object under a digest, as an outside writer would.
    pub async fn write_object(storage: &S3Storage, sha256: &str, bytes: &[u8]) {
        let operator = super::s3_operator(storage).expect("the test store opens");
        operator
            .write(&format!("{}/{}", &sha256[..2], sha256), bytes.to_vec())
            .await
            .expect("the test object writes");
    }

    /// Deletes one object of the store, as something outside the Space would.
    pub async fn delete(storage: &S3Storage, sha256: &str) {
        let operator = super::s3_operator(storage).expect("the test store opens");
        operator
            .delete(&format!("{}/{}", &sha256[..2], sha256))
            .await
            .expect("the test object deletes");
    }

    /// Returns the objects under the store's prefix, as path and length.
    pub async fn objects(storage: &S3Storage) -> Vec<(String, u64)> {
        let operator = super::s3_operator(storage).expect("the test store opens");
        let mut objects: Vec<_> = operator
            .list_with("")
            .recursive(true)
            .await
            .expect("the test bucket lists")
            .into_iter()
            .map(|entry| (entry.path().to_owned(), entry.metadata().content_length()))
            .collect();
        objects.sort();

        objects
    }

    /// Fails the test when the default chain has no credentials to read:
    /// `NEHA_TEST_S3` promises an S3 service, not a silently skipped test.
    fn require_credentials() {
        let missing: Vec<&str> = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]
            .into_iter()
            .filter(|name| std::env::var_os(name).is_none())
            .collect();
        if missing.is_empty() {
            return;
        }
        let verb = if missing.len() == 1 { "is" } else { "are" };
        panic!(
            "NEHA_TEST_S3 is set but {} {verb} not",
            missing.join(" and ")
        );
    }

    fn required(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("NEHA_TEST_S3 is set but {name} is not"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Blobs, BlobsError, test_s3};
    use crate::config::{S3Storage, Storage};
    use crate::home::Home;

    // The SHA-256 of `abc`, from FIPS 180-2.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn local_blobs(dir: &tempfile::TempDir) -> (Home, Blobs) {
        let home = Home::open(dir.path()).unwrap();
        let blobs = Blobs::open(&home, &Storage::Local).unwrap();

        (home, blobs)
    }

    #[tokio::test]
    async fn bytes_are_stored_once_under_their_digest() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        std::fs::create_dir_all(home.blobs_dir().join("tmp")).unwrap();
        std::fs::write(home.blobs_dir().join("tmp").join("interrupted"), b"half").unwrap();
        let blobs = Blobs::open(&home, &Storage::Local).unwrap();
        blobs.check().await.unwrap();
        assert!(
            !home.blobs_dir().join("tmp").exists(),
            "an interrupted write leaves nothing behind"
        );

        let first = blobs.put(b"abc").await.unwrap();
        let second = blobs.put(b"abc").await.unwrap();

        assert_eq!(first, ABC);
        assert_eq!(second, ABC);
        assert_eq!(blobs.get(ABC).await.unwrap(), b"abc");
        let objects: Vec<_> = std::fs::read_dir(home.blobs_dir().join(&ABC[..2]))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(objects, [std::ffi::OsStr::new(ABC)]);
        assert!(
            std::fs::read_dir(home.blobs_dir().join("tmp"))
                .map_or(true, |mut entries| entries.next().is_none()),
            "a stored blob leaves no scratch file"
        );
        assert!(blobs.get(&"00".repeat(32)).await.is_err());
    }

    #[tokio::test]
    async fn an_existing_object_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let (home, blobs) = local_blobs(&dir);
        // An object that exists under a digest is kept as it is: the store
        // writes content-addressed data and never replaces it.
        let stored = home.blobs_dir().join(&ABC[..2]).join(ABC);
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, b"tampered").unwrap();

        let put = blobs.put(b"abc").await.unwrap();

        assert_eq!(put, ABC);
        assert_eq!(std::fs::read(&stored).unwrap(), b"tampered");
        assert_eq!(blobs.get(ABC).await.unwrap(), b"tampered");
    }

    #[tokio::test]
    async fn a_missing_object_is_not_an_outage() {
        let dir = tempfile::tempdir().unwrap();
        let (_home, blobs) = local_blobs(&dir);

        let error = blobs.get(&"00".repeat(32)).await.unwrap_err();

        assert!(!error.is_unavailable(), "{error}");
    }

    #[tokio::test]
    async fn a_malformed_digest_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (_home, blobs) = local_blobs(&dir);

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

    #[test]
    fn a_store_that_is_down_answers_unavailable() {
        test_s3::block_on(async {
            // A store the tests can reach without the process environment: the
            // closed port fails the request itself, not the signing.
            let storage = S3Storage {
                bucket: "neha".to_owned(),
                region: "us-east-1".to_owned(),
                prefix: test_s3::unique_prefix(),
                endpoint: "http://127.0.0.1:1".to_owned(),
                profile: None,
                virtual_host: false,
                sse_kms_key_id: None,
            };
            let blobs = Blobs::open_with_credentials(&storage, "neha-test", "neha-test").unwrap();

            let error = blobs.put(b"abc").await.unwrap_err();

            assert!(error.is_unavailable(), "{error}");
        });
    }

    #[test]
    fn a_store_without_credentials_is_not_usable() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            // Only the config file and the EC2 metadata are disabled; the other
            // sources of the default chain are not promised to be empty.
            let blobs = Blobs::open_without_credentials(&storage).unwrap();

            let error = blobs.check().await.unwrap_err();

            assert!(!error.is_unavailable(), "{error}");
            assert!(error.to_string().contains("signing credential"), "{error}");
        });
    }

    #[test]
    fn s3_never_replaces_an_existing_object() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            // A different content already sits under this digest; the conditional
            // write must report the object exists instead of replacing it.
            test_s3::write_object(&storage, ABC, b"seeded").await;
            let dir = tempfile::tempdir().unwrap();
            let home = Home::open(dir.path()).unwrap();
            let blobs = Blobs::open(&home, &Storage::S3(storage.clone())).unwrap();

            let saved = blobs.put(b"abc").await.unwrap();

            assert_eq!(saved, ABC);
            assert_eq!(blobs.get(ABC).await.unwrap(), b"seeded");
            assert_eq!(
                test_s3::objects(&storage).await,
                [(format!("{}/{}", &ABC[..2], ABC), 6)],
                "the seeded object is the only object"
            );
        });
    }

    #[test]
    fn s3_holds_one_object_per_content() {
        test_s3::block_on(async {
            let Some(storage) = test_s3::storage() else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let home = Home::open(dir.path()).unwrap();
            let blobs = Blobs::open(&home, &Storage::S3(storage.clone())).unwrap();

            let first = blobs.put(b"abc").await.unwrap();
            let second = blobs.put(b"abc").await.unwrap();

            assert_eq!(first, ABC);
            assert_eq!(second, ABC);
            assert_eq!(blobs.get(ABC).await.unwrap(), b"abc");
            assert_eq!(
                test_s3::objects(&storage).await,
                [(format!("{}/{}", &ABC[..2], ABC), 3)],
                "the same bytes are one object under the Space's prefix"
            );
        });
    }
}
