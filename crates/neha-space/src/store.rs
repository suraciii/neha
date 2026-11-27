//! The record behind a seam.
//!
//! SQLite holds the record. One writer task owns the write connection and
//! assigns sequence numbers; readers use a pool and resume from a cursor.

use std::time::Duration;

use neha::event::EventClass;
use neha::id::Id;
use neha::record::{Author, Event, EventDraft, EventKind};
use neha::seq::Seq;
use neha::time::Timestamp;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Row, SqliteConnection, SqlitePool};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::home::Home;
use crate::schema::{self, SchemaError};

const EVENTS: &str = "SELECT seq, kind, class, version, at, author, acting, command, data \
                      FROM events";
const PAGE_SIZE: usize = 256;

/// A projection of the record, rebuilt by replaying events in order.
pub trait Projection {
    /// Applies one event, or reports why it does not fit.
    fn apply(&mut self, event: &Event) -> Result<(), String>;
}

/// A request to append the events of one command.
#[derive(Clone, Debug)]
pub struct AppendRequest {
    /// The command that writes the events.
    pub command: Id,
    /// A digest of the command's parameters.
    pub fingerprint: String,
    /// The events to append in order.
    pub events: Vec<EventDraft>,
    /// The command's result, once known.
    pub result: Option<serde_json::Value>,
    /// Another command whose result this append confirms in the same transaction.
    pub confirmation: Option<Confirmation>,
    /// Provider-native state to write in the same transaction, when this
    /// append records a Model reply.
    pub continuation: Option<ContinuationWrite>,
}

/// Provider-native state recorded beside the Model reply that produced it.
#[derive(Clone, Debug)]
pub struct ContinuationWrite {
    /// The run.
    pub run: Id,
    /// The step.
    pub step: u32,
    /// The state; `None` removes a failed step's stale state.
    pub continuation: Option<neha_loop::Continuation>,
}

/// A result written for another command in the same transaction.
#[derive(Clone, Debug)]
pub struct Confirmation {
    /// The command to confirm.
    pub command: Id,
    /// The result to store.
    pub result: serde_json::Value,
}

impl AppendRequest {
    /// Builds a request.
    #[must_use]
    pub fn new(command: Id, fingerprint: impl Into<String>, events: Vec<EventDraft>) -> Self {
        Self {
            command,
            fingerprint: fingerprint.into(),
            events,
            result: None,
            confirmation: None,
            continuation: None,
        }
    }

    /// Attaches the command's result.
    #[must_use]
    pub fn with_result(mut self, result: serde_json::Value) -> Self {
        self.result = Some(result);
        self
    }

    /// Confirms another command's result in the same transaction.
    #[must_use]
    pub fn with_confirmation(mut self, command: Id, result: serde_json::Value) -> Self {
        self.confirmation = Some(Confirmation { command, result });
        self
    }

    /// Records provider-native state with the reply.
    #[must_use]
    pub fn with_continuation(mut self, continuation: ContinuationWrite) -> Self {
        self.continuation = Some(continuation);
        self
    }
}

/// The events one append wrote, or the ones the same command wrote before.
#[derive(Clone, Debug)]
pub struct AppendResult {
    /// The recorded events.
    pub events: Vec<Event>,
    /// Whether this call found the command already recorded.
    pub existing: bool,
    /// The command's result, when the record holds it.
    pub result: Option<serde_json::Value>,
}

/// A recorded command.
#[derive(Clone, Debug)]
pub struct CommandRecord {
    /// A digest of the command's parameters.
    pub fingerprint: String,
    /// The cursor of the command's last event.
    pub last_seq: Seq,
    /// The command's result, once known.
    pub result: Option<serde_json::Value>,
}

/// A page of the record and the cursor to continue from.
#[derive(Clone, Debug)]
pub struct EventPage {
    /// The events this version can apply, in order.
    pub events: Vec<Event>,
    /// The cursor after the page.
    pub next: Seq,
}

enum WriteRequest {
    Append {
        request: AppendRequest,
        reply: oneshot::Sender<Result<AppendResult, StoreError>>,
    },
    RecordCommand {
        command: Id,
        fingerprint: String,
        result: serde_json::Value,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    ClaimCommand {
        command: Id,
        fingerprint: String,
        reply: oneshot::Sender<Result<Option<CommandRecord>, StoreError>>,
    },
    SettleCommand {
        command: Id,
        result: serde_json::Value,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A cloneable reader over the record.
#[derive(Clone)]
pub struct StoreReader {
    pool: SqlitePool,
}

/// A Space's record.
pub struct Store {
    writer: mpsc::Sender<WriteRequest>,
    reader: SqlitePool,
    writer_task: Option<tokio::task::JoinHandle<()>>,
}

impl Store {
    /// Opens the record in a home, creating or migrating it as needed.
    pub async fn open(home: &Home) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(home.db_path())
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));

        let mut writer = SqliteConnection::connect_with(&options).await?;
        schema::migrate(&mut writer).await?;

        let reader = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let (writer, writer_task) = spawn_writer(writer);

        Ok(Self {
            writer,
            reader,
            writer_task: Some(writer_task),
        })
    }

