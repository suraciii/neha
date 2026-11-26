//! Accounts, passwords, and device tokens.
//!
//! The first User joins with the Space's invitation and becomes the Admin.
//! A password is stored as an Argon2id hash, and a device token is stored only
//! as its SHA-256 digest.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha::time::Timestamp;
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Row, SqlitePool};
use thiserror::Error;

use crate::home::Home;
use crate::schema;

/// The shortest password a Space accepts.
pub const MIN_PASSWORD_BYTES: usize = 8;

/// The longest display name a Space accepts.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// A person who can sign in to the Space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// The account identifier.
    pub id: Id,
    /// The name the person signs in with.
    pub display_name: String,
    /// Whether the account is the Space's Admin.
    pub is_admin: bool,
}

/// The Space's accounts and their tokens.
pub struct Identity {
    pool: SqlitePool,
}

impl Identity {
    /// Opens the identity tables in a home.
    pub async fn open(home: &Home) -> Result<Self, IdentityError> {
        let options = SqliteConnectOptions::new()
            .filename(home.db_path())
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let mut connection = sqlx::SqliteConnection::connect_with(&options).await?;
        schema::migrate(&mut connection).await?;
        drop(connection);

        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;

        Ok(Self { pool })
    }

    /// Returns whether the Space has any account.
    pub async fn has_accounts(&self) -> Result<bool, IdentityError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&self.pool)
            .await?;

        Ok(count > 0)
    }

    /// Creates the first account as the Admin and returns a device token.
    pub async fn create_admin(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<(Account, String), IdentityError> {
        let display_name = validate_display_name(display_name)?;
        validate_password(password)?;
        if self.has_accounts().await? {
            return Err(IdentityError::AlreadyInitialized);
        }

        self.insert_account(&display_name, password, true).await
    }

    /// Adds an account beyond the Admin, as an invitation will; for tests.
    #[cfg(test)]
    pub(crate) async fn add_account_for_test(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<Account, IdentityError> {
        let display_name = validate_display_name(display_name)?;
        validate_password(password)?;
        self.insert_account(&display_name, password, false)
            .await
            .map(|(account, _)| account)
    }

    /// Inserts an account and returns it with a device token.
    async fn insert_account(
        &self,
        display_name: &str,
        password: &str,
        is_admin: bool,
    ) -> Result<(Account, String), IdentityError> {
        let id = Id::new();
        let password_hash = hash_password(password)?;
        let created_at = Timestamp::now().as_microseconds();
        let inserted = sqlx::query(
            "INSERT INTO accounts (id, display_name, password_hash, is_admin, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(display_name)
        .bind(password_hash)
        .bind(is_admin)
        .bind(created_at)
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Err(IdentityError::NameTaken);
            }
            Err(error) => return Err(error.into()),
        }
        let token = self.issue_token(id).await?;

        Ok((
            Account {
                id,
                display_name: display_name.to_owned(),
                is_admin,
            },
            token,
        ))
    }

    /// Checks a password and returns a device token.
    pub async fn login(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<(Account, String), IdentityError> {
        let row = sqlx::query(
            "SELECT id, display_name, is_admin, password_hash FROM accounts \
             WHERE display_name = ?",
        )
        .bind(display_name)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Err(IdentityError::InvalidCredentials);
        };
        let stored: String = row.try_get("password_hash")?;
        let parsed = PasswordHash::new(&stored).map_err(|_| IdentityError::Corrupt)?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| IdentityError::InvalidCredentials)?;

        let account = account_from_row(&row)?;
        let token = self.issue_token(account.id).await?;

        Ok((account, token))
    }

    /// Returns the account a device token belongs to.
    pub async fn authenticate(&self, token: &str) -> Result<Option<Account>, IdentityError> {
        let digest = digest(token);
        let row = sqlx::query(
            "SELECT a.id, a.display_name, a.is_admin FROM tokens t \
             JOIN accounts a ON a.id = t.account_id WHERE t.token_hash = ?",
        )
        .bind(digest.as_slice())
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(account_from_row).transpose()
    }

    async fn issue_token(&self, account: Id) -> Result<String, IdentityError> {
        let mut bytes = [0_u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let digest = digest(&token);
        let created_at = Timestamp::now().as_microseconds();
        sqlx::query("INSERT INTO tokens (token_hash, account_id, created_at) VALUES (?, ?, ?)")
            .bind(digest.as_slice())
            .bind(account.to_string())
            .bind(created_at)
            .execute(&self.pool)
            .await?;

        Ok(token)
    }
}

