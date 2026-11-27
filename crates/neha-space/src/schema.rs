//! The Space database schema.
//!
//! One SQLite database holds the record and the Space's secret-adjacent
//! tables. A home keeps it at a fixed name; the schema version lives inside
//! the database and is upgraded in place.

use sqlx::{Connection, SqliteConnection};
use thiserror::Error;

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = 5;

/// The statements that create the tables of schema version 1.
///
/// A database at version 1 already has them; a new database runs them too.
pub(crate) const VERSION_ONE_STATEMENTS: [&str; 7] = [
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

/// The statements that create the tables schema version 2 adds.
pub(crate) const VERSION_TWO_STATEMENTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS computer_codes (\
        device_id TEXT PRIMARY KEY, code_hash BLOB NOT NULL, \
        poll_hash BLOB NOT NULL, token_hash BLOB NOT NULL, \
        machine TEXT NOT NULL, workspace TEXT NOT NULL, \
        owner TEXT, computer TEXT, \
        created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, \
        tries INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS computer_tokens (\
        token_hash BLOB PRIMARY KEY, computer_id TEXT NOT NULL, \
        created_at INTEGER NOT NULL)",
];

/// The statements schema version 3 adds.
///
/// A Computer token is reserved with its claim, before the Computer is
/// published: `finalized` starts at 0 and only `finalize_pairing` makes the
/// token usable. Rows written before this version were all final, so the
/// column's default upgrades them to 1; every new insert names the value.
const VERSION_THREE_STATEMENTS: [&str; 1] =
    ["ALTER TABLE computer_tokens ADD COLUMN finalized INTEGER NOT NULL DEFAULT 1"];

/// The statements schema version 4 adds.
///
/// A local pairing ticket is stored only by its digest, for one start of the
/// Space; the clear table starts empty on every start.
const VERSION_FOUR_STATEMENTS: [&str; 1] = ["CREATE TABLE IF NOT EXISTS computer_tickets (\
        ticket_hash BLOB PRIMARY KEY, owner BLOB NOT NULL, \
        created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL)"];

/// The statements schema version 5 adds.
///
/// Provider-native state is tied to one Model reply. It is not part of the
/// record: losing it costs only the reasoning context of a resumed run, so it
/// lives beside the events as a cache rather than as an Authority event.
const VERSION_FIVE_STATEMENTS: [&str; 1] = [
    "CREATE TABLE IF NOT EXISTS run_continuations (        run TEXT NOT NULL, step INTEGER NOT NULL, protocol TEXT NOT NULL,         provider TEXT NOT NULL, model TEXT NOT NULL, state TEXT NOT NULL,         PRIMARY KEY (run, step))",
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

    let mut transaction = connection.begin().await?;
    match version {
        0 => {
            for statement in VERSION_ONE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_TWO_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_THREE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FOUR_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FIVE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
        }
        1 => {
            // A version 1 database has the record and the account tables;
            // only the Computer tables are new.
            for statement in VERSION_TWO_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_THREE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FOUR_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FIVE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
        }
        2 => {
            // Version 2 paired Computers: their token rows were final, which
            // the new column's default preserves.
            for statement in VERSION_THREE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FOUR_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FIVE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
        }
        3 => {
            // Version 3 pairings are final tokens; local tickets are new and
            // no pairing data is touched.
            for statement in VERSION_FOUR_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
            for statement in VERSION_FIVE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
        }
        4 => {
            // Version 4 has every table but the continuation cache.
            for statement in VERSION_FIVE_STATEMENTS {
                sqlx::query(statement).execute(&mut *transaction).await?;
            }
        }
        _ => {
            return Err(SchemaError::TooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {SCHEMA_VERSION}"
    )))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

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

#[cfg(test)]
mod tests {
    use super::{
        SCHEMA_VERSION, SchemaError, VERSION_FIVE_STATEMENTS, VERSION_FOUR_STATEMENTS,
        VERSION_ONE_STATEMENTS, VERSION_THREE_STATEMENTS, VERSION_TWO_STATEMENTS, migrate,
    };
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, SqliteConnection};

    const TABLES: [&str; 9] = [
        "meta",
        "events",
        "commands",
        "accounts",
        "tokens",
        "computer_codes",
        "computer_tokens",
        "computer_tickets",
        "run_continuations",
    ];

    async fn connection() -> SqliteConnection {
        SqliteConnection::connect_with(&SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap()
    }

    async fn user_version(connection: &mut SqliteConnection) -> i64 {
        sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *connection)
            .await
            .unwrap()
    }

    async fn table_exists(connection: &mut SqliteConnection, table: &str) -> bool {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(table)
                .fetch_optional(&mut *connection)
                .await
                .unwrap();

        found.is_some()
    }

    async fn connection_at_version_two() -> SqliteConnection {
        let mut connection = connection_at_version_one().await;
        for statement in VERSION_TWO_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut connection)
            .await
            .unwrap();

        connection
    }

    async fn connection_at_version_three() -> SqliteConnection {
        let mut connection = connection_at_version_two().await;
        for statement in VERSION_THREE_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 3")
            .execute(&mut connection)
            .await
            .unwrap();

        connection
    }

    async fn connection_at_version_one() -> SqliteConnection {
        let mut connection = connection().await;
        for statement in VERSION_ONE_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut connection)
            .await
            .unwrap();

        connection
    }

    #[tokio::test]
    async fn creates_every_table_from_an_empty_database() {
        let mut connection = connection().await;

        migrate(&mut connection).await.unwrap();

        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        for table in TABLES {
            assert!(table_exists(&mut connection, table).await, "no {table}");
        }
        migrate(&mut connection).await.unwrap();
        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        // Any token row written without the column is final: that keeps a
        // version 2 database's paired Machines working after the upgrade.
        sqlx::query(
            "INSERT INTO computer_tokens (token_hash, computer_id, created_at) \
             VALUES (x'00', 'legacy', 0)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        let finalized: i64 = sqlx::query_scalar(
            "SELECT finalized FROM computer_tokens WHERE computer_id = 'legacy'",
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(finalized, 1);
    }

    #[tokio::test]
    async fn upgrades_a_version_two_database_and_keeps_its_tokens_final() {
        let mut connection = connection_at_version_two().await;
        sqlx::query(
            "INSERT INTO computer_tokens (token_hash, computer_id, created_at) \
             VALUES (x'00', 'computer', 0)",
        )
        .execute(&mut connection)
        .await
        .unwrap();

        migrate(&mut connection).await.unwrap();

        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        let finalized: i64 = sqlx::query_scalar(
            "SELECT finalized FROM computer_tokens WHERE computer_id = 'computer'",
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(
            finalized, 1,
            "a token written before the column existed was already final"
        );
    }

    #[tokio::test]
    async fn upgrades_a_version_three_database_and_keeps_its_pairings() {
        let mut connection = connection_at_version_three().await;
        sqlx::query(
            "INSERT INTO computer_tokens (token_hash, computer_id, created_at, finalized) \
             VALUES (x'00', 'computer', 0, 1)",
        )
        .execute(&mut connection)
        .await
        .unwrap();

        migrate(&mut connection).await.unwrap();

        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        assert!(table_exists(&mut connection, "computer_tickets").await);
        assert!(table_exists(&mut connection, "computer_codes").await);
        let owner_type: String = sqlx::query_scalar(
            "SELECT type FROM pragma_table_info('computer_tickets') WHERE name = 'owner'",
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(owner_type.to_uppercase(), "BLOB");
        assert!(VERSION_FOUR_STATEMENTS.len() == 1);
        let token: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM computer_tokens WHERE computer_id = 'computer' AND finalized = 1",
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(token, 1, "pairing data survives the upgrade");
        let tickets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_tickets")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(tickets, 0);
    }

    #[tokio::test]
    async fn upgrades_a_version_one_database_without_touching_its_rows() {
        let mut connection = connection_at_version_one().await;
        sqlx::query(
            "INSERT INTO accounts (id, display_name, password_hash, is_admin, created_at) \
             VALUES ('account', 'Ada', 'unused', 1, 0)",
        )
        .execute(&mut connection)
        .await
        .unwrap();

        migrate(&mut connection).await.unwrap();

        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        assert!(table_exists(&mut connection, "computer_codes").await);
        assert!(table_exists(&mut connection, "computer_tokens").await);
        let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(accounts, 1);
    }

    #[tokio::test]
    async fn refuses_a_database_from_a_newer_build() {
        let mut connection = connection().await;
        let newer = SCHEMA_VERSION + 1;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA user_version = {newer}"
        )))
        .execute(&mut connection)
        .await
        .unwrap();

        let error = migrate(&mut connection).await;

        assert!(matches!(
            error,
            Err(SchemaError::TooNew { found, supported })
                if found == newer && supported == SCHEMA_VERSION
        ));
    }

    #[tokio::test]
    async fn upgrades_a_version_four_database_with_the_continuation_cache() {
        let mut connection = connection_at_version_three().await;
        for statement in VERSION_FOUR_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 4")
            .execute(&mut connection)
            .await
            .unwrap();

        migrate(&mut connection).await.unwrap();

        assert_eq!(user_version(&mut connection).await, SCHEMA_VERSION);
        assert!(table_exists(&mut connection, "run_continuations").await);
        assert!(VERSION_FIVE_STATEMENTS.len() == 1);
    }
}
