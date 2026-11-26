//! Exercises the built CLI against a real Space and a fake Provider.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use axum::Router;
use axum::response::Response;
use axum::routing::{get, post};

const PASSWORD: &str = "correct horse battery";
const BIN: &str = env!("CARGO_BIN_EXE_neha");

struct SpaceProcess(Child);

impl Drop for SpaceProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `neha serve` and waits for its address, and its invitation on a
/// fresh Space.
///
/// A Space that printed its address and then died would fail a test's next
/// command with a bare "Connection refused", so its health endpoint is probed
/// before the fixture is handed back. A start that fails is retried; when
/// every attempt fails, the panic carries the Space's own stderr.
fn serve(home: &Path) -> (SpaceProcess, String, Option<String>) {
    let mut failures = String::new();
    for attempt in 1..=3 {
        match serve_once(home) {
            Ok(space) => return space,
            Err(error) => {
                eprintln!("neha serve failed on start {attempt}:\n{error}");
                failures.push_str(&format!("start {attempt}:\n{error}\n"));
            }
        }
    }

    panic!("neha serve did not start after 3 attempts:\n{failures}");
}

/// Starts one `neha serve` and waits until it answers on its own address.
fn serve_once(home: &Path) -> Result<(SpaceProcess, String, Option<String>), String> {
    let mut child = Command::new(BIN)
        .args(["serve", "--port", "0"])
        .env("NEHA_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run neha serve: {error}"))?;
    let received = line_reader(child.stdout.take().unwrap());
    // The Space's stderr is kept, so a start that fails can report why.
    let errors = Arc::new(Mutex::new(Vec::new()));
    {
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::clone(&errors);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                errors.lock().expect("the stderr buffer").push(line);
            }
        });
    }

    let mut url = None;
    let mut invite = None;
    while url.is_none() || invite.is_none() {
        let line = match received.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                return Err(serve_failure(
                    &mut child,
                    &errors,
                    format!("it did not print its address and invitation: {error}"),
                ));
            }
        };
        if let Some(address) = line.strip_prefix("Neha is listening on ") {
            url = Some(address.to_owned());
        }
        if let Some(link) = line.strip_prefix("Invite: ") {
            invite = Some(link.to_owned());
        }
        if url.is_some() && invite.is_none() {
            // A Space that already has its Admin prints no invitation.
            match received.recv_timeout(Duration::from_millis(300)) {
                Ok(next) if next.starts_with("Invite: ") => {
                    invite = Some(next.trim_start_matches("Invite: ").to_owned());
                }
                Ok(_) | Err(_) => break,
            }
        }
    }
    let url = url.expect("the address was read");

    // A Space that printed its address may still die before it serves; prove
    // the port answers before a test's first command can race it.
    if !serves_health(&mut child, &url) {
        return Err(serve_failure(
            &mut child,
            &errors,
            format!("it did not answer /healthz on {url}"),
        ));
    }

    Ok((SpaceProcess(child), url, invite))
}

/// Sends `GET /healthz` to one advertised address until it answers.
///
/// The probe is a bounded blocking socket, so it proves the port accepts a
/// connection before a test's first command needs it. It gives up when the
/// Space exits or the attempts run out.
fn serves_health(child: &mut Child, url: &str) -> bool {
    use std::io::{Read as _, Write as _};

    let address = url.strip_prefix("http://").unwrap_or(url);
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    for _ in 0..50 {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return false;
        }
        if std::net::TcpStream::connect(address).is_ok_and(|mut stream| {
            if stream.write_all(request.as_bytes()).is_err() {
                return false;
            }
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .is_ok_and(|_| response.starts_with("HTTP/1.1 200"))
        }) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    false
}

/// Kills one failed Space and explains why it failed, with its stderr.
fn serve_failure(
    child: &mut Child,
    errors: &Mutex<Vec<String>>,
    reason: impl std::fmt::Display,
) -> String {
    let exited = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!(" (it exited: {status})"))
        .unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();
    let stderr = errors.lock().expect("the stderr buffer").join("\n");
    if stderr.is_empty() {
        format!("{reason}{exited}")
    } else {
        format!("{reason}{exited}\nstderr:\n{stderr}")
    }
}

fn cli(home: &Path, credential: &Path, arguments: &[&str]) -> Output {
    cli_in(None, home, credential, arguments)
}

/// Runs the CLI, optionally from another directory than this test's.
fn cli_in(directory: Option<&Path>, home: &Path, credential: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new(BIN);
    command
        .args(arguments)
        .env("NEHA_HOME", home)
        .env("NEHA_CREDENTIAL", credential)
        .env("NEHA_PROVIDER_KEY", "sk-test")
        .env("NEHA_PASSWORD", PASSWORD);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }

    command.output().expect("neha runs")
}

fn run(home: &Path, credential: &Path, arguments: &[&str]) -> String {
    let output = cli(home, credential, arguments);
    assert!(
        output.status.success(),
        "`neha {}` failed:\nstdout: {}\nstderr: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Reads a piped stream line by line on its own thread, and hands the lines
/// over until the receiving end goes away.
fn line_reader(stream: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    received
}

/// Starts the CLI in the background and returns readers of its stdout and
/// stderr.
fn spawn_cli(
    home: &Path,
    credential: &Path,
    arguments: &[&str],
) -> (Child, mpsc::Receiver<String>, mpsc::Receiver<String>) {
    let mut child = Command::new(BIN)
        .args(arguments)
        .env("NEHA_HOME", home)
        .env("NEHA_CREDENTIAL", credential)
        .env("NEHA_PROVIDER_KEY", "sk-test")
        .env("NEHA_PASSWORD", PASSWORD)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("neha runs");
    let stdout = line_reader(child.stdout.take().unwrap());
    let stderr = line_reader(child.stderr.take().unwrap());

    (child, stdout, stderr)
}

/// Reads a background CLI's stdout until it prints this line, appending to
/// `output`.
fn read_until(lines: &mpsc::Receiver<String>, output: &mut String, expected: &str) {
    read_until_matching(lines, output, &format!("`{expected}`"), |line| {
        line == expected
    });
}

/// Reads a background CLI's stdout until a line satisfies `matches`, appends
/// every line it read to `output`, and returns the matching line.
fn read_until_matching(
    lines: &mpsc::Receiver<String>,
    output: &mut String,
    describe: &str,
    matches: impl Fn(&str) -> bool,
) -> String {
    loop {
        let line = lines
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("the CLI did not print {describe}:\n{output}"));
        output.push_str(&line);
        output.push('\n');
        if matches(&line) {
            return line;
        }
    }
}

/// Returns the first word of a field a command printed.
fn first_word(field: &str) -> String {
    field
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Waits for a background CLI and returns whether it succeeded and all stdout.
fn finish(mut child: Child, lines: mpsc::Receiver<String>, output: String) -> (bool, String) {
    let status = child.wait().expect("neha exits");
    let mut output = output;
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(10)) {
        output.push_str(&line);
        output.push('\n');
    }

    (status.success(), output)
}

fn field(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("`{output}` has no line starting with `{prefix}`"))
        .trim()
        .to_owned()
}

