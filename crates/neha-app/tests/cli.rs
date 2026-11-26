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

/// A running `neha serve`, kept alive until it drops.
struct SpaceProcess {
    child: Child,
    lines: mpsc::Receiver<String>,
    output: String,
}

impl SpaceProcess {
    /// Reads this Space's stdout until it prints this line.
    fn expect_line(&mut self, expected: &str) {
        let mut output = std::mem::take(&mut self.output);
        read_until_matching(&self.lines, &mut output, &format!("`{expected}`"), |line| {
            line == expected
        });
        self.output = output;
    }
}

impl Drop for SpaceProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `neha serve` and waits for its address, and its invitation on a
/// fresh Space.
fn serve(home: &Path) -> (SpaceProcess, String, Option<String>) {
    serve_on(home, 0)
}

/// Starts `neha serve` on one loopback port, so a test can restart a Space at
/// the address its Computers already saved; port 0 picks a free one.
///
/// A Space that printed its address and then died would fail a test's next
/// command with a bare "Connection refused", so its health endpoint is probed
/// before the fixture is handed back. A start that fails is retried; when
/// every attempt fails, the panic carries the Space's own stderr.
fn serve_on(home: &Path, port: u16) -> (SpaceProcess, String, Option<String>) {
    let mut failures = String::new();
    for attempt in 1..=3 {
        match serve_once(home, port) {
            Ok(space) => return space,
            Err(error) => {
                eprintln!("neha serve failed on start {attempt}:\n{error}");
                failures.push_str(&format!("start {attempt}:\n{error}\n"));
            }
        }
    }

    panic!("neha serve did not start after 3 attempts:\n{failures}");
}

/// Returns the loopback port a Space URL names.
fn space_port(url: &str) -> u16 {
    url.rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("the Space URL has no port: {url}"))
}

/// Starts one `neha serve` and waits until it answers on its own address.
fn serve_once(home: &Path, port: u16) -> Result<(SpaceProcess, String, Option<String>), String> {
    let mut child = Command::new(BIN)
        .args(["serve", "--port", &port.to_string()])
        .env("NEHA_HOME", home)
        // The Space writes its one-use pairing ticket into the per-user
        // configuration directory; keep that inside the fixture.
        .env("XDG_CONFIG_HOME", home.join("xdg"))
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
    let mut output = String::new();
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
        output.push_str(&line);
        output.push('\n');
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
                Ok(next) => {
                    output.push_str(&next);
                    output.push('\n');
                    break;
                }
                Err(_) => break,
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

    Ok((
        SpaceProcess {
            child,
            lines: received,
            output,
        },
        url,
        invite,
    ))
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

/// Returns the File id under the Bot Message whose text is `text`.
fn bot_file_id(read: &str, text: &str) -> String {
    let lines: Vec<&str> = read.lines().collect();
    let position = lines
        .iter()
        .position(|line| line.contains("Bot:") && line.contains(text))
        .unwrap_or_else(|| panic!("no Bot Message says {text:?}:\n{read}"));
    lines[position + 1]
        .trim()
        .strip_prefix("file ")
        .unwrap_or_else(|| panic!("the Message after {text:?} names no File:\n{read}"))
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
    /// One assistant message with several tool calls, in order.
    Tools(Vec<(String, serde_json::Value)>),
    Fail,
}

/// The SSE body that answers a request with one text.
fn text_body(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        serde_json::to_string(text).unwrap()
    )
}

/// The SSE body that answers a request with several tool calls in one step.
fn tool_body(index: usize, calls: &[(String, serde_json::Value)]) -> String {
    let mut body = String::new();
    for (position, (name, arguments)) in calls.iter().enumerate() {
        let chunk = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
            "index": position,
            "id": format!("call-{index}-{position}"),
            "type": "function",
            "function": {
                "name": name,
                "arguments": serde_json::to_string(arguments).unwrap(),
            }
        }]}}]});
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
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
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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

    /// Answers the next request with several tool calls in one step.
    fn calls(&self, calls: &[(&str, serde_json::Value)]) {
        self.push(ScriptedReply::Tools(
            calls
                .iter()
                .map(|(name, arguments)| ((*name).to_owned(), arguments.clone()))
                .collect(),
        ));
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

    /// How many requests the fake Provider has received.
    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn scripted_provider() -> (String, Script, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (replies, queued) = tokio::sync::mpsc::unbounded_channel::<ScriptedReply>();
    let queued = Arc::new(tokio::sync::Mutex::new(queued));
    let requests = Arc::new(AtomicUsize::new(0));
    let script_requests = Arc::clone(&requests);
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
                        tool_body(index, &[(name, arguments)])
                    }
                    ScriptedReply::Tools(calls) => tool_body(index, &calls),
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
            requests: script_requests,
        },
        task,
    )
}

/// Runs a Local-Computer test only where this machine can fence a Bot.
///
/// `NEHA_TEST_BWRAP` is the gate, as for the runtime's own fence tests. Once
/// it is set, a sandbox that cannot start makes the test fail with
/// bubblewrap's own error: CI promises the real path, so a broken sandbox
/// must be red instead of silently skipped.
fn local_computer_ready() -> bool {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the Local-Computer test: NEHA_TEST_BWRAP is not set");
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

/// Waits for `computer list` to show one Local Computer online, and returns
/// the listing that showed it.
fn wait_for_computer_online(home: &Path, credential: &Path, computer: &str) -> String {
    let wanted = format!("{computer}  local  online  ");
    let mut listing = String::new();
    wait_until("the Computer to come online", || {
        listing = run(home, credential, &["computer", "list"]);
        listing.lines().any(|line| line.starts_with(&wanted))
    });

    listing
}

/// Waits for `computer list` to show one Local Computer offline.
fn wait_for_computer_offline(home: &Path, credential: &Path, computer: &str) {
    let wanted = format!("{computer}  local  offline  ");
    wait_until("the Computer to go offline", || {
        run(home, credential, &["computer", "list"])
            .lines()
            .any(|line| line.starts_with(&wanted))
    });
}

/// The machine-side environment one `neha computer` command resolves its
/// defaults from: the Space home (the pairing ticket lives there), the machine
/// user's home (the default Workspace root lives there), and the configuration
/// directory.
struct MachineEnv {
    space_home: PathBuf,
    user_home: PathBuf,
    computer_home: PathBuf,
}

impl MachineEnv {
    /// A machine whose directories all live under `machine`.
    fn under(space_home: &Path, machine: &Path) -> Self {
        Self {
            space_home: space_home.to_path_buf(),
            user_home: machine.join("user"),
            computer_home: machine.join("config"),
        }
    }

    /// The default Workspace root this machine resolves to.
    fn workspace_root(&self) -> PathBuf {
        self.user_home.join(".neha").join("workspaces")
    }

    /// The pairing this machine saved.
    fn config_path(&self) -> PathBuf {
        self.computer_home.join("computer.json")
    }

    /// Adds this machine's directories to a command.
    fn apply(&self, command: &mut Command) {
        command
            .env("NEHA_HOME", &self.space_home)
            .env("HOME", &self.user_home)
            .env("NEHA_COMPUTER_HOME", &self.computer_home)
            // The Space and this machine are the same machine, so they share
            // one configuration tree; pairing finds the Space's ticket there.
            .env("XDG_CONFIG_HOME", self.space_home.join("xdg"));
    }
}

/// Runs one machine-side `neha computer` command for a machine, and returns
/// both of its streams.
fn computer_command(machine: &MachineEnv, credential: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new(BIN);
    command.args(arguments).env("NEHA_CREDENTIAL", credential);
    machine.apply(&mut command);
    command.output().expect("neha computer runs")
}

/// Returns every pairing ticket a Space wrote under its configuration tree.
///
/// The fixture gives `neha serve` an `XDG_CONFIG_HOME` under the Space's home,
/// and the Space writes `$XDG_CONFIG_HOME/neha/spaces/<space-key>/local-computer.ticket`.
fn space_tickets(space_home: &Path) -> Vec<PathBuf> {
    let spaces = space_home.join("xdg").join("neha").join("spaces");
    let Ok(entries) = std::fs::read_dir(spaces) else {
        return Vec::new();
    };
    let mut tickets: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("local-computer.ticket"))
        .filter(|path| path.is_file())
        .collect();
    tickets.sort();
    tickets
}

/// Returns the one pairing ticket waiting for this Space.
fn ticket_path(space_home: &Path) -> PathBuf {
    let tickets = space_tickets(space_home);
    assert_eq!(
        tickets.len(),
        1,
        "the Space must write exactly one pairing ticket: {tickets:?}"
    );
    tickets.into_iter().next().expect("the ticket was counted")
}

/// A machine-side `neha computer pair` or `neha computer run`, killed on drop.
struct ComputerDaemon {
    child: Option<Child>,
    lines: mpsc::Receiver<String>,
    output: String,
}

