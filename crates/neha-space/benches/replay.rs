//! Measures how a Space start's replay grows with the record.
//!
//! `cargo bench -p neha-space --bench replay` seeds synthetic records of 10^5,
//! 10^6, and 10^7 events in temporary homes, replays each into the real
//! projection, and prints a markdown table of wall-clock time and resident
//! memory. The benchmark is not part of `just ci`: the largest record needs
//! about 5 GB of disk and a minute of replay.
//!
//! Seeding writes the rows `Store::append` would write, through raw SQL in
//! batches, because the append path costs one transaction per command. The
//! event mix follows one run of a Bot with three tool calls, as the built-in
//! loop records it: a user Message with its run, the run's queue and start
//! with its access, a reply and an action pair per step with the step after
//! them, a final reply with no calls, then the Bot's reply Message and the
//! run's finish.

use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use neha::event::EventClass;
use neha::id::Id;
use neha::record::{Author, EventKind, KIND_VERSION};
use neha_space::home::Home;
use neha_space::state::State;
use neha_space::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection};

/// The record sizes the default run measures.
const COUNTS: [usize; 3] = [100_000, 1_000_000, 10_000_000];

/// The events one successful run of a Bot writes.
const CYCLE_EVENTS: usize = 20;

/// Rows per `INSERT`; nine columns, below SQLite's 32766-variable limit.
const INSERT_BATCH: usize = 500;

/// Rows per transaction.
const TRANSACTION_ROWS: usize = 100_000;

/// The first recorded instant, in microseconds since the epoch.
const FIRST_AT: i64 = 1_700_000_000_000_000;

/// The text of a user Message.
const USER_TEXT: &str = "The parser test fails on the second fixture. Read the module, \
    reproduce the failure locally, and tell me which branch turns the newline into a \
    truncated token.";

/// The text of a Bot's reply Message.
const BOT_TEXT: &str = "The failure comes from the tokenizer, not the parser: the scanner \
    treats a trailing newline as the end of the buffered line and drops the token that \
    starts after it. The second fixture ends with a newline, so its last token never \
    reaches the parser. I changed the scanner to keep the pending token until the next \
    non-empty input, and the fixture now passes with the same expected tree.";

/// A markdown row plus the numbers a pull request quotes.
#[derive(Debug)]
struct Row {
    events: usize,
    seed: Duration,
    db_bytes: u64,
    first: Duration,
    second: Duration,
    rss_kib: u64,
    peak_kib: u64,
    messages: usize,
    runs: usize,
    steps: usize,
    actions: usize,
    sqlite: String,
}

/// One recorded row before it is inserted.
struct RawRow {
    seq: i64,
    at: i64,
    kind: &'static str,
    class: &'static str,
    author: String,
    acting: Option<String>,
    command: String,
    data: String,
}

/// One event before the seeding loop assigns its sequence number and instant.
struct Spec {
    kind: EventKind,
    author: Author,
    acting: Option<Id>,
    command: Id,
    data: Value,
}

impl Spec {
    fn raw(self, seq: i64, at: i64) -> RawRow {
        RawRow {
            seq,
            at,
            kind: self.kind.as_str(),
            class: match self.kind.class() {
                EventClass::Presentation => "presentation",
                EventClass::Authority => "authority",
            },
            author: serde_json::to_string(&self.author).expect("an Author is JSON"),
            acting: self.acting.map(|id| id.to_string()),
            command: self.command.to_string(),
            data: self.data.to_string(),
        }
    }
}

/// Deterministic UUID version 7 identifiers, one counter for every entity.
#[derive(Default)]
struct Ids(u64);

impl Ids {
    fn next(&mut self) -> Id {
        self.0 += 1;
        format!("0190a5e0-0000-7000-8000-{:012x}", self.0)
            .parse()
            .expect("a canonical UUID version 7")
    }
}

/// The token counts a Model call reports.
fn usage(input: u64, output: u64) -> Value {
    json!({
        "uncached_input": input,
        "cache_read": 0,
        "cache_write": 0,
        "output": output,
        "reasoning": 0,
    })
}

