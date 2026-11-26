//! Accounts, passwords, paired Computers, and device tokens.
//!
//! The first User joins with the Space's invitation and becomes the Admin.
//! A password is stored as an Argon2id hash, and a device token is stored only
//! as its SHA-256 digest.
//!
//! A real Computer pairs by showing a code: the machine keeps its token and
//! polling secret, the User enters the code, and the Space stores only their
//! digests.

use std::time::Duration;

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

/// The bytes of random device identifier a pairing starts with.
const DEVICE_BYTES: usize = 16;

/// The characters of a pairing code.
const CODE_CHARS: usize = 8;

/// Base32 without the characters that look alike: `I` and `O`, `0` and `1`.
///
/// Eight characters from the 32 remaining ones carry 40 bits.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

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
    #[cfg(test)]
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Account, CODE_ALPHABET, Identity, IdentityError, MAX_CODE_TRIES, MAX_PENDING_PAIRINGS,
        PAIRING_TTL_SECS, PairingClaim, PairingRequest, PairingState, digest,
    };
    use crate::home::Home;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use neha::id::Id;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, SqliteConnection};

    async fn identity() -> (tempfile::TempDir, Home, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let identity = Identity::open(&home).await.unwrap();

        (dir, home, identity)
    }

    /// Claims and finishes a pairing the way the command does, for tests that
    /// only need a bonded Computer.
    ///
    /// The command's real order is reserve, finalize, append; these tests have
    /// no record to append, so the two identity steps run together.
    trait ClaimPairing {
        async fn claim_pairing(
            &self,
            code: &str,
            owner: Id,
            computer: Id,
        ) -> Result<bool, IdentityError>;
        async fn claim_pairing_at(
            &self,
            now: i64,
            code: &str,
            owner: Id,
            computer: Id,
        ) -> Result<bool, IdentityError>;
    }

    impl ClaimPairing for Identity {
        async fn claim_pairing(
            &self,
            code: &str,
            owner: Id,
            computer: Id,
        ) -> Result<bool, IdentityError> {
            if !self.reserve_pairing(code, owner, computer).await? {
                return Ok(false);
            }
            self.finalize_pairing(owner, computer).await
        }

        async fn claim_pairing_at(
            &self,
            now: i64,
            code: &str,
            owner: Id,
            computer: Id,
        ) -> Result<bool, IdentityError> {
            if !self.reserve_pairing_at(now, code, owner, computer).await? {
                return Ok(false);
            }
            self.finalize_pairing(owner, computer).await
        }
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

    #[tokio::test]
    async fn shows_a_pairing_code_once_and_uses_it_once() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/home/ada/neha", "machine-token", "poll-secret")
            .await
            .unwrap();

        let request = identity.pairing_request(&start.code).await.unwrap();

        assert_eq!(
            request,
            Some(PairingRequest {
                device: start.device.clone(),
                machine: "laptop".to_owned(),
                workspace: "/home/ada/neha".to_owned(),
            })
        );
        assert_eq!(
            identity.pairing_request("ZZZZZZZZ").await.unwrap(),
            None,
            "an unknown code names no pairing"
        );
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .claim_pairing(&start.code, owner, computer)
                .await
                .unwrap()
        );
        assert!(
            identity
                .pairing_request(&start.code)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !identity
                .claim_pairing(&start.code, owner, Id::new())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_reserved_pairing_is_pending_until_it_is_finalized() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();

        assert!(
            identity
                .reserve_pairing(&start.code, owner, computer)
                .await
                .unwrap()
        );

        // The code is consumed and the token row is reserved, but not final:
        // the machine may not act yet.
        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Pending)
        );
        assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
        let finalized: i64 =
            sqlx::query_scalar("SELECT finalized FROM computer_tokens WHERE computer_id = ?")
                .bind(computer.to_string())
                .fetch_one(&identity.pool)
                .await
                .unwrap();
        assert_eq!(finalized, 0, "the reserve writes the token unfinalized");
        assert_eq!(
            identity.pairing_claims().await.unwrap(),
            vec![PairingClaim {
                owner,
                computer,
                finalized: false,
            }]
        );

        assert!(identity.finalize_pairing(owner, computer).await.unwrap());
        let finalized: i64 =
            sqlx::query_scalar("SELECT finalized FROM computer_tokens WHERE computer_id = ?")
                .bind(computer.to_string())
                .fetch_one(&identity.pool)
                .await
                .unwrap();
        assert_eq!(finalized, 1);
        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Approved { computer })
        );
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(computer),
            "only the final step admits the machine"
        );
        assert!(
            identity
                .pairing_claims()
                .await
                .unwrap()
                .first()
                .is_some_and(|claim| claim.finalized)
        );
    }

    #[tokio::test]
    async fn releasing_a_reserved_pairing_makes_the_code_usable_again() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .reserve_pairing(&start.code, owner, computer)
                .await
                .unwrap()
        );

        assert!(identity.release_pairing(owner, computer).await.unwrap());

        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Pending),
            "a released claim leaves the machine waiting, not approved"
        );
        assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
        assert!(identity.pairing_claims().await.unwrap().is_empty());
        assert!(
            identity
                .pairing_request(&start.code)
                .await
                .unwrap()
                .is_some(),
            "the User may enter the code again"
        );
        assert!(
            identity
                .reserve_pairing(&start.code, Id::new(), Id::new())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_typed_code_ignores_case_and_surrounding_space() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let typed = format!(" {} ", start.code.to_lowercase());

        let request = identity.pairing_request(&typed).await.unwrap();

        assert!(request.is_some());
    }

    #[tokio::test]
    async fn a_pairing_code_expires() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let start = identity
            .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let expires_at = now + PAIRING_TTL_SECS * 1_000_000;

        assert!(
            identity
                .pairing_request_at(expires_at - 1, &start.code)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            identity
                .pairing_request_at(expires_at, &start.code)
                .await
                .unwrap(),
            None
        );
        assert!(
            !identity
                .claim_pairing_at(expires_at, &start.code, Id::new(), Id::new())
                .await
                .unwrap()
        );
        assert_eq!(
            identity
                .pairing_at(expires_at, &start.device, "poll")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_approved_pairing_expires_with_its_code() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let start = identity
            .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let computer = Id::new();
        assert!(
            identity
                .claim_pairing_at(now, &start.code, Id::new(), computer)
                .await
                .unwrap()
        );
        let expires_at = now + PAIRING_TTL_SECS * 1_000_000;
        assert_eq!(
            identity
                .pairing_at(expires_at - 1, &start.device, "poll")
                .await
                .unwrap(),
            Some(PairingState::Approved { computer })
        );
        assert_eq!(
            identity
                .pairing_at(expires_at, &start.device, "poll")
                .await
                .unwrap(),
            None,
            "an expired pairing is not approved or pending, it is gone"
        );
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(computer),
            "the Computer and its token outlive the pairing row"
        );
    }

    #[tokio::test]
    async fn a_new_pairing_evicts_the_oldest_waiting_one() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        // Distinct starts so the oldest waiting pairing is well defined.
        let mut starts = Vec::new();
        for index in 0..MAX_PENDING_PAIRINGS {
            starts.push(
                identity
                    .begin_pairing_at(now + index, "laptop", "/ws", "token", "poll")
                    .await
                    .unwrap(),
            );
        }

        // A caller who never approves anything must not lock a User out: the
        // oldest waiting pairing gives way to the new one.
        let newest = identity
            .begin_pairing_at(now + MAX_PENDING_PAIRINGS, "laptop", "/ws", "token", "poll")
            .await
            .expect("a full quota evicts instead of refusing");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert_eq!(rows, MAX_PENDING_PAIRINGS, "the store stays bounded");

        assert_eq!(
            identity
                .pairing_at(now, &starts[0].device, "poll")
                .await
                .unwrap(),
            None,
            "the oldest waiting pairing is gone"
        );
        assert_eq!(
            identity
                .pairing_at(now, &starts[1].device, "poll")
                .await
                .unwrap(),
            Some(PairingState::Pending)
        );
        assert_eq!(
            identity
                .pairing_at(now, &newest.device, "poll")
                .await
                .unwrap(),
            Some(PairingState::Pending)
        );
    }

    #[tokio::test]
    async fn claimed_pairings_are_never_evicted() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let mut starts = Vec::new();
        for index in 0..MAX_PENDING_PAIRINGS {
            starts.push(
                identity
                    .begin_pairing_at(now + index, "laptop", "/ws", "token", "poll")
                    .await
                    .unwrap(),
            );
        }
        // One pairing is claimed and finished: it leaves the waiting count and
        // must survive any eviction.
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .claim_pairing_at(now, &starts[0].code, owner, computer)
                .await
                .unwrap()
        );
        let oldest_waiting = starts[1].device.clone();
        // Bring the waiting count back to the cap, then one more start evicts
        // the oldest waiting pairing; the claim is never a candidate.
        for offset in [MAX_PENDING_PAIRINGS, MAX_PENDING_PAIRINGS + 1] {
            identity
                .begin_pairing_at(now + offset, "laptop", "/ws", "token", "poll")
                .await
                .unwrap();
        }

        assert_eq!(
            identity
                .pairing_at(now, &oldest_waiting, "poll")
                .await
                .unwrap(),
            None,
            "an unclaimed pairing is the one evicted"
        );
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(computer),
            "a finished claim keeps its token"
        );
        assert_eq!(
            identity.pairing_claims().await.unwrap(),
            vec![PairingClaim {
                owner,
                computer,
                finalized: true,
            }]
        );
    }

    #[tokio::test]
    async fn starting_a_pairing_clears_expired_unclaimed_rows() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        for _ in 0..MAX_PENDING_PAIRINGS {
            identity
                .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
                .await
                .unwrap();
        }
        let later = now + PAIRING_TTL_SECS * 1_000_000;

        let start = identity
            .begin_pairing_at(later, "laptop", "/ws", "token", "poll")
            .await
            .expect("expired pairings stop holding the quota");

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "expired rows are deleted, the new one is kept");
        assert_eq!(
            identity
                .pairing_at(later, &start.device, "poll")
                .await
                .unwrap(),
            Some(PairingState::Pending)
        );
    }

    #[tokio::test]
    async fn expired_cleanup_never_deletes_a_claimed_pairing() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let start = identity
            .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .reserve_pairing_at(now, &start.code, owner, computer)
                .await
                .unwrap()
        );
        let later = now + PAIRING_TTL_SECS * 1_000_000;
        identity
            .begin_pairing_at(later, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();

        // The claim's code row expired, but only the start-up reconciliation
        // may move a claim, so it is still there to be finished.
        assert_eq!(
            identity.pairing_claims().await.unwrap(),
            vec![PairingClaim {
                owner,
                computer,
                finalized: false,
            }]
        );
        assert!(
            identity.finalize_pairing(owner, computer).await.unwrap(),
            "a claimed row outlives its code's expiry"
        );
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(computer)
        );
    }

    #[tokio::test]
    async fn a_token_held_by_another_computer_cannot_be_reserved() {
        let (_dir, _home, identity) = identity().await;
        let first = identity
            .begin_pairing("laptop", "/ws", "shared-token", "poll")
            .await
            .unwrap();
        let first_computer = Id::new();
        assert!(
            identity
                .claim_pairing(&first.code, Id::new(), first_computer)
                .await
                .unwrap()
        );

        let second = identity
            .begin_pairing("desktop", "/ws", "shared-token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let second_computer = Id::new();
        assert!(
            matches!(
                identity
                    .reserve_pairing(&second.code, owner, second_computer)
                    .await,
                Err(IdentityError::TokenTaken)
            ),
            "the token's primary key reserves it for one Computer only"
        );
        // A failed reserve rolls the claim back: the second code stays usable
        // and no half-claim is left behind.
        assert_eq!(
            identity.pairing(&second.device, "poll").await.unwrap(),
            Some(PairingState::Pending)
        );
        let secondary: Option<String> =
            sqlx::query_scalar("SELECT owner FROM computer_codes WHERE device_id = ?")
                .bind(&second.device)
                .fetch_one(&identity.pool)
                .await
                .unwrap();
        assert_eq!(secondary, None, "the losing reserve leaves no claim");
        assert_eq!(
            identity
                .authenticate_computer("shared-token")
                .await
                .unwrap(),
            Some(first_computer),
            "the first Computer keeps its token"
        );
    }

    #[tokio::test]
    async fn a_reserved_token_cannot_be_reserved_by_another_claim() {
        let (_dir, _home, identity) = identity().await;
        let first = identity
            .begin_pairing("laptop", "/ws", "token", "poll-one")
            .await
            .unwrap();
        let second = identity
            .begin_pairing("desktop", "/ws", "token", "poll-two")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .reserve_pairing(&first.code, owner, computer)
                .await
                .unwrap()
        );

        let other_owner = Id::new();
        let other_computer = Id::new();
        assert!(
            matches!(
                identity
                    .reserve_pairing(&second.code, other_owner, other_computer)
                    .await,
                Err(IdentityError::TokenTaken)
            ),
            "an unfinalized reservation already owns the token"
        );
        assert_eq!(
            identity.pairing_claims().await.unwrap(),
            vec![PairingClaim {
                owner,
                computer,
                finalized: false,
            }]
        );
    }

    #[tokio::test]
    async fn finishing_a_cleaned_up_claim_reports_that_it_is_gone() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let start = identity
            .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .reserve_pairing_at(now, &start.code, owner, computer)
                .await
                .unwrap()
        );
        sqlx::query("DELETE FROM computer_codes WHERE owner = ?")
            .bind(owner.to_string())
            .execute(&identity.pool)
            .await
            .unwrap();

        assert!(
            !identity.finalize_pairing(owner, computer).await.unwrap(),
            "a claim whose row is gone cannot report success"
        );
        assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
    }

    #[tokio::test]
    async fn finishing_a_claim_twice_is_idempotent() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let computer = Id::new();
        assert!(
            identity
                .reserve_pairing(&start.code, owner, computer)
                .await
                .unwrap()
        );
        assert!(identity.finalize_pairing(owner, computer).await.unwrap());
        assert!(identity.finalize_pairing(owner, computer).await.unwrap());
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(computer)
        );
    }

    #[tokio::test]
    async fn two_machines_starting_at_once_keep_the_store_bounded() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        for _ in 0..MAX_PENDING_PAIRINGS - 1 {
            identity
                .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
                .await
                .unwrap();
        }

        let (first, second) = tokio::join!(
            identity.begin_pairing_at(now, "one", "/ws", "token", "poll"),
            identity.begin_pairing_at(now, "two", "/ws", "token", "poll"),
        );
        assert!(first.is_ok(), "{first:?}");
        assert!(second.is_ok(), "{second:?}");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert_eq!(rows, MAX_PENDING_PAIRINGS);
    }

    #[tokio::test]
    async fn a_pairing_code_stops_working_after_its_tries() {
        let (_dir, _home, identity) = identity().await;
        let now = 1_000_000_i64;
        let start = identity
            .begin_pairing_at(now, "laptop", "/ws", "token", "poll")
            .await
            .unwrap();

        for _ in 0..MAX_CODE_TRIES {
            assert!(
                identity
                    .pairing_request_at(now, &start.code)
                    .await
                    .unwrap()
                    .is_some()
            );
        }

        assert!(
            identity
                .pairing_request_at(now, &start.code)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !identity
                .claim_pairing_at(now, &start.code, Id::new(), Id::new())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn pairing_stays_pending_until_the_user_approves_it() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();

        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Pending)
        );
        assert_eq!(
            identity.pairing(&start.device, "wrong").await.unwrap(),
            None
        );
        assert_eq!(
            identity.pairing("unknown-device", "poll").await.unwrap(),
            None
        );

        let computer = Id::new();
        assert!(
            identity
                .claim_pairing(&start.code, Id::new(), computer)
                .await
                .unwrap()
        );
        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Approved { computer })
        );
    }

    #[tokio::test]
    async fn only_one_of_two_concurrent_claims_wins() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();
        let owner = Id::new();
        let first = Id::new();
        let second = Id::new();

        let (one, two) = tokio::join!(
            identity.claim_pairing(&start.code, owner, first),
            identity.claim_pairing(&start.code, owner, second),
        );
        let one = one.unwrap();
        let two = two.unwrap();

        assert_ne!(one, two, "one claim must lose");
        let winner = if one { first } else { second };
        assert_eq!(
            identity.pairing(&start.device, "poll").await.unwrap(),
            Some(PairingState::Approved { computer: winner })
        );
        assert_eq!(
            identity.authenticate_computer("token").await.unwrap(),
            Some(winner)
        );
    }

    #[tokio::test]
    async fn the_pairing_tables_store_only_hashes() {
        let (_dir, _home, identity) = identity().await;
        let token = "machine-token-plaintext";
        let poll_secret = "poll-secret-plaintext";
        let start = identity
            .begin_pairing("laptop", "/ws", token, poll_secret)
            .await
            .unwrap();
        assert!(
            identity
                .claim_pairing(&start.code, Id::new(), Id::new())
                .await
                .unwrap()
        );

        let code_hash: Vec<u8> = sqlx::query_scalar("SELECT code_hash FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        let poll_hash: Vec<u8> = sqlx::query_scalar("SELECT poll_hash FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        let token_hash: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();

        assert_eq!(code_hash, digest(&start.code));
        assert_eq!(poll_hash, digest(poll_secret));
        assert_eq!(token_hash, digest(token));
        for hash in [&code_hash, &poll_hash, &token_hash] {
            assert_eq!(hash.len(), 32);
        }
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM computer_tokens")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert_eq!(stored, digest(token));
        assert_ne!(stored, token.as_bytes());
    }

    #[tokio::test]
    async fn a_computer_authenticates_with_the_token_the_machine_keeps() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "machine-token", "poll")
            .await
            .unwrap();
        let computer = Id::new();
        assert!(
            identity
                .claim_pairing(&start.code, Id::new(), computer)
                .await
                .unwrap()
        );

        assert_eq!(
            identity
                .authenticate_computer("machine-token")
                .await
                .unwrap(),
            Some(computer)
        );
        assert_eq!(
            identity.authenticate_computer("not-a-token").await.unwrap(),
            None
        );
        let (_account, account_token) = identity
            .create_admin("Ada", "correct horse battery")
            .await
            .unwrap();
        assert_eq!(
            identity
                .authenticate_computer(&account_token)
                .await
                .unwrap(),
            None,
            "an account token is not a Computer token"
        );
    }

    #[tokio::test]
    async fn pairing_codes_and_devices_have_the_expected_shape() {
        let (_dir, _home, identity) = identity().await;
        let start = identity
            .begin_pairing("laptop", "/ws", "token", "poll")
            .await
            .unwrap();

        assert_eq!(CODE_ALPHABET.len(), 32);
        assert_eq!(start.code.len(), 8);
        assert!(start.code.bytes().all(|byte| CODE_ALPHABET.contains(&byte)));
        assert_eq!(URL_SAFE_NO_PAD.decode(&start.device).unwrap().len(), 16);
    }

    async fn version_one_database(path: &std::path::Path, token: &str) -> Account {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        for statement in crate::schema::VERSION_ONE_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut connection)
            .await
            .unwrap();
        let account = Account {
            id: Id::new(),
            display_name: "Legacy Ada".to_owned(),
            is_admin: true,
        };
        sqlx::query(
            "INSERT INTO accounts (id, display_name, password_hash, is_admin, created_at) \
             VALUES (?, ?, 'unused', 1, 0)",
        )
        .bind(account.id.to_string())
        .bind(&account.display_name)
        .execute(&mut connection)
        .await
        .unwrap();
        let token_hash = digest(token);
        sqlx::query("INSERT INTO tokens (token_hash, account_id, created_at) VALUES (?, ?, 0)")
            .bind(token_hash.as_slice())
            .bind(account.id.to_string())
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();

        account
    }

    #[tokio::test]
    async fn opening_a_version_one_database_keeps_accounts_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let account = version_one_database(&home.db_path(), "legacy-token").await;

        let identity = Identity::open(&home).await.unwrap();

        assert!(identity.has_accounts().await.unwrap());
        assert_eq!(
            identity.authenticate("legacy-token").await.unwrap(),
            Some(account)
        );
    }

    #[tokio::test]
    async fn opening_a_home_renames_and_upgrades_the_legacy_database() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("db").join("neha-1.sqlite3");
        let account = version_one_database(&legacy, "legacy-token").await;

        let home = Home::open(dir.path()).unwrap();
        let identity = Identity::open(&home).await.unwrap();

        assert!(!legacy.exists(), "the legacy file must be renamed");
        assert!(home.db_path().exists());
        assert_eq!(
            identity.authenticate("legacy-token").await.unwrap(),
            Some(account)
        );
    }

    async fn version_two_database(path: &std::path::Path, token: &str, computer: Id) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        for statement in crate::schema::VERSION_ONE_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        for statement in crate::schema::VERSION_TWO_STATEMENTS {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut connection)
            .await
            .unwrap();
        let token_hash = digest(token);
        sqlx::query(
            "INSERT INTO computer_tokens (token_hash, computer_id, created_at) \
             VALUES (?, ?, 0)",
        )
        .bind(token_hash.as_slice())
        .bind(computer.to_string())
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn opening_a_version_two_database_keeps_paired_computers_active() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let computer = Id::new();
        version_two_database(&home.db_path(), "paired-token", computer).await;

        let identity = Identity::open(&home).await.unwrap();

        assert_eq!(
            identity
                .authenticate_computer("paired-token")
                .await
                .unwrap(),
            Some(computer),
            "a token written before schema version 3 still authenticates"
        );
        let finalized: i64 = sqlx::query_scalar("SELECT finalized FROM computer_tokens")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert_eq!(finalized, 1, "the migration marks old tokens final");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers_wait_for_the_database_instead_of_erroring() {
        let (_dir, _home, identity) = identity().await;

        // The pool holds two connections, and eight writers start at once. A
        // slow disk without a busy timeout turns that into SQLITE_BUSY
        // ("database is locked") instead of a wait; this is the shape CI caught
        // in the claim race.
        for round in 0..12_i64 {
            let now = 1_000_000 + round * 1_000;
            let machines: Vec<String> = (0..8).map(|index| format!("machine-{index}")).collect();
            let tokens: Vec<String> = (0..8)
                .map(|index| format!("token-{round}-{index}"))
                .collect();
            let polls: Vec<String> = (0..8)
                .map(|index| format!("poll-{round}-{index}"))
                .collect();
            let starts = futures_util::future::join_all((0..8).map(|index| {
                identity.begin_pairing_at(
                    now,
                    &machines[index],
                    "/ws",
                    &tokens[index],
                    &polls[index],
                )
            }))
            .await;
            let starts: Vec<_> = starts
                .into_iter()
                .map(|start| start.expect("a concurrent start must wait, not fail"))
                .collect();

            let owners: Vec<Id> = (0..8).map(|_| Id::new()).collect();
            let computers: Vec<Id> = (0..8).map(|_| Id::new()).collect();
            let reservations =
                futures_util::future::join_all(starts.iter().zip(&owners).zip(&computers).map(
                    |((start, owner), computer)| {
                        identity.reserve_pairing_at(now, &start.code, *owner, *computer)
                    },
                ))
                .await;
            for reservation in reservations {
                assert!(
                    reservation.expect("a concurrent reserve must wait, not fail"),
                    "every owner gets its own code"
                );
            }
            for (owner, computer) in owners.iter().zip(&computers) {
                assert!(
                    identity
                        .release_pairing(*owner, *computer)
                        .await
                        .expect("a concurrent release must wait, not fail")
                );
            }
        }

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
            .fetch_one(&identity.pool)
            .await
            .unwrap();
        assert!(rows <= MAX_PENDING_PAIRINGS, "the store stayed bounded");
        assert!(
            identity.pairing_claims().await.unwrap().is_empty(),
            "released claims leave nothing behind"
        );
    }
}