impl ComputerDaemon {
    /// Starts a machine-side command for one machine.
    fn spawn(machine: &MachineEnv, credential: &Path, arguments: &[&str]) -> Self {
        let mut command = Command::new(BIN);
        command
            .args(arguments)
            .env("NEHA_CREDENTIAL", credential)
            .stdout(Stdio::piped());
        machine.apply(&mut command);
        let mut child = command.spawn().expect("neha computer starts");
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

/// Starts a Space with an Admin and, when asked, a Provider, then restarts it
/// on the same port.
///
/// Only a Space that already has an Admin writes the zero-code pairing ticket,
/// so the restart is what a User does after signing in; keeping the address is
/// what lets the saved CLI credential keep working.
fn space_with_admin(directory: &Path, provider: Option<&str>) -> (SpaceProcess, String) {
    let home = directory.join("home");
    let credential = directory.join("session").join("credential.json");
    let (server, url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    if let Some(provider) = provider {
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
    }

    let port = space_port(&url);
    drop(server);
    let (mut server, restarted_url, _invite) = serve_on(&home, port);
    assert_eq!(
        restarted_url, url,
        "the Space restarts on the address its Computers save"
    );
    let tickets = space_tickets(&home);
    assert_eq!(
        tickets.len(),
        1,
        "a Space with an Admin writes one zero-code pairing ticket: {tickets:?}"
    );
    server.expect_line("Pair this PC: neha computer pair");

    (server, url)
}

/// One Local Computer paired into a Space, with its daemon running.
struct PairedComputer {
    daemon: ComputerDaemon,
    machine: MachineEnv,
    computer: String,
    /// The default Workspace root this machine resolves to.
    workspace: PathBuf,
    /// What `computer pair` printed.
    paired: String,
    /// The `computer list` line that showed it online.
    listed: String,
}

/// Pairs one more machine into a running Space and starts its daemon.
///
/// The first pairing consumes the ticket file `neha serve` wrote; a later
/// machine asks the Space for its own ticket through the signed-in CLI.
fn pair_computer(home: &Path, credential: &Path, url: &str, machine: &Path) -> PairedComputer {
    pair_computer_to(home, credential, url, machine, None)
}

/// Like [`pair_computer`], with an explicit Workspace root when one is given.
///
/// `None` leaves the root at the machine's default (`$HOME/.neha/workspaces`);
/// `Some` proves `--workspace` overrides it.
fn pair_computer_to(
    home: &Path,
    credential: &Path,
    url: &str,
    machine: &Path,
    workspace: Option<&Path>,
) -> PairedComputer {
    let machine = MachineEnv::under(home, machine);
    // The CLI scans the configuration tree for the ticket the Space wrote, so
    // the first machine spends it; a later machine finds none and asks the
    // Space for one through its signed-in CLI.
    let mut arguments = vec!["computer", "pair", "--yes", "--space", url];
    if let Some(workspace) = workspace {
        arguments.push("--workspace");
        arguments.push(workspace.to_str().unwrap());
    }
    let output = computer_command(&machine, credential, &arguments);
    assert!(
        output.status.success(),
        "`neha computer pair --yes` failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let paired = String::from_utf8_lossy(&output.stdout).into_owned();
    let computer = first_word(&field(&paired, "Paired Computer "));
    let workspace = std::fs::canonicalize(workspace.unwrap_or(&machine.workspace_root()))
        .expect("pairing created the Workspace root");

    // Pairing only saves the configuration; serving the Space is its own
    // command, exactly as a User starts it.
    let mut daemon = ComputerDaemon::spawn(&machine, credential, &["computer", "run"]);
    daemon.expect_line(&format!("Computer {computer} is running for {url}."));
    daemon.expect_line(&format!("Workspace root: {}", workspace.display()));
    let listed = wait_for_computer_online(home, credential, &computer);

    PairedComputer {
        daemon,
        machine,
        computer,
        workspace,
        paired,
        listed,
    }
}

/// A Space with a paired Local Computer, its running daemon, and one Bot on it.
///
/// The daemon is declared first so it is stopped before the Space it serves:
/// dropping the Space under a connected daemon would print a channel failure.
struct LocalComputer {
    daemon: ComputerDaemon,
    /// The Space the Computer is paired to; dropping it stops the Space.
    server: Option<SpaceProcess>,
    home: PathBuf,
    credential: PathBuf,
    machine: MachineEnv,
    /// The machine's Workspace root, resolved as the daemon resolved it.
    workspace: PathBuf,
    url: String,
    /// What pairing and the first commands printed.
    paired: String,
    listed: String,
    created: String,
    computer: String,
    bot: String,
    chat: String,
    /// Holds the fixture's scratch directory until the processes are gone.
    directory: tempfile::TempDir,
}

impl LocalComputer {
    /// Returns the fixture's scratch directory.
    fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Stops the Space and starts it again on the same address, without
    /// waiting for the Computer to reconnect.
    fn restart_space_only(&mut self) {
        let port = space_port(&self.url);
        drop(self.server.take());
        let (server, url, _invite) = serve_on(&self.home, port);
        assert_eq!(url, self.url, "the Space restarts on its own address");
        self.server = Some(server);
    }

    /// Stops the Space and starts it again on the same address.
    ///
    /// The daemon reconnects by itself, as it does for a User restarting
    /// `neha serve`; the saved CLI session keeps working because the address
    /// does not change.
    fn restart_space(&mut self) {
        let port = space_port(&self.url);
        drop(self.server.take());
        let (server, url, _invite) = serve_on(&self.home, port);
        assert_eq!(url, self.url, "the Space restarts on its own address");
        self.server = Some(server);
        wait_for_computer_online(&self.home, &self.credential, &self.computer);
    }
}

/// Quotes one argument for the shell line `script` runs.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// One interactive `neha` command on a pseudo-terminal.
///
/// The approval card is only printed when stdin is a terminal; `script(1)`
/// gives the child a pty and forwards a test's answer to it, so a test reads
/// the real card and answers it exactly as a User does.
struct TtyCli {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    errors: mpsc::Receiver<String>,
    output: String,
}

/// Returns whether this machine can give a command a pseudo-terminal.
fn tty_available() -> bool {
    Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

impl TtyCli {
    /// Starts one CLI command on a pseudo-terminal.
    fn spawn(home: &Path, credential: &Path, arguments: &[&str]) -> Self {
        let line = std::iter::once(BIN.to_owned())
            .chain(arguments.iter().map(|argument| shell_quote(argument)))
            .collect::<Vec<_>>()
            .join(" ");
        let mut child = Command::new("script")
            .args(["-qec", &line, "/dev/null"])
            .env("NEHA_HOME", home)
            .env("NEHA_CREDENTIAL", credential)
            .env("NEHA_PROVIDER_KEY", "sk-test")
            .env("NEHA_PASSWORD", PASSWORD)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("script runs");
        let lines = line_reader(child.stdout.take().unwrap());
        let errors = line_reader(child.stderr.take().unwrap());
        let stdin = child.stdin.take();

        Self {
            child: Some(child),
            stdin,
            lines,
            errors,
            output: String::new(),
        }
    }

    /// Reads the terminal until a line contains `needle`, and returns that line.
    fn expect_containing(&mut self, needle: &str) -> String {
        let mut output = std::mem::take(&mut self.output);
        let line = read_until_matching(
            &self.lines,
            &mut output,
            &format!("a line containing `{needle}`"),
            |line| line.contains(needle),
        );
        self.output = output;

        line
    }

    /// Answers the card with one line of input.
    fn answer(&mut self, answer: &str) {
        use std::io::Write as _;
        let stdin = self.stdin.as_mut().expect("the terminal is open");
        writeln!(stdin, "{answer}").expect("the terminal takes the answer");
        stdin.flush().expect("the terminal takes the answer");
    }

    /// Waits for the command to end, and returns its success and all output.
    fn finish(mut self) -> (bool, String) {
        // Closing the input does not end the command; the child exits on its
        // own after its final line, and `script` follows it.
        drop(self.stdin.take());
        let mut output = std::mem::take(&mut self.output);
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(10)) {
            output.push_str(&line);
            output.push('\n');
        }
        let status = self.child.take().expect("the terminal runs").wait();
        while let Ok(line) = self.errors.recv_timeout(Duration::from_millis(50)) {
            output.push_str(&line);
            output.push('\n');
        }

        (status.is_ok_and(|status| status.success()), output)
    }
}

impl Drop for TtyCli {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Waits until `run show` reports one run in `status`, and returns that view.
fn wait_for_run_status(home: &Path, credential: &Path, run_id: &str, status: &str) -> String {
    let mut shown = String::new();
    wait_until("the run to change status", || {
        shown = run(home, credential, &["run", "show", run_id]);
        shown
            .lines()
            .any(|line| line == format!("Run {run_id}: {status}"))
    });

    shown
}

/// Returns the run and approval ids a non-TTY `chat send` left waiting.
fn waiting_run(sent: &str) -> (String, String) {
    let advice = sent
        .lines()
        .find(|line| line.starts_with("run ") && line.contains("is waiting for approval"))
        .unwrap_or_else(|| panic!("`chat send` must report the waiting run:\n{sent}"));
    let run_id = advice
        .split_whitespace()
        .nth(1)
        .expect("the waiting line names the run")
        .to_owned();
    let approval = advice
        .split_once("is waiting for approval ")
        .expect("the waiting line names the approval")
        .1
        .split(|character: char| character == ';' || character.is_whitespace())
        .next()
        .expect("the waiting line names the approval")
        .to_owned();

    (run_id, approval)
}

/// Answers the first-use approval a Bot on a Local Computer asks for.
///
/// The first Computer action of a Bot on a Local Computer always asks, whatever
/// rules exist, so the fixture spends it on a harmless command: tests that use
/// [`local_computer`] then act as a User who has already approved the first use.
fn approve_first_use(home: &Path, credential: &Path, chat: &str, script: &Script) {
    script.call("shell", serde_json::json!({"command": "printf first-use"}));
    script.say("The first use is approved.");
    let sent = run(
        home,
        credential,
        &["chat", "send", "--chat", chat, "first use"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let allowed = run(home, credential, &["approval", "allow", &approval]);
    assert!(
        allowed.contains(&format!("Allowed {approval} for run {run_id}.")),
        "{allowed}"
    );
    wait_for_run_status(home, credential, &run_id, "succeeded");
}

/// Pairs a Local Computer into a fresh Space and creates one Bot on it, then
/// approves the Bot's first use of the Computer.
fn local_computer(directory: tempfile::TempDir, provider: &str, script: &Script) -> LocalComputer {
    let computer = bare_local_computer(directory, provider);
    approve_first_use(&computer.home, &computer.credential, &computer.chat, script);

    computer
}

/// Pairs a Local Computer into a fresh Space and creates one Bot on it.
///
/// The pairing and the Bot creation take the paths a User takes, and the Bot
/// is created without naming a Computer so the default (Local) is itself under
/// test. A `directory` outside `/tmp` keeps the sandbox's fresh `/tmp` out of
/// a fence assertion.
fn bare_local_computer(directory: tempfile::TempDir, provider: &str) -> LocalComputer {
    let home = directory.path().join("home");
    let credential = directory.path().join("session").join("credential.json");
    let (server, url) = space_with_admin(directory.path(), Some(provider));
    let paired = pair_computer(&home, &credential, &url, &directory.path().join("machine"));
    let created = run(&home, &credential, &["bot", "create", "Scout"]);
    let bot = first_word(&field(&created, "Created Bot "));
    let chat = field(&created, "Chat:");
    assert_eq!(
        field(&created, "Computer:"),
        paired.computer,
        "a Bot created without --computer lands on the paired Local Computer"
    );
    assert!(
        paired
            .listed
            .lines()
            .any(|line| line.starts_with(&format!("{}  local  ", paired.computer))),
        "`computer list` shows the paired Local Computer: {}",
        paired.listed
    );

    LocalComputer {
        daemon: paired.daemon,
        server: Some(server),
        home,
        credential,
        machine: paired.machine,
        workspace: paired.workspace,
        url,
        paired: paired.paired,
        listed: paired.listed,
        created,
        computer: paired.computer,
        bot,
        chat,
        directory,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_works_on_its_computer_with_tools() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("session").join("credential.json");
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
    let credential: PathBuf = directory.path().join("session").join("credential.json");
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
    let credential: PathBuf = directory.path().join("session").join("credential.json");
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

    // The Bot uses its CLI instead of File tools: it gets the File into its
    // Workspace, then sends one back as a Bot Message.
    // Run from another directory: without --out, the CLI still writes into
    // the Bot's Workspace (its HOME), not the shell's current directory.
    let get = format!("cd /tmp && neha file get {file}");
    script.call("shell", serde_json::json!({"command": get}));
    script.call(
        "shell",
        serde_json::json!({"command": "printf 'hosted result' > result.txt"}),
    );
    script.call(
        "shell",
        serde_json::json!({"command": "neha chat send --file result.txt 'Here is the result.'"}),
    );
    script.say("Done.");
    let answered = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "please answer"],
    );
    assert!(answered.contains("Done."), "{answered}");
    let workspace = home.join("computers").join(&computer).join("workspace");
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
        "hosted result"
    );
    let read = run(&home, &credential, &["chat", "read", &chat]);
    let delivered = bot_file_id(&read, "Here is the result.");
    assert_ne!(delivered, file, "the Bot delivered a new File");
    let out = downloads.join("fetched.txt");
    run(
        &home,
        &credential,
        &["file", "get", &delivered, "--out", out.to_str().unwrap()],
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "hosted result");

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
    assert_eq!(
        listed.lines().count(),
        2,
        "the user's File and the Bot's result: {listed}"
    );

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
async fn a_file_travels_through_a_local_computers_cli() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-file-")
        .tempdir_in("/var/tmp")
        .expect("the File test needs a writable /var/tmp");
    let computer = local_computer(directory, &base, &script);
    let folder = computer.workspace.join(&computer.bot);
    let notes = computer.path().join("notes.txt");
    std::fs::write(&notes, "over the channel\n").unwrap();

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
    let file = first_word(listed.lines().next().expect("the uploaded File is listed"));

    // The skill's commands run by name in the bwrap sandbox and use the run
    // token from the shell environment, not the User's saved credential.
    script.call(
        "shell",
        serde_json::json!({"command": "printf %s \"$NEHA_BOT_TOKEN\""}),
    );
    script.call(
        "shell",
        serde_json::json!({"command": format!("neha file get {file}")}),
    );
    script.call(
        "shell",
        serde_json::json!({"command": "printf 'local result' > result.txt"}),
    );
    script.call(
        "shell",
        serde_json::json!({"command": "neha chat send --file result.txt 'Local result.'"}),
    );
    script.say("Sent it back.");
    let answered = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "send it back"],
    );
    assert!(answered.contains("Sent it back."), "{answered}");
    let run_id = first_word(&field(&answered, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.contains("[redacted]"),
        "the token is redacted in the record: {shown}"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    let delivered = bot_file_id(&read, "Local result.");
    let out = computer.path().join("downloaded.txt");
    run(
        &computer.home,
        &computer.credential,
        &["file", "get", &delivered, "--out", out.to_str().unwrap()],
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "local result");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes.txt")).unwrap(),
        "over the channel\n"
    );

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_needs_a_space_name_only_when_the_destination_is_implicit() {
    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("session").join("credential.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
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
    let credential: PathBuf = directory.path().join("session").join("credential.json");
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
    let credential: PathBuf = directory.path().join("session").join("credential.json");
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
        &directory.path().join("session").join("credential.json"),
        &["join", "http://127.0.0.1:1/invite", "--name", "Ada"],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_outside_the_grants_waits_for_the_owner() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-grant-")
        .tempdir_in("/var/tmp")
        .expect("a writable scratch directory outside /tmp");
    let outside_dir = tempfile::Builder::new()
        .prefix("neha-e2e-grant-folder-")
        .tempdir_in("/var/tmp")
        .expect("a writable scratch folder outside the Computer workspace");
    let outside = outside_dir.path().join("outside.txt");
    let computer = bare_local_computer(directory, &base);
    approve_first_use(
        &computer.home,
        &computer.credential,
        &computer.chat,
        &script,
    );

    script.call(
        "write_file",
        serde_json::json!({
            "path": outside.to_string_lossy(),
            "content": "first",
        }),
    );
    script.say("I wrote it.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "write outside"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(shown.contains("grant"), "{shown}");
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    assert_eq!(
        run(
            &computer.home,
            &computer.credential,
            &["run", "show", &run_id]
        )
        .lines()
        .next(),
        Some(format!("Run {run_id}: succeeded").as_str())
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "first");

    // Always allow grants the folder; a later write runs without a card.
    script.call(
        "write_file",
        serde_json::json!({
            "path": outside.to_string_lossy(),
            "content": "second",
        }),
    );
    script.say("Second write.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "write again"],
    );
    let (run_id, approval) = waiting_run(&sent);
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval, "--always"],
    );
    assert_eq!(
        run(
            &computer.home,
            &computer.credential,
            &["run", "show", &run_id]
        )
        .lines()
        .next(),
        Some(format!("Run {run_id}: succeeded").as_str())
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "second");

    script.call(
        "write_file",
        serde_json::json!({
            "path": outside.to_string_lossy(),
            "content": "third",
        }),
    );
    script.say("Third write.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "write once more"],
    );
    assert!(sent.contains("Third write."), "{sent}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "third");

    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_owner_grants_access_and_a_run_can_name_its_directory() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-access-")
        .tempdir_in("/var/tmp")
        .expect("a writable scratch directory outside /tmp");
    let granted = directory.path().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let computer = local_computer(directory, &base, &script);

    // The Space refuses a non-absolute grant and one that spells `..`; the CLI
    // checks the input before canonicalize would hide its shape.
    for bad in ["relative/path", "/var/tmp/../var/tmp"] {
        let refused = cli(
            &computer.home,
            &computer.credential,
            &["bot", "update", &computer.bot, "--grant", bad],
        );
        assert!(!refused.status.success(), "accepted {bad}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("absolute") || stderr.contains("`..`"),
            "stderr: {stderr}"
        );
    }

    let updated = run(
        &computer.home,
        &computer.credential,
        &[
            "bot",
            "update",
            &computer.bot,
            "--full-access",
            "on",
            "--grant",
            granted.to_str().unwrap(),
        ],
    );
    assert!(updated.contains("full access: true"), "{updated}");
    assert!(updated.contains("grants: 1"), "{updated}");
    let shown = run(
        &computer.home,
        &computer.credential,
        &["bot", "show", &computer.bot],
    );
    assert!(shown.contains("full access: true"), "{shown}");
    assert!(shown.contains(granted.to_str().unwrap()), "{shown}");

    // A run's working directory is where its shell starts.
    let work = computer.path().join("run-directory");
    std::fs::create_dir(&work).unwrap();
    script.call("shell", serde_json::json!({"command": "pwd > started.txt"}));
    script.say("I worked in the directory.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "--cwd",
            work.to_str().unwrap(),
            "where are you?",
        ],
    );
    assert!(sent.contains("I worked in the directory"), "{sent}");
    assert_eq!(
        std::fs::read_to_string(work.join("started.txt"))
            .unwrap()
            .trim(),
        work.to_str().unwrap()
    );

    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_runs_on_a_local_computer() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // The scratch space sits outside `/tmp`, so the sandbox's fresh `/tmp`
    // cannot mask a configuration directory its hidden-root mounts failed to
    // hide: the shell assertion below is about the hidden root itself.
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-local-")
        .tempdir_in("/var/tmp")
        .expect("the credential test needs a writable /var/tmp outside /tmp");
    let computer = local_computer(directory, &base, &script);
    // A shell in the Bot's folder writes a file there; another command reads a
    // path outside the folder, as the machine's user can.
    script.call(
        "shell",
        serde_json::json!({"command": "pwd; echo hi > a.txt"}),
    );
    script.say("The work ran in my own folder.");
    script.call("shell", serde_json::json!({"command": "cat /etc/hostname"}));
    script.say("I read the whole machine.");
    let folder = computer.workspace.join(&computer.bot);

    // Pairing without a code admitted this machine as a Local Computer.
    assert!(
        computer.paired.contains(&format!(
            "Paired Computer {} with {}.",
            computer.computer, computer.url
        )),
        "{}",
        computer.paired
    );
    // The Bot was created on the paired Local Computer by default, and its
    // folder sits under the machine user's default Workspace root.
    assert_eq!(field(&computer.created, "Computer:"), computer.computer);
    assert_eq!(
        computer.workspace,
        std::fs::canonicalize(computer.machine.workspace_root()).unwrap(),
        "the Local Workspace root defaults to $HOME/.neha/workspaces"
    );
    let shown = run(
        &computer.home,
        &computer.credential,
        &["bot", "show", &computer.bot],
    );
    assert!(
        shown.contains(&format!(
            "Computer: {} (Local, online: true)",
            computer.computer
        )),
        "{shown}"
    );
    assert!(
        !shown.contains("Workspace:"),
        "a Local Computer records no workspace path:\n{shown}"
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
    let config_path = computer.machine.config_path();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_local_computer_pairs_without_a_code_and_takes_the_default_bot() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);

    // Pairing is zero-code: it printed the Computer it admitted, saved the
    // configuration privately, and spent the Space's one-use ticket.
    assert!(
        computer.paired.contains(&format!(
            "Paired Computer {} with {}.",
            computer.computer, computer.url
        )),
        "{}",
        computer.paired
    );
    assert!(
        !computer.paired.contains("Pairing code:"),
        "zero-code pairing prints no code: {}",
        computer.paired
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(computer.machine.config_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the saved pairing is private to the machine user"
        );
    }
    assert!(
        space_tickets(&computer.home).is_empty(),
        "pairing spends the Space's one-use ticket"
    );

    // `bot create` without a Computer lands on the paired Local Computer, whose
    // listing and `bot show` name it as Local and say where it works.
    assert_eq!(field(&computer.created, "Computer:"), computer.computer);
    let listed = format!("{}  local  online  ", computer.computer);
    assert!(
        computer.listed.lines().any(|line| line == listed),
        "`computer list` shows the Local Computer online: {}",
        computer.listed
    );
    let shown = run(
        &computer.home,
        &computer.credential,
        &["bot", "show", &computer.bot],
    );
    assert!(
        shown.contains(&format!(
            "Computer: {} (Local, online: true)",
            computer.computer
        )),
        "{shown}"
    );
    assert!(
        !shown.contains("Workspace:"),
        "a Local Computer records no workspace path:\n{shown}"
    );

    // A relative write lands in the Bot's folder under the default Workspace
    // root: `$HOME/.neha/workspaces/<bot-id>`.
    let folder = computer.workspace.join(&computer.bot);
    script.call(
        "shell",
        serde_json::json!({"command": "pwd; echo hi > a.txt"}),
    );
    script.say("I work under the default Workspace root.");
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
    assert!(
        sent.contains("I work under the default Workspace root."),
        "{sent}"
    );
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.contains(folder.to_str().unwrap()),
        "the shell runs in the default Bot folder:\n{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "hi\n",
        "the write lands in {}",
        folder.display()
    );
    assert_eq!(
        computer.workspace,
        computer.machine.user_home.join(".neha").join("workspaces"),
        "the default Workspace root is $HOME/.neha/workspaces"
    );

    drop(computer);
    provider_task.abort();
}

#[test]
fn a_workspace_inside_a_computers_configuration_directory_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let machine = MachineEnv::under(directory.path(), &directory.path().join("machine"));
    let inside = machine.computer_home.join("workspace");
    let output = computer_command(
        &machine,
        &directory.path().join("session").join("credential.json"),
        &[
            "computer",
            "pair",
            "--yes",
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
        !stdout.contains("Paired Computer"),
        "pairing must fail before it saves anything:\n{stdout}"
    );
    assert!(
        stderr.contains("is inside the Computer's configuration directory"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_saved_workspace_inside_the_configuration_directory_is_refused_at_start() {
    let directory = tempfile::tempdir().unwrap();
    let machine = MachineEnv::under(directory.path(), &directory.path().join("machine"));
    std::fs::create_dir_all(&machine.computer_home).unwrap();
    // A saved pairing that names a workspace inside the configuration
    // directory must refuse to serve today.
    std::fs::write(
        machine.config_path(),
        serde_json::json!({
            "space_url": "http://127.0.0.1:1",
            "computer": "01890a5d-ac96-774b-bcce-b302099a8057",
            "token": "machine-token",
            "workspace": machine.computer_home.join("workspace"),
        })
        .to_string(),
    )
    .unwrap();
    let output = computer_command(
        &machine,
        &directory.path().join("session").join("credential.json"),
        &["computer", "run"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "serving a saved config must fail:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("is running for"),
        "the daemon must not connect:\n{stdout}"
    );
    assert!(
        stderr.contains("is inside the Computer's configuration directory"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_on_a_local_computer_writes_only_its_own_folder() {
    if !local_computer_ready() {
        return;
    }
    // The workspace sits outside `/tmp`: the sandbox gets a fresh `/tmp`, so
    // a relative escape has to reach this host folder for the OS to refuse it.
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-fence-")
        .tempdir_in("/var/tmp")
        .expect("the fence test needs a writable /var/tmp outside /tmp");
    let (base, script, provider_task) = scripted_provider().await;
    let computer = local_computer(directory, &base, &script);
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
async fn a_message_waits_while_a_local_computer_is_offline() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);

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
    let mut restarted = ComputerDaemon::spawn(
        &computer.machine,
        &computer.credential,
        &[
            "computer",
            "run",
            "--workspace",
            computer.workspace.to_str().unwrap(),
        ],
    );
    restarted.expect_line(&format!(
        "Computer {} is running for {}.",
        computer.computer, computer.url
    ));
    restarted.expect_line(&format!("Workspace root: {}", computer.workspace.display()));
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
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);
    // The first reply asks for a slow shell; the second reply is only wanted
    // if the run wrongly went on after the Computer left.
    script.call("shell", serde_json::json!({"command": "sleep 2"}));
    script.say("The slow command finished.");

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
async fn an_offline_local_computer_does_not_block_another_bot() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);
    script.say("The hosted Bot answered.");

    // A second Bot works on a Hosted Computer inside the Space.
    let created = run(
        &computer.home,
        &computer.credential,
        &["bot", "create", "Helper", "--computer", "hosted"],
    );
    let helper_chat = field(&created, "Chat:");

    // The Local Computer leaves, so its Bot's Message waits in the record.
    computer.daemon.kill();
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);
    let (mut waiting, stdout, _stderr) = spawn_cli(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "are you there?"],
    );
    let mut output = String::new();
    read_until(&stdout, &mut output, "queued (position 1)");

    // The unrelated Hosted Bot still answers while that run waits.
    let answered = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &helper_chat, "are you free?"],
    );
    assert!(answered.contains("The hosted Bot answered."), "{answered}");
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
async fn a_local_computers_messages_run_in_the_order_they_were_sent() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);
    // Each run holds its reply open, so the next Messages queue behind it and
    // every run takes the reply holding its own place in the queue.
    script.hold("First reply.");
    script.hold("Second reply.");
    script.hold("Third reply.");

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
    assert_eq!(replies.len(), 4, "{read}");
    assert!(
        replies[0].ends_with("Bot: The first use is approved."),
        "{read}"
    );
    assert!(replies[1].ends_with("Bot: First reply."), "{read}");
    assert!(replies[2].ends_with("Bot: Second reply."), "{read}");
    assert!(replies[3].ends_with("Bot: Third reply."), "{read}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_moves_to_another_local_computer() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential = directory.path().join("session").join("credential.json");
    let (server, url) = space_with_admin(directory.path(), Some(&base));

    // Two machines pair into the same Space. Each has its own user home,
    // configuration directory, and default Workspace root.
    let a = pair_computer(&home, &credential, &url, &directory.path().join("a"));
    let b_workspace = directory.path().join("b").join("workspace");
    let b = pair_computer_to(
        &home,
        &credential,
        &url,
        &directory.path().join("b"),
        Some(&b_workspace),
    );
    assert_ne!(
        a.workspace, b.workspace,
        "the two machines keep separate folders"
    );
    assert_eq!(
        b.workspace,
        std::fs::canonicalize(&b_workspace).unwrap(),
        "`--workspace` overrides the default Workspace root"
    );

    // The Bot starts on Computer A and writes a working file in its folder.
    let created = run(
        &home,
        &credential,
        &["bot", "create", "Scout", "--computer", &a.computer],
    );
    let bot = first_word(&field(&created, "Created Bot "));
    let chat = field(&created, "Chat:");
    approve_first_use(&home, &credential, &chat, &script);
    let a_folder = a.workspace.join(&bot);
    script.call(
        "shell",
        serde_json::json!({"command": "echo old > old.txt; pwd"}),
    );
    script.say("I wrote old.txt on A.");
    let sent = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "work on A"],
    );
    assert!(sent.contains("I wrote old.txt on A."), "{sent}");
    assert_eq!(
        std::fs::read_to_string(a_folder.join("old.txt")).unwrap(),
        "old\n",
        "the working file is written in A's Bot folder"
    );
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(&home, &credential, &["run", "show", &run_id]);
    assert!(
        shown.contains(a_folder.to_str().unwrap()),
        "the first run works in A's folder:\n{shown}"
    );
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {} (Local, online: true)", a.computer)),
        "{shown}"
    );

    // The User moves the Bot from A to B.
    let moved = run(
        &home,
        &credential,
        &["bot", "move", &bot, "--computer", &b.computer],
    );
    assert!(
        moved.contains(&format!(
            "now works on Computer {} (local, online: true)",
            b.computer
        )),
        "{moved}"
    );
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {} (Local, online: true)", b.computer)),
        "`bot show` reports the new Computer:\n{shown}"
    );
    assert!(
        !shown.contains("Workspace:"),
        "a Local Computer records no workspace path:\n{shown}"
    );