    /// Appends the events of one command, or returns the ones it wrote before.
    pub async fn append(&self, request: AppendRequest) -> Result<AppendResult, StoreError> {
        let (reply, answer) = oneshot::channel();

        self.writer
            .send(WriteRequest::Append { request, reply })
            .await
            .map_err(|_| StoreError::WriterGone)?;

        answer.await.map_err(|_| StoreError::WriterGone)?
    }

    /// Reads up to `limit` events after a cursor.
    ///
    /// An event whose kind this version does not know is skipped when it is
    /// presentation, and stops the read when it is an authority event.
    pub async fn events_from(&self, cursor: Seq, limit: usize) -> Result<EventPage, StoreError> {
        read_events(&self.reader, cursor, limit).await
    }

    /// Returns a reader that can read the record without the writer.
    #[must_use]
    pub fn reader(&self) -> StoreReader {
        StoreReader {
            pool: self.reader.clone(),
        }
    }

    /// Returns the cursor of the last recorded event.
    pub async fn last_seq(&self) -> Result<Seq, StoreError> {
        let text: String = sqlx::query_scalar("SELECT value FROM meta WHERE key = 'last_seq'")
            .fetch_one(&self.reader)
            .await?;
        text.parse::<i64>()
            .ok()
            .and_then(Seq::from_i64)
            .ok_or_else(|| corrupt("last_seq is not a cursor"))
    }

    /// Replays the whole record into a projection.
    pub async fn replay(&self, projection: &mut impl Projection) -> Result<Seq, StoreError> {
        let mut cursor = Seq::ZERO;
        loop {
            let page = self.events_from(cursor, PAGE_SIZE).await?;
            for event in &page.events {
                projection.apply(event).map_err(StoreError::Corrupt)?;
            }
            if page.next <= cursor {
                return Ok(cursor);
            }
            cursor = page.next;
        }
    }

    /// Returns a recorded command, if the record holds it.
    pub async fn command(&self, command: Id) -> Result<Option<CommandRecord>, StoreError> {
        let row =
            sqlx::query("SELECT fingerprint, last_seq, result FROM commands WHERE command_id = ?")
                .bind(command.to_string())
                .fetch_optional(&self.reader)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let fingerprint: String = row.try_get("fingerprint")?;
        let last: i64 = row.try_get("last_seq")?;
        let last_seq = Seq::from_i64(last).ok_or_else(|| corrupt("command cursor"))?;
        let stored: Option<String> = row.try_get("result")?;
        let result = stored
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| corrupt("command result is not JSON"))
            })
            .transpose()?;

        Ok(Some(CommandRecord {
            fingerprint,
            last_seq,
            result,
        }))
    }

    /// Records a command's result without appending events.
    ///
    /// An idempotent command that changed nothing still owes its caller the
    /// same answer on a retry.
    pub async fn record_command_result(
        &self,
        command: Id,
        fingerprint: impl Into<String>,
        result: serde_json::Value,
    ) -> Result<(), StoreError> {
        let (reply, answer) = oneshot::channel();
        self.writer
            .send(WriteRequest::RecordCommand {
                command,
                fingerprint: fingerprint.into(),
                result,
                reply,
            })
            .await
            .map_err(|_| StoreError::WriterGone)?;
        answer.await.map_err(|_| StoreError::WriterGone)?
    }

    /// Returns the provider-native state recorded for one run step.
    pub async fn continuation(
        &self,
        run: Id,
        step: u32,
    ) -> Result<Option<neha_loop::Continuation>, StoreError> {
        let row = sqlx::query(
            "SELECT protocol, provider, model, state FROM run_continuations \
             WHERE run = ? AND step = ?",
        )
        .bind(run.to_string())
        .bind(i64::from(step))
        .fetch_optional(&self.reader)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let protocol: String = row.try_get("protocol")?;
        let provider: String = row.try_get("provider")?;
        let model: String = row.try_get("model")?;
        let stored: String = row.try_get("state")?;
        let state =
            serde_json::from_str(&stored).map_err(|_| corrupt("continuation state is not JSON"))?;

        Ok(Some(neha_loop::Continuation {
            protocol,
            provider,
            model,
            state,
        }))
    }

    /// Claims a command identifier before an idempotent effect runs.
    ///
    /// A `None` answer means this call inserted the claim. An answer with a
    /// record means another call already used the identifier, either still
    /// pending (`result` is `None`) or settled. Claiming first means two
    /// effects with one identifier cannot both run.
    pub async fn claim_command(
        &self,
        command: Id,
        fingerprint: impl Into<String>,
    ) -> Result<Option<CommandRecord>, StoreError> {
        let (reply, answer) = oneshot::channel();
        self.writer
            .send(WriteRequest::ClaimCommand {
                command,
                fingerprint: fingerprint.into(),
                reply,
            })
            .await
            .map_err(|_| StoreError::WriterGone)?;
        answer.await.map_err(|_| StoreError::WriterGone)?
    }

    /// Settles a command claimed before its effect ran.
    pub async fn settle_command(
        &self,
        command: Id,
        result: serde_json::Value,
    ) -> Result<(), StoreError> {
        let (reply, answer) = oneshot::channel();
        self.writer
            .send(WriteRequest::SettleCommand {
                command,
                result,
                reply,
            })
            .await
            .map_err(|_| StoreError::WriterGone)?;
        answer.await.map_err(|_| StoreError::WriterGone)?
    }

    /// Stops the writer and closes the record.
    pub async fn close(mut self) -> Result<(), StoreError> {
        let (reply, answer) = oneshot::channel();
        let _ = self.writer.send(WriteRequest::Shutdown { reply }).await;
        let _ = answer.await;
        if let Some(task) = self.writer_task.take() {
            let _ = task.await;
        }
        self.reader.close().await;

        Ok(())
    }
}