/// Writes one setup block, then one run cycle per call.
struct Generator {
    user: Id,
    computer: Id,
    bot: Id,
    chat: Id,
    ids: Ids,
}

impl Generator {
    fn new() -> Self {
        let mut ids = Ids::default();
        Self {
            user: ids.next(),
            computer: ids.next(),
            bot: ids.next(),
            chat: ids.next(),
            ids,
        }
    }

    /// An event the Bot writes for the User it acts for.
    fn bot(&self, kind: EventKind, command: Id, data: Value) -> Spec {
        Spec {
            kind,
            author: Author::Bot(self.bot),
            acting: Some(self.user),
            command,
            data,
        }
    }

    /// An event the User writes.
    fn user(&self, kind: EventKind, command: Id, data: Value) -> Spec {
        Spec {
            kind,
            author: Author::User(self.user),
            acting: None,
            command,
            data,
        }
    }

    /// The events that create the User, Computer, Bot, and Chat a cycle uses.
    fn setup(&mut self) -> Vec<Spec> {
        let joined = self.ids.next();
        let computer = self.ids.next();
        let bot = self.ids.next();
        let chat = self.ids.next();
        vec![
            self.user(
                EventKind::UserJoined,
                joined,
                json!({"display_name": "Bench User", "is_admin": true}),
            ),
            self.user(
                EventKind::ComputerCreated,
                computer,
                json!({
                    "computer": self.computer,
                    "owner": self.user,
                    "kind": "hosted",
                    "workspace": "/tmp/neha-bench",
                }),
            ),
            self.user(
                EventKind::BotCreated,
                bot,
                json!({
                    "bot": self.bot,
                    "owner": self.user,
                    "name": "Bench Bot",
                    "computer": self.computer,
                    "chat": self.chat,
                }),
            ),
            self.user(
                EventKind::ChatCreated,
                chat,
                json!({"chat": self.chat, "owner": self.user, "bot": self.bot}),
            ),
        ]
    }

    /// The events one successful three-call run writes, in record order.
    fn cycle(&mut self) -> Vec<Spec> {
        let message = self.ids.next();
        let run = self.ids.next();
        let reply = self.ids.next();
        let sent = self.ids.next();
        let started = self.ids.next();
        let mut events = Vec::with_capacity(CYCLE_EVENTS);
        // One command records the Message and the run it queues.
        events.push(self.user(
            EventKind::MessageAdded,
            sent,
            json!({
                "message": message,
                "chat": self.chat,
                "text": USER_TEXT,
                "run": run,
            }),
        ));
        events.push(Spec {
            kind: EventKind::RunQueued,
            author: Author::Bot(self.bot),
            acting: Some(self.user),
            command: sent,
            data: json!({
                "run": run,
                "chat": self.chat,
                "bot": self.bot,
                "command": sent,
                "message": message,
            }),
        });
        // One command records the start and the access the run carries.
        events.push(self.bot(
            EventKind::RunStarted,
            started,
            json!({
                "run": run,
                "chat": self.chat,
                "bot": self.bot,
                "command": sent,
                "message": message,
            }),
        ));
        events.push(self.bot(
            EventKind::RunAccess,
            started,
            json!({
                "run": run,
                "full_access": false,
                "grants": [],
                "cwd_writable": false,
            }),
        ));
        // Each step records its reply, then each action, then the step itself.
        for step in 1..=3_u32 {
            let action = self.ids.next();
            let replied = self.ids.next();
            let acted = self.ids.next();
            let finished = self.ids.next();
            let stepped = self.ids.next();
            events.push(self.bot(
                EventKind::RunReply,
                replied,
                json!({
                    "run": run,
                    "step": step,
                    "content": null,
                    "calls": [{
                        "call": format!("call-{step}"),
                        "tool": "shell",
                        "arguments": {"command": "cargo test parser"},
                        "refused": null,
                    }],
                    "usage": usage(1500, 120),
                }),
            ));
            events.push(self.bot(
                EventKind::ActionStarted,
                acted,
                json!({
                    "run": run,
                    "step": step,
                    "action": action,
                    "tool": "shell",
                    "arguments": {"command": "cargo test parser"},
                }),
            ));
            events.push(self.bot(
                EventKind::ActionFinished,
                finished,
                json!({
                    "run": run,
                    "step": step,
                    "action": action,
                    "tool": "shell",
                    "status": "succeeded",
                    "output": "ok",
                }),
            ));
            events.push(self.bot(
                EventKind::RunStep,
                stepped,
                json!({"run": run, "step": step, "usage": usage(1500, 120), "tool_calls": 1}),
            ));
        }
        // The step with no tool calls still records its reply and its step.
        let final_reply = self.ids.next();
        let final_step = self.ids.next();
        events.push(self.bot(
            EventKind::RunReply,
            final_reply,
            json!({
                "run": run,
                "step": 4_u32,
                "content": BOT_TEXT,
                "calls": [],
                "usage": usage(1200, 300),
            }),
        ));
        events.push(self.bot(
            EventKind::RunStep,
            final_step,
            json!({
                "run": run,
                "step": 4_u32,
                "usage": usage(1200, 300),
                "tool_calls": 0,
            }),
        ));
        // One command records the reply Message and the run's finish.
        let completed = self.ids.next();
        events.push(self.bot(
            EventKind::MessageAdded,
            completed,
            json!({
                "message": reply,
                "chat": self.chat,
                "text": BOT_TEXT,
                "run": run,
            }),
        ));
        events.push(self.bot(
            EventKind::RunFinished,
            completed,
            json!({
                "run": run,
                "status": "succeeded",
                "reply": reply,
                "usage": usage(5700, 660),
                "error": null,
            }),
        ));
        events
    }
}