async fn fake_provider() -> (String, tokio::task::JoinHandle<()>) {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}/v1"), task)
}

/// A Space that serves one File's bytes and names it in no header.
async fn unnamed_file_server(bytes: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/api/files/{file}", get(move || async move { bytes }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), task)
}

#[derive(Clone, Debug)]
enum ScriptedReply {
    Text(String),
    /// Answers once the test releases it, so a run can be held open.
    Hold(String),
    Tool {
        name: String,
        arguments: serde_json::Value,
    },
    Fail,
}

/// The SSE body that answers a request with one text.
fn text_body(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        serde_json::to_string(text).unwrap()
    )
}

/// Lets each held reply finish when the test releases it.
#[derive(Clone)]
struct Gate(Arc<tokio::sync::Semaphore>);

impl Gate {
    fn new() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(0)))
    }

    /// Lets one held reply finish.
    fn release(&self) {
        self.0.add_permits(1);
    }

    async fn wait(&self) {
        // Spend the permit: one release lets one held reply finish.
        if let Ok(permit) = self.0.acquire().await {
            permit.forget();
        }
    }
}

/// The replies a fake Provider gives, one per request in the order queued.
#[derive(Clone)]
struct Script {
    replies: tokio::sync::mpsc::UnboundedSender<ScriptedReply>,
    gate: Gate,
}

impl Script {
    fn push(&self, reply: ScriptedReply) {
        self.replies
            .send(reply)
            .expect("the fake Provider is still running");
    }

    /// Answers the next request with this text.
    fn say(&self, text: &str) {
        self.push(ScriptedReply::Text(text.to_owned()));
    }

    /// Answers the next request with one tool call.
    fn call(&self, name: &str, arguments: serde_json::Value) {
        self.push(ScriptedReply::Tool {
            name: name.to_owned(),
            arguments,
        });
    }

    /// Refuses the next request, so its run fails.
    fn fail(&self) {
        self.push(ScriptedReply::Fail);
    }

    /// Answers the next request with this text, once the test releases it.
    fn hold(&self, text: &str) {
        self.push(ScriptedReply::Hold(text.to_owned()));
    }

    /// Lets the oldest held reply finish.
    fn release(&self) {
        self.gate.release();
    }
}

async fn scripted_provider() -> (String, Script, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (replies, queued) = tokio::sync::mpsc::unbounded_channel::<ScriptedReply>();
    let queued = Arc::new(tokio::sync::Mutex::new(queued));
    let requests = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new();
    let script_gate = gate.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let queued = Arc::clone(&queued);
            let requests = Arc::clone(&requests);
            let gate = gate.clone();
            async move {
                let index = requests.fetch_add(1, Ordering::SeqCst);
                let reply =
                    tokio::time::timeout(Duration::from_secs(10), queued.lock().await.recv())
                        .await
                        .expect("the fake Provider got more requests than the script answers")
                        .expect("the script is closed");
                let body = match reply {
                    ScriptedReply::Text(text) => text_body(&text),
                    ScriptedReply::Hold(text) => {
                        gate.wait().await;
                        text_body(&text)
                    }
                    ScriptedReply::Fail => {
                        // A rejected request is not retried, so the run fails once.
                        return Response::builder()
                            .status(400)
                            .body(axum::body::Body::from("the fake Provider refused"))
                            .unwrap();
                    }
                    ScriptedReply::Tool { name, arguments } => {
                        let chunk = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
                            "index": 0,
                            "id": format!("call-{index}"),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(&arguments).unwrap(),
                            }
                        }]}}]});
                        format!("data: {chunk}\n\ndata: [DONE]\n\n")
                    }
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (
        format!("http://{addr}/v1"),
        Script {
            replies,
            gate: script_gate,
        },
        task,
    )
}

/// Runs a real-Computer test only where this machine can fence a Bot.
///
/// `NEHA_TEST_BWRAP` is the gate, as for the runtime's own fence tests. Once
/// it is set, a sandbox that cannot start makes the test fail with
/// bubblewrap's own error: CI promises the real path, so a broken sandbox
/// must be red instead of silently skipped.
fn real_computer_ready() -> bool {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return false;
    }
    let probe = tempfile::tempdir().unwrap();
    if let Err(error) = neha_computer::LocalRuntime::machine(probe.path().join("probe")) {
        panic!("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox: {error}");
    }

    true
}

/// Waits up to ten seconds for a state the real processes reach on their own.
///
/// The wait is bounded and re-checks the condition; it never opens a window a
/// later assertion depends on.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits for `computer list` to show one Computer online, and returns the
/// listing that showed it.
fn wait_for_computer_online(home: &Path, credential: &Path, computer: &str) -> String {
    let wanted = format!("{computer}  real  online  ");
    let mut listing = String::new();
    wait_until("the Computer to come online", || {
        listing = run(home, credential, &["computer", "list"]);
        listing.lines().any(|line| line.starts_with(&wanted))
    });

    listing
}