fn spawn_writer(
    connection: SqliteConnection,
) -> (mpsc::Sender<WriteRequest>, tokio::task::JoinHandle<()>) {
    let (writer, mut messages) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut connection = connection;
        while let Some(message) = messages.recv().await {
            match message {
                WriteRequest::Append { request, reply } => {
                    let _ = reply.send(append(&mut connection, request).await);
                }
                WriteRequest::RecordCommand {
                    command,
                    fingerprint,
                    result,
                    reply,
                } => {
                    let _ = reply
                        .send(record_command(&mut connection, command, fingerprint, result).await);
                }
                WriteRequest::ClaimCommand {
                    command,
                    fingerprint,
                    reply,
                } => {
                    let _ = reply.send(claim_command(&mut connection, command, fingerprint).await);
                }
                WriteRequest::SettleCommand {
                    command,
                    result,
                    reply,
                } => {
                    let _ = reply.send(settle_command(&mut connection, command, result).await);
                }
                WriteRequest::Shutdown { reply } => {
                    let _ = reply.send(());
                    break;
                }
            }
        }
    });

    (writer, task)
}

async fn record_command(
    connection: &mut SqliteConnection,
    command: Id,
    fingerprint: String,
    result: serde_json::Value,
) -> Result<(), StoreError> {
    let mut transaction = connection.begin().await?;
    let existing = sqlx::query("SELECT fingerprint, result FROM commands WHERE command_id = ?")
        .bind(command.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
    if let Some(row) = existing {
        let stored: String = row.try_get("fingerprint")?;
        if stored != fingerprint {
            return Err(StoreError::IdempotencyConflict(command));
        }
        transaction.commit().await?;
        return Ok(());
    }
    let stored = serde_json::to_string(&result).map_err(|_| corrupt("command result"))?;
    sqlx::query(
        "INSERT INTO commands (command_id, fingerprint, first_seq, last_seq, result) \
         VALUES (?, ?, 0, 0, ?)",
    )
    .bind(command.to_string())
    .bind(fingerprint)
    .bind(stored)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(())
}

async fn read_command(
    connection: &mut SqliteConnection,
    command: Id,
) -> Result<Option<CommandRecord>, StoreError> {
    let row =
        sqlx::query("SELECT fingerprint, last_seq, result FROM commands WHERE command_id = ?")
            .bind(command.to_string())
            .fetch_optional(&mut *connection)
            .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let fingerprint: String = row.try_get("fingerprint")?;
    let last: i64 = row.try_get("last_seq")?;
    let last_seq = Seq::from_i64(last).ok_or_else(|| corrupt("command cursor"))?;
    let stored: Option<String> = row.try_get("result")?;
    let result = stored
        .map(|text| serde_json::from_str(&text).map_err(|_| corrupt("command result is not JSON")))
        .transpose()?;

    Ok(Some(CommandRecord {
        fingerprint,
        last_seq,
        result,
    }))
}

async fn claim_command(
    connection: &mut SqliteConnection,
    command: Id,
    fingerprint: String,
) -> Result<Option<CommandRecord>, StoreError> {
    let mut transaction = connection.begin().await?;
    if let Some(record) = read_command(&mut transaction, command).await? {
        if record.fingerprint != fingerprint {
            return Err(StoreError::IdempotencyConflict(command));
        }
        transaction.commit().await?;
        return Ok(Some(record));
    }
    sqlx::query(
        "INSERT INTO commands (command_id, fingerprint, first_seq, last_seq, result) \
         VALUES (?, ?, 0, 0, NULL)",
    )
    .bind(command.to_string())
    .bind(fingerprint)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(None)
}

async fn settle_command(
    connection: &mut SqliteConnection,
    command: Id,
    result: serde_json::Value,
) -> Result<(), StoreError> {
    let stored = serde_json::to_string(&result).map_err(|_| corrupt("command result"))?;
    let mut transaction = connection.begin().await?;
    let updated =
        sqlx::query("UPDATE commands SET result = ? WHERE command_id = ? AND result IS NULL")
            .bind(stored)
            .bind(command.to_string())
            .execute(&mut *transaction)
            .await?;
    if updated.rows_affected() == 0 {
        let exists = sqlx::query("SELECT 1 FROM commands WHERE command_id = ?")
            .bind(command.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
        if exists.is_none() {
            return Err(StoreError::UnclaimedCommand(command));
        }
        // Already settled: a repeated settle is harmless.
    }
    transaction.commit().await?;

    Ok(())
}

async fn append(
    connection: &mut SqliteConnection,
    request: AppendRequest,
) -> Result<AppendResult, StoreError> {
    if request.events.is_empty() {
        return Err(StoreError::EmptyCommand);
    }
    for draft in &request.events {
        draft
            .validate()
            .map_err(|error| StoreError::InvalidEvent(error.to_string()))?;
    }

    let mut transaction = connection.begin().await?;
    let existing = sqlx::query(
        "SELECT fingerprint, first_seq, last_seq, result FROM commands WHERE command_id = ?",
    )
    .bind(request.command.to_string())
    .fetch_optional(&mut *transaction)
    .await?;

    if let Some(row) = existing {
        let fingerprint: String = row.try_get("fingerprint")?;
        if fingerprint != request.fingerprint {
            return Err(StoreError::IdempotencyConflict(request.command));
        }
        let first: i64 = row.try_get("first_seq")?;
        let last: i64 = row.try_get("last_seq")?;
        let stored: Option<String> = row.try_get("result")?;
        let result = stored
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| corrupt("command result is not JSON"))
            })
            .transpose()?;
        let events = load_range(&mut transaction, first, last).await?;
        transaction.commit().await?;

        return Ok(AppendResult {
            events,
            existing: true,
            result,
        });
    }

    let last_text: String = sqlx::query_scalar("SELECT value FROM meta WHERE key = 'last_seq'")
        .fetch_one(&mut *transaction)
        .await?;
    let mut last: i64 = last_text
        .parse()
        .map_err(|_| corrupt("last_seq is not a number"))?;
    let mut events = Vec::with_capacity(request.events.len());
    let finished_runs: Vec<String> = request
        .events
        .iter()
        .filter(|draft| draft.kind == EventKind::RunFinished)
        .filter_map(|draft| {
            draft
                .data
                .get("run")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect();

    for draft in request.events {
        last += 1;
        let seq = Seq::from_i64(last).ok_or_else(|| corrupt("sequence exhausted"))?;
        let at = Timestamp::now();
        insert_event(&mut transaction, seq, &draft, at, request.command).await?;
        events.push(Event {
            seq,
            kind: draft.kind,
            version: draft.version,
            at,
            author: draft.author,
            acting: draft.acting,
            command: Some(request.command),
            data: draft.data,
        });
    }

    sqlx::query("UPDATE meta SET value = ? WHERE key = 'last_seq'")
        .bind(last.to_string())
        .execute(&mut *transaction)
        .await?;
    let stored_result = request
        .result
        .as_ref()
        .map(|result| serde_json::to_string(result).map_err(|_| corrupt("command result")));
    let stored_result = stored_result.transpose()?;
    sqlx::query(
        "INSERT INTO commands (command_id, fingerprint, first_seq, last_seq, result) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(request.command.to_string())
    .bind(request.fingerprint)
    .bind(events.first().map(|event| event.seq.as_i64()))
    .bind(events.last().map(|event| event.seq.as_i64()))
    .bind(stored_result)
    .execute(&mut *transaction)
    .await?;
    if let Some(confirmation) = request.confirmation {
        let stored =
            serde_json::to_string(&confirmation.result).map_err(|_| corrupt("confirmed result"))?;
        let updated = sqlx::query("UPDATE commands SET result = ? WHERE command_id = ?")
            .bind(stored)
            .bind(confirmation.command.to_string())
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(corrupt("the command to confirm is not recorded"));
        }
    }
    for run in finished_runs {
        sqlx::query("DELETE FROM run_continuations WHERE run = ?")
            .bind(run)
            .execute(&mut *transaction)
            .await?;
    }
    if let Some(continuation) = request.continuation {
        let run = continuation.run.to_string();
        let step = i64::from(continuation.step);
        match continuation.continuation {
            Some(state) => {
                let stored =
                    serde_json::to_string(&state.state).map_err(|_| corrupt("continuation"))?;
                sqlx::query(
                    "INSERT INTO run_continuations (run, step, protocol, provider, model, state) \
                     VALUES (?, ?, ?, ?, ?, ?) \
                     ON CONFLICT(run, step) DO UPDATE SET protocol = excluded.protocol, \
                     provider = excluded.provider, model = excluded.model, state = excluded.state",
                )
                .bind(run)
                .bind(step)
                .bind(state.protocol)
                .bind(state.provider)
                .bind(state.model)
                .bind(stored)
                .execute(&mut *transaction)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM run_continuations WHERE run = ? AND step = ?")
                    .bind(run)
                    .bind(step)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
    }
    transaction.commit().await?;

    Ok(AppendResult {
        events,
        existing: false,
        result: request.result,
    })
}

async fn insert_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    seq: Seq,
    draft: &EventDraft,
    at: Timestamp,
    command: Id,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO events (seq, kind, class, version, at, author, acting, command, data) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(seq.as_i64())
    .bind(draft.kind.as_str())
    .bind(draft.kind.class().as_str())
    .bind(i64::from(draft.version))
    .bind(at.as_microseconds())
    .bind(serde_json::to_string(&draft.author).map_err(|_| corrupt("author"))?)
    .bind(draft.acting.map(|id| id.to_string()))
    .bind(command.to_string())
    .bind(serde_json::to_string(&draft.data).map_err(|_| corrupt("event data"))?)
    .execute(&mut **transaction)
    .await?;

    Ok(())
}

async fn load_range(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    first: i64,
    last: i64,
) -> Result<Vec<Event>, StoreError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{EVENTS} WHERE seq BETWEEN ? AND ? ORDER BY seq"
    )))
    .bind(first)
    .bind(last)
    .fetch_all(&mut **transaction)
    .await?;
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(event) = decode_event(row)? {
            events.push(event);
        }
    }

    Ok(events)
}