#[derive(Deserialize, Serialize)]
struct SeedReport {
    events: usize,
    seed_millis: u64,
    db_bytes: u64,
    sqlite: String,
}

#[derive(Deserialize, Serialize)]
struct ReplayReport {
    first_millis: u64,
    second_millis: u64,
    rss_kib: u64,
    peak_kib: u64,
    messages: usize,
    runs: usize,
    steps: usize,
    actions: usize,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Measure every record size, each in fresh child processes.
    All,
    /// Seed one record and print its seed report.
    Seed,
    /// Replay one seeded record and print its replay report.
    Replay,
}

struct Args {
    mode: Mode,
    counts: Vec<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut mode = Mode::All;
    let mut counts: Option<Vec<usize>> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => mode = Mode::Seed,
            "--replay" => mode = Mode::Replay,
            // `cargo bench` passes `--bench` to a harness-less benchmark.
            "--bench" => {}
            "--events" => {
                let value = args.next().ok_or("--events needs a value")?;
                let parsed = value
                    .split(',')
                    .map(str::parse::<usize>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| format!("--events: {error}"))?;
                counts = Some(parsed);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let counts = counts.unwrap_or_else(|| COUNTS.to_vec());
    match mode {
        Mode::All => {}
        Mode::Seed | Mode::Replay if counts.len() != 1 => {
            return Err("--seed and --replay take exactly one --events value".to_owned());
        }
        Mode::Seed | Mode::Replay => {}
    }
    Ok(Args { mode, counts })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("replay benchmark: {error}");
            std::process::exit(2);
        }
    };
    let result = match args.mode {
        Mode::All => run_all(&args.counts).await,
        Mode::Seed => run_seed(args.counts[0]).await.map(|report| {
            println!(
                "{}",
                serde_json::to_string(&report).expect("a report is JSON")
            );
        }),
        Mode::Replay => run_replay(args.counts[0]).await.map(|report| {
            println!(
                "{}",
                serde_json::to_string(&report).expect("a report is JSON")
            );
        }),
    };
    if let Err(error) = result {
        eprintln!("replay benchmark: {error}");
        std::process::exit(1);
    }
}