/// Waits for `computer list` to show one Computer offline.
fn wait_for_computer_offline(home: &Path, credential: &Path, computer: &str) {
    let wanted = format!("{computer}  real  offline  ");
    wait_until("the Computer to go offline", || {
        run(home, credential, &["computer", "list"])
            .lines()
            .any(|line| line.starts_with(&wanted))
    });
}

/// Runs one machine-side `neha computer` command for a configuration
/// directory, and returns both of its streams.
fn computer_command(computer_home: &Path, arguments: &[&str]) -> Output {
    Command::new(BIN)
        .args(arguments)
        .env("NEHA_COMPUTER_HOME", computer_home)
        .output()
        .expect("neha computer runs")
}

/// A machine-side `neha computer pair` or `neha computer run`, killed on drop.
struct ComputerDaemon {
    child: Option<Child>,
    lines: mpsc::Receiver<String>,
    output: String,
}

impl ComputerDaemon {
    /// Starts a computer command for this machine's own Neha directory.
    fn spawn(computer_home: &Path, arguments: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(arguments)
            .env("NEHA_COMPUTER_HOME", computer_home)
            .stdout(Stdio::piped())
            .spawn()
            .expect("neha computer starts");
        let lines = line_reader(child.stdout.take().unwrap());

        Self {
            child: Some(child),
            lines,
            output: String::new(),
        }
    }

    /// Reads this daemon's stdout until it prints this line.
    fn expect_line(&mut self, expected: &str) {
        let mut output = std::mem::take(&mut self.output);
        read_until_matching(&self.lines, &mut output, &format!("`{expected}`"), |line| {
            line == expected
        });
        self.output = output;
    }

    /// Reads this daemon's stdout until a line starts with `prefix`, and
    /// returns that line.
    fn expect_prefix(&mut self, prefix: &str) -> String {
        let mut output = std::mem::take(&mut self.output);
        let line = read_until_matching(
            &self.lines,
            &mut output,
            &format!("a line starting with `{prefix}`"),
            |line| line.starts_with(prefix),
        );
        self.output = output;

        line
    }

    /// Stops the daemon's process without ending it.
    ///
    /// The channel stays open, so the Space still sees the Computer online,
    /// but nothing the Space sends is ever answered until the daemon is
    /// killed: that is how a test holds a channel request open.
    fn pause(&self) {
        let Some(child) = &self.child else {
            return;
        };
        let status = Command::new("kill")
            .arg("-STOP")
            .arg(child.id().to_string())
            .status()
            .expect("kill runs");
        assert!(status.success(), "the daemon could not be stopped");
    }

    /// Stops the daemon and waits for it to go.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ComputerDaemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A Space with a paired real Computer, its running daemon, and one Bot on it.
///
/// The daemon is declared first so it is stopped before the Space it serves:
/// dropping the Space under a connected daemon would print a channel failure.
struct RealComputer {
    daemon: ComputerDaemon,
    /// The Space the Computer is paired to; dropping it stops the Space.
    _server: SpaceProcess,
    home: PathBuf,
    credential: PathBuf,
    computer_home: PathBuf,
    /// The machine's workspace, resolved as the daemon resolved it.
    workspace: PathBuf,
    url: String,
    /// What the pairing and the first commands printed.
    added: String,
    listed: String,
    created: String,
    computer: String,
    bot: String,
    chat: String,
    /// Holds the fixture's scratch directory until the processes are gone.
    directory: tempfile::TempDir,
}

impl RealComputer {
    /// Returns the fixture's scratch directory.
    fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Pairs a real Computer into a fresh Space and creates one Bot on it.
///
/// The pairing, the daemon, and the Bot creation are the paths a User takes.
/// A `directory` outside `/tmp` keeps the sandbox's fresh `/tmp` out of a
/// fence assertion.
fn real_computer(directory: tempfile::TempDir, provider: &str) -> RealComputer {
    let home = directory.path().join("home");
    let credential = directory.path().join("credential.json");
    let computer_home = directory.path().join("computer");
    // The Bot Workspace sits beside the configuration directory, never inside
    // it: a workspace inside the configuration directory is refused.
    let workspace = directory.path().join("workspace");
    let (server, url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            provider,
            "--model",
            "test-model",
        ],
    );