impl StoreReader {
    /// Reads a bounded page after a cursor.
    pub async fn events_from(&self, cursor: Seq, limit: usize) -> Result<EventPage, StoreError> {
        read_events(&self.pool, cursor, limit).await
    }
}

async fn read_events(
    pool: &SqlitePool,
    cursor: Seq,
    limit: usize,
) -> Result<EventPage, StoreError> {
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{EVENTS} WHERE seq > ? ORDER BY seq LIMIT ?"
    )))
    .bind(cursor.as_i64())
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut events = Vec::with_capacity(rows.len());
    let mut next = cursor;
    for row in &rows {
        let seq = Seq::from_i64(row.try_get("seq")?).ok_or_else(|| corrupt("negative seq"))?;
        if let Some(event) = decode_event(row)? {
            events.push(event);
        }
        next = seq;
    }

    Ok(EventPage { events, next })
}

fn decode_event(row: &sqlx::sqlite::SqliteRow) -> Result<Option<Event>, StoreError> {
    let kind_text: String = row.try_get("kind")?;
    let class: String = row.try_get("class")?;
    let kind: EventKind = match kind_text.parse() {
        Ok(kind) => kind,
        Err(_) if class == EventClass::Presentation.as_str() => return Ok(None),
        Err(_) => {
            return Err(StoreError::UpgradeRequired { kind: kind_text });
        }
    };

    let seq = Seq::from_i64(row.try_get("seq")?).ok_or_else(|| corrupt("negative seq"))?;
    let version = u16::try_from(row.try_get::<i64, _>("version")?)
        .map_err(|_| corrupt("version out of range"))?;
    let at = Timestamp::from_microseconds(row.try_get("at")?)
        .ok_or_else(|| corrupt("timestamp out of range"))?;
    let author_text: String = row.try_get("author")?;
    let author: Author =
        serde_json::from_str(&author_text).map_err(|_| corrupt("author is not valid JSON"))?;
    let acting = parse_id(row.try_get("acting")?)?;
    let command = parse_id(row.try_get("command")?)?;
    let data_text: String = row.try_get("data")?;
    let data = serde_json::from_str(&data_text).map_err(|_| corrupt("event data is not JSON"))?;
    if version != kind.supported_version() {
        return Err(StoreError::UpgradeRequired {
            kind: kind.as_str().to_owned(),
        });
    }
    kind.validate_author(author, acting)
        .map_err(|error| corrupt(&error.to_string()))?;
    kind.decode(&data)
        .map_err(|error| corrupt(&error.to_string()))?;

    Ok(Some(Event {
        seq,
        kind,
        version,
        at,
        author,
        acting,
        command,
        data,
    }))
}