async fn run_all(counts: &[usize]) -> Result<(), Box<dyn Error>> {
    let mut rows = Vec::new();
    for &count in counts {
        let home = home_path(count);
        // An interrupted earlier run leaves its home behind.
        let _ = std::fs::remove_dir_all(&home);
        let row = match scenario(count) {
            Ok(row) => row,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(error);
            }
        };
        let _ = std::fs::remove_dir_all(&home);
        rows.push(row);
    }
    print_table(&rows);
    Ok(())
}

fn scenario(events: usize) -> Result<Row, Box<dyn Error>> {
    let seed: SeedReport = serde_json::from_value(child_output("--seed", events)?)?;
    let replay: ReplayReport = serde_json::from_value(child_output("--replay", events)?)?;
    Ok(Row {
        events: seed.events,
        seed: Duration::from_millis(seed.seed_millis),
        db_bytes: seed.db_bytes,
        first: Duration::from_millis(replay.first_millis),
        second: Duration::from_millis(replay.second_millis),
        rss_kib: replay.rss_kib,
        peak_kib: replay.peak_kib,
        messages: replay.messages,
        runs: replay.runs,
        steps: replay.steps,
        actions: replay.actions,
        sqlite: seed.sqlite,
    })
}

/// Runs this binary again for one mode and one record size.
fn child_output(mode: &str, events: usize) -> Result<Value, Box<dyn Error>> {
    let output = Command::new(std::env::current_exe()?)
        .arg(mode)
        .arg("--events")
        .arg(events.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "the {mode} child for {events} events exited {}",
            output.status
        )
        .into());
    }
    let text = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(text.trim())?)
}

async fn run_seed(events: usize) -> Result<SeedReport, Box<dyn Error>> {
    let home = Home::open(home_path(events))?;
    let started = Instant::now();
    // Open the store first: it creates and migrates the schema the raw rows use.
    let store = Store::open(&home).await?;
    let mut connection = SqliteConnection::connect_with(&seed_options(&home)).await?;
    let (written, sqlite) = seed(&mut connection, events).await?;
    // Best effort: a checkpoint keeps the database file self-contained so the
    // replay process starts from a plain file. A held reader only leaves the
    // WAL in place, which a later open still reads.
    let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut connection)
        .await;
    drop(connection);
    drop(store);
    let db_bytes = database_bytes(&home);
    Ok(SeedReport {
        events: written,
        seed_millis: millis(started.elapsed()),
        db_bytes,
        sqlite,
    })
}

fn seed_options(home: &Home) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(home.db_path())
        .journal_mode(SqliteJournalMode::Wal)
        // Seeding is not measured, so it does not pay for durability.
        .synchronous(SqliteSynchronous::Off)
}

fn database_bytes(home: &Home) -> u64 {
    let database = std::fs::metadata(home.db_path()).map_or(0, |file| file.len());
    let wal = std::fs::metadata(home.db_path().with_extension("sqlite3-wal"))
        .map_or(0, |file| file.len());
    database.saturating_add(wal)
}

async fn seed(
    connection: &mut SqliteConnection,
    events: usize,
) -> Result<(usize, String), Box<dyn Error>> {
    let sqlite: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut *connection)
        .await?;
    sqlx::query("BEGIN").execute(&mut *connection).await?;
    let mut generator = Generator::new();
    let mut batch: Vec<RawRow> = Vec::with_capacity(INSERT_BATCH);
    let mut written = 0_usize;
    let mut rows_in_transaction = 0_usize;
    for spec in generator.setup() {
        written += 1;
        batch.push(spec.raw(written as i64, FIRST_AT + written as i64));
    }
    while written + CYCLE_EVENTS <= events {
        for spec in generator.cycle() {
            written += 1;
            batch.push(spec.raw(written as i64, FIRST_AT + written as i64));
        }
        if batch.len() >= INSERT_BATCH {
            rows_in_transaction += batch.len();
            insert_batch(&mut *connection, &batch).await?;
            batch.clear();
        }
        if rows_in_transaction >= TRANSACTION_ROWS {
            sqlx::query("COMMIT").execute(&mut *connection).await?;
            sqlx::query("BEGIN").execute(&mut *connection).await?;
            rows_in_transaction = 0;
        }
    }
    if !batch.is_empty() {
        insert_batch(&mut *connection, &batch).await?;
    }
    sqlx::query("UPDATE meta SET value = ? WHERE key = 'last_seq'")
        .bind(written.to_string())
        .execute(&mut *connection)
        .await?;
    sqlx::query("COMMIT").execute(&mut *connection).await?;
    Ok((written, sqlite))
}

