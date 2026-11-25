//! Exercises the built CLI against a real Space and a fake Provider.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
    Command::new(BIN)
        .args(arguments)
        .env("NEHA_HOME", home)
        .env("NEHA_CREDENTIAL", credential)
        .env("NEHA_PROVIDER_KEY", "sk-test")
        .output()
        .expect("neha runs")
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

#[tokio::test(flavor = "multi_thread")]
async fn a_user_runs_a_space_and_chats_with_a_bot() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let credential: PathBuf = directory.path().join("credential.json");
    let (provider, provider_task) = fake_provider().await;

    let (server, url, invite) = serve(&home);
    let invite = invite.expect("a fresh Space prints an invitation");

    run(
        &home,
        &credential,
        &["join", &invite, "--name", "Ada", "--password", PASSWORD],
    );
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
        &[
            "login",
            "--name",
            "Ada",
            "--server",
            &new_url,
            "--password",
            PASSWORD,
        ],
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