fn account_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Account, IdentityError> {
    let id: String = row.try_get("id")?;
    Ok(Account {
        id: id.parse().map_err(|_| IdentityError::Corrupt)?,
        display_name: row.try_get("display_name")?,
        is_admin: row.try_get("is_admin")?,
    })
}

fn validate_display_name(display_name: &str) -> Result<String, IdentityError> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(IdentityError::InvalidName);
    }

    Ok(trimmed.to_owned())
}

fn validate_password(password: &str) -> Result<(), IdentityError> {
    if password.len() < MIN_PASSWORD_BYTES {
        return Err(IdentityError::WeakPassword);
    }

    Ok(())
}

fn hash_password(password: &str) -> Result<String, IdentityError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string();

    Ok(hash)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Accounts or sign-in failed.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// The database failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The schema could not be created.
    #[error("schema error: {0}")]
    Schema(#[from] schema::SchemaError),
    /// A password could not be hashed.
    #[error("password hash error: {0}")]
    Hash(#[from] argon2::password_hash::Error),
    /// The name is empty or too long.
    #[error("display name is empty or too long")]
    InvalidName,
    /// The password is too short.
    #[error("password is shorter than {MIN_PASSWORD_BYTES} bytes")]
    WeakPassword,
    /// Another account already uses the name.
    #[error("display name is taken")]
    NameTaken,
    /// The Space already has its first account.
    #[error("the Space already has its Admin")]
    AlreadyInitialized,
    /// The name or password does not match.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// A stored value does not decode.
    #[error("identity record is corrupt")]
    Corrupt,
}

#[cfg(test)]
mod tests {
    use super::{Identity, IdentityError};
    use crate::home::Home;

    async fn identity() -> (tempfile::TempDir, Home, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let identity = Identity::open(&home).await.unwrap();

        (dir, home, identity)
    }

    #[tokio::test]
    async fn creates_the_admin_and_authenticates_its_token() {
        let (_dir, _home, identity) = identity().await;
        let (account, token) = identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();

        let authenticated = identity.authenticate(&token).await.unwrap();

        assert!(account.is_admin);
        assert_eq!(authenticated, Some(account));
    }

    #[tokio::test]
    async fn logs_in_with_the_password() {
        let (_dir, _home, identity) = identity().await;
        identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();

        let (account, token) = identity
            .login("Ada", "correct horse battery")
            .await
            .unwrap();

        assert_eq!(account.display_name, "Ada");
        assert!(identity.authenticate(&token).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rejects_a_wrong_password() {
        let (_dir, _home, identity) = identity().await;
        identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();

        let error = identity.login("Ada", "wrong password").await;

        assert!(matches!(error, Err(IdentityError::InvalidCredentials)));
    }

    #[tokio::test]
    async fn rejects_a_second_admin_and_a_taken_name() {
        let (_dir, _home, identity) = identity().await;
        identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();

        let second = identity
            .create_admin("Grace", "correct horse battery")
            .await;

        assert!(matches!(second, Err(IdentityError::AlreadyInitialized)));
        assert!(identity.has_accounts().await.unwrap());
    }

    #[tokio::test]
    async fn stores_only_the_token_digest() {
        let (_dir, _home, identity) = identity().await;
        let (_account, token) = identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();

        let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM tokens")
            .fetch_all(&identity.pool)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 32);
        assert_ne!(rows[0], token.as_bytes());
        assert!(
            identity
                .authenticate("not a token")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn validates_the_name_and_password() {
        let (_dir, _home, identity) = identity().await;

        assert!(matches!(
            identity.create_admin("   ", "correct horse battery").await,
            Err(IdentityError::InvalidName)
        ));
        assert!(matches!(
            identity.create_admin("Ada", "short").await,
            Err(IdentityError::WeakPassword)
        ));
    }
}
