//! Accounts, passwords, paired Computers, and device tokens.
//!
//! The first User joins with the Space's invitation and becomes the Admin.
//! A password is stored as an Argon2id hash, and a device token is stored only
//! as its SHA-256 digest.
//!
//! A real Computer pairs by showing a code: the machine keeps its token and
//! polling secret, the User enters the code, and the Space stores only their
//! digests.

use serde::Serialize;

use std::time::Duration;
use ts_rs::TS;

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha::time::Timestamp;
use rand::Rng as _;
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Row, SqlitePool};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::home::Home;
use crate::schema;

/// The shortest password a Space accepts.
pub const MIN_PASSWORD_BYTES: usize = 8;

/// The longest display name a Space accepts.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// How long a pairing code stays valid, in seconds.
pub const PAIRING_TTL_SECS: i64 = 600;

/// How many tries a pairing code allows before it is void.
pub const MAX_CODE_TRIES: i64 = 5;

/// How many pairings may wait for a User at once.
///
/// A machine whose code nobody entered holds a slot until the code expires.
/// At the cap a new pairing evicts the oldest waiting one instead of failing:
/// an unauthenticated caller can churn rows, but cannot lock a User out.
pub const MAX_PENDING_PAIRINGS: i64 = 16;

/// The most unused local tickets one owner may hold at once.
///
/// Issuing past the cap drops the oldest, so a caller that keeps asking for
/// tickets cannot make the table grow without bound.
pub const MAX_LOCAL_TICKETS_PER_OWNER: i64 = 8;

/// The bytes of random device identifier a pairing starts with.
const DEVICE_BYTES: usize = 16;

/// The characters of a pairing code.
const CODE_CHARS: usize = 8;

/// Base32 without the characters that look alike: `I` and `O`, `0` and `1`.
///
/// Eight characters from the 32 remaining ones carry 40 bits.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A person who can sign in to the Space.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[ts(export_to = "identity.ts")]
pub struct Account {
    /// The account identifier.
    pub id: Id,
    /// The name the person signs in with.
    pub display_name: String,
    /// Whether the account is the Space's Admin.
    pub is_admin: bool,
}

/// What a Space tells a machine when pairing starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingStart {
    /// The device identifier the machine polls with.
    pub device: String,
    /// The code the User enters.
    pub code: String,
}

/// What a machine learns when it polls its pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingState {
    /// No User has approved the machine yet.
    Pending,
    /// The Space bonded the machine to a Computer.
    Approved {
        /// The Computer the machine now controls.
        computer: Id,
    },
}

/// What the Space knows about a pairing before a User approves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingRequest {
    /// The device identifier the machine polls with.
    pub device: String,
    /// The name the machine gave itself.
    pub machine: String,
    /// The folder on the machine the Computer uses.
    pub workspace: String,
}

/// A pairing a User claimed, as the start-up reconciliation sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingClaim {
    /// The User who entered the code.
    pub owner: Id,
    /// The Computer the claim is for.
    pub computer: Id,
    /// Whether the machine's token was recorded (`finalize_pairing` ran).
    pub finalized: bool,
}