    // The machine shows a code, a signed-in User enters it, and the daemon the
    // machine then starts connects back to the Space.
    let mut daemon = ComputerDaemon::spawn(
        &computer_home,
        &[
            "computer",
            "pair",
            "--space",
            &url,
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    let printed = daemon.expect_prefix("Pairing code: ");
    let code = printed.trim_start_matches("Pairing code: ").trim();
    let added = run(&home, &credential, &["computer", "add", code]);
    let computer = first_word(&field(&added, "Added Computer "));
    daemon.expect_line(&format!("Connected to {url}."));
    let listed = wait_for_computer_online(&home, &credential, &computer);
    let created = run(
        &home,
        &credential,
        &["bot", "create", "Scout", "--computer", &computer],
    );
    let bot = first_word(&field(&created, "Created Bot "));
    let chat = field(&created, "Chat:");
    let workspace = std::fs::canonicalize(&workspace).expect("the daemon's workspace exists");

    RealComputer {
        daemon,
        _server: server,
        home,
        credential,
        computer_home,
        workspace,
        url,
        added,
        listed,
        created,
        computer,
        bot,
        chat,
        directory,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_works_on_its_computer_with_tools() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let (base, script, provider_task) = scripted_provider().await;
    script.call(
        "write_file",
        serde_json::json!({"path": "hello.txt", "content": "hello neha"}),
    );
    script.call("read_file", serde_json::json!({"path": "hello.txt"}));
    script.say("The file says hello neha.");
    script.call("shell", serde_json::json!({"command": "printf 42"}));
    script.say("The command printed 42.");

    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            &base,
            "--model",
            "test-model",
        ],
    );
    let created = run(&home, &credential, &["bot", "create", "Scout"]);
    let computer = field(&created, "Computer:");
    let chat = field(&created, "Chat:");

    let sent = run(&home, &credential, &["chat", "send", "make hello.txt"]);
    assert!(sent.contains("The file says hello neha."), "{sent}");
    let run_id = field(&sent, "run ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let shown = run(&home, &credential, &["run", "show", &run_id]);
    assert!(shown.contains("write_file"), "{shown}");
    assert!(shown.contains("read_file"), "{shown}");
    assert!(shown.contains("hello neha"), "{shown}");
    let workspace = home.join("computers").join(&computer).join("workspace");
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "hello neha"
    );

    let followed = cli(
        &home,
        &credential,
        &["chat", "send", "--follow", "run printf 42"],
    );
    let stdout = String::from_utf8_lossy(&followed.stdout);
    let stderr = String::from_utf8_lossy(&followed.stderr);
    assert!(followed.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("The command printed 42."), "{stdout}");
    assert!(stderr.contains("shell"), "stderr: {stderr}");
    let second_run = field(&stdout, "run ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let shown = run(&home, &credential, &["run", "show", &second_run]);
    assert!(shown.contains("shell"), "{shown}");
    assert!(shown.contains("42"), "{shown}");

    let _ = chat;
    drop(server);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_runs_a_space_and_chats_with_a_bot() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let (provider, provider_task) = fake_provider().await;

    let (server, url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");

    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    assert!(run(&home, &credential, &["whoami"]).contains("Ada"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&credential).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            &provider,
            "--model",
            "test-model",
        ],
    );
    assert!(run(&home, &credential, &["model", "list"]).contains("test-model"));

    let created = run(&home, &credential, &["bot", "create", "Scout"]);
    let bot = field(&created, "Created Bot ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let chat = field(&created, "Chat:");
    assert!(run(&home, &credential, &["bot", "list"]).contains("Scout"));
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(shown.contains("workspace"), "{shown}");

    // With one Chat, `chat send` needs no identifier.
    let sent = run(&home, &credential, &["chat", "send", "hello"]);
    assert!(sent.contains("Hello from fake"), "{sent}");

    let read = run(&home, &credential, &["chat", "read", &chat]);
    assert!(read.contains("User: hello"), "{read}");
    assert!(read.contains("Bot: Hello from fake"), "{read}");

    let run_id = field(&sent, "run ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let shown_run = run(&home, &credential, &["run", "show", &run_id]);
    assert!(shown_run.contains("succeeded"), "{shown_run}");

    drop(server);

    let (restarted, new_url, _) = serve(&home);
    std::fs::remove_file(&credential).unwrap();
    run(
        &home,
        &credential,
        &["login", "--name", "Ada", "--server", &new_url],
    );
    let read_again = run(&home, &credential, &["chat", "read", &chat]);
    assert!(read_again.contains("Hello from fake"), "{read_again}");

    // The sealed Provider key and the recorded history still work after a
    // restart.
    let sent_again = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "again"],
    );
    assert!(sent_again.contains("Hello from fake"), "{sent_again}");
    drop(restarted);

    provider_task.abort();
    assert!(!url.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_travels_between_a_user_and_a_bot() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let downloads = directory.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    let notes = directory.path().join("notes.txt");
    std::fs::write(&notes, "the quick brown fox\n").unwrap();
    let notes = notes.to_str().unwrap();
    let (base, script, provider_task) = scripted_provider().await;

    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            &base,
            "--model",
            "test-model",
        ],
    );
    let created = run(&home, &credential, &["bot", "create", "Scout"]);
    let bot = field(&created, "Created Bot ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let computer = field(&created, "Computer:");
    let chat = field(&created, "Chat:");

    // A Message needs text, a File, or both.
    let nothing = cli(&home, &credential, &["chat", "send"]);
    assert!(!nothing.status.success());
    assert!(
        String::from_utf8_lossy(&nothing.stderr).contains("--file"),
        "stderr: {}",
        String::from_utf8_lossy(&nothing.stderr)
    );

    // A File alone is a Message.
    script.say("Got the file.");
    let sent = run(&home, &credential, &["chat", "send", "--file", notes]);
    assert!(sent.contains("Got the file."), "{sent}");
    let file = field(&sent, "file ")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let read = run(&home, &credential, &["chat", "read", &chat]);
    assert!(read.contains(&format!("User:\n  file {file}")), "{read}");
    let listed = run(&home, &credential, &["chat", "files"]);
    assert!(
        listed.contains(&format!("{file}  notes.txt  20 bytes  text/plain")),
        "{listed}"
    );

    // The default destination is the current directory, under the File's name.
    let got = cli_in(
        Some(&downloads),
        &home,
        &credential,
        &["file", "get", &file],
    );
    assert!(
        got.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&got.stderr)
    );
    assert!(
        String::from_utf8_lossy(&got.stdout).contains("notes.txt"),
        "stdout: {}",
        String::from_utf8_lossy(&got.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(downloads.join("notes.txt")).unwrap(),
        "the quick brown fox\n"
    );
    // It never replaces a file the User already has.
    let again = cli_in(
        Some(&downloads),
        &home,
        &credential,
        &["file", "get", &file],
    );
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));

