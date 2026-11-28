//! Tests for the structured diagnostics a started process emits.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_neha");
const WAIT: Duration = Duration::from_secs(10);

/// A running Space whose stdout and stderr the test reads line by line.
struct Served {
    child: Child,
    stdout: Receiver<String>,
    stdout_lines: Vec<String>,
    stderr: Receiver<String>,
}

impl Served {
    /// Reads stdout until the Space prints its invitation.
    fn start(mut self) -> Self {
        self.stdout_lines = self
            .read_stdout_until(|line| line.starts_with("Invite: "))
            .expect("a fresh Space prints its invitation");
        self
    }

    fn read_stdout_until(&mut self, predicate: impl Fn(&str) -> bool) -> Option<Vec<String>> {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "stdout stopped before the expected line"
            );
            match self.stdout.recv_timeout(remaining) {
                Ok(line) => {
                    self.stdout_lines.push(line.clone());
                    if predicate(&line) {
                        return Some(std::mem::take(&mut self.stdout_lines));
                    }
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Sends SIGTERM and returns every stderr line up to the empty queue.
    #[cfg(unix)]
    fn stop(mut self) -> (Vec<String>, std::process::ExitStatus) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success(), "SIGTERM is delivered");

        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait works") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "serve did not stop after SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(5));
        };

        let mut lines = Vec::new();
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "stderr did not close after the stop");
            match self.stderr.recv_timeout(remaining) {
                Ok(line) => lines.push(line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    // The reader thread closes the channel when the child's
                    // stderr closes; the child is gone, so the channel is
                    // empty once the reader has caught up.
                    break;
                }
            }
        }

        (lines, status)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env("NEHA_HOME", home)
        .env_remove("NEHA_LOG")
        .env_remove("NEHA_LOG_FORMAT")
        .env_remove("NEHA_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}

fn spawn_serve(home: &Path, envs: &[(&str, &str)]) -> Served {
    let mut child = command(home, &["serve", "--port", "0"], envs)
        .spawn()
        .expect("neha serve starts");
    let stdout = child.stdout.take().expect("serve stdout");
    let stderr = child.stderr.take().expect("serve stderr");
    let stdout = line_reader(stdout);
    let stderr = line_reader(stderr);
    Served {
        child,
        stdout,
        stdout_lines: Vec::new(),
        stderr,
    }
}

fn line_reader(stream: impl std::io::Read + Send + 'static) -> Receiver<String> {
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

fn parse(lines: &[String]) -> Vec<serde_json::Value> {
    lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stderr line is not JSON: {line}: {error}"))
        })
        .collect()
}

fn event<'a>(events: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    events
        .iter()
        .find(|event| event["event"] == name)
        .unwrap_or_else(|| panic!("no `{name}` event in {events:?}"))
}

/// Runs a command that must exit on its own, and fails instead of hanging.
fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut child = command(home, args, envs).spawn().expect("neha starts");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let stdout = std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        let _ = std::io::copy(&mut stdout, &mut bytes);
        bytes
    });
    let stderr = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let _ = std::io::copy(&mut stderr, &mut bytes);
        bytes
    });

    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait works") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command did not exit; it probably started serving");
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: stderr.join().expect("stderr reader"),
    }
}

/// Sends one HTTP/1.1 GET and returns the whole response text.
fn http_get(address: &str, path: &str, token: &str) -> String {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(address).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    response
}

#[cfg(unix)]
#[test]
fn metrics_export_no_environment_resource_attributes() {
    let home = tempfile::tempdir().expect("home");
    let token = "metrics-canary-test-token-0123456789";
    let served = spawn_serve(
        home.path(),
        &[
            ("NEHA_LOG", "off"),
            ("NEHA_METRICS_TOKEN", token),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "private.customer=CANARY_RESOURCE",
            ),
        ],
    )
    .start();
    let address = served
        .stdout_lines
        .iter()
        .find_map(|line| line.strip_prefix("Neha is listening on http://"))
        .expect("the listening line")
        .to_owned();

    let response = http_get(&address, "/metrics", token);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        !response.contains("CANARY_RESOURCE"),
        "an environment resource attribute reached the scrape: {response}"
    );
    assert!(!response.contains("target_info"), "{response}");

    let stopped = served.stop();
    assert!(stopped.1.success());
}