/// The Space's accounts, their tokens, and its paired Computers.
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
            .synchronous(SqliteSynchronous::Normal)
            // Pairing writes are short transactions, but two of them can meet
            // on the writer lock (a claim, a reconciliation, two machines
            // starting at once). Wait for the other side like the record store
            // does instead of surfacing SQLITE_BUSY as a 500.
            .busy_timeout(Duration::from_secs(5));
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
    #[cfg(feature = "test-support")]
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

    /// Starts pairing a real Computer.
    ///
    /// The machine generated `token` and `poll_secret` and keeps them; the
    /// Space stores only their digests. The returned device identifier and
    /// code appear once.
    pub async fn begin_pairing(
        &self,
        machine: &str,
        workspace: &str,
        token: &str,
        poll_secret: &str,
    ) -> Result<PairingStart, IdentityError> {
        self.begin_pairing_at(now(), machine, workspace, token, poll_secret)
            .await
    }

    /// Returns the state a machine polls for, or `None` when the device or
    /// secret is unknown, or no User approved the machine before the code
    /// expired. A poll never consumes the code.
    pub async fn pairing(
        &self,
        device: &str,
        poll_secret: &str,
    ) -> Result<Option<PairingState>, IdentityError> {
        self.pairing_at(now(), device, poll_secret).await
    }

    /// Looks up an unexpired, unclaimed code for a User to approve.
    ///
    /// Returns `None` when the code is unknown, already used, expired, or out
    /// of tries. A lookup that finds the pairing uses one try, so a code that
    /// is entered over and over stops working.
    pub async fn pairing_request(
        &self,
        code: &str,
    ) -> Result<Option<PairingRequest>, IdentityError> {
        self.pairing_request_at(now(), code).await
    }

    /// Claims a pairing for a User before the Computer is recorded.
    ///
    /// One transaction wins the code (`owner IS NULL` is the compare-and-set
    /// that stops two Users from claiming the same machine) and reserves the
    /// machine's token row, unfinalized. The token's primary key is then the
    /// database's word that no other Computer can take it, and the machine
    /// keeps polling pending. The caller records the Computer and
    /// [`finalize_pairing`](Self::finalize_pairing); if either fails it calls
    /// [`release_pairing`](Self::release_pairing) so the code works again.
    pub async fn reserve_pairing(
        &self,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError> {
        self.reserve_pairing_at(now(), code, owner, computer).await
    }

    /// Finishes a claim by marking the reserved token final.
    ///
    /// Returns `false` when the claim is gone (rolled back by a failed
    /// command, or by the start-up reconciliation). Idempotent: calling it
    /// again for the same Computer keeps one final token row. A pre-v3 claim
    /// has no reserved row, so this writes the final one to heal it.
    pub async fn finalize_pairing(&self, owner: Id, computer: Id) -> Result<bool, IdentityError> {
        self.finalize_pairing_at(now(), owner, computer).await
    }

    /// Rolls a claim back so the code can be claimed again.
    ///
    /// A failed command's claim left no record, so the machine must stay
    /// pending and the code must remain usable. Any token row written for the
    /// claim is removed as well, so authentication never names a Computer the
    /// record does not hold.
    pub async fn release_pairing(&self, owner: Id, computer: Id) -> Result<bool, IdentityError> {
        self.release_pairing_at(owner, computer).await
    }

    /// Moves a claim past its expiry, for tests that exercise recovery.
    ///
    /// Production code never does this: it only makes a row that a real clock
    /// would have expired reachable without waiting the ten minutes.
    #[cfg(feature = "test-support")]
    pub(crate) async fn expire_pairing_for_test(
        &self,
        owner: Id,
        computer: Id,
    ) -> Result<(), IdentityError> {
        sqlx::query("UPDATE computer_codes SET expires_at = 0 WHERE owner = ? AND computer = ?")
            .bind(owner.to_string())
            .bind(computer.to_string())
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Returns every pairing that a User claimed, final or not.
    ///
    /// The Space reconciles these with its record at start: a claim whose
    /// Computer is not recorded is rolled back, and a recorded Computer whose
    /// claim is not final is finished. That closes both crash windows between
    /// claiming a code, appending the record, and writing the token.
    pub async fn pairing_claims(&self) -> Result<Vec<PairingClaim>, IdentityError> {
        let rows = sqlx::query(
            "SELECT c.owner, c.computer, \
             EXISTS(SELECT 1 FROM computer_tokens t \
                    WHERE t.token_hash = c.token_hash AND t.computer_id = c.computer \
                      AND t.finalized = 1) AS finalized \
             FROM computer_codes c WHERE c.owner IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            let owner: Option<String> = row.try_get("owner")?;
            let computer: Option<String> = row.try_get("computer")?;
            let (Some(owner), Some(computer)) = (owner, computer) else {
                return Err(IdentityError::Corrupt);
            };
            claims.push(PairingClaim {
                owner: owner.parse().map_err(|_| IdentityError::Corrupt)?,
                computer: computer.parse().map_err(|_| IdentityError::Corrupt)?,
                finalized: row.try_get("finalized")?,
            });
        }

        Ok(claims)
    }

    /// Returns the Space's Admin, once the first User has joined.
    pub async fn admin_account(&self) -> Result<Option<Account>, IdentityError> {
        let row = sqlx::query(
            "SELECT id, display_name, is_admin FROM accounts WHERE is_admin = 1 LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(account_from_row).transpose()
    }

    /// Forgets every local ticket.
    ///
    /// A local ticket is bound to one start of the Space, so a new start
    /// forgets whatever a previous one left before it issues its own.
    pub async fn clear_local_tickets(&self) -> Result<(), IdentityError> {
        sqlx::query("DELETE FROM computer_tickets")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Stores the digest of a local ticket for `ttl`, owned by `owner`.
    ///
    /// Returns the digest, which lets the caller delete the exact file it
    /// wrote. Every issue also drops tickets that have expired and keeps at
    /// most [`MAX_LOCAL_TICKETS_PER_OWNER`] unused ones per owner, so a caller
    /// that keeps asking cannot make stale rows accumulate.
    pub async fn remember_local_ticket(
        &self,
        ticket: &str,
        owner: Id,
        ttl: Duration,
    ) -> Result<[u8; 32], IdentityError> {
        let ticket_hash = digest(ticket);
        let owner_bytes = owner.to_string().into_bytes();
        let mut connection = self.pool.acquire().await?;
        begin_immediate(&mut connection).await?;
        let result = async {
            let now = now();
            let expires_at = now.saturating_add(i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX));
            sqlx::query("DELETE FROM computer_tickets WHERE expires_at <= ?")
                .bind(now)
                .execute(&mut *connection)
                .await?;
            let held: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM computer_tickets WHERE owner = ?")
                    .bind(&owner_bytes)
                    .fetch_one(&mut *connection)
                    .await?;
            if held >= MAX_LOCAL_TICKETS_PER_OWNER {
                let excess = held - (MAX_LOCAL_TICKETS_PER_OWNER - 1);
                sqlx::query(
                    "DELETE FROM computer_tickets WHERE ticket_hash IN (\
                     SELECT ticket_hash FROM computer_tickets WHERE owner = ? \
                     ORDER BY created_at ASC, rowid ASC LIMIT ?)",
                )
                .bind(&owner_bytes)
                .bind(excess)
                .execute(&mut *connection)
                .await?;
            }
            sqlx::query(
                "INSERT INTO computer_tickets (ticket_hash, owner, created_at, expires_at) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(ticket_hash.as_slice())
            .bind(&owner_bytes)
            .bind(now)
            .bind(expires_at)
            .execute(&mut *connection)
            .await?;

            Ok(ticket_hash)
        }
        .await;

        finish_immediate(&mut connection, result).await
    }

    /// Claims a local ticket, returning its owner and digest once.
    ///
    /// An unknown, expired, or already claimed ticket gives `None`. Expired
    /// rows are dropped in the same immediate transaction, so the delete and
    /// the expiry check cannot race another pairing.
    pub async fn claim_local_ticket(
        &self,
        ticket: &str,
    ) -> Result<Option<(Id, [u8; 32])>, IdentityError> {
        let ticket_hash = digest(ticket);
        let now = now();
        let mut connection = self.pool.acquire().await?;
        begin_immediate(&mut connection).await?;
        let result = async {
            sqlx::query("DELETE FROM computer_tickets WHERE expires_at <= ?")
                .bind(now)
                .execute(&mut *connection)
                .await?;
            let owner: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT owner FROM computer_tickets WHERE ticket_hash = ?")
                    .bind(ticket_hash.as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some(owner) = owner else {
                return Ok(None);
            };
            let owner = String::from_utf8(owner).map_err(|_| IdentityError::Corrupt)?;
            let deleted = sqlx::query("DELETE FROM computer_tickets WHERE ticket_hash = ?")
                .bind(ticket_hash.as_slice())
                .execute(&mut *connection)
                .await?;
            if deleted.rows_affected() != 1 {
                return Ok(None);
            }
            let owner = owner.parse::<Id>().map_err(|_| IdentityError::Corrupt)?;

            Ok(Some((owner, ticket_hash)))
        }
        .await;

        finish_immediate(&mut connection, result).await
    }

    /// Deletes tokens whose Computer the record does not hold.
    ///
    /// A pairing writes its token before the `ComputerCreated` event and rolls
    /// it back when the append fails, but a crash between the two can leave a
    /// final token with no Computer. The record is the truth: anything the
    /// replay no longer names is forgotten here, before the Space serves.
    /// Returns how many rows were removed.
    pub async fn forget_unrecorded_computer_tokens(
        &self,
        recorded: &[Id],
    ) -> Result<usize, IdentityError> {
        let computers: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT computer_id FROM computer_tokens")
                .fetch_all(&self.pool)
                .await?;
        let recorded: std::collections::HashSet<String> =
            recorded.iter().map(ToString::to_string).collect();
        let mut removed = 0;
        for computer in computers {
            if !recorded.contains(&computer) {
                removed += sqlx::query("DELETE FROM computer_tokens WHERE computer_id = ?")
                    .bind(&computer)
                    .execute(&self.pool)
                    .await?
                    .rows_affected() as usize;
            }
        }

        Ok(removed)
    }

    /// Records the final token of a Computer created by a local pairing.
    ///
    /// Local pairing has no device code to wait on, so the token is final at
    /// once; the record's `ComputerCreated` is what makes it meaningful.
    pub async fn record_computer_token(
        &self,
        computer: Id,
        credential: &str,
    ) -> Result<(), IdentityError> {
        sqlx::query(
            "INSERT INTO computer_tokens (token_hash, computer_id, created_at, finalized) \
             VALUES (?, ?, ?, 1)",
        )
        .bind(digest(credential).as_slice())
        .bind(computer.to_string())
        .bind(now())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Forgets a Computer token whose Computer could not be recorded.
    pub async fn forget_computer_token(&self, credential: &str) -> Result<(), IdentityError> {
        sqlx::query("DELETE FROM computer_tokens WHERE token_hash = ?")
            .bind(digest(credential).as_slice())
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Returns the Computer a machine token belongs to.
    pub async fn authenticate_computer(&self, token: &str) -> Result<Option<Id>, IdentityError> {
        let digest = digest(token);
        let computer: Option<String> = sqlx::query_scalar(
            "SELECT computer_id FROM computer_tokens WHERE token_hash = ? AND finalized = 1",
        )
        .bind(digest.as_slice())
        .fetch_optional(&self.pool)
        .await?;

        computer
            .map(|computer| computer.parse().map_err(|_| IdentityError::Corrupt))
            .transpose()
    }

    async fn begin_pairing_at(
        &self,
        now: i64,
        machine: &str,
        workspace: &str,
        token: &str,
        poll_secret: &str,
    ) -> Result<PairingStart, IdentityError> {
        let device = random_base64(DEVICE_BYTES);
        let code = random_code();
        let code_hash = digest(&code);
        let poll_hash = digest(poll_secret);
        let token_hash = digest(token);
        let expires_at = now.saturating_add(PAIRING_TTL_SECS.saturating_mul(1_000_000));
        // One write transaction holds the quota check and the insert together:
        // `BEGIN IMMEDIATE` takes the write lock up front, so two machines
        // starting at once cannot both read a count under the cap and insert.
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await?;
        let inserted = async {
            // An expired pairing can never be approved again, so its row stops
            // holding a place. Only an unclaimed row is cleaned up here: a
            // claim belongs to a command or the start-up reconciliation, never
            // to the expiry sweeper.
            sqlx::query("DELETE FROM computer_codes WHERE expires_at <= ? AND owner IS NULL")
                .bind(now)
                .execute(&mut *connection)
                .await?;
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM computer_codes WHERE owner IS NULL AND expires_at > ?",
            )
            .bind(now)
            .fetch_one(&mut *connection)
            .await?;
            if waiting >= MAX_PENDING_PAIRINGS {
                // The cap must not let a caller who never approves anything
                // lock Users out: evict the oldest waiting pairing and carry
                // on. Claimed pairings are never evicted.
                sqlx::query(
                    "DELETE FROM computer_codes WHERE device_id = (\
                     SELECT device_id FROM computer_codes \
                     WHERE owner IS NULL AND expires_at > ? \
                     ORDER BY created_at ASC, device_id ASC LIMIT 1)",
                )
                .bind(now)
                .execute(&mut *connection)
                .await?;
            }
            sqlx::query(
                "INSERT INTO computer_codes (\
                 device_id, code_hash, poll_hash, token_hash, machine, workspace, \
                 owner, computer, created_at, expires_at, tries) \
                 VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, 0)",
            )
            .bind(&device)
            .bind(code_hash.as_slice())
            .bind(poll_hash.as_slice())
            .bind(token_hash.as_slice())
            .bind(machine)
            .bind(workspace)
            .bind(now)
            .bind(expires_at)
            .execute(&mut *connection)
            .await?;

            Ok(())
        }
        .await;
        match inserted {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(PairingStart { device, code })
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn pairing_at(
        &self,
        now: i64,
        device: &str,
        poll_secret: &str,
    ) -> Result<Option<PairingState>, IdentityError> {
        let row = sqlx::query(
            "SELECT poll_hash, token_hash, owner, computer, expires_at FROM computer_codes \
             WHERE device_id = ?",
        )
        .bind(device)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };

        let poll_hash: Vec<u8> = row.try_get("poll_hash")?;
        let matches: bool = digest(poll_secret)
            .as_slice()
            .ct_eq(poll_hash.as_slice())
            .into();
        if !matches {
            return Ok(None);
        }

        // A pairing outlives its usefulness with its code: even an approved
        // row stops answering once the pairing expired. The Computer and its
        // token stay usable; only the poll answer goes away.
        let expires_at: i64 = row.try_get("expires_at")?;
        if now >= expires_at {
            return Ok(None);
        }
        let token_hash: Vec<u8> = row.try_get("token_hash")?;
        let owner: Option<String> = row.try_get("owner")?;
        let computer: Option<String> = row.try_get("computer")?;
        match (owner, computer) {
            (Some(_), Some(computer)) => {
                let computer: Id = computer.parse().map_err(|_| IdentityError::Corrupt)?;
                // A claim that was not finished has no token row: the machine
                // stays pending until the Space recorded its Computer and
                // finalized the pairing.
                let finalized: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM computer_tokens \
                     WHERE token_hash = ? AND computer_id = ? AND finalized = 1)",
                )
                .bind(token_hash.as_slice())
                .bind(computer.to_string())
                .fetch_one(&self.pool)
                .await?;
                if finalized {
                    Ok(Some(PairingState::Approved { computer }))
                } else {
                    Ok(Some(PairingState::Pending))
                }
            }
            (None, None) => Ok(Some(PairingState::Pending)),
            _ => Err(IdentityError::Corrupt),
        }
    }

    async fn pairing_request_at(
        &self,
        now: i64,
        code: &str,
    ) -> Result<Option<PairingRequest>, IdentityError> {
        let code_hash = digest(&normalize_code(code));
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE computer_codes SET tries = tries + 1 \
             WHERE code_hash = ? AND owner IS NULL AND expires_at > ? AND tries < ?",
        )
        .bind(code_hash.as_slice())
        .bind(now)
        .bind(MAX_CODE_TRIES)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT device_id, machine, workspace FROM computer_codes WHERE code_hash = ?",
        )
        .bind(code_hash.as_slice())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(Some(PairingRequest {
            device: row.try_get("device_id")?,
            machine: row.try_get("machine")?,
            workspace: row.try_get("workspace")?,
        }))
    }

    async fn reserve_pairing_at(
        &self,
        now: i64,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError> {
        let code_hash = digest(&normalize_code(code));
        let owner = owner.to_string();
        let computer = computer.to_string();
        let mut connection = self.pool.acquire().await?;
        begin_immediate(&mut connection).await?;
        let result = async {
            // The compare-and-set is the claim: one update wins the row, and
            // no other statement can run between the check and the write.
            let updated = sqlx::query(
                "UPDATE computer_codes SET owner = ?, computer = ? \
                 WHERE code_hash = ? AND owner IS NULL AND expires_at > ? AND tries < ?",
            )
            .bind(&owner)
            .bind(&computer)
            .bind(code_hash.as_slice())
            .bind(now)
            .bind(MAX_CODE_TRIES)
            .execute(&mut *connection)
            .await?;
            if updated.rows_affected() != 1 {
                return Ok(false);
            }
            let token_hash: Vec<u8> = sqlx::query_scalar(
                "SELECT token_hash FROM computer_codes WHERE owner = ? AND computer = ?",
            )
            .bind(&owner)
            .bind(&computer)
            .fetch_one(&mut *connection)
            .await?;
            // Reserve the machine's token here, before the Computer is
            // published. The primary key is the uniqueness constraint: if the
            // token already belongs to another claim or Computer, the reserve
            // fails and the whole claim rolls back, so no Computer can be
            // recorded with a token it cannot own.
            let inserted = sqlx::query(
                "INSERT INTO computer_tokens (token_hash, computer_id, created_at, finalized) \
                 VALUES (?, ?, ?, 0)",
            )
            .bind(token_hash.as_slice())
            .bind(&computer)
            .bind(now)
            .execute(&mut *connection)
            .await;
            match inserted {
                Ok(_) => {}
                Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                    return Err(IdentityError::TokenTaken);
                }
                Err(error) => return Err(error.into()),
            }

            Ok(true)
        }
        .await;

        finish_immediate(&mut connection, result).await
    }

    async fn finalize_pairing_at(
        &self,
        now: i64,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError> {
        let owner = owner.to_string();
        let computer = computer.to_string();
        let mut connection = self.pool.acquire().await?;
        begin_immediate(&mut connection).await?;
        let result = async {
            let token_hash: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT token_hash FROM computer_codes WHERE owner = ? AND computer = ?",
            )
            .bind(&owner)
            .bind(&computer)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(token_hash) = token_hash else {
                return Ok(false);
            };
            let updated = sqlx::query(
                "UPDATE computer_tokens SET finalized = 1 WHERE token_hash = ? AND computer_id = ?",
            )
            .bind(token_hash.as_slice())
            .bind(&computer)
            .execute(&mut *connection)
            .await?;
            if updated.rows_affected() == 0 {
                // No token row: either this claim predates the reserve (a
                // version 2 database's crash window) or the token belongs to
                // another Computer. The latter is refused without saying
                // which.
                let bound: Option<String> = sqlx::query_scalar(
                    "SELECT computer_id FROM computer_tokens WHERE token_hash = ?",
                )
                .bind(token_hash.as_slice())
                .fetch_optional(&mut *connection)
                .await?;
                match bound.as_deref() {
                    None => {
                        let recovered = sqlx::query(
                            "INSERT INTO computer_tokens \
                             (token_hash, computer_id, created_at, finalized) \
                             VALUES (?, ?, ?, 1) ON CONFLICT(token_hash) DO NOTHING",
                        )
                        .bind(token_hash.as_slice())
                        .bind(&computer)
                        .bind(now)
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                        if recovered != 1 {
                            return Err(IdentityError::ClaimNotFinishable);
                        }
                    }
                    Some(bound) if bound == computer => {}
                    Some(_) => return Err(IdentityError::ClaimNotFinishable),
                }
            }

            Ok(true)
        }
        .await;

        finish_immediate(&mut connection, result).await
    }

    async fn release_pairing_at(&self, owner: Id, computer: Id) -> Result<bool, IdentityError> {
        let owner = owner.to_string();
        let computer = computer.to_string();
        let mut connection = self.pool.acquire().await?;
        begin_immediate(&mut connection).await?;
        let result = async {
            let token_hash: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT token_hash FROM computer_codes WHERE owner = ? AND computer = ?",
            )
            .bind(&owner)
            .bind(&computer)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(token_hash) = token_hash else {
                return Ok(false);
            };
            sqlx::query(
                "UPDATE computer_codes SET owner = NULL, computer = NULL \
                 WHERE owner = ? AND computer = ?",
            )
            .bind(&owner)
            .bind(&computer)
            .execute(&mut *connection)
            .await?;
            // A claim is only released when its Computer is not recorded, and
            // a recorded Computer is never released, so a token row here is
            // stale.
            sqlx::query("DELETE FROM computer_tokens WHERE token_hash = ? AND computer_id = ?")
                .bind(token_hash.as_slice())
                .bind(&computer)
                .execute(&mut *connection)
                .await?;

            Ok(true)
        }
        .await;

        finish_immediate(&mut connection, result).await
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

/// Takes the write lock up front for a write transaction.
///
/// `sqlx`'s `begin()` starts a deferred transaction. In WAL mode a deferred
/// transaction that reads first and writes after another writer commits is
/// refused with SQLITE_BUSY immediately: the busy handler does not retry an
/// expired snapshot. `BEGIN IMMEDIATE` waits for the writer lock instead (up
/// to the busy timeout set in [`Identity::open`]).
async fn begin_immediate(connection: &mut sqlx::SqliteConnection) -> Result<(), IdentityError> {
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await?;

    Ok(())
}

/// Commits `result`'s transaction, or rolls it back when it carries an error.
async fn finish_immediate<T>(
    connection: &mut sqlx::SqliteConnection,
    result: Result<T, IdentityError>,
) -> Result<T, IdentityError> {
    match result {
        Ok(value) => {
            sqlx::query("COMMIT").execute(&mut *connection).await?;
            Ok(value)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
            Err(error)
        }
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

fn now() -> i64 {
    Timestamp::now().as_microseconds()
}

fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

fn random_code() -> String {
    let mut rng = rand::rng();

    (0..CODE_CHARS)
        .map(|_| CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

fn random_base64(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut value);

    URL_SAFE_NO_PAD.encode(value)
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
    /// A pairing claim cannot be finished: its code row is gone, or its token
    /// already belongs to another Computer.
    #[error("the pairing claim cannot be finished")]
    ClaimNotFinishable,
    /// The machine's token already belongs to another claim or Computer.
    #[error("the machine token is already in use")]
    TokenTaken,
}

/// What the integration tests need from inside this module.
///
/// Integration tests live in `tests/` and see only the crate's public
/// interface. The fixtures and accessors for the internals the tests inspect
/// live here, behind the `test-support` feature, so a production build keeps
/// them out.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use neha::id::Id;

    use super::{Account, Identity, IdentityError, PairingStart};

    /// The characters a pairing code is drawn from.
    pub const CODE_ALPHABET: &[u8] = super::CODE_ALPHABET;

    /// The statements that create a version-one record.
    pub const VERSION_ONE_STATEMENTS: [&str; 7] = crate::schema::VERSION_ONE_STATEMENTS;

    /// The statements that upgrade a version-one record to version two.
    pub const VERSION_TWO_STATEMENTS: [&str; 2] = crate::schema::VERSION_TWO_STATEMENTS;

    /// Hashes a token the way the identity store does.
    pub fn digest(token: &str) -> [u8; 32] {
        super::digest(token)
    }

    /// The connection pool an identity test queries directly.
    pub fn pool(identity: &Identity) -> &sqlx::SqlitePool {
        &identity.pool
    }

    impl Identity {
        /// Adds an account beyond the Admin, as an invitation will; for tests.
        pub async fn test_add_account_for_test(
            &self,
            display_name: &str,
            password: &str,
        ) -> Result<Account, IdentityError> {
            self.add_account_for_test(display_name, password).await
        }

        /// Moves a claim past its expiry, for tests that exercise recovery.
        pub async fn test_expire_pairing_for_test(
            &self,
            owner: Id,
            computer: Id,
        ) -> Result<(), IdentityError> {
            self.expire_pairing_for_test(owner, computer).await
        }

        /// Starts a pairing at an explicit instant.
        pub async fn test_begin_pairing_at(
            &self,
            now: i64,
            machine: &str,
            workspace: &str,
            token: &str,
            poll_secret: &str,
        ) -> Result<PairingStart, IdentityError> {
            self.begin_pairing_at(now, machine, workspace, token, poll_secret)
                .await
        }

        /// Answers one pairing request at an explicit instant.
        pub async fn test_pairing_request_at(
            &self,
            now: i64,
            code: &str,
        ) -> Result<Option<super::PairingRequest>, IdentityError> {
            self.pairing_request_at(now, code).await
        }

        /// Returns the state a machine polls for at an explicit instant.
        pub async fn test_pairing_at(
            &self,
            now: i64,
            device: &str,
            poll_secret: &str,
        ) -> Result<Option<super::PairingState>, IdentityError> {
            self.pairing_at(now, device, poll_secret).await
        }

        /// Reserves a pairing claim at an explicit instant.
        pub async fn test_reserve_pairing_at(
            &self,
            now: i64,
            code: &str,
            owner: Id,
            computer: Id,
        ) -> Result<bool, IdentityError> {
            self.reserve_pairing_at(now, code, owner, computer).await
        }
    }
}