    // The Bot takes the File into its Workspace and delivers one back.
    script.call("take_file", serde_json::json!({ "file": file }));
    script.call(
        "write_file",
        serde_json::json!({ "path": "answer.txt", "content": "the answer" }),
    );
    script.call("put_file", serde_json::json!({ "path": "answer.txt" }));
    script.say("Done.");
    let answered = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "please answer"],
    );
    assert!(answered.contains("Done."), "{answered}");
    let workspace = home.join("computers").join(&computer).join("workspace");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "the quick brown fox\n"
    );
    let listed = run(&home, &credential, &["chat", "files", "--bot", &bot]);
    let delivered = listed
        .lines()
        .find(|line| line.contains("answer.txt"))
        .and_then(|line| line.split_whitespace().next())
        .expect("the Bot's File is listed");
    let out = downloads.join("fetched.txt");
    run(
        &home,
        &credential,
        &["file", "get", delivered, "--out", out.to_str().unwrap()],
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "the answer");

    // A File the Space cannot hold never leaves this machine.
    let big = directory.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(neha::file::MAX_FILE_BYTES + 1)
        .unwrap();
    let refused = cli(
        &home,
        &credential,
        &[
            "chat",
            "send",
            "--chat",
            &chat,
            "--file",
            big.to_str().unwrap(),
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("at most"));
    let listed = run(&home, &credential, &["chat", "files", "--chat", &chat]);
    assert_eq!(listed.lines().count(), 2, "{listed}");

    // A File the Space does not hold is refused by name.
    let missing = cli(
        &home,
        &credential,
        &["file", "get", &neha::id::Id::new().to_string()],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not_found"));

    drop(server);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_needs_a_space_name_only_when_the_destination_is_implicit() {
    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("credential.json");
    let (space, space_task) = unnamed_file_server(b"the unnamed bytes").await;
    std::fs::write(
        &credential,
        serde_json::json!({
            "url": space,
            "token": "test-token",
            "account": {
                "id": neha::id::Id::new().to_string(),
                "display_name": "Ada",
                "is_admin": true,
            },
        })
        .to_string(),
    )
    .unwrap();
    let file = neha::id::Id::new().to_string();

    // A path named with --out is the destination, so the missing name is fine.
    let out = directory.path().join("explicit.bin");
    let got = cli_in(
        Some(directory.path()),
        directory.path(),
        &credential,
        &["file", "get", &file, "--out", out.to_str().unwrap()],
    );
    assert!(
        got.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&got.stderr)
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"the unnamed bytes");
    assert!(
        String::from_utf8_lossy(&got.stdout).contains("explicit.bin"),
        "stdout: {}",
        String::from_utf8_lossy(&got.stdout)
    );

    // Without --out the name would become the destination, so the CLI asks for one.
    let got = cli_in(
        Some(directory.path()),
        directory.path(),
        &credential,
        &["file", "get", &file],
    );
    assert!(!got.status.success());
    let stderr = String::from_utf8_lossy(&got.stderr);
    assert!(stderr.contains("did not name the File"), "{stderr}");
    assert!(stderr.contains("--out"), "{stderr}");

    space_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_message_waits_while_the_first_run_holds_the_bot() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let (base, script, provider_task) = scripted_provider().await;
    // The test holds the first two runs open, so each Message sees the place
    // the Space recorded for it instead of racing a timer.
    script.hold("First reply.");
    script.hold("Second reply.");
    script.say("Third reply.");

    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            &base,
            "--model",
            "test-model",
        ],
    );
    run(&home, &credential, &["bot", "create", "Scout"]);

    let (first, first_lines, _) = spawn_cli(&home, &credential, &["chat", "send", "first"]);
    let mut first_output = String::new();
    read_until(&first_lines, &mut first_output, "running");
    let (second, second_lines, _) = spawn_cli(&home, &credential, &["chat", "send", "second"]);
    let mut second_output = String::new();
    read_until(&second_lines, &mut second_output, "queued (position 2)");
    let (third, third_lines, _) = spawn_cli(&home, &credential, &["chat", "send", "third"]);
    let mut third_output = String::new();
    read_until(&third_lines, &mut third_output, "queued (position 3)");

    // The first run answers, the second run takes the Bot and waits, and the
    // third Message moves up one place.
    script.release();
    read_until(&third_lines, &mut third_output, "waiting (position 2)");
    // The second run answers, so the third run starts.
    script.release();

    let (first_ok, first_output) = finish(first, first_lines, first_output);
    let (second_ok, second_output) = finish(second, second_lines, second_output);
    let (third_ok, third_output) = finish(third, third_lines, third_output);

    assert!(first_ok, "the first send failed:\n{first_output}");
    assert!(
        first_output.contains("queued (position 1)"),
        "{first_output}"
    );
    assert!(first_output.contains("First reply."), "{first_output}");
    assert!(second_ok, "the second send failed:\n{second_output}");
    assert!(second_output.contains("Second reply."), "{second_output}");
    assert!(third_ok, "the third send failed:\n{third_output}");
    // The running run holds place 1, so the third Message joins at place 3 and
    // moves up when the first run ends.
    assert!(
        third_output.contains("waiting (position 2)"),
        "{third_output}"
    );
    assert!(third_output.contains("Third reply."), "{third_output}");

    drop(server);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_run_reports_its_error_and_fails_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let (base, script, provider_task) = scripted_provider().await;
    script.fail();

    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    run(
        &home,
        &credential,
        &[
            "provider",
            "add",
            "--base-url",
            &base,
            "--model",
            "test-model",
        ],
    );
    run(&home, &credential, &["bot", "create", "Scout"]);

    let failed = cli(&home, &credential, &["chat", "send", "please fail"]);
    let stdout = String::from_utf8_lossy(&failed.stdout);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "stdout: {stdout}");
    assert!(stdout.contains("(failed)"), "stdout: {stdout}");
    assert!(stderr.contains("could not answer"), "stderr: {stderr}");

    drop(server);
    provider_task.abort();
}

