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

    /// Seals one MCP server's credential for the Space or one User.
    ///
    /// A server's credential is never in the record and never reaches a
    /// Computer: only the Space reads it when it calls the server.
    pub fn store_mcp_credential(
        &self,
        server: Id,
        user: Option<Id>,
        credential: &str,
    ) -> Result<(), SecretsError> {
        let directory = self.directory.join(mcp_directory(server));
        if let Err(error) = fs::create_dir_all(&directory) {
            return Err(SecretsError::Io(error));
        }
        let sealed = self.seal(credential.as_bytes())?;
        write_private(&directory.join(mcp_file(user)), &sealed)
    }

    /// Removes every credential of one MCP server.
    pub fn discard_mcp_server(&self, server: Id) {
        let _ = fs::remove_dir_all(self.directory.join(mcp_directory(server)));
    }

    /// Removes one MCP server's credential for the Space or one User.
    pub fn discard_mcp_credential(&self, server: Id, user: Option<Id>) {
        let _ = fs::remove_file(
            self.directory
                .join(mcp_directory(server))
                .join(mcp_file(user)),
        );
    }

    /// Opens one MCP server's credential, when one is stored.
    pub fn mcp_credential(
        &self,
        server: Id,
        user: Option<Id>,
    ) -> Result<Option<String>, SecretsError> {
        let path = self
            .directory
            .join(mcp_directory(server))
            .join(mcp_file(user));
        let sealed = match fs::read(&path) {
            Ok(sealed) => sealed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(SecretsError::Io(error)),
        };
        let plain = self.open(&sealed)?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| SecretsError::Corrupt)
    }

    /// Opens a Provider's API key.
    pub fn provider_key(&self, provider: Id) -> Result<String, SecretsError> {
        let sealed = fs::read(self.directory.join(provider_file(provider)))?;
        let plain = self.open(&sealed)?;
        String::from_utf8(plain).map_err(|_| SecretsError::Corrupt)
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

/// The directory that holds one MCP server's credentials.
fn mcp_directory(server: Id) -> String {
    format!("mcp-{server}")
}

/// The file that holds one credential of a server.
///
/// A User's credential and the Space's are separate files, so one never
/// stands in for the other.
fn mcp_file(user: Option<Id>) -> String {
    match user {
        Some(user) => format!("user-{user}.key"),
        None => "space.key".to_owned(),
    }
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
