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
        }
    }

    /// Attaches the command's result.
    #[must_use]
    pub fn with_result(mut self, result: serde_json::Value) -> Self {
        self.result = Some(result);
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
    SetResult {
        command: Id,
        result: serde_json::Value,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
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
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{EVENTS} WHERE seq > ? ORDER BY seq LIMIT ?"
        )))
        .bind(cursor.as_i64())
        .bind(limit)
        .fetch_all(&self.reader)
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

    /// Stores the result of a command that finished later, such as a run.
    pub async fn set_result(
        &self,
        command: Id,
        result: serde_json::Value,
    ) -> Result<(), StoreError> {
        let (reply, answer) = oneshot::channel();
        self.writer
            .send(WriteRequest::SetResult {
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
                WriteRequest::SetResult {
                    command,
                    result,
                    reply,
                } => {
                    let _ = reply.send(set_result(&mut connection, command, result).await);
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

async fn append(
    connection: &mut SqliteConnection,
    request: AppendRequest,
) -> Result<AppendResult, StoreError> {
    if request.events.is_empty() {
        return Err(StoreError::EmptyCommand);
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

async fn set_result(
    connection: &mut SqliteConnection,
    command: Id,
    result: serde_json::Value,
) -> Result<(), StoreError> {
    let text = serde_json::to_string(&result).map_err(|_| corrupt("command result"))?;
    sqlx::query("UPDATE commands SET result = ? WHERE command_id = ?")
        .bind(text)
        .bind(command.to_string())
        .execute(&mut *connection)
        .await?;

    Ok(())
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

    use super::{AppendRequest, Projection, Store, StoreError};
    use crate::home::Home;

    async fn open() -> (tempfile::TempDir, Home, Store) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let store = Store::open(&home).await.unwrap();

        (dir, home, store)
    }

    fn draft(kind: EventKind) -> EventDraft {
        EventDraft::new(
            kind,
            Author::User(Id::new()),
            serde_json::json!({"ok": true}),
        )
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
