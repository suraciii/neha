//! The in-process runtime's integration tests: real files, real processes, real
//! sandboxes where the environment provides them.

use std::time::Duration;

use neha::file::MAX_FILE_BYTES;
use neha_computer::test_support::{
    LocalRuntime, MAX_SCAN_DEPTH, MAX_SCRIPT_ARGUMENT_BYTES, MAX_SCRIPT_ARGUMENTS,
    MAX_SCRIPT_NAME_BYTES, MAX_WRITE_BYTES, RuntimeError, file_runtime,
    file_runtime_without_hidden, find_bwrap, full_access_runtime, hidden, is_full_access,
    machine_mode_is_bwrap_less, machine_with_bwrap, open_write_root, probe_hosted,
    read_file_bounded, sandbox_args, validate_hosted_mounts, write_roots,
};
use neha_computer::{Action, ActionStatus, ByteError, ComputerRuntime};

fn omitted_in_text(text: &str) -> u64 {
    let start = text.find("[... ").expect("an omission marker") + 5;
    let end = text[start..].find(" bytes omitted").unwrap() + start;
    text[start..end].parse().unwrap()
}

fn runtime() -> (tempfile::TempDir, LocalRuntime) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = LocalRuntime::new(directory.path().join("workspace")).unwrap();

    (directory, runtime)
}

/// Returns whether the bubblewrap tests are switched on.
fn bwrap_enabled() -> bool {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return false;
    }
    true
}

/// Makes a FIFO without opening it.
#[cfg(unix)]
fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo exists");
    assert!(status.success(), "mkfifo failed");
}

