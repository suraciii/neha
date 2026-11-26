//! Exercises the built CLI against a real Space and a fake Provider.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use axum::Router;
use axum::response::Response;
use axum::routing::post;

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
fn serve(home: &Path) -> (SpaceProcess, String, Option<String>) {
    let mut child = Command::new(BIN)
        .args(["serve", "--port", "0"])
        .env("NEHA_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("neha serve starts");
    let stdout = child.stdout.take().unwrap();
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let mut url = None;
    let mut invite = None;
    while url.is_none() || invite.is_none() {
        let line = received
            .recv_timeout(Duration::from_secs(10))
            .expect("neha serve prints its address and invitation");
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

    (SpaceProcess(child), url.unwrap(), invite)
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

#[derive(Clone, Debug)]
enum ScriptedReply {
    Text(String),
    Tool {
        name: String,
        arguments: serde_json::Value,
    },
}

/// The replies a fake Provider gives, one per request in the order queued.
#[derive(Clone)]
struct Script {
    replies: tokio::sync::mpsc::UnboundedSender<ScriptedReply>,
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
}

async fn scripted_provider() -> (String, Script, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (replies, queued) = tokio::sync::mpsc::unbounded_channel::<ScriptedReply>();
    let queued = Arc::new(tokio::sync::Mutex::new(queued));
    let requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let queued = Arc::clone(&queued);
            let requests = Arc::clone(&requests);
            async move {
                let index = requests.fetch_add(1, Ordering::SeqCst);
                let reply = tokio::time::timeout(Duration::from_secs(10), queued.lock().await.recv())
                    .await
                    .expect("the fake Provider got more requests than the script answers")
                    .expect("the script is closed");
                let body = match reply {
                    ScriptedReply::Text(text) => {
                        format!(
                            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
                            serde_json::to_string(&text).unwrap()
                        )
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

    (format!("http://{addr}/v1"), Script { replies }, task)
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