#[cfg(unix)]
#[test]
fn serve_emits_json_start_and_stop_events_after_sigterm() {
    let home = tempfile::tempdir().expect("home");
    let served = spawn_serve(home.path(), &[("NEHA_LOG", "info")]).start();
    let invite = served
        .stdout_lines
        .iter()
        .find_map(|line| line.strip_prefix("Invite: "))
        .expect("the invitation line")
        .to_owned();
    let service = served
        .stdout_lines
        .iter()
        .find_map(|line| line.strip_prefix("Neha is listening on "))
        .expect("the listening line");
    assert!(
        service.starts_with("http://127.0.0.1:"),
        "stdout: {service}"
    );

    // The signal is installed before the Space publishes its address; stopping
    // immediately after the listening line exercises that window.
    let (stderr, status) = served.stop();
    assert!(status.success(), "a SIGTERM stop exits cleanly: {status}");
    let events = parse(&stderr);

    let start = event(&events, "space.start");
    assert_eq!(start["service"], "neha-space");
    assert_eq!(start["service_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(start["level"], "INFO");
    assert!(start["schema_version"].is_number());
    assert!(start["protocol_version"].is_number());
    assert!(start["timestamp"].is_string());
    assert!(start["addr"].is_string());

    let stop = event(&events, "space.stop");
    assert_eq!(stop["reason"], "sigterm");
    assert!(stop["uptime_s"].is_number());

    // The invitation secret is user-visible output, never a diagnostic field.
    let secret = invite.rsplit('#').next().unwrap_or(&invite);
    assert!(
        !stderr.iter().any(|line| line.contains(secret)),
        "the invitation secret appeared on stderr"
    );
}

#[cfg(unix)]
#[test]
fn off_silences_every_diagnostic() {
    let home = tempfile::tempdir().expect("home");
    let served = spawn_serve(home.path(), &[("NEHA_LOG", "off")]).start();
    let (stderr, status) = served.stop();
    assert!(status.success());
    assert!(stderr.is_empty(), "NEHA_LOG=off wrote: {stderr:?}");
}

#[test]
fn a_malformed_log_filter_fails_before_the_home_opens() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("never-created");
    let output = run(
        &home,
        &["serve", "--port", "0"],
        &[("NEHA_LOG", "[span=debug")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid NEHA_LOG"), "stderr: {stderr}");
    assert!(
        !home.exists(),
        "the home is not opened before a bad filter fails"
    );
}

#[test]
fn a_failed_start_writes_a_private_report() {
    let home = tempfile::tempdir().expect("home");
    std::fs::write(home.path().join("config.toml"), "not = [valid toml\n").expect("config");

    let output = run(
        home.path(),
        &["serve", "--port", "0"],
        &[("NEHA_LOG", "info")],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());

    let stderr = String::from_utf8_lossy(&output.stderr);
    let events: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let failed = event(&events, "space.start.failed");
    assert_eq!(failed["phase"], "config");
    assert_eq!(failed["error_kind"], "config");
    assert_eq!(
        failed["error_message"],
        "the configuration file is not valid TOML at line 1, column 18"
    );

    let path = PathBuf::from(failed["file"].as_str().expect("the report path"));
    assert!(path.starts_with(home.path().join("logs")));
    let report = std::fs::read_to_string(&path).expect("the report is readable");
    let report: serde_json::Value = serde_json::from_str(&report).expect("the report is JSON");
    assert_eq!(report["phase"], "config");
    assert!(report["warning"].is_string());
    assert_eq!(report["error"], failed["error_message"]);
    assert!(
        !report.to_string().contains("valid toml"),
        "the report does not carry the configuration text"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the report is private");
    }
}

#[test]
fn a_missing_browser_client_also_leaves_a_report() {
    let home = tempfile::tempdir().expect("home");
    let missing = home.path().join("missing-ui");
    let missing = missing.to_str().expect("a UTF-8 path");
    let output = run(
        home.path(),
        &["serve", "--port", "0", "--ui", missing],
        &[("NEHA_LOG", "info")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no index.html"),
        "the original CLI error is kept: {stderr}"
    );
    let events: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let failed = event(&events, "space.start.failed");
    assert_eq!(failed["phase"], "ui");
    assert_eq!(failed["error_kind"], "ui");
    assert_eq!(
        failed["error_message"],
        "the browser client has no index.html"
    );
    let path = PathBuf::from(failed["file"].as_str().expect("the report path"));
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the report")).expect("JSON");
    assert_eq!(report["phase"], "ui");
    assert_eq!(report["error"], failed["error_message"]);
}
