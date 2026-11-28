//! The CLI's grant arguments: naming a folder to grant and to revoke.
//!
//! These drive `checked_grant` and `revocable_grant` against real folders, so
//! they live with the other integration tests; the parser tests stay in the
//! module they parse.

use std::path::PathBuf;

use neha_app::test_support::{checked_grant, revocable_grant};

#[test]
fn a_grant_folder_must_exist_and_be_a_folder() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("shared");
    std::fs::create_dir(&folder).unwrap();
    assert_eq!(
        checked_grant(&folder).unwrap(),
        std::fs::canonicalize(&folder)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );

    let error = checked_grant(&directory.path().join("missing"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not exist"), "{error}");
    let file = directory.path().join("notes.txt");
    std::fs::write(&file, "notes").unwrap();
    let error = checked_grant(&file).unwrap_err().to_string();
    assert!(error.contains("must be a folder"), "{error}");
}

#[test]
fn a_revoke_reads_a_live_folder_like_a_grant() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("shared");
    std::fs::create_dir(&real).unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // A grant stores the folder a symlink names; the same argument must
    // find that folder again when it is revoked.
    let granted = checked_grant(&link).unwrap();
    assert_eq!(
        granted,
        std::fs::canonicalize(&real)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(
        revocable_grant(&link, std::slice::from_ref(&granted)).unwrap(),
        granted
    );

    // A folder that is gone has no target to read, so the recorded value
    // is what a revoke names.
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_dir(&real).unwrap();
    assert_eq!(
        revocable_grant(&PathBuf::from(&granted), std::slice::from_ref(&granted)).unwrap(),
        granted
    );
}

#[test]
fn a_revoke_keeps_a_name_that_ends_in_a_space() {
    let directory = tempfile::tempdir().unwrap();
    let plain = directory.path().join("logs");
    let spaced = directory.path().join("logs ");
    std::fs::create_dir(&plain).unwrap();
    std::fs::create_dir(&spaced).unwrap();
    let plain_grant = checked_grant(&plain).unwrap();
    let spaced_grant = checked_grant(&spaced).unwrap();
    let shown = vec![plain_grant.clone(), spaced_grant.clone()];

    // The argument's trailing space is the name: the exact spelling wins
    // over the trimmed form, so `logs ` cannot take the grant for `logs`.
    assert_eq!(
        revocable_grant(&spaced, &shown).unwrap(),
        spaced_grant,
        "the exact spelling names its own folder"
    );
    assert_eq!(revocable_grant(&plain, &shown).unwrap(), plain_grant);
    // A raw spelling normalized at the edge still names the folder.
    let slash = format!("{}/", plain.display());
    assert_eq!(
        revocable_grant(std::path::Path::new(&slash), &shown).unwrap(),
        plain_grant
    );
}

#[test]
fn a_revoke_removes_the_recorded_grant_a_later_link_points_elsewhere() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let first_grant = checked_grant(&first).unwrap();
    let second_grant = checked_grant(&second).unwrap();
    let shown = vec![first_grant.clone(), second_grant.clone()];

    // The granted folder may become a symlink to another granted folder
    // after it was recorded. The record's own path is what the revoke
    // names, so the other grant is not removed instead.
    std::fs::rename(&first, directory.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(&second, &first).unwrap();
    assert_eq!(
        revocable_grant(&first, &shown).unwrap(),
        first_grant,
        "the recorded path wins over the link's target"
    );
    assert_eq!(
        revocable_grant(&second, &shown).unwrap(),
        second_grant,
        "a path the record shows is removed exactly as recorded"
    );

    // An ancestor that becomes a symlink does not redirect the revoke
    // either: the record's path is matched before anything is resolved.
    let nested_parent = directory.path().join("nested");
    let nested = nested_parent.join("shared");
    std::fs::create_dir_all(&nested).unwrap();
    let nested_grant = checked_grant(&nested).unwrap();
    std::fs::rename(&nested_parent, directory.path().join("nested-moved")).unwrap();
    let other_parent = directory.path().join("other-parent");
    std::fs::create_dir_all(other_parent.join("shared")).unwrap();
    std::os::unix::fs::symlink(&other_parent, &nested_parent).unwrap();
    assert_eq!(
        revocable_grant(&nested, std::slice::from_ref(&nested_grant)).unwrap(),
        nested_grant,
        "a link above the granted folder does not change what was recorded"
    );

    // A path no grant shows is read as an alias while it exists, so the
    // grant a symlink path added is still reachable.
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&second, &alias).unwrap();
    assert_eq!(
        revocable_grant(&alias, &shown).unwrap(),
        second_grant,
        "an alias resolves to the recorded folder"
    );
    // A path no grant shows and no folder names is sent as the User
    // spelled it, so the Space refuses it with the name the User used.
    let missing = directory.path().join("missing");
    assert_eq!(
        revocable_grant(&missing, &shown).unwrap(),
        missing.to_string_lossy().into_owned()
    );
}