    // The first use counts per (Bot, acting User, Computer): Computer B asks
    // on its first action, even though Computer A was already approved.
    approve_first_use(&home, &credential, &chat, &script);

    // The next Message runs in B's fresh folder: the working file stayed on A.
    let b_folder = b.workspace.join(&bot);
    script.call("shell", serde_json::json!({"command": "pwd; cat old.txt"}));
    script.say("I am on B.");
    let sent = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "work on B"],
    );
    assert!(sent.contains("I am on B."), "{sent}");
    assert!(
        b_folder.is_dir(),
        "the Bot's folder on B is created by its first action"
    );
    assert!(
        !b_folder.join("old.txt").exists(),
        "a working file does not travel with the Bot"
    );
    assert_eq!(
        std::fs::read_to_string(a_folder.join("old.txt")).unwrap(),
        "old\n",
        "A keeps the working file the Bot left there"
    );
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(&home, &credential, &["run", "show", &run_id]);
    assert!(
        shown.contains(b_folder.to_str().unwrap()),
        "the second run works in B's folder:\n{shown}"
    );
    assert!(
        shown.contains("shell (failed)"),
        "`cat old.txt` in B's folder must fail:\n{shown}"
    );

    // The Chat keeps the whole conversation across the move.
    let read = run(&home, &credential, &["chat", "read", &chat]);
    assert!(read.contains("Bot: I wrote old.txt on A."), "{read}");
    assert!(read.contains("Bot: I am on B."), "{read}");

    // Restarting the Space replays the move: the Bot is still on B, and the
    // next Message still runs there.
    let port = space_port(&url);
    drop(server);
    let (server, new_url, _) = serve_on(&home, port);
    assert_eq!(
        new_url, url,
        "the Space restarts on the address its Computers saved"
    );
    std::fs::remove_file(&credential).unwrap();
    run(
        &home,
        &credential,
        &["login", "--name", "Ada", "--server", &new_url],
    );
    wait_for_computer_online(&home, &credential, &b.computer);
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {} (Local, online: true)", b.computer)),
        "a restart replays the move:\n{shown}"
    );

    script.call("shell", serde_json::json!({"command": "pwd"}));
    script.say("B still holds my Computer.");
    let sent = run(
        &home,
        &credential,
        &["chat", "send", "--chat", &chat, "after the restart"],
    );
    assert!(sent.contains("B still holds my Computer."), "{sent}");
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(&home, &credential, &["run", "show", &run_id]);
    assert!(
        shown.contains(b_folder.to_str().unwrap()),
        "after a restart the Bot still runs on B:\n{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(a_folder.join("old.txt")).unwrap(),
        "old\n",
        "A's working file survives the move and the restart"
    );

    // Moving back to A is a record event like any other.
    let moved = run(
        &home,
        &credential,
        &["bot", "move", &bot, "--computer", &a.computer],
    );
    assert!(
        moved.contains(&format!(
            "now works on Computer {} (local, online: true)",
            a.computer
        )),
        "{moved}"
    );
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {} (Local, online: true)", a.computer)),
        "{shown}"
    );

    // Stop the machines before the Space, so teardown prints no channel errors.
    drop(b.daemon);
    drop(a.daemon);
    drop(server);
    provider_task.abort();
}

