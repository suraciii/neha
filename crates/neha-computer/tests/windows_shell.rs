//! Native Windows behaviour that the Unix fence tests cannot cover.
#![cfg(windows)]

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