fn parse_id(text: Option<String>) -> Result<Option<Id>, StoreError> {
    text.map(|text| text.parse().map_err(|_| corrupt("id is not a UUID")))
        .transpose()
}

fn corrupt(reason: &str) -> StoreError {
    StoreError::Corrupt(reason.to_owned())
}

/// The record cannot be read or written.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The home could not be used.
    #[error("home error: {0}")]
    Home(#[from] crate::home::HomeError),
    /// A command identifier was reused with different parameters.
    #[error("command {0} was reused with different parameters")]
    IdempotencyConflict(Id),
    /// The record holds an authority event this version does not know.
    #[error("this version must understand the recorded event kind {kind}")]
    UpgradeRequired {
        /// The unknown kind.
        kind: String,
    },
    /// A stored value does not decode.
    #[error("record is corrupt: {0}")]
    Corrupt(String),
    /// The writer task is gone.
    #[error("record writer is gone")]
    WriterGone,
    /// A command wrote no events.
    #[error("a command must write at least one event")]
    EmptyCommand,
    /// A command was not claimed before its result was settled.
    #[error("command {0} was not claimed")]
    UnclaimedCommand(Id),
    /// An event does not fit its kind's schema.
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    /// The database schema cannot be created or is newer than this build.
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use neha::id::Id;
    use neha::record::{Author, Event, EventDraft, EventKind};
    use neha::seq::Seq;
    use sqlx::Connection as _;
    use sqlx::sqlite::SqliteConnectOptions;

    use super::{AppendRequest, ContinuationWrite, Projection, Store, StoreError};
    use crate::home::Home;

    async fn open() -> (tempfile::TempDir, Home, Store) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let store = Store::open(&home).await.unwrap();

        (dir, home, store)
    }

    fn draft(kind: EventKind) -> EventDraft {
        let user = Id::new();
        match kind {
            EventKind::UserJoined => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({"display_name": "Ada", "is_admin": true}),
            ),
            EventKind::ProviderAdded => {
                let provider = Id::new();
                let model = Id::new();
                EventDraft::new(
                    kind,
                    Author::User(user),
                    serde_json::json!({
                        "provider": provider,
                        "protocol": "chat_completions",
                        "base_url": "http://127.0.0.1:9/v1",
                        "models": [{
                            "id": model,
                            "name": "model",
                            "context_limit": 1000,
                            "max_output": 100,
                            "reads_images": false,
                        }],
                        "default_model": model,
                    }),
                )
            }
            EventKind::RunContext => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({"run": Id::new(), "working_dir": "/tmp"}),
            )
            .acting(Id::new()),
            EventKind::BotUpdated => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({"bot": Id::new(), "full_access": true}),
            ),
            EventKind::BotMoved => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "bot": Id::new(),
                    "from": Id::new(),
                    "to": Id::new(),
                }),
            ),
            EventKind::BotCreated => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "bot": Id::new(),
                    "owner": user,
                    "name": "Scout",
                    "computer": Id::new(),
                    "chat": Id::new(),
                }),
            ),
            EventKind::ComputerCreated => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "computer": Id::new(),
                    "owner": user,
                    "kind": "managed",
                    "workspace": "/tmp/workspace",
                }),
            ),
            EventKind::ChatCreated => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({"chat": Id::new(), "owner": user, "bot": Id::new()}),
            ),
            EventKind::MessageAdded => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "message": Id::new(),
                    "chat": Id::new(),
                    "text": "hello",
                    "run": null,
                }),
            ),
            EventKind::FileAdded => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "file": Id::new(),
                    "chat": Id::new(),
                    "name": "report.csv",
                    "media_type": "text/csv",
                    "size": 7,
                    "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                }),
            ),
            EventKind::RunQueued => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "chat": Id::new(),
                    "bot": Id::new(),
                    "command": Id::new(),
                    "message": Id::new(),
                }),
            )
            .acting(user),
            EventKind::RunStarted => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "chat": Id::new(),
                    "bot": Id::new(),
                    "command": Id::new(),
                    "message": Id::new(),
                }),
            )
            .acting(user),
            EventKind::RunAccess => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "full_access": false,
                    "grants": ["/data/shared"],
                }),
            )
            .acting(user),
            EventKind::RunFinished => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "status": "succeeded",
                    "reply": null,
                    "usage": null,
                    "error": null,
                }),
            )
            .acting(user),
            EventKind::RunStep => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "step": 1,
                    "tool_calls": 0,
                }),
            )
            .acting(user),
            EventKind::ActionStarted => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "step": 1,
                    "action": Id::new(),
                    "tool": "shell",
                    "arguments": {"command": "true"},
                }),
            )
            .acting(user),
            EventKind::ActionFinished => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "step": 1,
                    "action": Id::new(),
                    "tool": "shell",
                    "status": "succeeded",
                    "output": "ok",
                }),
            )
            .acting(user),
            EventKind::RunReply => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "run": Id::new(),
                    "step": 1,
                    "content": null,
                    "calls": [{"call": "call-1-0", "tool": "shell", "arguments": {"command": "true"}}],
                    "usage": null,
                }),
            )
            .acting(user),
            EventKind::ApprovalRequested => EventDraft::new(
                kind,
                Author::Bot(Id::new()),
                serde_json::json!({
                    "approval": Id::new(),
                    "run": Id::new(),
                    "bot": Id::new(),
                    "computer": Id::new(),
                    "chat": Id::new(),
                    "acting": user,
                    "action": Id::new(),
                    "index": 0,
                    "step": 1,
                    "call": "call-1-0",
                    "tool": "shell",
                    "arguments": {"command": "true"},
                    "summary": "shell: true",
                    "reason": "first_use_local",
                    "lasting_scope": null,
                    "undoable": "no",
                }),
            )
            .acting(user),
            EventKind::ApprovalDecided => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "approval": Id::new(),
                    "run": Id::new(),
                    "allow": true,
                    "lasting": false,
                    "user": user,
                }),
            )
            .acting(user),
            EventKind::RuleAdded => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({
                    "rule": Id::new(),
                    "bot": Id::new(),
                    "user": user,
                    "effect": "ask",
                    "tool": "shell",
                }),
            )
            .acting(user),
            EventKind::RuleRemoved => EventDraft::new(
                kind,
                Author::User(user),
                serde_json::json!({"rule": Id::new(), "bot": Id::new(), "user": user}),
            )
            .acting(user),
        }
    }

    fn request(events: Vec<EventDraft>) -> AppendRequest {
        AppendRequest::new(Id::new(), "fingerprint", events)
    }

    #[tokio::test]
    async fn append_assigns_monotonic_sequences() {
        let (_dir, _home, store) = open().await;
        let result = store
            .append(request(vec![
                draft(EventKind::UserJoined),
                draft(EventKind::BotCreated),
                draft(EventKind::MessageAdded),
            ]))
            .await
            .unwrap();

        let seqs: Vec<i64> = result
            .events
            .iter()
            .map(|event| event.seq.as_i64())
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert!(!result.existing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_appends_get_unique_sequences() {
        let (_dir, _home, store) = open().await;
        let store = Arc::new(store);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                store
                    .append(request(vec![draft(EventKind::MessageAdded)]))
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let page = store.events_from(Seq::ZERO, 100).await.unwrap();
        let mut seqs: Vec<i64> = page.events.iter().map(|event| event.seq.as_i64()).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn the_same_command_returns_its_recorded_events() {
        let (_dir, _home, store) = open().await;
        let first = request(vec![draft(EventKind::BotCreated)]);
        let recorded = store.append(first.clone()).await.unwrap();

        let again = store.append(first).await.unwrap();
        let mut changed = request(vec![draft(EventKind::BotCreated)]);
        changed.command = recorded.events[0].command.unwrap();
        changed.fingerprint = "another".to_owned();
        let conflict = store.append(changed).await;

        assert!(again.existing);
        assert_eq!(again.events, recorded.events);
        assert!(matches!(conflict, Err(StoreError::IdempotencyConflict(_))));
    }

    #[tokio::test]
    async fn a_claim_reserves_an_idempotent_command_until_it_settles() {
        let (_dir, _home, store) = open().await;
        let command = Id::new();
        let fingerprint = "v1:reset";
        assert!(
            store
                .claim_command(command, fingerprint)
                .await
                .unwrap()
                .is_none(),
            "the first claim reserves the command"
        );
        let pending = store
            .claim_command(command, fingerprint)
            .await
            .unwrap()
            .expect("the second claim sees the reservation");
        assert!(pending.result.is_none(), "the reservation is unsettled");

        store
            .settle_command(command, serde_json::json!({"computer": Id::new()}))
            .await
            .unwrap();
        let settled = store.command(command).await.unwrap().unwrap();
        assert!(settled.result.is_some());

        let conflict = store.claim_command(command, "v1:other").await;
        assert!(matches!(conflict, Err(StoreError::IdempotencyConflict(_))));
    }

    #[tokio::test]
    async fn a_reopened_store_returns_the_same_record() {
        let (_dir, home, store) = open().await;
        let recorded = store
            .append(request(vec![draft(EventKind::MessageAdded)]))
            .await
            .unwrap();
        store.close().await.unwrap();

        let store = Store::open(&home).await.unwrap();
        let page = store.events_from(Seq::ZERO, 10).await.unwrap();

        assert_eq!(page.events, recorded.events);
        assert_eq!(page.next, recorded.events[0].seq);
        store.close().await.unwrap();
    }

    struct Collector(Vec<Event>);

    impl Projection for Collector {
        fn apply(&mut self, event: &Event) -> Result<(), String> {
            self.0.push(event.clone());
            Ok(())
        }
    }

    async fn connect_raw(home: &Home) -> sqlx::SqliteConnection {
        sqlx::SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(home.db_path())
                .create_if_missing(false),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn replay_stops_at_a_future_version_of_a_known_kind() {
        let (_dir, home, store) = open().await;
        store
            .append(request(vec![draft(EventKind::UserJoined)]))
            .await
            .unwrap();
        let mut connection = connect_raw(&home).await;
        sqlx::query("UPDATE events SET version = 2 WHERE seq = 1")
            .execute(&mut connection)
            .await
            .unwrap();

        let mut collector = Collector(Vec::new());
        let error = store.replay(&mut collector).await;

        assert!(matches!(error, Err(StoreError::UpgradeRequired { .. })));
    }

    #[tokio::test]
    async fn replay_stops_at_an_author_that_does_not_fit_the_kind() {
        let (_dir, home, store) = open().await;
        let message = draft(EventKind::MessageAdded);
        store.append(request(vec![message])).await.unwrap();
        let mut connection = connect_raw(&home).await;
        sqlx::query("UPDATE events SET author = ? WHERE seq = 1")
            .bind(serde_json::to_string(&Author::Bot(Id::new())).unwrap())
            .execute(&mut connection)
            .await
            .unwrap();

        let mut collector = Collector(Vec::new());
        let error = store.replay(&mut collector).await;

        assert!(matches!(error, Err(StoreError::Corrupt(_))));
    }

    #[tokio::test]
    async fn a_completion_confirms_its_command_in_one_transaction() {
        let (_dir, _home, store) = open().await;
        let starting = Id::new();
        let first = store
            .append(AppendRequest::new(
                starting,
                "start",
                vec![draft(EventKind::MessageAdded)],
            ))
            .await
            .unwrap();
        let completion = Id::new();
        store
            .append(
                AppendRequest::new(completion, "finish", vec![draft(EventKind::RunFinished)])
                    .with_confirmation(starting, serde_json::json!({"status": "succeeded"})),
            )
            .await
            .unwrap();

        let record = store.command(starting).await.unwrap().unwrap();
        assert_eq!(record.result.unwrap()["status"], "succeeded");
        assert_eq!(record.last_seq, first.events.last().unwrap().seq);
    }

    #[tokio::test]
    async fn an_unknown_completion_rolls_the_events_back() {
        let (_dir, _home, store) = open().await;
        let before = store.last_seq().await.unwrap();

        let error = store
            .append(
                AppendRequest::new(Id::new(), "finish", vec![draft(EventKind::RunFinished)])
                    .with_confirmation(Id::new(), serde_json::json!({"status": "failed"})),
            )
            .await;

        assert!(error.is_err());
        assert_eq!(store.last_seq().await.unwrap(), before);
    }

    #[tokio::test]
    async fn an_event_that_does_not_fit_its_schema_is_rejected() {
        let (_dir, _home, store) = open().await;
        let before = store.last_seq().await.unwrap();
        let mut missing = draft(EventKind::MessageAdded);
        missing.data = serde_json::json!({"message": Id::new(), "chat": Id::new()});
        let mut extra = draft(EventKind::MessageAdded);
        extra.data["surprise"] = serde_json::json!(true);
        let mut wrong_type = draft(EventKind::ProviderAdded);
        wrong_type.data["models"][0]["context_limit"] = serde_json::json!("many");
        let mut future = draft(EventKind::MessageAdded);
        future.version = 2;
        let mut bad_author = draft(EventKind::RunStarted);
        bad_author.author = Author::User(Id::new());

        for invalid in [missing, extra, wrong_type, future, bad_author] {
            let error = store.append(request(vec![invalid])).await;
            assert!(
                matches!(error, Err(StoreError::InvalidEvent(_))),
                "the record accepted invalid data"
            );
        }
        assert_eq!(store.last_seq().await.unwrap(), before);
    }

    #[tokio::test]
    async fn an_unknown_authority_event_stops_the_reader() {
        let (_dir, home, store) = open().await;
        insert_raw(&home, 50, "neha.future.authority", "authority").await;

        let error = store.events_from(Seq::ZERO, 10).await;

        assert!(matches!(error, Err(StoreError::UpgradeRequired { .. })));
    }

    #[tokio::test]
    async fn an_unknown_presentation_event_is_skipped() {
        let (_dir, home, store) = open().await;
        insert_raw(&home, 50, "neha.future.progress", "presentation").await;

        let page = store.events_from(Seq::ZERO, 10).await.unwrap();

        assert!(page.events.is_empty());
        assert_eq!(page.next.as_i64(), 50);
    }

    #[tokio::test]
    async fn replay_applies_events_in_order() {
        struct Collector(Vec<Event>);

        impl Projection for Collector {
            fn apply(&mut self, event: &Event) -> Result<(), String> {
                self.0.push(event.clone());
                Ok(())
            }
        }

        let (_dir, _home, store) = open().await;
        store
            .append(request(vec![
                draft(EventKind::UserJoined),
                draft(EventKind::MessageAdded),
            ]))
            .await
            .unwrap();
        let mut collector = Collector(Vec::new());

        let cursor = store.replay(&mut collector).await.unwrap();

        assert_eq!(collector.0.len(), 2);
        assert_eq!(collector.0[0].seq.as_i64(), 1);
        assert_eq!(collector.0[1].seq.as_i64(), 2);
        assert_eq!(cursor.as_i64(), 2);
    }

    #[tokio::test]
    async fn a_reply_continuation_is_written_with_its_reply() {
        let (_dir, _home, store) = open().await;
        let run = Id::new();
        let user = Id::new();
        let bot = Id::new();
        let continuation = neha_loop::Continuation {
            protocol: "anthropic_messages".to_owned(),
            provider: "provider".to_owned(),
            model: "model".to_owned(),
            state: serde_json::json!({"blocks": [{"type": "thinking", "signature": "sig"}]}),
        };
        let reply = EventDraft::new(
            EventKind::RunReply,
            Author::Bot(bot),
            serde_json::json!({
                "run": run,
                "step": 1,
                "content": null,
                "calls": [{"call": "call-1", "tool": "shell", "arguments": {"command": "true"}}],
                "usage": null,
            }),
        )
        .acting(user);
        store
            .append(
                AppendRequest::new(Id::new(), "run.reply", vec![reply]).with_continuation(
                    ContinuationWrite {
                        run,
                        step: 1,
                        continuation: Some(continuation.clone()),
                    },
                ),
            )
            .await
            .unwrap();

        assert_eq!(
            store.continuation(run, 1).await.unwrap(),
            Some(continuation.clone())
        );

        // A later failed retry of the same step clears the stale state.
        let reply = EventDraft::new(
            EventKind::RunReply,
            Author::Bot(bot),
            serde_json::json!({
                "run": run,
                "step": 2,
                "content": null,
                "calls": [{"call": "call-2", "tool": "shell", "arguments": {"command": "true"}}],
                "usage": null,
            }),
        )
        .acting(user);
        store
            .append(
                AppendRequest::new(Id::new(), "run.reply.2", vec![reply]).with_continuation(
                    ContinuationWrite {
                        run,
                        step: 2,
                        continuation: Some(continuation),
                    },
                ),
            )
            .await
            .unwrap();
        store
            .append(
                AppendRequest::new(
                    Id::new(),
                    "run.reply.2.clear",
                    vec![EventDraft::new(
                    EventKind::RunStep,
                    Author::Bot(bot),
                    serde_json::json!({"run": run, "step": 2, "usage": null, "tool_calls": 1}),
                )
                .acting(user)],
                )
                .with_continuation(ContinuationWrite {
                    run,
                    step: 2,
                    continuation: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(store.continuation(run, 2).await.unwrap(), None);

        let finished = EventDraft::new(
            EventKind::RunFinished,
            Author::Bot(bot),
            serde_json::json!({
                "run": run,
                "status": "succeeded",
                "reply": Id::new(),
                "usage": null,
            }),
        )
        .acting(user);
        store
            .append(AppendRequest::new(
                Id::new(),
                "run.finished",
                vec![finished],
            ))
            .await
            .unwrap();
        assert_eq!(store.continuation(run, 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_command_must_write_an_event() {
        let (_dir, _home, store) = open().await;

        let error = store.append(request(Vec::new())).await;

        assert!(matches!(error, Err(StoreError::EmptyCommand)));
    }

    async fn insert_raw(home: &Home, seq: i64, kind: &str, class: &str) {
        let mut connection = sqlx::SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(home.db_path())
                .create_if_missing(false),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (seq, kind, class, version, at, author, acting, command, data) \
             VALUES (?, ?, ?, 1, 0, ?, NULL, NULL, '{}')",
        )
        .bind(seq)
        .bind(kind)
        .bind(class)
        .bind(serde_json::to_string(&Author::User(Id::new())).unwrap())
        .execute(&mut connection)
        .await
        .unwrap();
    }
}
