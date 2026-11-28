//! Native Windows behaviour that the Unix fence tests cannot cover.

use neha_computer::{Action, ActionStatus, ComputerRuntime, LocalRuntime};

#[tokio::test]
async fn the_managed_runtime_runs_powershell_and_keeps_files_in_the_folder() {
    let folder = tempfile::tempdir().unwrap();
    let runtime = LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap();

    let shell = runtime
        .execute(Action::Shell {
            command: "Write-Output hello".to_owned(),
        })
        .await;
    assert_eq!(shell.status, ActionStatus::Succeeded, "{shell:?}");
    assert!(shell.output.text.contains("hello"), "{shell:?}");

    let write = runtime
        .execute(Action::WriteFile {
            path: "note.txt".to_owned(),
            content: "kept".to_owned(),
        })
        .await;
    assert_eq!(write.status, ActionStatus::Succeeded, "{write:?}");

    let read = runtime
        .execute(Action::ReadFile {
            path: "note.txt".to_owned(),
        })
        .await;
    assert_eq!(read.status, ActionStatus::Succeeded, "{read:?}");
    assert_eq!(read.output.text, "kept");
}

#[tokio::test]
async fn the_managed_runtime_refuses_grants_cwd_and_full_access() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().to_str().unwrap().to_owned();

    let granted = LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap();
    assert!(
        granted
            .with_machine_access(false, &[outside.clone()], None)
            .is_err()
    );

    let cwd = LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap();
    assert!(cwd.with_machine_access(false, &[], Some(&outside)).is_err());

    let full = LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap();
    assert!(full.with_machine_access(true, &[], None).is_err());
}

#[tokio::test]
async fn managed_file_tools_refuse_a_hard_link_from_outside() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside").unwrap();
    std::fs::hard_link(&secret, folder.path().join("borrowed.txt")).unwrap();

    let runtime = LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap();
    let read = runtime
        .execute(Action::ReadFile {
            path: "borrowed.txt".to_owned(),
        })
        .await;
    assert_ne!(read.status, ActionStatus::Succeeded, "{read:?}");

    let write = runtime
        .execute(Action::WriteFile {
            path: "borrowed.txt".to_owned(),
            content: "changed".to_owned(),
        })
        .await;
    assert_ne!(write.status, ActionStatus::Succeeded, "{write:?}");
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside");
}

#[tokio::test]
async fn cancelling_a_long_shell_ends_it() {
    use std::sync::Arc;
    use std::time::Duration;

    let folder = tempfile::tempdir().unwrap();
    let runtime = Arc::new(LocalRuntime::machine_with_hidden(folder.path(), Vec::new()).unwrap());
    let running = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            runtime
                .execute(Action::Shell {
                    command: "Start-Sleep -Seconds 30".to_owned(),
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(runtime.cancel_in_flight(), "the shell should be in flight");
    let result = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the cancelled shell must settle")
        .unwrap();
    assert_ne!(result.status, ActionStatus::Succeeded, "{result:?}");
}