#[test]
fn a_move_to_an_unknown_or_hosted_computer_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential = directory.path().join("session").join("credential.json");
    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);
    // With no Local Computer paired, a Bot created without one is Hosted.
    let created = run(&home, &credential, &["bot", "create", "Scout"]);
    let bot = first_word(&field(&created, "Created Bot "));
    let hosted = field(&created, "Computer:");
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {hosted} (Hosted, online: true)")),
        "a Bot without a Local Computer is Hosted:\n{shown}"
    );

    // A Computer this User does not own, or one that never existed, is refused.
    let unknown = neha::id::Id::new().to_string();
    let refused = cli(
        &home,
        &credential,
        &["bot", "move", &bot, "--computer", &unknown],
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("no such Computer"), "stderr: {stderr}");

    // A Hosted Computer is not a move target.
    let refused = cli(
        &home,
        &credential,
        &["bot", "move", &bot, "--computer", &hosted],
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("a Bot moves to a Local Computer"),
        "stderr: {stderr}"
    );

    // A refused move leaves the Bot where it was.
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {hosted} (Hosted, online: true)")),
        "a refused move must not move the Bot:\n{shown}"
    );

    drop(server);
}

#[test]
fn a_bot_created_with_computer_hosted_works_in_the_space() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential = directory.path().join("session").join("credential.json");
    let (server, _url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");
    run(&home, &credential, &["join", &invite, "--name", "Ada"]);

    // `--computer hosted` is a keyword naming a fresh Hosted Computer.
    let explicit = run(
        &home,
        &credential,
        &["bot", "create", "Other", "--computer", "hosted"],
    );
    let bot = first_word(&field(&explicit, "Created Bot "));
    let hosted = field(&explicit, "Computer:");
    let shown = run(&home, &credential, &["bot", "show", &bot]);
    assert!(
        shown.contains(&format!("Computer: {hosted} (Hosted, online: true)")),
        "{shown}"
    );
    let workspace = home.join("computers").join(&hosted).join("workspace");
    assert!(
        shown.contains(&format!("Workspace: {}", workspace.display())),
        "a Hosted Computer names its workspace:\n{shown}"
    );
    let listing = run(&home, &credential, &["computer", "list"]);
    assert!(
        listing
            .lines()
            .any(|line| line.starts_with(&format!("{hosted}  hosted  online  "))),
        "`computer list` shows the Hosted Computer:\n{listing}"
    );

    // Without a Local Computer paired, the default is Hosted as well.
    let fallback = run(&home, &credential, &["bot", "create", "Default"]);
    let fallback_bot = first_word(&field(&fallback, "Created Bot "));
    let fallback_computer = field(&fallback, "Computer:");
    assert_ne!(
        fallback_computer, hosted,
        "every Hosted Bot has its own Computer"
    );
    let shown = run(&home, &credential, &["bot", "show", &fallback_bot]);
    assert!(
        shown.contains(&format!(
            "Computer: {fallback_computer} (Hosted, online: true)"
        )),
        "a Bot without a Local Computer gets a Hosted one:\n{shown}"
    );

    // `--computer local` explains how to get one.
    let refused = cli(
        &home,
        &credential,
        &["bot", "create", "Local", "--computer", "local"],
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("there is no Local Computer; run `neha computer pair` on this machine"),
        "stderr: {stderr}"
    );

    drop(server);
}

