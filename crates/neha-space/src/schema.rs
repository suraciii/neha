//! The Space database schema.
//!
//! One SQLite database holds the record and the Space's secret-adjacent
//! tables. A home names the database file after the schema version.

use sqlx::SqliteConnection;
use thiserror::Error;

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = 1;

const STATEMENTS: [&str; 7] = [
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "INSERT INTO meta (key, value) VALUES ('last_seq', '0')",
    "CREATE TABLE events (\
        seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, class TEXT NOT NULL, \
        version INTEGER NOT NULL, at INTEGER NOT NULL, author TEXT NOT NULL, \
        acting TEXT, command TEXT, data TEXT NOT NULL)",
    "CREATE INDEX events_command ON events(command)",
    "CREATE TABLE commands (\
        command_id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, \
        first_seq INTEGER NOT NULL, last_seq INTEGER NOT NULL, result TEXT)",
    "CREATE TABLE accounts (\
        id TEXT PRIMARY KEY, display_name TEXT NOT NULL UNIQUE, \
        password_hash TEXT NOT NULL, is_admin INTEGER NOT NULL, \
        created_at INTEGER NOT NULL)",
    "CREATE TABLE tokens (\
        token_hash BLOB PRIMARY KEY, account_id TEXT NOT NULL, \
        created_at INTEGER NOT NULL)",
];

/// Creates or upgrades the Space database.
pub async fn migrate(connection: &mut SqliteConnection) -> Result<(), SchemaError> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *connection)
        .await?;
    if version > SCHEMA_VERSION {
        return Err(SchemaError::TooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    for statement in STATEMENTS {
        sqlx::query(statement).execute(&mut *connection).await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {SCHEMA_VERSION}"
    )))
    .execute(&mut *connection)
    .await?;

    Ok(())
}

/// The database schema cannot be created.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// SQLite failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The database is newer than this build.
    #[error("schema version {found} is newer than {supported}")]
    TooNew {
        /// The version found.
        found: i64,
        /// The version supported.
        supported: i64,
    },
}
