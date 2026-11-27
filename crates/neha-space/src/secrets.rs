//! Provider credentials encrypted at rest.
//!
//! A Provider's API key lives in the Space's `secrets` directory, sealed with
//! AES-256-GCM under a master key. The master key comes from
//! `NEHA_MASTER_KEY`, or from a 0600 file the Space creates on first use.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use rand::RngCore as _;
use thiserror::Error;

use crate::home::Home;

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const VAPID_BYTES: usize = 32;

/// The Space's sealed credentials.
pub struct Secrets {
    key: [u8; KEY_BYTES],
    directory: PathBuf,
}

impl Secrets {
    /// Loads the master key, creating it on first use.
    pub fn load(home: &Home) -> Result<Self, SecretsError> {
        let directory = home.root().join("secrets");
        let key = match std::env::var("NEHA_MASTER_KEY") {
            Ok(text) => parse_master_key(&text)?,
            Err(_) => load_or_create_master_key(&directory.join("master.key"))?,
        };

        Ok(Self { key, directory })
    }

    /// Seals a Provider's API key to its file.
    pub fn store_provider_key(&self, provider: Id, api_key: &str) -> Result<(), SecretsError> {
        let sealed = self.seal(api_key.as_bytes())?;
        write_private(&self.directory.join(provider_file(provider)), &sealed)
    }

    /// Removes a Provider key that no command recorded.
    pub fn discard_provider_key(&self, provider: Id) {
        let _ = fs::remove_file(self.directory.join(provider_file(provider)));
    }

    /// Opens a Provider's API key.
    pub fn provider_key(&self, provider: Id) -> Result<String, SecretsError> {
        let sealed = fs::read(self.directory.join(provider_file(provider)))?;
        let plain = self.open(&sealed)?;
        String::from_utf8(plain).map_err(|_| SecretsError::Corrupt)
    }

    /// Returns the VAPID signing key, creating it on first use.
    ///
    /// The key is sealed under the Space's master key. Rotating it silently
    /// would invalidate every browser subscription, so a missing key is the
    /// only case that writes a new one.
    pub fn vapid_private_key(&self) -> Result<[u8; VAPID_BYTES], SecretsError> {
        let path = self.directory.join("vapid.key");
        if path.exists() {
            let sealed = fs::read(&path)?;
            let plain = self.open(&sealed)?;
            return plain.try_into().map_err(|_| SecretsError::Corrupt);
        }
        let mut key = [0_u8; VAPID_BYTES];
        rand::rng().fill_bytes(&mut key);
        let sealed = self.seal(&key)?;
        write_private(&path, &sealed)?;

        Ok(key)
    }

    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| SecretsError::Key)?;
        let mut nonce = [0_u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| SecretsError::Encrypt)?;
        let mut bytes = Vec::with_capacity(NONCE_BYTES + sealed.len());
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&sealed);

        Ok(bytes)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SecretsError> {
        if sealed.len() <= NONCE_BYTES {
            return Err(SecretsError::Corrupt);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_BYTES);
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| SecretsError::Key)?;
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| SecretsError::Decrypt)
    }
}

fn provider_file(provider: Id) -> String {
    format!("provider-{provider}.key")
}

fn load_or_create_master_key(path: &PathBuf) -> Result<[u8; KEY_BYTES], SecretsError> {
    if path.exists() {
        let bytes = fs::read(path)?;
        return bytes.try_into().map_err(|_| SecretsError::Corrupt);
    }

    let mut key = [0_u8; KEY_BYTES];
    rand::rng().fill_bytes(&mut key);
    write_private(path, &key)?;

    Ok(key)
}

fn parse_master_key(text: &str) -> Result<[u8; KEY_BYTES], SecretsError> {
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(text)
        && bytes.len() == KEY_BYTES
    {
        return bytes.try_into().map_err(|_| SecretsError::Corrupt);
    }
    let bytes = decode_hex(text).ok_or(SecretsError::KeyFormat)?;
    bytes.try_into().map_err(|_| SecretsError::KeyFormat)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != KEY_BYTES * 2 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), SecretsError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;

    Ok(())
}

/// A credential cannot be sealed or opened.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// The secrets directory cannot be read or written.
    #[error("cannot use the secrets directory: {0}")]
    Io(#[from] std::io::Error),
    /// The master key has the wrong form.
    #[error("NEHA_MASTER_KEY must be 32 bytes, base64url or hex")]
    KeyFormat,
    /// The cipher cannot use the key.
    #[error("the master key is not usable")]
    Key,
    /// Sealing failed.
    #[error("cannot seal the credential")]
    Encrypt,
    /// Opening failed, most likely because the master key changed.
    #[error("cannot open the credential")]
    Decrypt,
    /// A stored value does not decode.
    #[error("the credential file is corrupt")]
    Corrupt,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use neha::id::Id;

    use super::{Secrets, SecretsError};
    use crate::home::Home;

    #[tokio::test]
    async fn seals_and_opens_a_provider_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let provider = Id::new();

        secrets.store_provider_key(provider, "sk-secret").unwrap();
        let opened = secrets.provider_key(provider).unwrap();

        assert_eq!(opened, "sk-secret");
        let raw = fs::read(
            home.root()
                .join("secrets")
                .join(format!("provider-{provider}.key")),
        )
        .unwrap();
        assert!(!raw.windows(9).any(|window| window == b"sk-secret"));
    }

    #[tokio::test]
    async fn a_reopened_space_opens_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let provider = Id::new();
        Secrets::load(&home)
            .unwrap()
            .store_provider_key(provider, "sk-secret")
            .unwrap();

        let opened = Secrets::load(&home)
            .unwrap()
            .provider_key(provider)
            .unwrap();

        assert_eq!(opened, "sk-secret");
    }

    #[tokio::test]
    async fn a_different_master_key_cannot_open_the_file() {
        let first = tempfile::tempdir().unwrap();
        let first_home = Home::open(first.path()).unwrap();
        let provider = Id::new();
        Secrets::load(&first_home)
            .unwrap()
            .store_provider_key(provider, "sk-secret")
            .unwrap();
        let sealed = fs::read(
            first_home
                .root()
                .join("secrets")
                .join(format!("provider-{provider}.key")),
        )
        .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_home = Home::open(second.path()).unwrap();
        fs::write(
            second_home
                .root()
                .join("secrets")
                .join(format!("provider-{provider}.key")),
            sealed,
        )
        .unwrap();
        let error = Secrets::load(&second_home).unwrap().provider_key(provider);

        assert!(matches!(error, Err(SecretsError::Decrypt)));
    }
}