#[test]
fn an_older_pairing_is_refused_until_the_machine_pairs_again() {
    let directory = tempfile::tempdir().unwrap();
    let machine = MachineEnv::under(directory.path(), &directory.path().join("machine"));
    // An older release saved the pairing beside the user's home; it is not
    // migrated, and the current configuration directory stays empty.
    std::fs::create_dir_all(machine.user_home.join(".neha")).unwrap();
    std::fs::write(
        machine.user_home.join(".neha").join("computer.json"),
        serde_json::json!({
            "space_url": "http://127.0.0.1:1",
            "computer": "01890a5d-ac96-774b-bcce-b302099a8057",
            "token": "machine-token",
            "workspace": machine.user_home.join(".neha").join("workspaces"),
        })
        .to_string(),
    )
    .unwrap();
    let credential = directory.path().join("session").join("credential.json");

    for arguments in [
        vec!["computer", "run"],
        vec!["computer", "pair", "--yes", "--space", "http://127.0.0.1:1"],
    ] {
        let output = computer_command(&machine, &credential, &arguments);
        assert!(
            !output.status.success(),
            "`neha {}` must refuse an older pairing: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("an older pairing is at ~/.neha/computer.json"),
            "`neha {}` must name the older pairing: {stderr}",
            arguments.join(" ")
        );
        assert!(
            stderr.contains("`neha computer pair` again"),
            "`neha {}` must say how to replace it: {stderr}",
            arguments.join(" ")
        );
    }
}