#[test]
fn an_invitation_without_a_secret_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let output = cli(
        directory.path(),
        &directory.path().join("credential.json"),
        &["join", "http://127.0.0.1:1/invite", "--name", "Ada"],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_runs_on_a_real_computer() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // A shell in the Bot's folder writes a file there; another command reads a
    // path outside the folder, as the machine's user can.
    script.call(
        "shell",
        serde_json::json!({"command": "pwd; echo hi > a.txt"}),
    );
    script.say("The work ran in my own folder.");
    script.call("shell", serde_json::json!({"command": "cat /etc/hostname"}));
    script.say("I read the whole machine.");
    // The scratch space sits outside `/tmp`, so the sandbox's fresh `/tmp`
    // cannot mask a configuration directory its hidden-root mounts failed to
    // hide: the shell assertion below is about the hidden root itself.
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-real-")
        .tempdir_in("/var/tmp")
        .expect("the credential test needs a writable /var/tmp outside /tmp");
    let computer = real_computer(directory, &base);
    let workspace = computer.workspace.to_string_lossy().into_owned();
    let folder = computer.workspace.join(&computer.bot);

    // The code the machine printed admitted a real Computer, and its daemon
    // shows it online with its workspace.
    assert!(computer.added.contains("(real)."), "{}", computer.added);
    let listed = format!("{}  real  online  {workspace}", computer.computer);
    assert!(
        computer.listed.lines().any(|line| line == listed),
        "`computer list` must show the paired Computer online: {}",
        computer.listed
    );
    // The Bot was created on that Computer, and no managed one.
    assert_eq!(field(&computer.created, "Computer:"), computer.computer);
    let shown = run(
        &computer.home,
        &computer.credential,
        &["bot", "show", &computer.bot],
    );
    assert!(
        shown.contains(&format!(
            "Computer: {} (real, online: true)",
            computer.computer
        )),
        "{shown}"
    );

    // A relative path names the Bot's own folder: that is its working
    // directory and the file lands in its folder on the machine.
    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "where do you work?",
        ],
    );
    assert!(sent.contains("The work ran in my own folder."), "{sent}");
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.contains(folder.to_str().unwrap()),
        "the shell must run in the Bot's folder:\n{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "hi\n",
        "a relative write must land in the Bot's folder on the machine"
    );

    // Reading the whole machine is allowed: `cat /etc/hostname` prints the
    // machine's own hostname, not one the sandbox made up.
    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "cat /etc/hostname",
        ],
    );
    assert!(sent.contains("I read the whole machine."), "{sent}");
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    let hostname = std::fs::read_to_string("/etc/hostname").expect("this machine has a hostname");
    assert!(
        shown.contains(hostname.trim()),
        "a Bot must read outside its folder:\n{shown}"
    );

    // The channel credential in the Computer's configuration directory is out
    // of the Bot's reach, through the file tools and through the sandbox.
    let config_path = computer.computer_home.join("computer.json");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    let token = config["token"].as_str().expect("the config holds a token");
    assert!(!token.is_empty(), "the daemon stores its channel token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the config is private to the machine user"
        );
    }
    script.call(
        "read_file",
        serde_json::json!({"path": config_path.to_str().unwrap()}),
    );
    script.call(
        "shell",
        serde_json::json!({"command": format!("cat {}", config_path.display())}),
    );
    script.say("The configuration is out of reach.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "read the config"],
    );
    assert!(
        sent.contains("The configuration is out of reach."),
        "{sent}"
    );
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.contains("the path is hidden from this Computer"),
        "the file tool must refuse the configuration directory:\n{shown}"
    );
    assert!(
        shown.contains("read_file (failed)"),
        "the file tool read must fail:\n{shown}"
    );
    assert!(
        shown.contains("shell (failed)"),
        "the sandbox must not show the configuration directory:\n{shown}"
    );
    assert!(
        !shown.contains(token),
        "the channel credential must never reach the model:\n{shown}"
    );
    // The host file is still there, private to the machine user.
    assert!(config_path.is_file(), "the config stays on the host");

    drop(computer);
    provider_task.abort();
}