async fn insert_batch(
    connection: &mut SqliteConnection,
    rows: &[RawRow],
) -> Result<(), sqlx::Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "INSERT INTO events (seq, kind, class, version, at, author, acting, command, data) ",
    );
    builder.push_values(rows, |mut row, event| {
        row.push_bind(event.seq)
            .push_bind(event.kind)
            .push_bind(event.class)
            .push_bind(i64::from(KIND_VERSION))
            .push_bind(event.at)
            .push_bind(event.author.as_str())
            .push_bind(event.acting.as_deref())
            .push_bind(event.command.as_str())
            .push_bind(event.data.as_str());
    });
    builder.build().execute(connection).await?;
    Ok(())
}

async fn run_replay(events: usize) -> Result<ReplayReport, Box<dyn Error>> {
    let home = Home::open(home_path(events))?;
    let store = Store::open(&home).await?;
    let mut state = State::default();
    let rss_before = resident_kib();
    let started = Instant::now();
    store.replay(&mut state).await?;
    let first = started.elapsed();
    let rss_after = resident_kib();
    let peak = peak_resident_kib();
    let report = ReplayReport {
        first_millis: millis(first),
        second_millis: 0,
        rss_kib: rss_after.saturating_sub(rss_before),
        peak_kib: peak,
        messages: state.messages.len(),
        runs: state.runs.len(),
        steps: state.runs.values().map(|run| run.steps.len()).sum(),
        actions: state.runs.values().map(|run| run.actions.len()).sum(),
    };
    // The second replay reads the file the seeding and the first replay just
    // touched, so it shows the cost without database reads from disk. It does
    // not measure a cold start: the page cache is warm from seeding.
    drop(state);
    let mut second = State::default();
    let started = Instant::now();
    store.replay(&mut second).await?;
    let second_millis = millis(started.elapsed());
    drop(second);
    Ok(ReplayReport {
        second_millis,
        ..report
    })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn print_table(rows: &[Row]) {
    println!(
        "| events | seed | database | replay (first) | replay (second) | events/s | RSS delta | peak RSS | messages | runs | steps | actions |"
    );
    println!("|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|");
    for row in rows {
        let mut line = String::new();
        let seconds = |value: Duration| format!("{:.1}s", value.as_secs_f64());
        let millis = |value: Duration| format!("{:.2}s", value.as_secs_f64());
        write!(
            line,
            "| {} | {} | {:.0} MiB | {} | {} | {:.0} | {:.0} MiB | {:.0} MiB | {} | {} | {} | {} |",
            row.events,
            seconds(row.seed),
            row.db_bytes as f64 / (1024.0 * 1024.0),
            millis(row.first),
            millis(row.second),
            row.events as f64 / row.first.as_secs_f64(),
            row.rss_kib as f64 / 1024.0,
            row.peak_kib as f64 / 1024.0,
            row.messages,
            row.runs,
            row.steps,
            row.actions,
        )
        .expect("a String write cannot fail");
        println!("{line}");
    }
    println!();
    println!(
        "Measured on {} with {} CPUs and SQLite {}.",
        std::env::consts::OS,
        std::thread::available_parallelism().map_or(0, |count| count.get()),
        rows.first().map_or("unknown", |row| row.sqlite.as_str()),
    );
}

fn resident_kib() -> u64 {
    status_kib("VmRSS:")
}

fn peak_resident_kib() -> u64 {
    status_kib("VmHWM:")
}

/// Reads one field, in kibibytes, from `/proc/self/status`; 0 elsewhere.
fn status_kib(field: &str) -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn home_path(events: usize) -> PathBuf {
    workspace_target()
        .join("bench-homes")
        .join(events.to_string())
}

fn workspace_target() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        },
        PathBuf::from,
    )
}