#[test]
fn a_local_pairing_uses_the_space_ticket() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential = directory.path().join("session").join("credential.json");
    let (server, url) = space_with_admin(directory.path(), None);

    // The first machine names the ticket file the Space wrote, and pairing
    // spends it.
    let explicit = MachineEnv::under(&home, &directory.path().join("explicit"));
    let ticket = ticket_path(&home);
    let output = computer_command(
        &explicit,
        &credential,
        &[
            "computer",
            "pair",
            "--yes",
            "--space",
            &url,
            "--ticket-file",
            ticket.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "pairing with an explicit ticket failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Paired Computer "),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!ticket.is_file(), "pairing spends the ticket file");
    assert!(space_tickets(&home).is_empty(), "no ticket is left waiting");

    // A fresh Space start writes a fresh per-Space ticket, which the CLI finds
    // on its own: zero-code pairing needs no path at all.
    let port = space_port(&url);
    drop(server);
    let (server, restarted, _) = serve_on(&home, port);
    assert_eq!(
        restarted, url,
        "the Space restarts on the address its Computers save"
    );
    let tickets = space_tickets(&home);
    assert_eq!(
        tickets.len(),
        1,
        "a restart writes one ticket under spaces/<space-key>: {tickets:?}"
    );
    let automatic = MachineEnv::under(&home, &directory.path().join("automatic"));
    let output = computer_command(
        &automatic,
        &credential,
        &["computer", "pair", "--yes", "--space", &url],
    );
    assert!(
        output.status.success(),
        "pairing by discovery failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Paired Computer "),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        space_tickets(&home).is_empty(),
        "the ticket the CLI found is gone after pairing"
    );

    drop(server);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_local_action_waits_for_approval() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // The action appends, so running it twice would show up in the file; its
    // tab also proves a summary is escaped for the terminal.
    script.call(
        "shell",
        serde_json::json!({"command": "printf 'x\ty' >> proof.txt"}),
    );
    script.say("The proof was approved.");
    let directory = tempfile::tempdir().unwrap();
    let computer = bare_local_computer(directory, &base);
    let folder = computer.workspace.join(&computer.bot);

    // A non-TTY send reports the waiting run and exits successfully.
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "write the proof"],
    );
    let (run_id, approval) = waiting_run(&sent);
    assert!(
        sent.contains(&format!("answer with `neha approval allow {approval}`")),
        "{sent}"
    );

    // The run waits, and the approval covers an action that has not started.
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.starts_with(&format!("Run {run_id}: waiting")),
        "{shown}"
    );
    assert!(shown.contains(&format!("  approval {approval}")), "{shown}");
    assert!(
        !shown.contains("action "),
        "an unanswered approval covers an action that has not started:\n{shown}"
    );

    // The list and the show name the pending approval for this Bot.
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    let row = format!(
        "{approval}  {run_id}  {}  shell  first_use_local  shell: printf 'x\\ty' >> proof.txt",
        computer.bot
    );
    assert!(listed.lines().any(|line| line == row), "{listed}");
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(shown.contains("  step "), "{shown}");
    assert!(shown.contains("  index 0"), "{shown}");
    assert!(shown.contains("  action "), "{shown}");
    let arguments = "{\"command\":\"printf 'x\\ty' >> proof.txt\"}";
    assert!(
        shown.contains(&format!("  arguments {arguments}")),
        "the show names the full action:\n{shown}"
    );
    assert!(
        !shown.contains("  rule "),
        "a first use is not the result of a rule:\n{shown}"
    );

    // A lasting answer cannot cover the first use of a Local Computer.
    let lasting = cli(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval, "--always"],
    );
    assert!(!lasting.status.success());
    let stderr = String::from_utf8_lossy(&lasting.stderr);
    assert!(
        stderr.contains("a lasting rule cannot cover the first use of a Local Computer"),
        "stderr: {stderr}"
    );

    let allowed = run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    assert_eq!(
        allowed.trim(),
        format!("Allowed {approval} for run {run_id}.")
    );
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert!(shown.contains("  reply "), "{shown}");
    // The action started after the answer and ran exactly once.
    assert_eq!(shown.matches("shell (succeeded)").count(), 1, "{shown}");
    assert_eq!(
        std::fs::read_to_string(folder.join("proof.txt")).unwrap(),
        "x\ty",
        "an allowed action runs exactly once"
    );
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(
        listed.trim().is_empty(),
        "the answered approval is no longer pending:\n{listed}"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert!(read.contains("Bot: The proof was approved."), "{read}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_action_does_not_ask_after_an_allow() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    script.call(
        "shell",
        serde_json::json!({"command": "printf x >> remembered.txt"}),
    );
    script.say("The first action ran.");
    script.call(
        "shell",
        serde_json::json!({"command": "printf x >> remembered.txt"}),
    );
    script.say("The second action ran.");
    let directory = tempfile::tempdir().unwrap();
    let computer = bare_local_computer(directory, &base);
    let folder = computer.workspace.join(&computer.bot);

    // The first use asks; the allow is remembered for this Bot and User.
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "first"],
    );
    let (run_id, approval) = waiting_run(&sent);
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert_eq!(
        std::fs::read_to_string(folder.join("remembered.txt")).unwrap(),
        "x"
    );

    // The same action does not ask again.
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "second"],
    );
    assert!(
        !sent.contains("is waiting for approval"),
        "the remembered first use must not ask again:\n{sent}"
    );
    assert!(sent.contains("The second action ran."), "{sent}");
    assert_eq!(
        std::fs::read_to_string(folder.join("remembered.txt")).unwrap(),
        "xx",
        "both actions ran"
    );
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(listed.trim().is_empty(), "{listed}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_always_allow_rule_answers_the_next_action() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);

    // An Ask rule makes the next matching action wait.
    let added = run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    assert!(
        added.contains(": ask shell (command-prefix \"printf\")"),
        "{added}"
    );
    // A rule makes metacharacters ask, so the command stays a plain form.
    script.call("shell", serde_json::json!({"command": "printf always"}));
    script.say("The always-allowed action ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "always allow"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(listed.contains("  rule  shell: printf always"), "{listed}");
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(shown.contains("  rule "), "{shown}");
    assert!(shown.contains("  index 0"), "{shown}");
    assert!(
        shown.contains("  arguments {\"command\":\"printf always\"}"),
        "{shown}"
    );

    // Always allow answers this approval and derives a lasting allow rule.
    let allowed = run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval, "--always"],
    );
    assert_eq!(
        allowed.trim(),
        format!("Allowed {approval} for run {run_id}.")
    );
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert!(shown.contains("shell (succeeded)"), "{shown}");
    let rules = run(&computer.home, &computer.credential, &["rule", "list"]);
    let rows: Vec<&str> = rules.lines().collect();
    assert_eq!(
        rows.len(),
        1,
        "the derived allow rule replaces the ask rule it covers:\n{rules}"
    );
    assert!(
        rows[0].ends_with("  allow  shell  command-prefix printf"),
        "{rules}"
    );
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(listed.trim().is_empty(), "{listed}");

    // A following identical action does not ask.
    script.call("shell", serde_json::json!({"command": "printf always"}));
    script.say("The next action ran without asking.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "again"],
    );
    assert!(
        !sent.contains("is waiting for approval"),
        "the derived allow rule answers the next matching action:\n{sent}"
    );
    assert!(
        sent.contains("The next action ran without asking."),
        "{sent}"
    );
    let run_id = first_word(&field(&sent, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(shown.contains("shell (succeeded)"), "{shown}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_action_ends_the_run() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    // A denied run never asks the Model again, so only its tool call is queued.
    script.call(
        "shell",
        serde_json::json!({"command": "printf x >> denied.txt"}),
    );
    let directory = tempfile::tempdir().unwrap();
    let computer = bare_local_computer(directory, &base);
    let folder = computer.workspace.join(&computer.bot);

    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "do not do it"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let denied = run(
        &computer.home,
        &computer.credential,
        &["approval", "deny", &approval],
    );
    assert_eq!(
        denied.trim(),
        format!("Denied {approval} for run {run_id}.")
    );
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "failed");
    assert!(
        shown.contains("  error You denied the action, so I stopped."),
        "{shown}"
    );
    assert!(
        !shown.contains("shell (succeeded)"),
        "a denied action never runs:\n{shown}"
    );
    assert!(
        !folder.join("denied.txt").exists(),
        "a denied action writes nothing"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert!(
        read.contains("Bot: You denied the action, so I stopped."),
        "{read}"
    );
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(listed.trim().is_empty(), "{listed}");

    // A deny does not spend the first use: the same action asks again, and
    // allowing it then runs it once.
    script.call(
        "shell",
        serde_json::json!({"command": "printf x >> denied.txt"}),
    );
    script.say("It ran after a second answer.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "try again"],
    );
    assert!(
        sent.contains("is waiting for approval"),
        "a deny leaves the first use unapproved:\n{sent}"
    );
    let (second_run, second_approval) = waiting_run(&sent);
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &second_approval],
    );
    wait_for_run_status(
        &computer.home,
        &computer.credential,
        &second_run,
        "succeeded",
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("denied.txt")).unwrap(),
        "x",
        "the retried action runs exactly once"
    );

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ask_rule_matches_only_its_command_prefix() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);

    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    script.call("shell", serde_json::json!({"command": "printf matched"}));
    script.say("The matching command ran.");

    // A command with the prefix asks.
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "matching"],
    );
    let (run_id, approval) = waiting_run(&sent);
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");

    // A command without the prefix does not ask.
    script.call("shell", serde_json::json!({"command": "echo unmatched"}));
    script.say("The other command ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "other"],
    );
    assert!(
        !sent.contains("is waiting for approval"),
        "only commands with the prefix should ask:\n{sent}"
    );
    assert!(sent.contains("The other command ran."), "{sent}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_run_yields_to_the_next_message() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);
    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    // The first run asks before its action; the second run takes the next
    // reply while the first waits, and the first finishes after the answer.
    script.call("shell", serde_json::json!({"command": "printf one"}));
    script.say("The second one finished.");
    script.say("The first one finished.");
    let first = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "first"],
    );
    let (first_run, approval) = waiting_run(&first);

    // The second Message's run finishes while the first still waits.
    let second = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "second"],
    );
    assert!(second.contains("The second one finished."), "{second}");
    let second_run = first_word(&field(&second, "run "));
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &second_run],
    );
    assert!(
        shown.starts_with(&format!("Run {second_run}: succeeded")),
        "{shown}"
    );
    let waiting = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &first_run],
    );
    assert!(
        waiting.starts_with(&format!("Run {first_run}: waiting")),
        "the first run must still wait for its answer:\n{waiting}"
    );

    // Answering lets the first run finish.
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    let shown = wait_for_run_status(
        &computer.home,
        &computer.credential,
        &first_run,
        "succeeded",
    );
    assert!(shown.contains("  reply "), "{shown}");
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert!(read.contains("Bot: The second one finished."), "{read}");
    assert!(read.contains("Bot: The first one finished."), "{read}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_approval_survives_a_space_restart() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);
    let folder = computer.workspace.join(&computer.bot);
    let added = run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "touch",
        ],
    );
    let ask = first_word(&field(&added, "Added rule "));
    // A plain command: with a shell rule, a redirection would be ambiguous and
    // would ask without naming that rule.
    script.call(
        "shell",
        serde_json::json!({"command": "touch restarted.txt"}),
    );
    script.say("The action ran after the restart.");

    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "wait through a restart",
        ],
    );
    let (run_id, approval) = waiting_run(&sent);

    // The Space restarts while the approval waits.
    computer.restart_space();
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.starts_with(&format!("Run {run_id}: waiting")),
        "a restart keeps the waiting run waiting:\n{shown}"
    );
    assert!(shown.contains(&format!("  approval {approval}")), "{shown}");
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(
        listed.contains(&format!(
            "{approval}  {run_id}  {}  shell  rule  ",
            computer.bot
        )),
        "{listed}"
    );
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(
        shown.contains(&format!("  rule {ask}")),
        "the pending approval still names its rule:\n{shown}"
    );
    assert!(shown.contains("  index 0"), "{shown}");
    assert!(
        shown.contains("  arguments {\"command\":\"touch restarted.txt\"}"),
        "{shown}"
    );

    // The answer after the restart resumes the run, and it runs once.
    let allowed = run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    assert_eq!(
        allowed.trim(),
        format!("Allowed {approval} for run {run_id}.")
    );
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert_eq!(
        shown.matches("shell (succeeded)").count(),
        1,
        "a resumed action runs exactly once:\n{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("restarted.txt")).unwrap(),
        "",
        "the resumed action's side effect is there exactly once"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert!(
        read.contains("Bot: The action ran after the restart."),
        "{read}"
    );

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_allow_rule_wins_over_a_wider_ask_rule() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);

    // A wide Ask rule covers every shell command; a narrower Allow rule wins
    // on the commands it names.
    let added = run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
        ],
    );
    assert!(added.trim_end().ends_with(": ask shell"), "{added}");
    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "allow",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );

    // The more specific Allow rule answers without asking.
    script.call("shell", serde_json::json!({"command": "printf fine"}));
    script.say("The allowed command ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "allowed"],
    );
    assert!(
        !sent.contains("is waiting for approval"),
        "a narrower allow rule wins over a wider ask rule:\n{sent}"
    );
    assert!(sent.contains("The allowed command ran."), "{sent}");

    // A command only the wide Ask rule matches still asks.
    script.call("shell", serde_json::json!({"command": "echo asked"}));
    script.say("The asked command ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "asked"],
    );
    let (run_id, approval) = waiting_run(&sent);
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compound_command_asks_only_when_a_shell_rule_exists() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);

    // With no shell rule, a compound command runs under the default allow.
    script.call(
        "shell",
        serde_json::json!({"command": "printf one; printf two"}),
    );
    script.say("The compound command ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "compound"],
    );
    assert!(
        !sent.contains("is waiting for approval"),
        "with no shell rule the default allows the command:\n{sent}"
    );
    assert!(sent.contains("The compound command ran."), "{sent}");

    // A shell rule makes the prefix match after leading whitespace...
    let added = run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    let ask = first_word(&field(&added, "Added rule "));
    script.call("shell", serde_json::json!({"command": "  printf padded"}));
    script.say("The padded command ran.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "padded"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(shown.contains("  rule "), "{shown}");
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");

    // ...and a compound command is ambiguous for a prefix rule, so it asks
    // instead of slipping past the rule under another first word.
    script.call(
        "shell",
        serde_json::json!({"command": "echo safe; printf danger"}),
    );
    script.say("The compound command ran after the answer.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "compound again"],
    );
    let (run_id, approval) = waiting_run(&sent);
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.starts_with(&format!("Run {run_id}: waiting")),
        "{shown}"
    );
    // An ambiguous form is guarded by no single rule, so the approval names
    // none: the User answers the action themselves.
    let shown = run(
        &computer.home,
        &computer.credential,
        &["approval", "show", &approval],
    );
    assert!(
        !shown.contains(&format!("  rule {ask}")),
        "an ambiguous action names no rule:\n{shown}"
    );
    run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert!(shown.contains("shell (succeeded)"), "{shown}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tty_card_names_the_scope_and_answers() {
    if !local_computer_ready() {
        return;
    }
    if !tty_available() {
        eprintln!("skipping the TTY card test: script(1) is not available");
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let computer = local_computer(directory, &base, &script);
    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    script.call("shell", serde_json::json!({"command": "printf card"}));
    script.say("The card answered.");
    let mut tty = TtyCli::spawn(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "on the card"],
    );

    // The card names the reason, what Always allow would cover, and whether
    // the action can be undone.
    let card = tty.expect_containing("for shell");
    assert!(card.contains("Approval "), "{card}");
    let reason = tty.expect_containing("reason:");
    assert!(reason.contains("a rule you set"), "{reason}");
    let scope = tty.expect_containing("Always allow covers:");
    assert!(
        scope.contains("shell commands starting with \"printf\""),
        "{scope}"
    );
    let undo = tty.expect_containing("cannot be undone");
    assert!(undo.contains("This action cannot be undone."), "{undo}");

    // Answering on the terminal is a real answer: Always allow derives the rule.
    tty.answer("A");
    let (ok, output) = tty.finish();
    assert!(ok, "the interactive send must finish:\n{output}");
    assert!(output.contains("The card answered."), "{output}");
    let rules = run(&computer.home, &computer.credential, &["rule", "list"]);
    assert!(
        rules.contains("  allow  shell  command-prefix printf"),
        "Always allow on the card creates the rule:\n{rules}"
    );
    let listed = run(&computer.home, &computer.credential, &["approval", "list"]);
    assert!(listed.trim().is_empty(), "{listed}");

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decided_allow_resumes_after_a_restart_before_the_action() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);
    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "printf",
        ],
    );
    script.call("shell", serde_json::json!({"command": "printf resumed"}));
    script.say("It resumed after the restart.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &["chat", "send", "--chat", &computer.chat, "wait for me"],
    );
    let (run_id, approval) = waiting_run(&sent);

    // The Computer leaves, so the allow can be decided while the pump cannot
    // start the action: the restart below then lands before the action.
    computer.daemon.kill();
    wait_for_computer_offline(&computer.home, &computer.credential, &computer.computer);
    let allowed = run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    assert_eq!(
        allowed.trim(),
        format!("Allowed {approval} for run {run_id}.")
    );
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.starts_with(&format!("Run {run_id}: running")),
        "a decided allow leaves the run ready to resume:\n{shown}"
    );
    assert!(
        !shown.contains("action "),
        "the allow has not started the action yet:\n{shown}"
    );

    // The Space restarts before the action starts; the decided allow survives.
    computer.restart_space_only();
    let shown = run(
        &computer.home,
        &computer.credential,
        &["run", "show", &run_id],
    );
    assert!(
        shown.starts_with(&format!("Run {run_id}: running")),
        "a decided allow survives a restart before the action:\n{shown}"
    );
    assert!(!shown.contains("error "), "{shown}");

    // Reconnecting the Computer resumes the run: the action starts, runs, and
    // answers exactly once.
    let mut daemon = ComputerDaemon::spawn(
        &computer.machine,
        &computer.credential,
        &["computer", "run"],
    );
    daemon.expect_line(&format!(
        "Computer {} is running for {}.",
        computer.computer, computer.url
    ));
    wait_for_computer_online(&computer.home, &computer.credential, &computer.computer);
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "succeeded");
    assert_eq!(
        shown.matches("shell (succeeded)").count(),
        1,
        "a resumed action runs exactly once:\n{shown}"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert_eq!(
        read.matches("It resumed after the restart.").count(),
        1,
        "the resumed step replies exactly once:\n{read}"
    );

    drop(daemon);
    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_continues_after_a_completed_step() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-recover-")
        .tempdir_in("/var/tmp")
        .expect("the recovery test needs a writable /var/tmp");
    let mut computer = local_computer(directory, &base, &script);
    let folder = computer.workspace.join(&computer.bot);

    // Step one appends once; the next Model call is held so the Space can stop
    // after the record holds the completed step.
    script.call(
        "shell",
        serde_json::json!({"command": "printf x >> count.txt"}),
    );
    script.hold("the held answer");
    script.say("The recovered answer.");
    let (sender, receiver) = std::sync::mpsc::channel();
    let home = computer.home.clone();
    let credential = computer.credential.clone();
    let chat = computer.chat.clone();
    std::thread::spawn(move || {
        let output = cli(
            &home,
            &credential,
            &["chat", "send", "--chat", &chat, "recover this"],
        );
        let _ = sender.send(output);
    });
    // The fixture's first use used the first two replies; request four is the
    // step after our action, so the action's result and step are in the record.
    wait_until("the first step to be recorded", || script.requests() >= 4);

    computer.restart_space();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = run(
            &computer.home,
            &computer.credential,
            &["chat", "read", &computer.chat],
        );
        if read.contains("The recovered answer.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the recovered answer:\n{read}\n\
             count: {:?}",
            std::fs::read_to_string(folder.join("count.txt"))
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(folder.join("count.txt")).unwrap(),
        "x",
        "the completed action is replayed, not run again"
    );
    let _ = receiver.recv_timeout(Duration::from_secs(10));

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_replays_a_refused_call_and_runs_the_one_after_it() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::Builder::new()
        .prefix("neha-e2e-refused-")
        .tempdir_in("/var/tmp")
        .expect("the recovery test needs a writable /var/tmp");
    let mut computer = local_computer(directory, &base, &script);
    let folder = computer.workspace.join(&computer.bot);

    // One step has a refused call first, then a valid action. The refusal is
    // recorded as a failed result; a replay must keep it and run the valid
    // action once, not fail the run.
    script.calls(&[
        ("unknown_tool", serde_json::json!({})),
        (
            "shell",
            serde_json::json!({"command": "printf x >> count.txt"}),
        ),
    ]);
    script.hold("the held answer");
    script.say("The recovered answer.");
    let (sender, receiver) = std::sync::mpsc::channel();
    let home = computer.home.clone();
    let credential = computer.credential.clone();
    let chat = computer.chat.clone();
    std::thread::spawn(move || {
        let output = cli(
            &home,
            &credential,
            &["chat", "send", "--chat", &chat, "recover a refusal"],
        );
        let _ = sender.send(output);
    });
    wait_until("the first step to be recorded", || script.requests() >= 4);

    computer.restart_space();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = run(
            &computer.home,
            &computer.credential,
            &["chat", "read", &computer.chat],
        );
        if read.contains("The recovered answer.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the recovered answer:\n{read}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(folder.join("count.txt")).unwrap(),
        "x",
        "the action after a refused call runs exactly once"
    );
    let _ = receiver.recv_timeout(Duration::from_secs(10));

    drop(computer);
    provider_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allow_at_an_action_already_started_fails_after_a_restart() {
    if !local_computer_ready() {
        return;
    }
    let (base, script, provider_task) = scripted_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let mut computer = local_computer(directory, &base, &script);
    run(
        &computer.home,
        &computer.credential,
        &[
            "rule",
            "add",
            "--bot",
            &computer.bot,
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "sleep",
        ],
    );
    // The action is still running when the Space goes away.
    script.call("shell", serde_json::json!({"command": "sleep 3"}));
    script.say("The action finished.");
    let sent = run(
        &computer.home,
        &computer.credential,
        &[
            "chat",
            "send",
            "--chat",
            &computer.chat,
            "restart mid-action",
        ],
    );
    let (run_id, approval) = waiting_run(&sent);
    let allowed = run(
        &computer.home,
        &computer.credential,
        &["approval", "allow", &approval],
    );
    assert_eq!(
        allowed.trim(),
        format!("Allowed {approval} for run {run_id}.")
    );

    // The approved action starts and stays in flight; a restart now cannot
    // resume it without running the action twice.
    wait_until("the approved action to start", || {
        run(
            &computer.home,
            &computer.credential,
            &["run", "show", &run_id],
        )
        .contains("shell (unknown)")
    });
    computer.restart_space();
    let shown = wait_for_run_status(&computer.home, &computer.credential, &run_id, "failed");
    assert!(
        shown.contains("cannot be verified"),
        "a restart after an unverifiable action started must not resume it:\n{shown}"
    );
    assert!(
        !shown.contains("shell (succeeded)"),
        "the action must not be reported as finished:\n{shown}"
    );
    let read = run(
        &computer.home,
        &computer.credential,
        &["chat", "read", &computer.chat],
    );
    assert!(
        read.contains("Bot: I could not continue after the interruption:"),
        "{read}"
    );
    assert!(
        !read.contains("The action finished."),
        "the Model's next reply must never be used:\n{read}"
    );

    drop(computer);
    provider_task.abort();
}