#[test]
fn a_workspace_inside_a_computers_configuration_directory_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let computer_home = directory.path().join("computer");
    let inside = computer_home.join("workspace");
    let output = computer_command(
        &computer_home,
        &[
            "computer",
            "pair",
            "--space",
            "http://127.0.0.1:1",
            "--workspace",
            inside.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "pairing with a workspace in the config directory must fail:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("Pairing code:"),
        "pairing must fail before it shows a code:\n{stdout}"
    );
    assert!(
        stderr.contains("is inside the Computer's configuration directory"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_saved_workspace_inside_the_configuration_directory_is_refused_at_start() {
    let directory = tempfile::tempdir().unwrap();
    let computer_home = directory.path().join("computer");
    std::fs::create_dir_all(&computer_home).unwrap();
    // The default workspace of an earlier version lived here; a saved pairing
    // that names it must refuse to serve today.
    std::fs::write(
        computer_home.join("computer.json"),
        serde_json::json!({
            "space_url": "http://127.0.0.1:1",
            "computer": "01890a5d-ac96-774b-bcce-b302099a8057",
            "token": "machine-token",
            "workspace": computer_home.join("workspace"),
        })
        .to_string(),
    )
    .unwrap();
    let output = computer_command(&computer_home, &["computer", "run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "serving a saved config must fail:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("Connected to"),
        "the daemon must not connect:\n{stdout}"
    );
    assert!(
        stderr.contains("is inside the Computer's configuration directory"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_on_a_real_computer_writes_only_its_own_folder() {
    if !real_computer_ready() {
        return;
    }
    // The workspace sits outside `/tmp`: the sandbox gets a fresh `/tmp`, so
    // a relative escape has to reach this real folder for the OS to refuse it.
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-fence-")
        .tempdir_in("/var/tmp")
        .expect("the fence test needs a writable /var/tmp outside /tmp");
    let (base, script, provider_task) = scripted_provider().await;
    script.call(
        "shell",
        serde_json::json!({"command": "echo bad > ../escaped.txt"}),
    );
    script.call(
        "write_file",
        serde_json::json!({"path": "../escaped.txt", "content": "bad"}),
    );
    script.call(
        "write_file",
        serde_json::json!({"path": "inside.txt", "content": "inside"}),
    );
    script.say("Done trying.");
    let computer = real_computer(directory, &base);
    let folder = computer.workspace.join(&computer.bot);
    let escaped = computer.workspace.join("escaped.txt");

    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "try to write outside",
        ],
    );
    assert!(sent.contains("Done trying."), "{sent}");
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.contains("shell (failed)"),
        "a shell write outside the Bot's folder must fail:\n{shown}"
    );
    assert!(
        shown.contains("write_file (failed)"),
        "a file write outside the Bot's folder must fail:\n{shown}"
    );
    assert!(
        shown.contains("write_file (succeeded)"),
        "a file write inside the Bot's folder must succeed:\n{shown}"
    );
    assert!(
        !escaped.exists(),
        "a refused write must not reach the machine: {}",
        escaped.display()
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("inside.txt")).unwrap(),
        "inside",
        "the write inside the Bot's folder must reach the machine"
    );

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_travels_over_a_real_computers_channel() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let content = "the quick brown fox\n";
    let directory = tempfile::tempdir().unwrap();
    let computer = real_computer(directory, &base);
    let folder = computer.workspace.join(&computer.bot);
    let notes = computer.path().join("notes.txt");
    std::fs::write(&notes, content).unwrap();

    // A User uploads a File in the Chat.
    script.say("Got the note.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "--file",
            notes.to_str().unwrap(),
        ],
    );
    assert!(sent.contains("Got the note."), "{sent}");
    let listed = run(
        &computer.home,
        &computer.credential,
        &["chat", "files", "--bot", &computer.bot],
    );
    let file = listed
        .lines()
        .find(|line| line.contains("notes.txt"))
        .and_then(|line| line.split_whitespace().next())
        .expect("the uploaded File is listed")
        .to_owned();

    // The Bot takes the File into its folder and delivers it back: the bytes
    // cross the channel twice and must come back unchanged.
    script.call("take_file", serde_json::json!({"file": &file}));
    script.call(
        "put_file",
        serde_json::json!({"path": "notes.txt", "name": "roundtrip.txt"}),
    );
    script.say("Delivered it back.");
    let answered = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "please send it back",
        ],
    );
    assert!(answered.contains("Delivered it back."), "{answered}");
    let run_id = first_word(&field(&answered, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(shown.contains("take_file (succeeded)"), "{shown}");
    assert!(shown.contains("put_file (succeeded)"), "{shown}");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes.txt")).unwrap(),
        content,
        "taking the File must write it into the Bot's folder"
    );

    let listed = run(
        &computer.home,
        &computer.credential,
        &["chat", "files", "--bot", &computer.bot],
    );
    let delivered = listed
        .lines()
        .find(|line| line.contains("roundtrip.txt"))
        .and_then(|line| line.split_whitespace().next())
        .expect("the delivered File is listed")
        .to_owned();
    let out = computer.path().join("roundtrip.txt");
    run(
        &computer.home,
        &computer.credential,
        &["file", "get", &delivered, "--out", out.to_str().unwrap()],
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        content.as_bytes(),
        "the File's bytes must round-trip through the Bot's Computer"
    );

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_waits_while_a_real_computer_is_offline() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = real_computer(directory, &base);

    // The daemon stops, and the Space shows the Computer offline.
    computer.daemon.kill();
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);

    // A Message to a Bot on the offline Computer stays queued, and the command
    // keeps waiting instead of failing.
    script.say("Yes, I am back.");
    let (mut send, lines, _) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "are you there?"],
    );
    let mut output = String::new();
    read_until(&lines, &mut output, "queued (position 1)");
    assert!(
        send.try_wait().unwrap().is_none(),
        "the send must keep waiting on the queued run:\n{output}"
    );

    // Starting the daemon again reconnects the Computer, and the run goes.
    let mut restarted = ComputerDaemon::spawn(&computer.computer_home, &["computer", "run"]);
    restarted.expect_line(&format!("Connected to {}.", computer.url));
    let (ok, output) = finish(send, lines, output);
    assert!(
        ok,
        "the queued send must finish once the Computer is back:\n{output}"
    );
    assert!(output.contains("Yes, I am back."), "{output}");
    wait_for_computer_online(&computer.home, &computer.credential, &computer.computer);

    drop(restarted);
    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_fails_when_its_computer_leaves() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // The first reply asks for a slow shell; the second reply is only wanted
    // if the run wrongly went on after the Computer left.
    script.call("shell", serde_json::json!({"command": "sleep 2"}));
    script.say("The slow command finished.");
    let directory = tempfile::tempdir().unwrap();
    let mut computer = real_computer(directory, &base);

    let (send, stdout, stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--follow",
            "--chat",
            &computer.chat,
            "run the slow command",
        ],
    );
    let output = String::new();
    let mut error = String::new();
    // `--follow` prints an action as running while its result is still owed:
    // that is the moment the shell is in flight on the daemon.
    read_until_matching(&stderr, &mut error, "the shell action to start", |line| {
        line.contains("action shell (step 1): running")
    });

    // The machine leaves while its shell is still running.
    computer.daemon.kill();
    read_until_matching(
        &stderr,
        &mut error,
        "the run to report the lost Computer",
        |line| line.contains("I lost my Computer before the action returned."),
    );
    let (ok, output) = finish(send, stdout, output);
    assert!(
        !ok,
        "the run must fail when its Computer leaves:\nstdout: {output}\nstderr: {error}"
    );
    let run_id = first_word(&field(&output, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown
            .lines()
            .next()
            .unwrap_or_default()
            .ends_with(": failed"),
        "a run whose action result never returned must fail:\n{shown}"
    );
    assert!(
        shown.contains("shell (unknown)"),
        "the action whose result never returned must be unknown:\n{shown}"
    );
    assert!(
        !shown.contains("step 2"),
        "the Model must not be asked again after the Computer left:\n{shown}"
    );

    // The Chat says what happened, and the Model's unused second reply never
    // reaches the User.
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    let last = read.lines().last().expect("the Chat has a last Message");
    let (_, text) = last
        .split_once("Bot: ")
        .expect("the last Message is the Bot's");
    assert_eq!(
        text, "I lost my Computer before the action returned.",
        "{read}"
    );
    assert!(
        !read.contains("The slow command finished."),
        "the Model's next reply must never be used:\n{read}"
    );
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offline_real_computer_does_not_block_another_bot() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    script.say("The managed Bot answered.");
    let directory = tempfile::tempdir().unwrap();
    let mut computer = real_computer(directory, &base);

    // A second Bot works on a managed Computer inside the Space.
    let created = run(
        &computer.home,
        &computer.credential,
        &["bot", "create", "Helper"],
    );
    let helper_chat = field(&created, "Chat:");

    // The real Computer leaves, so its Bot's Message waits in the record.
    computer.daemon.kill();
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);
    let (mut waiting, stdout, _stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "are you there?"],
    );
    let mut output = String::new();
    read_until(&stdout, &mut output, "queued (position 1)");

    // The unrelated managed Bot still answers while that run waits.
    let answered = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &helper_chat, "are you free?"],
    );
    assert!(answered.contains("The managed Bot answered."), "{answered}");
    assert!(
        waiting.try_wait().unwrap().is_none(),
        "the queued send must still wait behind the offline Computer:\n{output}"
    );

    let _ = waiting.kill();
    let _ = waiting.wait();
    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_computers_messages_run_in_the_order_they_were_sent() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // Each run holds its reply open, so the next Messages queue behind it and
    // every run takes the reply holding its own place in the queue.
    script.hold("First reply.");
    script.hold("Second reply.");
    script.hold("Third reply.");
    let directory = tempfile::tempdir().unwrap();
    let computer = real_computer(directory, &base);

    let (first, first_lines, _stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "first"],
    );
    let mut first_output = String::new();
    read_until(&first_lines, &mut first_output, "running");
    let (second, second_lines, _stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "second"],
    );
    let mut second_output = String::new();
    read_until(&second_lines, &mut second_output, "queued (position 2)");
    let (third, third_lines, _stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "third"],
    );
    let mut third_output = String::new();
    read_until(&third_lines, &mut third_output, "queued (position 3)");

    // The first run answers, the second starts and waits, and the third moves
    // up one place behind it.
    script.release();
    read_until(&third_lines, &mut third_output, "waiting (position 2)");
    script.release();
    script.release();

    let (first_ok, first_output) = finish(first, first_lines, first_output);
    let (second_ok, second_output) = finish(second, second_lines, second_output);
    let (third_ok, third_output) = finish(third, third_lines, third_output);
    assert!(first_ok, "the first send failed:\n{first_output}");
    assert!(second_ok, "the second send failed:\n{second_output}");
    assert!(third_ok, "the third send failed:\n{third_output}");

    // Runs started in the order the Messages were sent: each got the reply
    // holding its place, and the Chat records them in that order.
    assert!(first_output.contains("First reply."), "{first_output}");
    assert!(second_output.contains("Second reply."), "{second_output}");
    assert!(third_output.contains("Third reply."), "{third_output}");
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    let replies: Vec<&str> = read
        .lines()
        .filter(|line| line.contains("] Bot: "))
        .collect();
    assert_eq!(replies.len(), 3, "{read}");
    assert!(replies[0].ends_with("Bot: First reply."), "{read}");
    assert!(replies[1].ends_with("Bot: Second reply."), "{read}");
    assert!(replies[2].ends_with("Bot: Third reply."), "{read}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_take_lost_with_its_computer_fails_the_run() {
    if !real_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let content = "the quick brown fox\n";
    let directory = tempfile::tempdir().unwrap();
    let mut computer = real_computer(directory, &base);
    let notes = computer.path().join("notes.txt");
    std::fs::write(&notes, content).unwrap();

    // A first Message uploads the File the Bot will take.
    script.say("Got the note.");
    run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "--file",
            notes.to_str().unwrap(),
        ],
    );
    let listed = run(
        &computer.home,
        &computer.credential,
        &["chat", "files", "--bot", &computer.bot],
    );
    let file = listed
        .lines()
        .find(|line| line.contains("notes.txt"))
        .and_then(|line| line.split_whitespace().next())
        .expect("the uploaded File is listed")
        .to_owned();

    // The daemon stops answering the channel before the Bot takes the File:
    // its process is only stopped, so the Computer still shows online and the
    // take starts, but the Write frame it sends is never answered.
    script.call("take_file", serde_json::json!({"file": &file}));
    script.say("The take finished.");
    computer.daemon.pause();
    let (send, stdout, stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--follow",
            "--chat",
            &computer.chat,
            "take the file",
        ],
    );
    let mut error = String::new();
    read_until_matching(&stderr, &mut error, "the take to start", |line| {
        line.contains("action take_file (step 1): running")
    });

    // The machine leaves while the take still owes the Space an answer.
    computer.daemon.kill();
    read_until_matching(
        &stderr,
        &mut error,
        "the run to report the lost Computer",
        |line| line.contains("I lost my Computer before the action returned."),
    );
    let output = String::new();
    let (ok, output) = finish(send, stdout, output);
    assert!(
        !ok,
        "a File take that never returned must fail the run:\nstdout: {output}\nstderr: {error}"
    );
    let run_id = first_word(&field(&output, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown
            .lines()
            .next()
            .unwrap_or_default()
            .ends_with(": failed"),
        "a lost File transfer must fail the run:\n{shown}"
    );
    assert!(
        shown.contains("take_file (unknown)"),
        "a File take whose answer never returned must be unknown:\n{shown}"
    );
    assert!(
        !shown.contains("step 2"),
        "the Model must not be asked again after losing the Computer:\n{shown}"
    );

    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    let last = read.lines().last().expect("the Chat has a last Message");
    let (_, text) = last
        .split_once("Bot: ")
        .expect("the last Message is the Bot's");
    assert_eq!(
        text, "I lost my Computer before the action returned.",
        "{read}"
    );
    assert!(
        !read.contains("The take finished."),
        "the Model's next reply must never be used:\n{read}"
    );
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);

    drop(computer);
    provider_task.abort();
}