/// Returns a real-machine runtime, or `None` when the bwrap tests are off.
///
/// `NEHA_TEST_BWRAP` opens them. Once it is set, a missing or broken
/// bubblewrap fails the test instead of skipping: the gate promises a
/// working sandbox.
fn machine_runtime() -> Option<(tempfile::TempDir, LocalRuntime)> {
    if !bwrap_enabled() {
        return None;
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

    Some((directory, runtime))
}

/// Returns a Hosted runtime, or `None` when the bwrap tests are off.
///
/// The workspace lives under the test's temporary directory, which the
/// sandbox does not mount; only the workspace itself appears at
/// `/workspace`.
fn hosted_runtime() -> Option<(tempfile::TempDir, LocalRuntime)> {
    if !bwrap_enabled() {
        return None;
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = LocalRuntime::hosted(directory.path().join("workspace"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a Hosted sandbox");

    Some((directory, runtime))
}

/// Returns a real-machine runtime that hides one directory outside `/tmp`,
/// or `None` when the bwrap tests are off.
///
/// `/var/tmp` stays visible in the sandbox, so a hidden root there proves
/// the sandbox hides it; a root under the replaced `/tmp` would prove
/// nothing.
fn machine_runtime_with_hidden() -> Option<(tempfile::TempDir, LocalRuntime, std::path::PathBuf)> {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return None;
    }
    let directory = outside_tempdir();
    let hidden = directory.path().join("computer");
    std::fs::create_dir(&hidden).unwrap();
    let runtime = LocalRuntime::machine_with_hidden(
        directory.path().join("bot"),
        vec![std::path::PathBuf::from(&hidden)],
    )
    .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

    Some((directory, runtime, hidden))
}

/// Returns a real-machine runtime that hides one file outside `/tmp`,
/// or `None` when the bwrap tests are off.
fn machine_runtime_with_hidden_file()
-> Option<(tempfile::TempDir, LocalRuntime, std::path::PathBuf)> {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return None;
    }
    let directory = outside_tempdir();
    let secret = directory.path().join("credential.json");
    std::fs::write(&secret, "session-secret-must-not-leak").unwrap();
    let runtime =
        LocalRuntime::machine_with_hidden(directory.path().join("bot"), vec![secret.clone()])
            .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox");

    Some((directory, runtime, secret))
}

/// A temporary directory outside `/tmp`, which the sandbox replaces with a
/// fresh tmpfs. A real shell test writes "outside" there.
fn outside_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("neha-outside-")
        .tempdir_in("/var/tmp")
        .expect("the fence test needs a writable /var/tmp outside /tmp")
}

#[tokio::test]
async fn writes_reads_and_lists_a_file() {
    let (_dir, runtime) = runtime();
    let written = runtime
        .execute(Action::WriteFile {
            path: "hello.txt".to_owned(),
            content: "hello neha".to_owned(),
        })
        .await;
    let read = runtime
        .execute(Action::ReadFile {
            path: "hello.txt".to_owned(),
        })
        .await;
    let listed = runtime
        .execute(Action::ListFiles {
            path: ".".to_owned(),
        })
        .await;

    assert_eq!(written.status, ActionStatus::Succeeded);
    assert_eq!(read.output.text, "hello neha");
    assert!(
        listed.output.text.contains("hello.txt"),
        "{}",
        listed.output.text
    );
    assert!(runtime.workspace().join("hello.txt").is_file());
}

#[tokio::test]
async fn refuses_paths_outside_the_workspace() {
    let (directory, runtime) = runtime();
    std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
    for path in ["../outside.txt", "/etc/passwd"] {
        let result = runtime
            .execute(Action::ReadFile {
                path: path.to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed, "{path}");
        assert!(
            result.output.text.contains("escapes"),
            "{path}: {}",
            result.output.text
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_symlink_outside_the_workspace() {
    let (directory, runtime) = runtime();
    std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();
    let result = runtime
        .execute(Action::ReadFile {
            path: "escape/outside.txt".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed);
    assert!(result.output.text.contains("symlink"));
}

#[tokio::test]
async fn refuses_to_write_through_a_dangling_symlink() {
    let (directory, runtime) = runtime();
    let outside = directory.path().join("created-outside.txt");
    std::os::unix::fs::symlink(&outside, runtime.workspace().join("escape")).unwrap();
    let result = runtime
        .execute(Action::WriteFile {
            path: "escape".to_owned(),
            content: "pwned".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed);
    assert!(result.output.text.contains("symlink"));
    assert!(!outside.exists(), "the write followed a dangling symlink");
}

#[tokio::test]
async fn bytes_are_written_and_read_with_parent_directories() {
    let (_dir, runtime) = runtime();
    let bytes = vec![0_u8, 1, 2, 255];
    runtime
        .write_bytes("out/report.bin", bytes.clone())
        .await
        .unwrap();

    let read = runtime.read_bytes("out/report.bin").await.unwrap();
    assert_eq!(read, bytes);
    assert!(runtime.workspace().join("out").is_dir());

    // An absolute path inside the workspace names the same file.
    let absolute = runtime.workspace().join("out/report.bin");
    let read = runtime
        .read_bytes(&absolute.to_string_lossy())
        .await
        .unwrap();
    assert_eq!(read, bytes);
}

#[tokio::test]
async fn byte_transfers_stay_inside_the_workspace() {
    let (directory, runtime) = runtime();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "secret").unwrap();
    for path in ["../outside.txt", &outside.to_string_lossy()] {
        assert!(runtime.read_bytes(path).await.is_err(), "read {path}");
        assert!(
            runtime.write_bytes(path, b"pwned".to_vec()).await.is_err(),
            "write {path}"
        );
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
}

#[cfg(unix)]
#[tokio::test]
async fn byte_transfers_refuse_a_symlink() {
    let (directory, runtime) = runtime();
    std::fs::write(directory.path().join("outside.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();
    let dangling = runtime.workspace().join("dangling");
    let created_outside = directory.path().join("created-outside.txt");
    std::os::unix::fs::symlink(&created_outside, &dangling).unwrap();

    for path in ["escape/outside.txt", "dangling"] {
        let error = runtime.read_bytes(path).await.unwrap_err().to_string();
        assert!(error.contains("symlink"), "{path}: {error}");
    }
    for path in ["escape/created-by-link.txt", "dangling"] {
        let error = runtime
            .write_bytes(path, b"x".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink"), "{path}: {error}");
    }
    assert!(!directory.path().join("created-by-link.txt").exists());
    assert!(!created_outside.exists());
}

#[tokio::test]
async fn read_bytes_refuses_what_is_not_a_file() {
    let (_dir, runtime) = runtime();
    std::fs::create_dir(runtime.workspace().join("dir")).unwrap();

    let error = runtime.read_bytes("dir").await.unwrap_err().to_string();
    assert!(error.contains("not a file"), "{error}");
    let error = runtime
        .read_bytes("missing.txt")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot read"), "{error}");
}

#[tokio::test]
async fn byte_transfers_refuse_more_than_the_file_limit() {
    let (_dir, runtime) = runtime();
    for (name, length) in [
        ("at-limit.bin", MAX_FILE_BYTES),
        ("over.bin", MAX_FILE_BYTES + 1),
    ] {
        std::fs::File::create(runtime.workspace().join(name))
            .unwrap()
            .set_len(length)
            .unwrap();
    }

    let error = runtime
        .read_bytes("over.bin")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("larger than"), "{error}");
    let error = runtime
        .write_bytes("over.bin", vec![0; MAX_FILE_BYTES as usize + 1])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("larger than"), "{error}");
    assert_eq!(
        std::fs::metadata(runtime.workspace().join("over.bin"))
            .unwrap()
            .len(),
        MAX_FILE_BYTES + 1,
        "a refused write leaves the file alone"
    );

    let at_limit = vec![0_u8; MAX_FILE_BYTES as usize];
    runtime
        .write_bytes("writable.bin", at_limit.clone())
        .await
        .unwrap();
    assert_eq!(runtime.read_bytes("at-limit.bin").await.unwrap(), at_limit);
}

#[tokio::test]
async fn runs_a_shell_command_and_reports_its_output() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::Shell {
            command: "printf 42".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded);
    assert!(result.output.text.contains("42"), "{}", result.output.text);
}

#[tokio::test]
async fn the_shell_does_not_see_the_space_environment() {
    let (_dir, runtime) = runtime();
    // The Space process holds a Provider key in this test; the shell
    // must not inherit it.
    unsafe { std::env::set_var("NEHA_PROVIDER_KEY", "sk-must-not-leak") };
    let result = runtime
        .execute(Action::Shell {
            command: "printenv NEHA_PROVIDER_KEY || true; echo done".to_owned(),
        })
        .await;
    unsafe { std::env::remove_var("NEHA_PROVIDER_KEY") };

    assert!(!result.output.text.contains("sk-must-not-leak"));
    assert!(result.output.text.contains("done"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_shell_that_exits_does_not_leave_its_background_child_holding_the_pipes() {
    let (_dir, runtime) = runtime();
    let runtime = runtime.with_timeout(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let result = runtime
        .execute(Action::Shell {
            command: "sleep 30 & echo started".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.output.text.contains("started"));
}

#[tokio::test]
async fn timeout_kills_a_grandchild_too() {
    let (_dir, runtime) = runtime();
    let runtime = runtime.with_timeout(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let result = runtime
        .execute(Action::Shell {
            command: "sleep 30 & wait".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn stdout_and_stderr_share_one_output_budget() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::Shell {
            command: "head -c 30000 /dev/zero | tr '\\0' 'a'; \
                      head -c 30000 /dev/zero | tr '\\0' 'b' >&2"
                .to_owned(),
        })
        .await;

    assert!(result.output.truncated);
    assert_eq!(result.output.omitted_bytes, 60_000 - 32_768);
    assert_eq!(
        omitted_in_text(&result.output.text),
        result.output.omitted_bytes
    );
    assert!(
        result.output.text.len() < 40_000,
        "the combined result is {} bytes",
        result.output.text.len()
    );
}

#[tokio::test]
async fn a_result_slightly_over_the_budget_names_the_true_omitted_count() {
    let (_dir, runtime) = runtime();
    for (bytes, omitted) in [(33_000_u64, 232_u64), (40_000, 7_232)] {
        let result = runtime
            .execute(Action::Shell {
                command: format!("head -c {bytes} /dev/zero | tr '\\0' 'a'"),
            })
            .await;

        assert!(result.output.truncated);
        assert_eq!(result.output.omitted_bytes, omitted, "{bytes} bytes");
        assert_eq!(
            omitted_in_text(&result.output.text),
            omitted,
            "the marker must name the true omitted count"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_shell_output_over_the_cap_keeps_its_real_tail_and_count() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::Shell {
            command: "yes y | head -c 9000000; printf 'END'".to_owned(),
        })
        .await;

    assert!(result.output.truncated);
    assert_eq!(result.output.omitted_bytes, 9_000_003 - 32_768);
    assert_eq!(
        omitted_in_text(&result.output.text),
        result.output.omitted_bytes
    );
    assert!(result.output.text.contains("END"));
}

#[tokio::test]
async fn a_large_file_keeps_a_head_a_tail_and_the_true_omitted_count() {
    let (_dir, runtime) = runtime();
    let path = runtime.workspace().join("large.txt");
    let mut content = "a".repeat(9 * 1024 * 1024);
    content.push_str("TAIL");
    std::fs::write(&path, &content).unwrap();
    let result = runtime
        .execute(Action::ReadFile {
            path: "large.txt".to_owned(),
        })
        .await;

    assert!(result.output.truncated);
    assert!(result.output.omitted_bytes > 8_000_000);
    assert!(result.output.text.ends_with("TAIL"));
}

#[tokio::test]
async fn a_command_that_runs_too_long_is_killed() {
    let (_dir, runtime) = runtime();
    let runtime = runtime.with_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    let result = runtime
        .execute(Action::Shell {
            command: "sleep 30".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.output.text.contains("time limit"));
}

#[tokio::test]
async fn large_output_keeps_a_head_and_a_tail() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::Shell {
            command: "head -c 40000 /dev/zero | tr '\\0' 'a'".to_owned(),
        })
        .await;

    assert!(result.output.truncated);
    assert!(result.output.omitted_bytes > 0);
    assert!(result.output.text.contains("bytes omitted"));
}

#[tokio::test]
async fn cancel_in_flight_kills_the_running_shell() {
    let (_dir, runtime) = runtime();
    let runtime = std::sync::Arc::new(runtime.with_timeout(Duration::from_secs(30)));
    assert!(!runtime.cancel_in_flight(), "nothing runs yet");

    let running = tokio::spawn({
        let runtime = std::sync::Arc::clone(&runtime);
        async move {
            runtime
                .execute(Action::Shell {
                    command: "sleep 30".to_owned(),
                })
                .await
        }
    });
    // The shell needs a moment to exist before it can be cancelled.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(runtime.cancel_in_flight(), "the running shell is cancelled");

    let started = std::time::Instant::now();
    let result = running.await.unwrap();
    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = runtime.cancel_in_flight();
}

#[cfg(unix)]
#[test]
fn a_machine_without_a_working_bubblewrap_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    use RuntimeError;

    let directory = tempfile::tempdir().unwrap();
    let error = machine_with_bwrap(directory.path().join("bot"), None, Vec::new())
        .err()
        .expect("a missing bwrap must refuse the machine");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");

    let fake = directory.path().join("bwrap");
    std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = machine_with_bwrap(directory.path().join("bot2"), Some(fake), Vec::new())
        .err()
        .expect("a bwrap that cannot start a sandbox must refuse the machine");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");
}

#[tokio::test]
async fn a_machine_tool_writes_and_reads_inside_the_bot_folder() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let folder = runtime.workspace().to_path_buf();
    let written = runtime
        .execute(Action::WriteFile {
            path: "notes/hello.txt".to_owned(),
            content: "hello neha".to_owned(),
        })
        .await;

    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
        "hello neha"
    );

    // Overwriting truncates: a shorter write replaces the whole file.
    let written = runtime
        .execute(Action::WriteFile {
            path: "notes/hello.txt".to_owned(),
            content: "hi".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
        "hi"
    );

    // `.` is allowed and creates the parent directories on the way.
    let written = runtime
        .execute(Action::WriteFile {
            path: "./top.txt".to_owned(),
            content: "top".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(folder.join("top.txt")).unwrap(),
        "top"
    );

    // An absolute path reads the machine; writing one is allowed when it
    // names the Bot's own folder.
    let absolute = folder.join("notes/hello.txt");
    let read = runtime
        .execute(Action::ReadFile {
            path: absolute.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "hi");
    let written = runtime
        .execute(Action::WriteFile {
            path: absolute.to_string_lossy().into_owned(),
            content: "absolute".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes/hello.txt")).unwrap(),
        "absolute"
    );

    let listed = runtime
        .execute(Action::ListFiles {
            path: ".".to_owned(),
        })
        .await;
    assert!(
        listed.output.text.contains("notes"),
        "{}",
        listed.output.text
    );
    assert!(
        listed.output.text.contains("top.txt"),
        "{}",
        listed.output.text
    );
}

#[tokio::test]
async fn a_machine_tool_refuses_to_write_outside_the_bot_folder() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let outside = directory.path().join("outside.txt");
    for path in ["../outside.txt", "nested/../../outside.txt"] {
        let result = runtime
            .execute(Action::WriteFile {
                path: path.to_owned(),
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Failed, "{path}");
        assert!(
            result.output.text.contains("`..`"),
            "{path}: {}",
            result.output.text
        );
    }
    let result = runtime
        .execute(Action::WriteFile {
            path: outside.to_string_lossy().into_owned(),
            content: "pwned".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed);
    assert!(
        result.output.text.contains("absolute"),
        "{}",
        result.output.text
    );
    assert!(!outside.exists(), "a refused write leaves no file");
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_tool_refuses_to_write_through_a_symlink() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(directory.path(), runtime.workspace().join("escape")).unwrap();

    let result = runtime
        .execute(Action::WriteFile {
            path: "escape/created.txt".to_owned(),
            content: "pwned".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed);
    assert!(
        result.output.text.contains("symlink"),
        "{}",
        result.output.text
    );
    assert!(!directory.path().join("created.txt").exists());

    // A symlink as the target is never written through, dangling or not.
    let dangling = runtime.workspace().join("dangling");
    std::os::unix::fs::symlink(runtime.workspace().join("created-by-link.txt"), &dangling).unwrap();
    let result = runtime
        .execute(Action::WriteFile {
            path: "dangling".to_owned(),
            content: "pwned".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed);
    assert!(
        result.output.text.contains("symlink"),
        "{}",
        result.output.text
    );
    assert!(!runtime.workspace().join("created-by-link.txt").exists());

    let error = runtime
        .write_bytes("escape/created.bin", b"pwned".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"), "{error}");
    assert!(!directory.path().join("created.bin").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_tool_refuses_to_write_a_hard_link() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "host content").unwrap();
    let linked = runtime.workspace().join("linked.txt");
    std::fs::hard_link(&outside, &linked).unwrap();

    let result = runtime
        .execute(Action::WriteFile {
            path: "linked.txt".to_owned(),
            content: "pwned".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed);
    assert!(
        result.output.text.contains("hard link"),
        "{}",
        result.output.text
    );
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "host content",
        "the host inode was written"
    );

    let error = runtime
        .write_bytes("linked.txt", b"pwned".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("hard link"), "{error}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host content");
    // The link is still there: a refusal changes nothing.
    assert_eq!(std::fs::metadata(&linked).unwrap().len(), 12);
}

#[tokio::test]
async fn a_machine_tool_reads_the_whole_machine() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "readable").unwrap();
    let read = runtime
        .execute(Action::ReadFile {
            path: outside.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "readable");

    let read = runtime
        .execute(Action::ReadFile {
            path: "/etc/hostname".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert!(!read.output.text.trim().is_empty());

    let missing = runtime
        .execute(Action::ReadFile {
            path: directory
                .path()
                .join("missing.txt")
                .to_string_lossy()
                .into_owned(),
        })
        .await;
    assert_eq!(missing.status, ActionStatus::Failed);
    assert!(
        missing.output.text.contains("cannot read"),
        "{}",
        missing.output.text
    );
}

#[tokio::test]
async fn a_machine_byte_transfer_reads_anywhere_and_writes_inside_the_folder() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let folder = runtime.workspace().to_path_buf();
    let outside = directory.path().join("outside.bin");
    std::fs::write(&outside, b"take me").unwrap();

    let read = runtime
        .read_bytes(&outside.to_string_lossy())
        .await
        .unwrap();
    assert_eq!(read, b"take me");

    runtime
        .write_bytes("deliver/report.bin", vec![0, 1, 2])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(folder.join("deliver/report.bin")).unwrap(),
        vec![0, 1, 2]
    );

    let error = runtime
        .write_bytes("../pwned.bin", b"x".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("`..`"), "{error}");
    assert!(!directory.path().join("pwned.bin").exists());
    let error = runtime
        .write_bytes(&outside.to_string_lossy(), b"x".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("absolute"), "{error}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"take me");
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_file_tool_refuses_a_hidden_root() {
    let directory = tempfile::tempdir().unwrap();
    let hidden = directory.path().join("computer");
    std::fs::create_dir(&hidden).unwrap();
    let credential = hidden.join("computer.json");
    std::fs::write(&credential, "channel-secret-must-not-leak").unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let visible = directory.path().join("visible.txt");
    std::fs::write(&visible, "readable").unwrap();
    let runtime = file_runtime(&root, &hidden);

    // A hidden root is refused by both read paths, and its bytes never
    // come back in the refusal.
    let read = runtime
        .execute(Action::ReadFile {
            path: credential.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
    assert!(
        read.output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        read.output.text
    );
    assert!(!read.output.text.contains("channel-secret-must-not-leak"));
    let error = runtime
        .read_bytes(&credential.to_string_lossy())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the path is hidden from this Computer"),
        "{error}"
    );
    assert!(!error.contains("channel-secret-must-not-leak"));
    let listed = runtime
        .execute(Action::ListFiles {
            path: hidden.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(listed.status, ActionStatus::Failed, "{listed:?}");
    assert!(
        listed
            .output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        listed.output.text
    );
    assert!(!listed.output.text.contains("computer.json"));

    // A normal machine file next to the hidden root is still readable.
    let read = runtime
        .execute(Action::ReadFile {
            path: visible.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "readable");

    // A symlink in the Bot's folder is caught after the open: the check
    // follows the opened descriptor, not the path the caller typed.
    std::os::unix::fs::symlink(&hidden, root.join("leak")).unwrap();
    let read = runtime
        .execute(Action::ReadFile {
            path: "leak/computer.json".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
    assert!(
        read.output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        read.output.text
    );
    assert!(!read.output.text.contains("channel-secret-must-not-leak"));
}

#[tokio::test]
async fn a_machine_write_refuses_a_hidden_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    // A write can only reach a hidden root through a misconfigured folder;
    // that is what the daemon refuses, and the write refuses it as well.
    let runtime = file_runtime(&root, &root);

    let written = runtime
        .execute(Action::WriteFile {
            path: "notes.txt".to_owned(),
            content: "pwned".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Failed, "{written:?}");
    assert!(
        written
            .output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        written.output.text
    );
    assert!(!root.join("notes.txt").exists());
    let error = runtime
        .write_bytes("notes.bin", b"pwned".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the path is hidden from this Computer"),
        "{error}"
    );
    assert!(!root.join("notes.bin").exists());
}

#[tokio::test]
async fn a_real_shell_cannot_read_a_hidden_root() {
    let Some((directory, runtime, hidden)) = machine_runtime_with_hidden() else {
        return;
    };
    let credential = hidden.join("computer.json");
    std::fs::write(&credential, "channel-secret-must-not-leak").unwrap();
    let visible = directory.path().join("visible.txt");
    std::fs::write(&visible, "readable").unwrap();

    let hidden_read = runtime
        .execute(Action::Shell {
            command: format!("cat {}", credential.display()),
        })
        .await;
    assert_eq!(hidden_read.status, ActionStatus::Failed, "{hidden_read:?}");
    assert!(
        !hidden_read
            .output
            .text
            .contains("channel-secret-must-not-leak"),
        "the sandbox read the credential: {}",
        hidden_read.output.text
    );

    // The shell still reads the machine outside the hidden root.
    let visible_read = runtime
        .execute(Action::Shell {
            command: format!("cat {}", visible.display()),
        })
        .await;
    assert_eq!(
        visible_read.status,
        ActionStatus::Succeeded,
        "{visible_read:?}"
    );
    assert!(visible_read.output.text.contains("readable"));
}

#[tokio::test]
async fn a_real_shell_starts_in_the_bot_folder() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let result = runtime
        .execute(Action::Shell {
            command: "pwd".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        result
            .output
            .text
            .contains(&runtime.workspace().to_string_lossy().into_owned()),
        "{}",
        result.output.text
    );
}

#[tokio::test]
async fn a_real_shell_writes_inside_the_bot_folder() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let folder = runtime.workspace().to_path_buf();
    let result = runtime
        .execute(Action::Shell {
            command: "printf hi > inside.txt && printf hi > \"$PWD/absolute.txt\"".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(folder.join("inside.txt")).unwrap(),
        "hi"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("absolute.txt")).unwrap(),
        "hi"
    );
}

#[tokio::test]
async fn a_real_shell_cannot_write_outside_the_bot_folder() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let outside = outside_tempdir();
    let sink = outside.path().join("pwned.txt");
    let result = runtime
        .execute(Action::Shell {
            command: format!("printf hi > {}", sink.display()),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(!sink.exists(), "the sandbox wrote outside the folder");
}

#[tokio::test]
async fn a_real_shell_reads_the_whole_machine() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let result = runtime
        .execute(Action::Shell {
            command: "cat /etc/hostname".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(!result.output.text.trim().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_cannot_escape_through_a_symlink() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let outside = outside_tempdir();
    let sink = outside.path().join("pwned.txt");
    std::os::unix::fs::symlink(outside.path(), runtime.workspace().join("escape")).unwrap();
    let result = runtime
        .execute(Action::Shell {
            command: "printf hi > escape/pwned.txt".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(!sink.exists(), "the sandbox escaped through a symlink");
}

#[tokio::test]
async fn a_real_shell_has_its_own_dev_shm() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    if !std::path::Path::new("/dev/shm").is_dir() {
        eprintln!("skipping: this machine has no /dev/shm");
        return;
    }
    let id = std::process::id();
    let host = format!("/dev/shm/neha-host-{id}");
    let sandbox = format!("/dev/shm/neha-sandbox-{id}");
    std::fs::write(&host, "host bytes").unwrap();
    let result = runtime
        .execute(Action::Shell {
            command: format!("cat {host} 2>/dev/null; printf sandbox > {sandbox}"),
        })
        .await;
    let _ = std::fs::remove_file(&host);

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        !result.output.text.contains("host bytes"),
        "the host /dev/shm is visible in the sandbox: {}",
        result.output.text
    );
    assert!(
        !std::path::Path::new(&sandbox).exists(),
        "the sandbox wrote the host's /dev/shm"
    );
}

#[tokio::test]
async fn a_real_shell_has_its_own_pid_namespace() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let host = std::fs::read_link("/proc/self/ns/pid").expect("the host has a pid namespace");
    let result = runtime
        .execute(Action::Shell {
            command: "readlink /proc/self/ns/pid".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        !result
            .output
            .text
            .contains(&host.to_string_lossy().into_owned()),
        "the sandbox shares the host's PID namespace: {}",
        result.output.text
    );
}

#[tokio::test]
async fn a_real_shell_cannot_signal_a_host_process() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let mut host = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("the host spawns a sleep");
    let pid = host.id();
    let result = runtime
        .execute(Action::Shell {
            command: format!("kill -0 {pid}"),
        })
        .await;
    let _ = host.kill();
    let _ = host.wait();

    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(
        result.output.text.contains("No such process")
            || result.output.text.contains("not permitted"),
        "{}",
        result.output.text
    );
}

#[tokio::test]
async fn a_real_shell_that_runs_too_long_is_killed() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let runtime = runtime.with_timeout(Duration::from_millis(500));
    let started = std::time::Instant::now();
    let result = runtime
        .execute(Action::Shell {
            command: "sleep 30".to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::TimedOut, "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn a_bot_folder_that_is_a_symlink_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, directory.path().join("bot")).unwrap();

    let error = machine_with_bwrap(directory.path().join("bot"), None, Vec::new())
        .err()
        .expect("a symlinked Bot folder is refused");
    assert!(matches!(error, RuntimeError::UnsafeFolder(_)), "{error}");
    assert!(error.to_string().contains("symlink"), "{error}");
    assert!(error.to_string().contains("clean"), "{error}");

    std::fs::write(directory.path().join("file"), "not a directory").unwrap();
    let error = machine_with_bwrap(directory.path().join("file"), None, Vec::new())
        .err()
        .expect("a file that is not a directory is refused");
    assert!(error.to_string().contains("not a directory"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_workspace_root_opens_without_following_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let root = LocalRuntime::open_workspace(&workspace).unwrap();
    assert!(workspace.is_dir());
    drop(root);

    let real = directory.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, directory.path().join("linked")).unwrap();
    assert!(
        LocalRuntime::open_workspace(directory.path().join("linked")).is_err(),
        "a symlinked workspace root is not followed"
    );
}

#[cfg(unix)]
#[test]
fn a_folder_holding_a_hard_link_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let bot = directory.path().join("bot");
    std::fs::create_dir_all(bot.join("sub")).unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "host").unwrap();
    std::fs::hard_link(&outside, bot.join("sub/inside.txt")).unwrap();

    let error = machine_with_bwrap(&bot, None, Vec::new())
        .err()
        .expect("a folder holding a hard link is refused");
    assert!(error.to_string().contains("hard link"), "{error}");
    assert!(error.to_string().contains("clean"), "{error}");
    assert!(
        error.to_string().contains("~/Neha/<bot-id>"),
        "the refusal tells the user what to clean: {error}"
    );
}

#[cfg(unix)]
#[test]
fn a_folder_holding_a_symlink_or_a_special_file_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let bot = directory.path().join("bot");
    std::fs::create_dir(&bot).unwrap();
    std::os::unix::fs::symlink(directory.path().join("elsewhere"), bot.join("link")).unwrap();
    let error = machine_with_bwrap(&bot, None, Vec::new())
        .err()
        .expect("a folder holding a symlink is refused");
    assert!(error.to_string().contains("symlink"), "{error}");

    std::fs::remove_file(bot.join("link")).unwrap();
    make_fifo(&bot.join("pipe"));
    let error = machine_with_bwrap(&bot, None, Vec::new())
        .err()
        .expect("a folder holding a FIFO is refused");
    assert!(error.to_string().contains("special file"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_folder_nested_past_the_scan_limit_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let bot = directory.path().join("bot");
    let mut deep = bot.clone();
    for _ in 0..=MAX_SCAN_DEPTH {
        deep = deep.join("d");
    }
    std::fs::create_dir_all(&deep).unwrap();

    let error = machine_with_bwrap(&bot, None, Vec::new())
        .err()
        .expect("a folder nested past the scan limit is refused");
    assert!(error.to_string().contains("nested"), "{error}");
}

#[test]
fn a_hidden_root_inside_the_bot_folder_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let bot = directory.path().join("bot");
    let hidden = bot.join("config");
    std::fs::create_dir_all(&hidden).unwrap();

    let error = machine_with_bwrap(&bot, None, vec![hidden])
        .err()
        .expect("a configuration directory inside the Bot folder is refused");
    assert!(
        error.to_string().contains("configuration directory"),
        "{error}"
    );
}

#[test]
fn a_bot_folder_inside_a_hidden_root_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let hidden = directory.path().join("config");
    let bot = hidden.join("bot");
    std::fs::create_dir_all(&bot).unwrap();

    let error = machine_with_bwrap(&bot, None, vec![hidden])
        .err()
        .expect("a Bot folder inside the configuration directory is refused");
    assert!(error.to_string().contains("hidden directory"), "{error}");
}

#[test]
fn a_machine_is_probed_before_it_serves() {
    if !bwrap_enabled() {
        return;
    }
    LocalRuntime::probe_sandbox().expect("bubblewrap starts one empty sandbox");
}

#[tokio::test]
async fn a_workspace_root_descriptor_opens_one_bot_folder() {
    if !bwrap_enabled() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let root = LocalRuntime::open_workspace(&workspace).unwrap();
    let runtime = LocalRuntime::machine_at(&root, "bot-id", Vec::new())
        .expect("the workspace root opens a Bot folder");
    assert!(workspace.join("bot-id").is_dir());

    let written = runtime
        .execute(Action::WriteFile {
            path: "hello.txt".to_owned(),
            content: "hi".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("bot-id/hello.txt")).unwrap(),
        "hi"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_special_file_does_not_block_a_read_or_a_write() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let fifo = runtime.workspace().join("pipe");
    make_fifo(&fifo);

    let read = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.execute(Action::ReadFile {
            path: "pipe".to_owned(),
        }),
    )
    .await
    .expect("a FIFO read must not block");
    assert_eq!(read.status, ActionStatus::Failed);
    assert!(
        read.output.text.contains("not a regular file"),
        "{}",
        read.output.text
    );

    let write = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.execute(Action::WriteFile {
            path: "pipe".to_owned(),
            content: "pwned".to_owned(),
        }),
    )
    .await
    .expect("a FIFO write must not block");
    assert_eq!(write.status, ActionStatus::Failed);
    assert!(
        write.output.text.contains("not a regular file"),
        "{}",
        write.output.text
    );

    let bytes = tokio::time::timeout(Duration::from_secs(5), runtime.read_bytes("pipe"))
        .await
        .expect("a FIFO byte read must not block");
    assert!(matches!(bytes, Err(ByteError::Failed(_))), "{bytes:?}");
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.write_bytes("pipe", b"pwned".to_vec()),
    )
    .await
    .expect("a FIFO byte write must not block");
    assert!(matches!(bytes, Err(ByteError::Failed(_))), "{bytes:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_read_follows_a_symlink_to_a_machine_file() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "readable through a link").unwrap();
    std::os::unix::fs::symlink(&outside, runtime.workspace().join("link.txt")).unwrap();

    let result = runtime
        .execute(Action::ReadFile {
            path: "link.txt".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(result.output.text, "readable through a link");

    let bytes = runtime.read_bytes("link.txt").await.unwrap();
    assert_eq!(bytes, b"readable through a link");
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_read_through_a_symlink_still_refuses_a_hidden_root() {
    let Some((_directory, runtime, hidden)) = machine_runtime_with_hidden() else {
        return;
    };
    let secret = hidden.join("computer.json");
    std::fs::write(&secret, "credential").unwrap();
    std::os::unix::fs::symlink(&secret, runtime.workspace().join("link.json")).unwrap();

    let result = runtime
        .execute(Action::ReadFile {
            path: "link.json".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(
        result.output.text.contains("hidden"),
        "{}",
        result.output.text
    );

    let bytes = runtime.read_bytes("link.json").await;
    assert!(
        bytes
            .as_ref()
            .is_err_and(|error| error.to_string().contains("hidden")),
        "{bytes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_inherits_only_its_own_bot_folder() {
    let Some((_dir_a, runtime_a)) = machine_runtime() else {
        return;
    };
    let Some((_dir_b, runtime_b)) = machine_runtime() else {
        return;
    };
    let own = runtime_a.workspace().to_path_buf();
    let other = runtime_b.workspace().to_path_buf();
    // Files a Bot folder can have; the other runtime is only cached, never
    // used, so its descriptor must still be close-on-exec.
    let result = runtime_a
        .execute(Action::Shell {
            command: "ls -l /proc/self/fd".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    let text = result.output.text;
    for line in text.lines() {
        let Some(target) = line.split(" -> ").nth(1) else {
            continue;
        };
        let target = target.trim();
        // Only the sandbox's own stdio and /proc enumeration handles may
        // be open; no host directory, and no mount source descriptor.
        assert!(
            target == "/dev/null" || target.starts_with("pipe:") || target.starts_with("/proc/"),
            "the user command inherited a descriptor: {line}"
        );
    }
    assert!(
        !text.contains(own.to_string_lossy().as_ref()),
        "the Bot's own folder descriptor leaked into the command: {text}"
    );
    assert!(
        !text.contains(other.to_string_lossy().as_ref()),
        "another Bot's folder leaked into this shell: {text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_spawned_again_mounts_the_same_folder() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let written = runtime
        .execute(Action::WriteFile {
            path: "marker.txt".to_owned(),
            content: "twice".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    for _ in 0..2 {
        let result = runtime
            .execute(Action::Shell {
                command: "cat marker.txt".to_owned(),
            })
            .await;
        assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
        assert!(
            result.output.text.contains("twice"),
            "the folder is still mounted on this spawn: {}",
            result.output.text
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_returns_the_command_exit_code_and_stdout() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let result = runtime
        .execute(Action::Shell {
            command: "echo hi; exit 7".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(
        result.output.text.contains("exit code: 7"),
        "{}",
        result.output.text
    );
    assert!(result.output.text.contains("hi"), "{}", result.output.text);
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_mounts_the_opened_bot_folder_after_its_path_moves() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let written = runtime
        .execute(Action::WriteFile {
            path: "marker.txt".to_owned(),
            content: "original".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    let folder = runtime.workspace().to_path_buf();
    let moved = directory.path().join("moved");
    std::fs::rename(&folder, &moved).unwrap();
    std::fs::create_dir(&folder).unwrap();

    let result = runtime
        .execute(Action::Shell {
            command: "cat marker.txt".to_owned(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        result.output.text.contains("original"),
        "the shell mounts the opened folder, not the new path: {}",
        result.output.text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_does_not_follow_a_replaced_bot_folder_path() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let written = runtime
        .execute(Action::WriteFile {
            path: "marker.txt".to_owned(),
            content: "original".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    let folder = runtime.workspace().to_path_buf();
    let moved = directory.path().join("moved");
    std::fs::rename(&folder, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &folder).unwrap();

    let result = runtime
        .execute(Action::Shell {
            command: "cat marker.txt".to_owned(),
        })
        .await;
    match result.status {
        // The mounted object is still the opened folder, whose marker is
        // only reachable through the old root.
        ActionStatus::Succeeded => assert!(
            result.output.text.contains("original"),
            "{}",
            result.output.text
        ),
        // A replacement the sandbox cannot mount is refused, not followed.
        ActionStatus::Failed => {}
        other => panic!("unexpected status {other:?}: {}", result.output.text),
    }
}

#[test]
fn a_byte_read_stops_a_file_that_grows_past_the_limit() {
    use std::io::Read as _;

    // A file that reported a small length and then grew: the reader keeps
    // producing bytes past the limit.
    let growing = std::io::repeat(0u8).take(MAX_FILE_BYTES + 1);
    let error = read_file_bounded(growing, MAX_FILE_BYTES, "growing.bin").unwrap_err();
    assert!(error.to_string().contains("larger than"), "{error}");
}

#[tokio::test]
async fn a_byte_read_of_a_file_that_grows_never_returns_more_than_one_file() {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_dir, runtime) = runtime();
    let path = runtime.workspace().join("growing.bin");
    std::fs::write(&path, "start").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) else {
                return;
            };
            let chunk = vec![b'x'; 256 * 1024];
            let mut written = 0u64;
            while !stop.load(Ordering::Relaxed) && written <= MAX_FILE_BYTES + 2 * 1024 * 1024 {
                if file.write_all(&chunk).is_err() {
                    break;
                }
                written += chunk.len() as u64;
            }
        })
    };
    let mut refused = false;
    for _ in 0..500 {
        match runtime.read_bytes("growing.bin").await {
            Ok(bytes) => {
                assert!(
                    bytes.len() as u64 <= MAX_FILE_BYTES,
                    "a byte read returned more than one File"
                );
            }
            Err(error) => {
                assert!(error.to_string().contains("larger than"), "{error}");
                refused = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(refused, "a file that grew past the limit is refused");
}

#[cfg(unix)]
#[test]
fn a_hidden_file_is_mounted_as_dev_null() {
    let directory = tempfile::tempdir().unwrap();
    let hidden_file = directory.path().join("credential.json");
    std::fs::write(&hidden_file, "session-secret").unwrap();
    let hidden_directory = directory.path().join("computer");
    std::fs::create_dir(&hidden_directory).unwrap();
    let folder = directory.path().join("bot");
    std::fs::create_dir(&folder).unwrap();

    let arguments = sandbox_args(
        folder.as_os_str(),
        &folder,
        &folder,
        &[hidden_file.clone(), hidden_directory.clone()],
        &[],
    );
    let shown: Vec<String> = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let file = hidden_file.to_string_lossy().into_owned();
    let file_at = shown
        .iter()
        .position(|argument| argument == &file)
        .expect("the hidden file is mounted");
    assert_eq!(
        &shown[file_at - 2..file_at],
        ["--ro-bind", "/dev/null"],
        "a hidden file is replaced by /dev/null: {shown:?}"
    );
    let directory = hidden_directory.to_string_lossy().into_owned();
    let directory_at = shown
        .iter()
        .position(|argument| argument == &directory)
        .expect("the hidden directory is mounted");
    assert_eq!(shown[directory_at - 1], "--tmpfs", "{shown:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_machine_file_tool_refuses_a_single_hidden_file() {
    let directory = tempfile::tempdir().unwrap();
    let secret = directory.path().join("credential.json");
    std::fs::write(&secret, "session-secret-must-not-leak").unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let visible = directory.path().join("visible.txt");
    std::fs::write(&visible, "readable").unwrap();
    let runtime = file_runtime(&root, &secret);

    // A hidden file is refused by both read paths, and its bytes never
    // come back in the refusal.
    let read = runtime
        .execute(Action::ReadFile {
            path: secret.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
    assert!(
        read.output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        read.output.text
    );
    assert!(!read.output.text.contains("session-secret-must-not-leak"));

    let error = runtime
        .read_bytes(&secret.to_string_lossy())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the path is hidden from this Computer"),
        "{error}"
    );
    assert!(!error.contains("session-secret-must-not-leak"));

    let read = runtime
        .execute(Action::ReadFile {
            path: visible.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "readable");
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_shell_cannot_read_a_hidden_file() {
    let Some((_directory, runtime, secret)) = machine_runtime_with_hidden_file() else {
        return;
    };
    let command = format!("cat '{}'", secret.display());
    let result = runtime
        .execute(Action::Shell {
            command: command.clone(),
        })
        .await;
    // The sandbox mounts /dev/null over the file, so the command reads
    // nothing (an empty read, or a refusal) — but the secret never
    // crosses into the output.
    assert!(
        !result.output.text.contains("session-secret-must-not-leak"),
        "the hidden file's bytes must never reach the shell: {}",
        result.output.text
    );
    // The host file is untouched: only the sandbox's view changed.
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "session-secret-must-not-leak"
    );
}

/// A denied path that does not exist yet is refused once it appears.
#[cfg(unix)]
#[tokio::test]
async fn a_denied_file_is_refused_even_when_it_appears_later() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let hidden = directory.path().join("computer");
    std::fs::create_dir(&hidden).unwrap();
    let visible = directory.path().join("visible.txt");
    std::fs::write(&visible, "readable").unwrap();
    let late = directory.path().join("credential.json");

    // The runtime is built while the credential file does not exist; the
    // daemon's file tools must refuse it when it appears later.
    let runtime = file_runtime(&root, &hidden).with_denied(vec![late.clone()]);
    std::fs::write(&late, "session-secret-must-not-leak").unwrap();

    let read = runtime
        .execute(Action::ReadFile {
            path: late.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
    assert!(
        read.output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        read.output.text
    );
    assert!(!read.output.text.contains("session-secret-must-not-leak"));
    let error = runtime
        .read_bytes(&late.to_string_lossy())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the path is hidden from this Computer"),
        "{error}"
    );
    assert!(!error.contains("session-secret-must-not-leak"));

    // A write to a denied path inside the Bot's folder is refused too,
    // even before the file exists.
    let inside = root.join("credential.json");
    let runtime = file_runtime(&root, &hidden).with_denied(vec![inside.clone()]);
    let write = runtime
        .execute(Action::WriteFile {
            path: "credential.json".to_owned(),
            content: "new".to_owned(),
        })
        .await;
    assert_eq!(write.status, ActionStatus::Failed, "{write:?}");
    assert!(
        write
            .output
            .text
            .contains("the path is hidden from this Computer"),
        "{}",
        write.output.text
    );
    assert!(
        !inside.exists(),
        "the denied write must not create the file"
    );

    // A normal file stays readable and writable.
    let read = runtime
        .execute(Action::ReadFile {
            path: visible.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "readable");
}

/// A file created inside a hidden directory after the runtime is built is
/// still out of the shell's reach, and the Workspace stays writable.
#[cfg(unix)]
#[tokio::test]
async fn a_file_created_later_inside_a_hidden_directory_stays_hidden() {
    let Some((_directory, runtime, hidden)) = machine_runtime_with_hidden() else {
        return;
    };
    let late = hidden.join("token.json");
    std::fs::write(&late, "channel-secret-must-not-leak").unwrap();

    let command = format!("ls {} && cat {}", hidden.display(), late.display());
    let result = runtime
        .execute(Action::Shell {
            command: command.clone(),
        })
        .await;
    assert!(
        !result.output.text.contains("channel-secret-must-not-leak"),
        "the hidden directory's late file must stay hidden: {}",
        result.output.text
    );
    // The Bot's Workspace is still writable.
    let write = runtime
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "written".to_owned(),
        })
        .await;
    assert_eq!(write.status, ActionStatus::Succeeded, "{write:?}");
    let read = runtime
        .execute(Action::ReadFile {
            path: "note.txt".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "written");
}
#[tokio::test]
async fn a_granted_folder_is_writable_outside_the_workspace() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let granted = directory.path().join("shared");
    std::fs::create_dir(&granted).unwrap();
    let runtime = runtime
        .with_machine_access(false, &[granted.to_string_lossy().into_owned()], None)
        .unwrap();
    let file = granted.join("written.txt");
    let written = runtime
        .execute(Action::WriteFile {
            path: file.to_string_lossy().into_owned(),
            content: "shared".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "shared");

    // A file outside both the workspace and the grant is refused.
    let outside = directory.path().join("outside.txt");
    let refused = runtime
        .execute(Action::WriteFile {
            path: outside.to_string_lossy().into_owned(),
            content: "no".to_owned(),
        })
        .await;
    assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");

    // The shell sees the same grant through the fence.
    let command = format!("echo shell > {}", granted.join("shell.txt").display());
    let shell = runtime.execute(Action::Shell { command }).await;
    assert_eq!(shell.status, ActionStatus::Succeeded, "{shell:?}");
    assert_eq!(
        std::fs::read_to_string(granted.join("shell.txt")).unwrap(),
        "shell\n"
    );
}

/// A grant that is a symlink, or sits below one, authorizes nothing: the
/// link's target is never opened as a write root.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_grant_authorizes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let target = directory.path().join("target");
    std::fs::create_dir(&target).unwrap();

    // The grant itself is a link to the folder the owner did not grant.
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[link.to_string_lossy().into_owned()], None)
        .unwrap();
    assert!(
        write_roots(&runtime).is_empty(),
        "a symlinked grant must not become a write root"
    );
    let written = runtime
        .execute(Action::WriteFile {
            path: link.join("escaped.txt").to_string_lossy().into_owned(),
            content: "no".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Failed, "{written:?}");
    assert!(!target.join("escaped.txt").exists());

    // A grant below a symlinked ancestor of the granted folder names the
    // link's target too, so it is refused the same way.
    let real = directory.path().join("real");
    let nested = real.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let ancestor = directory.path().join("ancestor");
    std::os::unix::fs::symlink(&real, &ancestor).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(
            false,
            &[ancestor.join("nested").to_string_lossy().into_owned()],
            None,
        )
        .unwrap();
    assert!(write_roots(&runtime).is_empty());
    let written = runtime
        .execute(Action::WriteFile {
            path: ancestor
                .join("nested/escaped.txt")
                .to_string_lossy()
                .into_owned(),
            content: "no".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Failed, "{written:?}");
    assert!(!nested.join("escaped.txt").exists());
}

/// A grant below an execute-only ancestor still opens: walking to a folder
/// never required reading its ancestors, and the security fix must not
/// change that.
#[cfg(target_os = "linux")]
#[test]
fn a_grant_below_an_execute_only_ancestor_still_opens() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let ancestor = directory.path().join("ancestor");
    let granted = ancestor.join("granted");
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o111)).unwrap();
    let opened = open_write_root(granted.to_str().unwrap())
        .expect("a traversable ancestor is not a grant error");
    assert_eq!(opened.0, granted);
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A granted shell never mounts a symlinked grant: the fence's write roots
/// come from the descriptors opened above, not from the link's target.
#[tokio::test]
async fn a_real_shell_cannot_write_through_a_symlinked_grant() {
    if !bwrap_enabled() {
        return;
    }
    let directory = outside_tempdir();
    let target = directory.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_machine_access(false, &[link.to_string_lossy().into_owned()], None)
        .unwrap();
    assert!(write_roots(&runtime).is_empty());

    let shell = runtime
        .execute(Action::Shell {
            command: format!("printf escaped > {}/escaped.txt", link.display()),
        })
        .await;
    assert_eq!(shell.status, ActionStatus::Failed, "{shell:?}");
    assert!(!target.join("escaped.txt").exists());
}

/// The normalized grant and run directory cross the channel with their
/// component names intact, so `logs` and `logs ` stay two folders and a
/// write lands in the one the owner named.
#[cfg(unix)]
#[tokio::test]
async fn a_grant_keeps_the_trailing_space_of_its_name() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let logs = directory.path().join("logs");
    let logs_space = directory.path().join("logs ");
    std::fs::create_dir(&logs).unwrap();
    std::fs::create_dir(&logs_space).unwrap();

    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[logs_space.to_string_lossy().into_owned()], None)
        .unwrap();
    assert_eq!(write_roots(&runtime).len(), 1);
    assert_eq!(
        std::fs::canonicalize(&write_roots(&runtime)[0].0).unwrap(),
        std::fs::canonicalize(&logs_space).unwrap()
    );
    let written = runtime
        .execute(Action::WriteFile {
            path: logs_space.join("note.txt").to_string_lossy().into_owned(),
            content: "space".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(logs_space.join("note.txt")).unwrap(),
        "space"
    );
    assert!(
        !logs.join("note.txt").exists(),
        "the trailing space must not be trimmed into the other folder"
    );

    // The same holds for the directory a run names.
    let run = directory.path().join("run");
    let run_space = directory.path().join("run ");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&run_space).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[], Some(run_space.to_str().unwrap()))
        .unwrap();
    let written = runtime
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "run".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(run_space.join("note.txt")).unwrap(),
        "run"
    );
    assert!(!run.join("note.txt").exists());
}

/// Dropping a running shell future (the worker is shut down mid-action)
/// ends the command's whole process tree, including a descendant in its own
/// process group, before the child is released.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_shell_future_ends_its_process_tree() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root);
    let started = directory.path().join("started");
    let alive = directory.path().join("alive");
    let command = format!(
        "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & \
         echo started > {}; sleep 30",
        alive.display(),
        started.display()
    );
    let mut running = Box::pin(runtime.execute(Action::Shell { command }));
    for _ in 0..400 {
        if started.exists() {
            break;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut running)
                .await
                .is_err(),
            "the shell ended before the future was dropped"
        );
    }
    assert!(started.exists(), "the shell started");

    drop(running);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !alive.exists(),
        "a dropped shell future ends the whole process tree"
    );
}

#[tokio::test]
async fn a_hard_link_in_a_grant_is_not_writable_by_the_shell() {
    if !bwrap_enabled() {
        return;
    }
    let directory = outside_tempdir();
    let granted = directory.path().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "safe").unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_machine_access(false, &[granted.to_string_lossy().into_owned()], None)
        .unwrap();
    // The first scan leaves the cached grant descriptor at EOF; a second
    // scan must open a fresh directory stream, not read from that offset.
    let first = runtime
        .execute(Action::Shell {
            command: format!("printf ok > {}/ok.txt", granted.display()),
        })
        .await;
    assert_eq!(first.status, ActionStatus::Succeeded, "{first:?}");

    std::fs::hard_link(&outside, granted.join("late.txt")).unwrap();
    let second = runtime
        .execute(Action::Shell {
            command: format!("printf changed > {}/late.txt", granted.display()),
        })
        .await;
    assert_eq!(second.status, ActionStatus::Failed, "{second:?}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
}

#[tokio::test]
async fn a_shell_mounts_every_grant_separately() {
    if !bwrap_enabled() {
        return;
    }
    let directory = outside_tempdir();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_machine_access(
            false,
            &[
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            None,
        )
        .unwrap();
    let command = format!(
        "printf first > {}/one; printf second > {}/two; printf own > {}/own.txt",
        first.display(),
        second.display(),
        directory.path().join("bot").display()
    );
    let result = runtime.execute(Action::Shell { command }).await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(std::fs::read_to_string(first.join("one")).unwrap(), "first");
    assert_eq!(
        std::fs::read_to_string(second.join("two")).unwrap(),
        "second"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("bot").join("own.txt")).unwrap(),
        "own"
    );
    assert!(!first.join("two").exists());
    assert!(!second.join("one").exists());
}

/// Full access runs the shell as the machine user with no sandbox. The
/// runtime's bwrap path does not exist, so a shell that started one would
/// fail instead of writing.
#[cfg(unix)]
#[tokio::test]
async fn full_access_runs_the_shell_without_a_sandbox() {
    let directory = outside_tempdir();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root);
    let target = directory.path().join("outside.txt");
    let written = runtime
        .execute(Action::Shell {
            command: format!("printf free > {}", target.display()),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "free");

    // The shell still starts in the Bot's folder.
    let pwd = runtime
        .execute(Action::Shell {
            command: "pwd".to_owned(),
        })
        .await;
    assert_eq!(pwd.status, ActionStatus::Succeeded, "{pwd:?}");
    assert!(
        pwd.output.text.contains(root.to_str().unwrap()),
        "{}",
        pwd.output.text
    );
}

/// Full access keeps the machine's own `/tmp`: the shell runs with no
/// sandbox, so a write there lands on the host instead of a fresh tmpfs.
#[cfg(unix)]
#[tokio::test]
async fn full_access_keeps_the_machines_own_tmp() {
    let directory = outside_tempdir();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root);
    let marker = format!("neha-full-{}", std::process::id());
    let target = format!("/tmp/{marker}");
    let result = runtime
        .execute(Action::Shell {
            command: format!("printf full > {target}"),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "full");
    std::fs::remove_file(&target).unwrap();
}

/// Full access writes with the machine user's own rights: any path the
/// user can write is a write root.
#[cfg(unix)]
#[tokio::test]
async fn full_access_writes_outside_the_workspace_without_a_grant() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root);
    let file = directory.path().join("anywhere/note.txt");
    let written = runtime
        .execute(Action::WriteFile {
            path: file.to_string_lossy().into_owned(),
            content: "full".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "full");

    // A File take uses the same writer.
    let delivered = directory.path().join("delivered.bin");
    runtime
        .write_bytes(&delivered.to_string_lossy(), vec![0, 1, 2])
        .await
        .unwrap();
    assert_eq!(std::fs::read(&delivered).unwrap(), vec![0, 1, 2]);
}

/// Full access ignores the hidden roots: the owner accepted that the Bot
/// can read the Computer's own configuration directory.
#[cfg(unix)]
#[tokio::test]
async fn full_access_reads_and_writes_a_hidden_root() {
    let directory = tempfile::tempdir().unwrap();
    let hidden = directory.path().join("computer");
    std::fs::create_dir(&hidden).unwrap();
    std::fs::write(hidden.join("computer.json"), "dummy-credential").unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &hidden);

    let read = runtime
        .execute(Action::ReadFile {
            path: hidden.join("computer.json").to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "dummy-credential");
    let bytes = runtime
        .read_bytes(&hidden.join("computer.json").to_string_lossy())
        .await
        .unwrap();
    assert_eq!(bytes, b"dummy-credential");
    let listed = runtime
        .execute(Action::ListFiles {
            path: hidden.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(listed.status, ActionStatus::Succeeded, "{listed:?}");
    assert!(
        listed.output.text.contains("computer.json"),
        "{}",
        listed.output.text
    );

    let note = hidden.join("note.txt");
    let written = runtime
        .execute(Action::WriteFile {
            path: note.to_string_lossy().into_owned(),
            content: "written".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "written");
}
#[tokio::test]
async fn a_run_directory_is_a_write_root() {
    if !bwrap_enabled() {
        return;
    }
    let directory = outside_tempdir();
    let work = directory.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_machine_access(false, &[], Some(work.to_str().unwrap()))
        .unwrap();

    // The shell starts in the run's directory and writes relative paths
    // there.
    let pwd = runtime
        .execute(Action::Shell {
            command: "pwd".to_owned(),
        })
        .await;
    assert_eq!(pwd.status, ActionStatus::Succeeded, "{pwd:?}");
    assert!(
        pwd.output.text.contains(work.to_str().unwrap()),
        "{}",
        pwd.output.text
    );
    let shell_write = runtime
        .execute(Action::Shell {
            command: "printf shell > note.txt".to_owned(),
        })
        .await;
    assert_eq!(
        shell_write.status,
        ActionStatus::Succeeded,
        "{shell_write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("note.txt")).unwrap(),
        "shell"
    );

    // The file tools resolve relative paths there and write through it.
    let tool_write = runtime
        .execute(Action::WriteFile {
            path: "tool.txt".to_owned(),
            content: "tool".to_owned(),
        })
        .await;
    assert_eq!(tool_write.status, ActionStatus::Succeeded, "{tool_write:?}");
    assert_eq!(
        std::fs::read_to_string(work.join("tool.txt")).unwrap(),
        "tool"
    );
    let read = runtime
        .execute(Action::ReadFile {
            path: "tool.txt".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "tool");

    // The run's directory is the only extra write root: a sibling stays
    // outside the fence.
    let outside = directory.path().join("outside.txt");
    let refused = runtime
        .execute(Action::WriteFile {
            path: outside.to_string_lossy().into_owned(),
            content: "no".to_owned(),
        })
        .await;
    assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
    assert!(!outside.exists());
    let error = runtime
        .write_bytes(&outside.to_string_lossy(), b"no".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("absolute"), "{error}");
}

/// A run's directory is a write root for the file tools without a sandbox:
/// relative paths resolve there and writes below it succeed.
#[cfg(unix)]
#[tokio::test]
async fn a_run_directory_is_a_write_root_for_the_file_tools() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let work = directory.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("report.txt"), "from the run").unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[], Some(work.to_str().unwrap()))
        .unwrap();

    let read = runtime
        .execute(Action::ReadFile {
            path: "report.txt".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "from the run");
    let written = runtime
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "note".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(work.join("note.txt")).unwrap(),
        "note"
    );

    // A File take's relative path goes through the same write root.
    runtime
        .write_bytes("deep/report.bin", vec![9])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(work.join("deep/report.bin")).unwrap(),
        vec![9]
    );

    // A path outside the run's directory is still refused.
    let outside = directory.path().join("outside.bin");
    let error = runtime
        .write_bytes(&outside.to_string_lossy(), b"x".to_vec())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("absolute"), "{error}");
    assert!(!outside.exists());
}

/// A run directory must name an existing folder.
#[cfg(unix)]
#[test]
fn a_run_directory_that_is_not_a_folder_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();

    let missing = directory.path().join("missing");
    let error = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[], Some(missing.to_str().unwrap()))
        .err()
        .expect("a missing run directory is refused");
    assert!(matches!(error, RuntimeError::Io(_)), "{error}");

    let file = directory.path().join("plain.txt");
    std::fs::write(&file, "not a folder").unwrap();
    let error = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[], Some(file.to_str().unwrap()))
        .err()
        .expect("a plain file is not a run directory");
    assert!(
        matches!(&error, RuntimeError::Access(message) if message.contains("not a folder")),
        "{error}"
    );
}

/// A hidden run directory is refused: its write root would re-expose what
/// the sandbox hides.
#[cfg(unix)]
#[test]
fn a_hidden_run_directory_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let hidden = directory.path().join("computer");
    std::fs::create_dir(&hidden).unwrap();
    let error = file_runtime(&root, &hidden)
        .with_machine_access(false, &[], Some(hidden.to_str().unwrap()))
        .err()
        .expect("a hidden run directory is refused");
    assert!(
        error
            .to_string()
            .contains("the folder is hidden from this Computer"),
        "{error}"
    );
}

/// Write roots are kept as candidates: the Bot's folder covers a root
/// inside it, but an outer root that the shell's scan refuses must not take
/// a clean inner root down with it.
#[cfg(unix)]
#[test]
fn nested_write_roots_stay_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let granted = directory.path().join("granted");
    std::fs::create_dir(&granted).unwrap();

    // A run directory inside the Bot's folder adds no root: the folder
    // already covers it.
    let inside_root = root.join("work");
    std::fs::create_dir(&inside_root).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(false, &[], Some(inside_root.to_str().unwrap()))
        .unwrap();
    assert!(write_roots(&runtime).is_empty());

    // A run directory inside a grant stays a candidate: the shell decides
    // after its scan which of the two it can mount.
    let inside_grant = granted.join("work");
    std::fs::create_dir(&inside_grant).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(
            false,
            &[granted.to_string_lossy().into_owned()],
            Some(inside_grant.to_str().unwrap()),
        )
        .unwrap();
    // The roots are named by the lexical path the Space sent, so compare
    // the folders they open, not the spelling.
    let opened = |root: &std::path::Path| {
        std::fs::canonicalize(root).expect("the write root names a real folder")
    };
    assert_eq!(write_roots(&runtime).len(), 2);
    assert_eq!(opened(&write_roots(&runtime)[0].0), opened(&granted));
    assert_eq!(opened(&write_roots(&runtime)[1].0), opened(&inside_grant));

    // Of two roots, the outer one covers the inner one for the shell, but
    // both stay as file-tool candidates because the outer scan may fail.
    let inner = granted.join("inner");
    std::fs::create_dir(&inner).unwrap();
    let runtime = file_runtime_without_hidden(&root)
        .with_machine_access(
            false,
            &[
                inner.to_string_lossy().into_owned(),
                granted.to_string_lossy().into_owned(),
            ],
            None,
        )
        .unwrap();
    assert_eq!(write_roots(&runtime).len(), 2);
    assert!(
        write_roots(&runtime)
            .iter()
            .any(|(path, _)| opened(path) == opened(&granted))
    );
    assert!(
        write_roots(&runtime)
            .iter()
            .any(|(path, _)| opened(path) == opened(&inner))
    );
}

/// The fenced constructor refuses what a full-access one must accept: a Bot
/// folder holding a symlink or a hard link, on a machine where bubblewrap
/// is not even looked for.
#[cfg(unix)]
#[test]
fn a_full_access_folder_opens_without_a_scan_or_a_probe() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let bot = "01890a5d-ac96-774b-bcce-b302099a8057";
    let folder = workspace.join(bot);
    std::fs::create_dir(&folder).unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, folder.join("link")).unwrap();
    std::fs::hard_link(&outside, folder.join("hard")).unwrap();

    let root = LocalRuntime::open_workspace(&workspace).unwrap();
    assert!(
        LocalRuntime::machine_at(&root, bot, Vec::new()).is_err(),
        "the fenced path refuses a folder that holds a link"
    );
    let runtime = LocalRuntime::full_access(&root, bot)
        .expect("full access opens the folder without a scan or a probe");

    assert!(is_full_access(&runtime));
    assert!(hidden(&runtime).is_empty());
    assert!(machine_mode_is_bwrap_less(&runtime));
    assert_eq!(runtime.workspace(), std::fs::canonicalize(&folder).unwrap());
}

/// A full-access command ends the descendants it left behind in another
/// process group or below a second fork, not only the group the shell
/// started.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_full_access_shell_ends_a_background_process_in_its_own_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root);
    let session = directory.path().join("session-alive");
    let double_fork = directory.path().join("double-fork-alive");
    let command = format!(
        "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & \
         (setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 &)",
        session.display(),
        double_fork.display()
    );

    let result = runtime.execute(Action::Shell { command }).await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !session.exists(),
        "a full-access command ends its own-session descendant when it ends"
    );
    assert!(
        !double_fork.exists(),
        "a full-access command ends a double-forked descendant when it ends"
    );
}

/// A timed-out full-access command also ends a descendant in another
/// process group: only the process group would not reach it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_full_access_timeout_ends_a_background_process_in_its_own_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = full_access_runtime(&root, &root).with_timeout(Duration::from_millis(200));
    let marker = directory.path().join("alive");
    let command = format!(
        "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & sleep 30",
        marker.display()
    );

    let result = runtime.execute(Action::Shell { command }).await;
    assert_eq!(result.status, ActionStatus::TimedOut, "{result:?}");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !marker.exists(),
        "a timed-out full-access command ends its whole process tree"
    );
}

/// A channel close kills a full-access command's whole tree, including a
/// descendant that left its process group.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_a_full_access_shell_ends_its_whole_tree() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let runtime = std::sync::Arc::new(full_access_runtime(&root, &root));
    let started = directory.path().join("started");
    let marker = directory.path().join("alive");
    let command = format!(
        "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & echo started > {}; sleep 30",
        marker.display(),
        started.display()
    );
    let running = tokio::spawn({
        let runtime = std::sync::Arc::clone(&runtime);
        async move {
            runtime
                .execute(Action::Shell {
                    command: command.clone(),
                })
                .await
        }
    });
    for _ in 0..200 {
        if started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(started.exists(), "the shell started");
    assert!(runtime.cancel_in_flight());
    let result = running.await.unwrap();
    assert_ne!(result.status, ActionStatus::Succeeded, "{result:?}");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !marker.exists(),
        "a cancelled full-access command ends its whole process tree"
    );
}

/// The cached runtime is reused between commands, so the Bot's folder must
/// be scanned again for every shell: the host may have added a hard link
/// after the runtime opened the folder.
#[tokio::test]
async fn a_cached_runtime_scans_the_bot_folder_before_every_shell() {
    let Some((directory, runtime)) = machine_runtime() else {
        return;
    };
    let folder = runtime.workspace().to_path_buf();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "safe").unwrap();

    let first = runtime
        .execute(Action::Shell {
            command: format!("printf first > {}/first.txt", folder.display()),
        })
        .await;
    assert_eq!(first.status, ActionStatus::Succeeded, "{first:?}");

    // The host adds a hard link to a file outside the folder between two
    // commands that reuse this runtime.
    std::fs::hard_link(&outside, folder.join("late.txt")).unwrap();
    let second = runtime
        .execute(Action::Shell {
            command: format!("printf changed > {}/late.txt", folder.display()),
        })
        .await;
    assert_eq!(second.status, ActionStatus::Failed, "{second:?}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
}

/// A clean inner grant still gets its own mount when an outer grant the
/// shell's scan refuses would cover it.
#[tokio::test]
async fn a_clean_inner_root_survives_a_refused_outer_root() {
    if !bwrap_enabled() {
        return;
    }
    // The grants sit outside `/tmp`, which the fence replaces with a fresh
    // tmpfs; only a visible directory proves the shell's write.
    let directory = outside_tempdir();
    let outer = directory.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "safe").unwrap();
    std::fs::hard_link(&outside, outer.join("linked.txt")).unwrap();
    let runtime = LocalRuntime::machine(directory.path().join("bot"))
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_machine_access(
            false,
            &[
                outer.to_string_lossy().into_owned(),
                inner.to_string_lossy().into_owned(),
            ],
            None,
        )
        .unwrap();

    // The outer root holds a hard link, so it is not mounted; the inner
    // root is clean and must still be writable for the shell.
    let shell = runtime
        .execute(Action::Shell {
            command: format!("printf inner > {}/note.txt", inner.display()),
        })
        .await;
    assert_eq!(shell.status, ActionStatus::Succeeded, "{shell:?}");
    assert_eq!(
        std::fs::read_to_string(inner.join("note.txt")).unwrap(),
        "inner"
    );

    // The refused outer root is not a shell write root.
    let refused = runtime
        .execute(Action::Shell {
            command: format!("printf changed > {}/linked.txt", outer.display()),
        })
        .await;
    assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
}

/// Access roots are opened for each access decision, not held from an older
/// one: a run directory archived and rebuilt under the same path must be
/// written through its new inode.
#[cfg(unix)]
#[tokio::test]
async fn access_roots_are_reopened_for_every_decision() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("bot");
    std::fs::create_dir(&root).unwrap();
    let work = directory.path().join("run");
    std::fs::create_dir(&work).unwrap();
    let base = file_runtime_without_hidden(&root);

    let first = base
        .with_machine_access(false, &[], Some(work.to_str().unwrap()))
        .unwrap();
    let written = first
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "first".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(work.join("note.txt")).unwrap(),
        "first"
    );

    // Between two runs the directory is archived and rebuilt under the same
    // path. A descriptor held by a cached runtime would still name the old
    // inode.
    let archived = directory.path().join("archived");
    std::fs::rename(&work, &archived).unwrap();
    std::fs::create_dir(&work).unwrap();
    let second = base
        .with_machine_access(false, &[], Some(work.to_str().unwrap()))
        .unwrap();
    let written = second
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "second".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(work.join("note.txt")).unwrap(),
        "second"
    );
    assert_eq!(
        std::fs::read_to_string(archived.join("note.txt")).unwrap(),
        "first"
    );
}

#[tokio::test]
async fn hosted_shell_has_a_private_rootfs() {
    let Some((_dir, runtime)) = hosted_runtime() else {
        return;
    };
    // The CLI is mounted at its fixed sandbox path: this build's own
    // directory is outside the minimal rootfs.
    let result = runtime
        .execute(Action::Shell {
            command:
                "pwd && printf x > note.txt && echo \"$HOME $TMPDIR\" && test -d /opt/neha-cli"
                    .to_owned(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        result.output.text.contains("stdout:\n/workspace\n"),
        "the Hosted shell did not start in its workspace: {}",
        result.output.text
    );
    assert!(
        result.output.text.contains("/workspace /tmp\n"),
        "the Hosted shell has the wrong HOME or TMPDIR: {}",
        result.output.text
    );
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("note.txt")).unwrap(),
        "x"
    );
}

#[tokio::test]
async fn hosted_shell_cannot_see_outside_the_workspace() {
    let Some((directory, runtime)) = hosted_runtime() else {
        return;
    };
    // The test's temporary directory is the host path above the workspace;
    // the sandbox replaces the host's /tmp, so that file is not there.
    let secret = directory.path().join("secret.txt");
    std::fs::write(&secret, "host-secret-must-not-leak").unwrap();

    let unseen = runtime
        .execute(Action::Shell {
            command: format!("test ! -e {}", secret.display()),
        })
        .await;
    assert_eq!(unseen.status, ActionStatus::Succeeded, "{unseen:?}");

    let read = runtime
        .execute(Action::Shell {
            command: format!("cat {}", secret.display()),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Failed, "{read:?}");
    assert!(
        !read.output.text.contains("host-secret-must-not-leak"),
        "the sandbox read a host file: {}",
        read.output.text
    );

    // A host directory the sandbox does not mount at all is absent too:
    // /var/tmp is outside the host /tmp that the sandbox replaces.
    let outside = outside_tempdir();
    let host_secret = outside.path().join("host-only.txt");
    std::fs::write(&host_secret, "host-only-bytes").unwrap();
    let unreachable = runtime
        .execute(Action::Shell {
            command: format!("test ! -e {}", host_secret.display()),
        })
        .await;
    assert_eq!(
        unreachable.status,
        ActionStatus::Succeeded,
        "{unreachable:?}"
    );

    // The system directories are read-only: /etc cannot take a file.
    let etc = runtime
        .execute(Action::Shell {
            command: "printf x > /etc/neha-hosted-test".to_owned(),
        })
        .await;
    assert_eq!(etc.status, ActionStatus::Failed, "{etc:?}");
    assert!(
        !std::path::Path::new("/etc/neha-hosted-test").exists(),
        "the sandbox wrote the host's /etc"
    );

    // /tmp is the sandbox's own fresh tmpfs, and it is writable.
    let marker = format!("neha-hosted-tmp-{}", std::process::id());
    let tmp = runtime
        .execute(Action::Shell {
            command: format!("printf x > /tmp/{marker} && cat /tmp/{marker}"),
        })
        .await;
    assert_eq!(tmp.status, ActionStatus::Succeeded, "{tmp:?}");
    assert!(
        tmp.output.text.contains("stdout:\nx\n"),
        "{}",
        tmp.output.text
    );
    assert!(
        !std::path::Path::new(&format!("/tmp/{marker}")).exists(),
        "the sandbox wrote the host's /tmp"
    );
}

#[tokio::test]
async fn hosted_file_tools_stay_in_the_workspace() {
    let Some((directory, runtime)) = hosted_runtime() else {
        return;
    };
    let written = runtime
        .execute(Action::WriteFile {
            path: "notes/hello.txt".to_owned(),
            content: "hello hosted".to_owned(),
        })
        .await;
    assert_eq!(written.status, ActionStatus::Succeeded, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("notes/hello.txt")).unwrap(),
        "hello hosted"
    );

    let read = runtime
        .execute(Action::ReadFile {
            path: "notes/hello.txt".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "hello hosted");

    // The managed rules still apply: no path escapes the workspace.
    let outside = directory.path().join("escape.txt");
    for path in [
        "../escape.txt".to_owned(),
        outside.to_string_lossy().into_owned(),
    ] {
        let refused = runtime
            .execute(Action::WriteFile {
                path,
                content: "pwned".to_owned(),
            })
            .await;
        assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
        assert!(
            refused.output.text.contains("escapes"),
            "{}",
            refused.output.text
        );
    }
    assert!(!outside.exists(), "a refused write leaves no file");
    let refused = runtime
        .execute(Action::ReadFile {
            path: "/etc/passwd".to_owned(),
        })
        .await;
    assert_eq!(refused.status, ActionStatus::Failed, "{refused:?}");
}

#[tokio::test]
async fn hosted_working_dir_must_stay_in_the_workspace() {
    let Some((directory, runtime)) = hosted_runtime() else {
        return;
    };
    let inside = runtime.workspace().join("sub");
    std::fs::create_dir(&inside).unwrap();
    let runtime = runtime
        .with_working_dir(inside.to_str().unwrap())
        .expect("a folder inside the workspace is accepted");

    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let error = runtime
        .with_working_dir(outside.to_str().unwrap())
        .err()
        .expect("a Hosted working directory outside the workspace is refused");
    assert!(matches!(error, RuntimeError::Access(_)), "{error}");
}

#[test]
fn a_hosted_sandbox_without_a_workspace_is_refused() {
    if !bwrap_enabled() {
        return;
    }
    let bwrap = find_bwrap().expect("NEHA_TEST_BWRAP is set but bwrap was not found on PATH");
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing-workspace");
    let error = probe_hosted(&bwrap, &missing)
        .expect_err("a missing workspace cannot be mounted into a Hosted sandbox");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");
}

#[test]
fn a_hosted_cli_directory_inside_the_home_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let cli = home.join("target").join("debug");
    std::fs::create_dir_all(&cli).unwrap();
    let error = validate_hosted_mounts(std::slice::from_ref(&home), Some(&cli))
        .expect_err("a CLI inside the protected home would expose that subtree");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");

    let error = validate_hosted_mounts(std::slice::from_ref(&cli), Some(&home))
        .expect_err("a home inside the CLI directory would expose the home");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");
}

#[test]
fn a_hosted_workspace_inside_a_system_mount_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let error =
        LocalRuntime::hosted_with_hidden(&workspace, vec![std::path::PathBuf::from("/usr")])
            .err()
            .expect("a home under a mounted system directory would leak");
    assert!(matches!(error, RuntimeError::NoSandbox(_)), "{error}");
}

#[test]
fn a_hosted_workspace_symlink_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = directory.path().join("workspace");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let error = LocalRuntime::hosted(&link)
        .err()
        .expect("a Workspace that is a symlink cannot be bound as the writable root");
    assert!(matches!(error, RuntimeError::UnsafeFolder(_)), "{error}");
}

#[test]
fn a_hosted_workspace_holding_a_link_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "host bytes").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
    let error = LocalRuntime::hosted(&workspace)
        .err()
        .expect("a linked entry could change a host file through the writable bind");
    assert!(matches!(error, RuntimeError::UnsafeFolder(_)), "{error}");

    std::fs::remove_file(workspace.join("link")).unwrap();
    std::fs::hard_link(&outside, workspace.join("hard")).unwrap();
    let error = LocalRuntime::hosted(&workspace)
        .err()
        .expect("a hard link would share an inode with a host file");
    assert!(matches!(error, RuntimeError::UnsafeFolder(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn runs_a_skill_script_with_its_arguments() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::RunScript {
            name: "echo-args.sh".to_owned(),
            content: "#!/bin/sh\nprintf '[%s]' \"$@\"\n".to_owned(),
            arguments: vec!["one".to_owned(), "two words".to_owned()],
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert!(
        result.output.text.contains("[one][two words]"),
        "{}",
        result.output.text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn managed_skill_scripts_run_from_a_private_file_and_clean_up_after_each_run() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_dir, runtime) = runtime();
    let project = tempfile::tempdir().unwrap();
    let runtime = runtime
        .with_working_dir(project.path().to_str().unwrap())
        .unwrap();
    let content = "#!/bin/sh\ncp -p \"$0\" \"$1/copy\" && printf '%s' \"$0\" > \"$1/path\" || exit 2\nexit \"$2\"\n# trailing bytes: é  ";
    let mut previous = None;
    for (exit, expected) in [("0", ActionStatus::Succeeded), ("1", ActionStatus::Failed)] {
        let observation = tempfile::tempdir().unwrap();
        let result = runtime
            .execute(Action::RunScript {
                name: "é".repeat(MAX_SCRIPT_NAME_BYTES / 2),
                content: content.to_owned(),
                arguments: vec![
                    observation.path().to_str().unwrap().to_owned(),
                    exit.to_owned(),
                ],
            })
            .await;
        assert_eq!(result.status, expected, "{result:?}");
        let copy = observation.path().join("copy");
        assert_eq!(std::fs::read(&copy).unwrap(), content.as_bytes());
        assert_eq!(
            std::fs::metadata(copy).unwrap().permissions().mode() & 0o777,
            0o500
        );
        let path = std::path::PathBuf::from(
            std::fs::read_to_string(observation.path().join("path")).unwrap(),
        );
        assert!(
            !path.starts_with(runtime.workspace()),
            "a script is never written inside the Workspace: {}",
            path.display()
        );
        assert!(!path.exists(), "a finished run removes its private script");
        assert_ne!(previous.as_ref(), Some(&path), "two runs share one path");
        previous = Some(path);
    }
    assert!(!project.path().join(".neha").exists());
    assert!(!runtime.workspace().join(".neha").exists());
}

/// A file another run of the same user left where the old staging wrote
/// cannot replace the action's own bytes.
#[cfg(unix)]
#[tokio::test]
async fn a_workspace_shadow_cannot_replace_the_executed_script() {
    let (_dir, runtime) = runtime();
    let shadow_directory = runtime.workspace().join(".neha/skills");
    std::fs::create_dir_all(&shadow_directory).unwrap();
    let shadow = shadow_directory.join("marker.sh");
    let shadow_content = "#!/bin/sh\nprintf shadow > result.txt\n";
    std::fs::write(&shadow, shadow_content).unwrap();
    let result = runtime
        .execute(Action::RunScript {
            name: "marker.sh".to_owned(),
            content: "#!/bin/sh\nprintf action > result.txt\nls .neha/skills\n".to_owned(),
            arguments: Vec::new(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("result.txt")).unwrap(),
        "action",
        "the Workspace shadow's bytes ran instead of the action's"
    );
    // The script saw exactly the file the test planted: run_script neither
    // used the old staging path nor cleaned up after it.
    let stdout = result
        .output
        .text
        .split_once("stdout:\n")
        .map(|(_, stdout)| stdout.trim_end())
        .unwrap_or_default();
    assert_eq!(
        stdout, "marker.sh",
        "the run touched the old staging directory: {result:?}"
    );
    assert_eq!(std::fs::read_to_string(&shadow).unwrap(), shadow_content);
    assert_eq!(std::fs::read_dir(&shadow_directory).unwrap().count(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_skill_script_leaves_no_file_behind() {
    let (_dir, runtime) = runtime();
    let runtime = std::sync::Arc::new(runtime.with_timeout(Duration::from_secs(30)));
    let observation = tempfile::tempdir().unwrap();
    let observed = observation.path().join("path");
    let running = tokio::spawn({
        let runtime = std::sync::Arc::clone(&runtime);
        let observed = observed.clone();
        async move {
            runtime
                .execute(Action::RunScript {
                    name: "slow.sh".to_owned(),
                    content: "#!/bin/sh\nprintf '%s' \"$0\" > \"$1\"\nsleep 30\n".to_owned(),
                    arguments: vec![observed.to_str().unwrap().to_owned()],
                })
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !observed.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the script did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        runtime.cancel_in_flight(),
        "the running script is cancelled"
    );
    let result = running.await.unwrap();
    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    let path = std::path::PathBuf::from(std::fs::read_to_string(&observed).unwrap());
    assert!(!path.starts_with(runtime.workspace()));
    assert!(
        !path.exists(),
        "a cancelled run left its private script behind: {}",
        path.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hosted_skill_scripts_run_from_the_sandbox_tmp() {
    let Some((_dir, runtime)) = hosted_runtime() else {
        return;
    };
    // A file another run of the same user left where the old staging wrote
    // must not replace the action's bytes, and the run must leave it alone.
    let shadow_directory = runtime.workspace().join(".neha/skills");
    std::fs::create_dir_all(&shadow_directory).unwrap();
    let shadow = shadow_directory.join("where.sh");
    let shadow_content = "#!/bin/sh\nprintf shadow > marker\n";
    std::fs::write(&shadow, shadow_content).unwrap();
    let result = runtime
        .execute(Action::RunScript {
            name: "where.sh".to_owned(),
            content: "#!/bin/sh\nprintf ran > marker\npwd\nprintf '%s' \"$0\"\nls -l \"$0\""
                .to_owned(),
            arguments: Vec::new(),
        })
        .await;
    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("marker")).unwrap(),
        "ran",
        "the Workspace shadow's bytes ran instead of the action's"
    );
    assert!(
        result.output.text.contains("/workspace\n"),
        "the script starts in the mounted workspace: {result:?}"
    );
    assert!(
        result.output.text.contains("/tmp/neha-script/where.sh."),
        "the sandbox mounts the script, not a Workspace path: {result:?}"
    );
    assert!(
        result.output.text.contains("-r-x------"),
        "the mounted script is read-only and executable: {result:?}"
    );
    assert_eq!(std::fs::read_to_string(&shadow).unwrap(), shadow_content);
    assert_eq!(std::fs::read_dir(&shadow_directory).unwrap().count(), 1);
}

#[tokio::test]
async fn a_skill_script_name_that_names_another_place_is_refused() {
    let (_dir, runtime) = runtime();
    let long = "n".repeat(MAX_SCRIPT_NAME_BYTES + 1);
    for name in [
        "",
        " ",
        " x",
        "x ",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "x\ny",
        long.as_str(),
    ] {
        let result = runtime
            .execute(Action::RunScript {
                name: name.to_owned(),
                content: "#!/bin/sh\ntrue\n".to_owned(),
                arguments: Vec::new(),
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed, "{name:?}");
    }
    assert!(
        !runtime.workspace().join(".neha").exists(),
        "a refused script writes nothing"
    );
}

#[tokio::test]
async fn a_skill_script_larger_than_the_write_limit_is_refused() {
    let (_dir, runtime) = runtime();
    let result = runtime
        .execute(Action::RunScript {
            name: "big.sh".to_owned(),
            content: "a".repeat(MAX_WRITE_BYTES + 1),
            arguments: Vec::new(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed);
    assert!(!runtime.workspace().join(".neha").exists());
}

#[tokio::test]
async fn a_skill_script_with_an_unusable_argument_is_refused() {
    let (_dir, runtime) = runtime();
    let long = "a".repeat(MAX_SCRIPT_ARGUMENT_BYTES + 1);
    for arguments in [
        vec![String::new()],
        vec![long],
        vec!["ok".to_owned(); MAX_SCRIPT_ARGUMENTS + 1],
    ] {
        let result = runtime
            .execute(Action::RunScript {
                name: "args.sh".to_owned(),
                content: "#!/bin/sh\ntrue\n".to_owned(),
                arguments,
            })
            .await;

        assert_eq!(result.status, ActionStatus::Failed);
    }
    assert!(!runtime.workspace().join(".neha").exists());
}

#[tokio::test]
async fn a_real_shell_runs_a_skill_script_from_the_sandbox() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let project = outside_tempdir();
    let runtime = runtime
        .with_machine_access(false, &[], project.path().to_str())
        .unwrap();
    let result = runtime
        .execute(Action::RunScript {
            name: "where.sh".to_owned(),
            content: "#!/bin/sh\npwd\nprintf '%s' \"$0\"\n".to_owned(),
            arguments: Vec::new(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Succeeded, "{result:?}");
    let expected = format!("{}\n/tmp/neha-script/where.sh.", project.path().display());
    assert!(result.output.text.contains(&expected), "{result:?}");
    assert!(!project.path().join(".neha").exists());
    assert!(!runtime.workspace().join(".neha").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_bubblewrap_without_data_mounts_refuses_a_skill_script() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let bwrap = directory.path().join("bwrap");
    // An older bubblewrap: it starts, but it does not know the option that
    // mounts a descriptor as a file.
    std::fs::write(
        &bwrap,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf 'bubblewrap 0.4.0\n'
    exit 0
fi
for argument in "$@"; do
    if [ "$argument" = "--ro-bind-data" ]; then
        printf 'bwrap: Unknown option --ro-bind-data\n' >&2
        exit 1
    fi
done
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
    let folder = directory.path().join("bot");
    let runtime = machine_with_bwrap(folder.clone(), Some(bwrap), Vec::new())
        .expect("the old bubblewrap answers the constructor's probe");
    let result = runtime
        .execute(Action::RunScript {
            name: "old.sh".to_owned(),
            content: "#!/bin/sh\nprintf ran > marker\n".to_owned(),
            arguments: Vec::new(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(
        result
            .output
            .text
            .contains("cannot mount a verified script"),
        "the failure names the missing capability: {result:?}"
    );
    assert!(
        !folder.join("marker").exists(),
        "the script ran without its verified mount"
    );
    assert!(!folder.join(".neha").exists());
}

#[tokio::test]
async fn a_skill_script_runs_under_the_sandbox() {
    let Some((_dir, runtime)) = machine_runtime() else {
        return;
    };
    let outside = outside_tempdir();
    let sink = outside.path().join("pwned.txt");
    let result = runtime
        .execute(Action::RunScript {
            name: "escape.sh".to_owned(),
            content: format!("#!/bin/sh\nprintf hi > {}\n", sink.display()),
            arguments: Vec::new(),
        })
        .await;

    assert_eq!(result.status, ActionStatus::Failed, "{result:?}");
    assert!(
        !sink.exists(),
        "the sandbox let a skill script write outside the folder"
    );
}
