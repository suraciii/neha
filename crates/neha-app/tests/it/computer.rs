//! The Local Computer's integration tests: real files, a fake Space, and a
//! real daemon speaking the channel protocol.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use neha::id::Id;
use neha_computer::channel::AccessFrame;
use neha_computer::{
    Action, ActionStatus, CHANNEL_VERSION, ComputerRuntime as _, DeviceFrame, HostFrame,
    LocalRuntime, ToolOutput, WireAction,
};
use tokio_tungstenite::tungstenite::Message;

use neha_app::client::test_support::{load_credential_from, write_private};
use neha_app::client::{Credential, SignedIn};
use neha_app::computer::test_support::{
    TestConfig, TestRuntimes, TestSocket, answer_is_yes, check_saved_origin, check_ticket_space,
    choose_ticket, config, config_directory_from, confirmation_needed, credential_path_from,
    decode_bytes, default_workspace_from, denied_paths_from, display_pairing_code, empty_runtimes,
    encode_bytes, execute_frame, folder_for, found_tickets, hidden_roots_for,
    is_loopback_space_url, legacy_relative_credential_path, load_config, local_session_origin,
    machine_name, migrate_legacy_credential_for, normalized_any_space_url, normalized_space_url,
    parse_ticket, refuse_older_pairing_from, refuse_workspace, refused_by_proxy,
    refused_credential, runtimes_all_full_access, runtimes_is_empty, runtimes_len, runtimes_with,
    same_origin, saved_session_origin, serve_channel, socket_url, standard_config_directory,
    write_receipt,
};

/// One saved pairing that names a fixed Computer.
fn config_at(workspace: PathBuf) -> TestConfig {
    config(workspace)
}

#[test]
fn the_saved_configuration_round_trips_and_is_private() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("computer.json");
    let config = config_at(directory.path().join("workspace"));

    config.save_to(&path).unwrap();

    assert_eq!(load_config(&path).unwrap(), config);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_machine_that_is_not_paired_is_told_to_pair() {
    let directory = tempfile::tempdir().unwrap();
    let error = load_config(&directory.path().join("computer.json"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("neha computer pair"), "{error}");
}

#[test]
fn the_configuration_directory_comes_from_the_environment_in_order() {
    // NEHA_COMPUTER_HOME wins outright.
    assert_eq!(
        config_directory_from(
            Some(PathBuf::from("/srv/neha")),
            Some(PathBuf::from("/home/ada/.config")),
            Some(PathBuf::from("/home/ada"))
        )
        .unwrap(),
        PathBuf::from("/srv/neha")
    );
    // Then XDG_CONFIG_HOME/neha.
    assert_eq!(
        config_directory_from(
            None,
            Some(PathBuf::from("/x/config")),
            Some(PathBuf::from("/home/ada"))
        )
        .unwrap(),
        PathBuf::from("/x/config/neha")
    );
    // Then the platform default under $HOME.
    assert_eq!(
        config_directory_from(None, None, Some(PathBuf::from("/home/ada"))).unwrap(),
        PathBuf::from("/home/ada/.config/neha")
    );
    // A relative XDG_CONFIG_HOME is ignored, as the XDG spec says.
    assert_eq!(
        config_directory_from(
            None,
            Some(PathBuf::from("relative/config")),
            Some(PathBuf::from("/home/ada"))
        )
        .unwrap(),
        PathBuf::from("/home/ada/.config/neha")
    );
    let error = config_directory_from(None, None, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("NEHA_COMPUTER_HOME"), "{error}");
}

#[test]
fn the_default_workspace_root_sits_under_the_user_home() {
    assert_eq!(
        default_workspace_from(Some(PathBuf::from("/home/ada"))).unwrap(),
        PathBuf::from("/home/ada/.neha/workspaces")
    );
    let error = default_workspace_from(None).unwrap_err().to_string();
    assert!(error.contains("--workspace"), "{error}");
}

#[test]
fn an_older_pairing_is_refused_until_it_is_removed() {
    let directory = tempfile::tempdir().unwrap();
    let user_home = directory.path();
    let older = user_home.join(".neha").join("computer.json");
    std::fs::create_dir_all(older.parent().unwrap()).unwrap();
    std::fs::write(&older, "{}").unwrap();
    // Even a new pairing saved at the new place does not make the old
    // authorization material safe to leave around.
    let configuration = user_home.join(".config/neha");
    std::fs::create_dir_all(&configuration).unwrap();
    std::fs::write(configuration.join("computer.json"), "{}").unwrap();

    let error = refuse_older_pairing_from(Some(user_home))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "an older pairing is at ~/.neha/computer.json; remove it and run `neha computer pair` again"
    );

    std::fs::remove_file(&older).unwrap();
    refuse_older_pairing_from(Some(user_home)).unwrap();
}

#[test]
fn a_ticket_file_is_one_line_or_json_with_a_url() {
    let plain = parse_ticket("ticket-a\n").unwrap();
    assert_eq!(plain.ticket(), "ticket-a");
    assert_eq!(plain.url(), None);
    let json = parse_ticket(r#"{"ticket": "ticket-b", "url": "http://127.0.0.1:7337"}"#).unwrap();
    assert_eq!(json.ticket(), "ticket-b");
    assert_eq!(json.url(), Some("http://127.0.0.1:7337"));
    let without_url = parse_ticket(r#"{"ticket": "ticket-c"}"#).unwrap();
    assert_eq!(without_url.url(), None);
    assert!(parse_ticket("").is_err());
    assert!(parse_ticket("   \n  ").is_err());
    assert!(parse_ticket(r#"{"ticket": ""}"#).is_err());
    assert!(parse_ticket("{not json").is_err());
}

#[test]
fn a_workspace_inside_the_configuration_directory_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = directory.path().join(".neha");
    std::fs::create_dir(&configuration).unwrap();
    let inside = configuration.join("workspace");
    std::fs::create_dir(&inside).unwrap();

    for workspace in [
        &inside,
        &configuration,
        &configuration.join("new/workspace"),
    ] {
        let error = refuse_workspace(&configuration, workspace)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("is inside the Computer's configuration directory"),
            "{error}"
        );
    }

    let outside = directory.path().join("Neha");
    std::fs::create_dir(&outside).unwrap();
    refuse_workspace(&configuration, &outside).unwrap();

    // A symlink is resolved before the two paths are compared.
    #[cfg(unix)]
    {
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&configuration, &alias).unwrap();
        let error = refuse_workspace(&configuration, &alias.join("workspace"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("is inside the Computer's configuration directory"),
            "{error}"
        );
    }

    // The reverse overlap is refused too: a Workspace may not contain the
    // configuration directory it is supposed to hide.
    let error = refuse_workspace(&configuration, directory.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("configuration directory") && error.contains("inside the workspace"),
        "{error}"
    );
}

/// A saved CLI session that names one Space URL.
fn session_at(url: &str) -> Credential {
    Credential {
        url: url.to_owned(),
        token: "session-token".to_owned(),
        account: SignedIn {
            id: Id::new(),
            display_name: "Ada".to_owned(),
            is_admin: false,
            role: Some(neha::role::Role::Member),
        },
    }
}

#[test]
fn a_saved_session_can_name_a_remote_https_space() {
    assert_eq!(saved_session_origin(None).unwrap(), None);
    assert_eq!(
        saved_session_origin(Some(&session_at("http://127.0.0.1:7337/"))).unwrap(),
        Some("http://127.0.0.1:7337".to_owned())
    );
    assert_eq!(
        saved_session_origin(Some(&session_at("https://space.example.com/"))).unwrap(),
        Some("https://space.example.com".to_owned())
    );

    // A remote Space must use https: the session's bearer token crosses the
    // network on the ticket fallback.
    let error = saved_session_origin(Some(&session_at("http://192.0.2.1:7337")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("https"), "{error}");

    // The local-ticket path only uses a session that names this machine.
    assert_eq!(
        local_session_origin(Some("http://127.0.0.1:7337")),
        Some("http://127.0.0.1:7337".to_owned())
    );
    assert_eq!(
        local_session_origin(Some("https://space.example.com")),
        None
    );
}

#[test]
fn a_remote_space_origin_must_be_https_and_an_origin() {
    assert_eq!(
        normalized_any_space_url("https://space.example.com/").unwrap(),
        "https://space.example.com"
    );
    assert!(is_loopback_space_url("http://127.0.0.1:7337"));
    assert!(!is_loopback_space_url("https://space.example.com"));

    // A remote Space carries a pairing token and then a daemon credential;
    // plaintext is refused.
    let error = normalized_any_space_url("http://space.example.com")
        .unwrap_err()
        .to_string();
    assert!(error.contains("https"), "{error}");
    // Credentials or a path in the origin would send secrets somewhere else.
    for value in [
        "https://user:secret@space.example.com",
        "https://space.example.com/somewhere",
        "https://space.example.com?query",
    ] {
        assert!(
            normalized_any_space_url(value).is_err(),
            "{value} must be refused"
        );
    }
}

#[test]
fn a_refused_credential_ends_the_retry_loop() {
    use tokio_tungstenite::tungstenite;

    let refused = |status| {
        let response = tungstenite::http::Response::builder()
            .status(status)
            .body(None)
            .unwrap();
        anyhow::Error::new(tungstenite::Error::Http(Box::new(response)))
            .context("the upgrade failed")
    };
    // Both a dead credential and a proxy/Host policy refusal are fatal; the
    // diagnostic differs, but neither should be retried forever.
    assert!(refused_credential(&refused(401)));
    assert!(refused_credential(&refused(403)));
    assert!(refused_by_proxy(&refused(403)));
    assert!(!refused_by_proxy(&refused(401)));
    assert!(!refused_credential(&refused(502)));
    assert!(!refused_credential(&anyhow::anyhow!(
        "the Space could not be reached"
    )));
}

#[test]
fn a_pairing_code_is_shown_with_one_dash() {
    assert_eq!(display_pairing_code("ABCDEFGH"), "ABCD-EFGH");
    assert_eq!(display_pairing_code("SHORT"), "SHORT");
    // A malformed answer must not split inside a multi-byte character.
    assert_eq!(
        display_pairing_code("あいうえおかきく"),
        "あいうえ-おかきく"
    );
    assert_eq!(display_pairing_code("éééééééé"), "éééé-éééé");
    assert!(!machine_name().is_empty());
}

#[test]
fn the_saved_session_only_pairs_with_its_own_origin() {
    assert!(same_origin("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap());
    assert!(same_origin("https://localhost:7337", "https://localhost:7337/").unwrap());
    // A different port, host, or scheme is a different origin.
    for (left, right) in [
        ("http://127.0.0.1:7337", "http://127.0.0.1:7338"),
        ("http://localhost:7337", "http://127.0.0.1:7337"),
        ("https://127.0.0.1:7337", "http://127.0.0.1:7337"),
    ] {
        assert!(!same_origin(left, right).unwrap(), "{left} vs {right}");
    }

    check_saved_origin("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap();
    let error = check_saved_origin("http://127.0.0.1:7338", "http://127.0.0.1:7337")
        .unwrap_err()
        .to_string();
    assert_eq!(error, "the saved session belongs to http://127.0.0.1:7337");
}

#[test]
fn a_ticket_naming_another_space_is_refused() {
    check_ticket_space("http://127.0.0.1:7337", "http://127.0.0.1:7337/").unwrap();
    let error = check_ticket_space("http://127.0.0.1:7338", "http://127.0.0.1:7337")
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "the ticket belongs to http://127.0.0.1:7338; pair with that Space or leave out --space"
    );
}

/// Writes one Space's ticket under `<config>/spaces/<space>/`.
fn ticket_file(config: &Path, space: &str, url: &str) -> PathBuf {
    let file = config
        .join("spaces")
        .join(space)
        .join("local-computer.ticket");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(r#"{{"ticket":"{space}-ticket","url":"{url}"}}"#),
    )
    .unwrap();
    file
}

#[test]
fn one_waiting_ticket_is_used_without_asking_which_space() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(".config/neha");
    std::fs::create_dir_all(&config).unwrap();
    assert!(found_tickets(&config).is_empty());

    let only = ticket_file(&config, "one", "http://127.0.0.1:7337");
    let found = found_tickets(&config);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].file(), only);
    let chosen = choose_ticket(found, None).unwrap().unwrap();
    assert_eq!(chosen.file(), only);
    assert_eq!(chosen.ticket(), "one-ticket");
}

#[test]
fn a_ticket_is_matched_to_the_space_url() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(".config/neha");
    let first = ticket_file(&config, "a", "http://127.0.0.1:7337");
    let second = ticket_file(&config, "b", "http://127.0.0.1:7338");
    let found = found_tickets(&config);
    assert_eq!(found.len(), 2);

    // The target is normalized before matching, so a trailing slash does
    // not pick the wrong ticket.
    let chosen = choose_ticket(found, Some("http://127.0.0.1:7338/"))
        .unwrap()
        .unwrap();
    assert_eq!(chosen.file(), second);
    assert_ne!(chosen.file(), first);
}

#[test]
fn a_ticket_without_its_space_asks_the_space_instead() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(".config/neha");
    ticket_file(&config, "a", "http://127.0.0.1:7337");
    assert!(
        choose_ticket(found_tickets(&config), Some("http://127.0.0.1:7338"))
            .unwrap()
            .is_none(),
        "a ticket for another Space leaves the saved session to ask the Space"
    );

    // A corrupt file, or one without a URL, cannot be matched and is
    // skipped: another Space's in-progress file must not break pairing.
    let junk = config.join("spaces/junk/local-computer.ticket");
    std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
    std::fs::write(&junk, "not a ticket").unwrap();
    let url_less = config.join("spaces/url-less/local-computer.ticket");
    std::fs::create_dir_all(url_less.parent().unwrap()).unwrap();
    std::fs::write(&url_less, r#"{"ticket":"no-url"}"#).unwrap();
    assert_eq!(found_tickets(&config).len(), 1);
}

#[test]
fn a_ticket_outside_a_space_folder_is_not_read() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(".config/neha");
    std::fs::create_dir_all(&config).unwrap();
    // The old flat location is not a Space ticket any more.
    std::fs::write(
        config.join("local-computer.ticket"),
        r#"{"ticket":"flat","url":"http://127.0.0.1:7337"}"#,
    )
    .unwrap();
    assert!(found_tickets(&config).is_empty());

    let in_space = ticket_file(&config, "space", "http://127.0.0.1:7337");
    let found = found_tickets(&config);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].file(), in_space);
}

#[test]
fn ambiguous_tickets_are_refused_with_a_way_to_choose() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(".config/neha");
    ticket_file(&config, "first", "http://127.0.0.1:7337");
    ticket_file(&config, "second", "http://127.0.0.1:7337");
    let error = choose_ticket(found_tickets(&config), Some("http://127.0.0.1:7337"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("several Space tickets match") && error.contains("--ticket-file"),
        "{error}"
    );

    // Without --space the saved session picks; if there is no session to
    // pick, the error names both ways to choose.
    ticket_file(&config, "third", "http://127.0.0.1:7338");
    let error = choose_ticket(found_tickets(&config), None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("--space") && error.contains("--ticket-file"),
        "{error}"
    );
}

#[test]
fn hidden_roots_cover_the_configuration_and_credential_directories() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = directory.path().join("computer-home");
    let standard = directory.path().join(".config/neha");
    let credential_directory = directory.path().join("elsewhere");
    let credential = credential_directory.join("credential.json");
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();

    // The roots are directories, created up front: a credential or ticket
    // file created after the daemon started is still inside a hidden root.
    let roots =
        hidden_roots_for(configuration.clone(), Some(standard.clone()), &credential).unwrap();
    assert_eq!(
        roots,
        vec![
            canonical(&configuration),
            canonical(&standard),
            canonical(&credential_directory),
        ]
    );
    assert!(configuration.is_dir() && standard.is_dir() && credential_directory.is_dir());

    // A relative NEHA_CREDENTIAL with no HOME or XDG still has its actual
    // parent directory hidden.
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let relative = credential_path_from(
        Some(PathBuf::from("session/credential.json")),
        None,
        None,
        &cwd,
    )
    .unwrap();
    assert_eq!(relative, canonical(&cwd).join("session/credential.json"));
    let roots = hidden_roots_for(configuration.clone(), None, &relative).unwrap();
    assert!(
        roots.contains(&canonical(&cwd).join("session")),
        "{roots:?}"
    );

    // A credential with no directory to hide is an explicit error.
    let error = hidden_roots_for(configuration.clone(), None, Path::new("/"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("session credential"), "{error}");

    // The configuration directory is not duplicated when it is also the
    // standard configuration directory.
    let roots =
        hidden_roots_for(standard.clone(), None, &standard.join("credential.json")).unwrap();
    assert_eq!(roots, vec![canonical(&standard)]);
}

/// A temporary directory outside `/tmp`, which bubblewrap replaces.
#[cfg(unix)]
fn outside_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("neha-credential-")
        .tempdir_in("/var/tmp")
        .expect("a temporary directory outside /tmp")
}

#[test]
fn the_credential_path_is_always_absolute_and_needs_a_home() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let canonical_cwd = std::fs::canonicalize(&cwd).unwrap();
    let canonical_home = std::fs::canonicalize(&home).unwrap();

    // A relative XDG_CONFIG_HOME is ignored, exactly as the daemon's
    // hidden roots ignore it: the credential may not land in the working
    // directory.
    assert_eq!(
        credential_path_from(
            None,
            Some(PathBuf::from("relative-xdg")),
            Some(home.clone()),
            &cwd
        )
        .unwrap(),
        canonical_home.join(".config/neha/credential.json")
    );

    // An absolute XDG_CONFIG_HOME is still honoured.
    let absolute = directory.path().join("xdg");
    std::fs::create_dir(&absolute).unwrap();
    assert_eq!(
        credential_path_from(None, Some(absolute.clone()), Some(home.clone()), &cwd).unwrap(),
        std::fs::canonicalize(&absolute)
            .unwrap()
            .join("neha/credential.json")
    );

    // A relative NEHA_CREDENTIAL is resolved against the working
    // directory, never stored as a relative path.
    assert_eq!(
        credential_path_from(
            Some(PathBuf::from("session/credential.json")),
            None,
            None,
            &cwd
        )
        .unwrap(),
        canonical_cwd.join("session/credential.json")
    );

    // Without any of them there is nowhere safe to keep the session.
    let error = credential_path_from(None, None, None, &cwd)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "cannot find a place for the session credential: set HOME, XDG_CONFIG_HOME, or NEHA_CREDENTIAL"
    );
}

#[test]
fn an_older_relative_credential_moves_to_the_resolved_path() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let relative_xdg = PathBuf::from("relative-xdg");
    let legacy = cwd.join(&relative_xdg).join("neha/credential.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "old-session").unwrap();

    migrate_legacy_credential_for(&cwd, None, Some(relative_xdg.clone()), Some(home.clone()))
        .unwrap();
    let target =
        credential_path_from(None, Some(relative_xdg.clone()), Some(home.clone()), &cwd).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old-session");
    assert!(
        !legacy.exists(),
        "the stale cwd-relative credential must not stay behind"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the credential stays private");
    }

    // Even when the resolved credential already exists, a relative copy
    // that reappears is removed rather than left in the working directory.
    std::fs::write(&legacy, "older-session").unwrap();
    migrate_legacy_credential_for(&cwd, None, Some(relative_xdg), Some(home)).unwrap();
    assert!(!legacy.exists());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old-session");
}

#[test]
fn the_same_relative_credential_is_kept_and_loaded_twice() {
    // cwd == HOME and XDG_CONFIG_HOME == ".config" resolve to the very
    // same file. Migration must neither copy nor delete it: it is already
    // the resolved, hidden session.
    let home = tempfile::tempdir().unwrap();
    let credential = home
        .path()
        .join(".config")
        .join("neha")
        .join("credential.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
    let session = session_at("http://127.0.0.1:7337");
    std::fs::write(&credential, serde_json::to_vec(&session).unwrap()).unwrap();
    let path = credential_path_from(
        None,
        Some(PathBuf::from(".config")),
        Some(home.path().to_path_buf()),
        home.path(),
    )
    .unwrap();
    assert_eq!(path, credential);

    for _ in 0..2 {
        migrate_legacy_credential_for(
            home.path(),
            None,
            Some(PathBuf::from(".config")),
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        let loaded = load_credential_from(&path).unwrap();
        assert_eq!(loaded.token, "session-token");
        assert_eq!(
            std::fs::read(&credential).unwrap(),
            serde_json::to_vec(&session).unwrap(),
            "loading twice must keep the same session"
        );
    }
    assert!(credential.is_file(), "the only valid session is kept");
}

#[cfg(unix)]
#[test]
fn a_symlinked_legacy_credential_is_the_same_directory_entry() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let credential = home.join(".config/neha/credential.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
    std::fs::write(&credential, "same-session").unwrap();
    // A relative XDG_CONFIG_HOME that is a symlink to the real
    // configuration directory: the legacy path resolves to the same file.
    std::os::unix::fs::symlink(home.join(".config"), cwd.join("alias")).unwrap();
    let legacy = cwd.join("alias/neha/credential.json");
    assert!(legacy.exists());

    migrate_legacy_credential_for(&cwd, None, Some(PathBuf::from("alias")), Some(home.clone()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&credential).unwrap(),
        "same-session"
    );
    assert!(
        credential.is_file(),
        "a symlinked alias of the session must not be deleted"
    );
}

#[cfg(unix)]
#[test]
fn a_hard_linked_legacy_credential_is_migrated_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let session = session_at("http://127.0.0.1:7337");
    let target = home.join(".config/neha/credential.json");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, serde_json::to_vec(&session).unwrap()).unwrap();
    // Same inode, different name: the old name must be removed even though
    // the bytes are already in the resolved file.
    let legacy = cwd.join("relative-xdg/neha/credential.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::hard_link(&target, &legacy).unwrap();

    migrate_legacy_credential_for(
        &cwd,
        None,
        Some(PathBuf::from("relative-xdg")),
        Some(home.clone()),
    )
    .unwrap();

    assert!(!legacy.exists(), "the old hard-link name is removed");
    assert!(target.is_file(), "the resolved session is kept");
    let loaded = load_credential_from(&target).unwrap();
    assert_eq!(loaded.token, "session-token");
    assert_eq!(
        credential_path_from(None, Some(PathBuf::from("relative-xdg")), Some(home), &cwd).unwrap(),
        target
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_legacy_credential_removes_only_the_link() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let session = session_at("http://127.0.0.1:7337");
    let marker = directory.path().join("marker.json");
    std::fs::write(&marker, serde_json::to_vec(&session).unwrap()).unwrap();
    let legacy = cwd.join("relative-xdg/neha/credential.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&marker, &legacy).unwrap();

    // The legacy path keeps its final name; it is not the marker's path.
    let known =
        legacy_relative_credential_path(&cwd, None, Some(Path::new("relative-xdg"))).unwrap();
    assert_eq!(
        known.file_name(),
        Some(std::ffi::OsStr::new("credential.json"))
    );
    assert_eq!(
        known,
        std::fs::canonicalize(&cwd)
            .unwrap()
            .join("relative-xdg/neha/credential.json")
    );
    assert_ne!(known, marker);

    migrate_legacy_credential_for(
        &cwd,
        None,
        Some(PathBuf::from("relative-xdg")),
        Some(home.clone()),
    )
    .unwrap();

    // Only the link name is gone; the file it pointed at is untouched.
    assert!(
        std::fs::symlink_metadata(&legacy).is_err(),
        "the old symbolic link is removed"
    );
    assert_eq!(
        std::fs::read(&marker).unwrap(),
        serde_json::to_vec(&session).unwrap(),
        "the link target is never deleted or rewritten"
    );
    let target =
        credential_path_from(None, Some(PathBuf::from("relative-xdg")), Some(home), &cwd).unwrap();
    assert_eq!(
        std::fs::read(&target).unwrap(),
        serde_json::to_vec(&session).unwrap()
    );
    // Two loads keep the same session.
    for _ in 0..2 {
        let loaded = load_credential_from(&target).unwrap();
        assert_eq!(loaded.token, "session-token");
    }
}

#[cfg(unix)]
#[test]
fn a_failed_cleanup_refuses_the_migration() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let legacy_directory = cwd.join("relative-xdg").join("neha");
    let legacy = legacy_directory.join("credential.json");
    std::fs::create_dir_all(&legacy_directory).unwrap();
    std::fs::write(&legacy, "old-session").unwrap();
    std::fs::set_permissions(&legacy_directory, std::fs::Permissions::from_mode(0o500)).unwrap();

    let result =
        migrate_legacy_credential_for(&cwd, None, Some(PathBuf::from("relative-xdg")), Some(home));

    std::fs::set_permissions(&legacy_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("cannot remove the older session credential"),
        "{error}"
    );
    assert!(
        legacy.is_file(),
        "a failed cleanup leaves the file in place"
    );
}

#[test]
fn the_relative_xdg_credential_sits_inside_the_hidden_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let relative_xdg = PathBuf::from("relative-xdg");
    let credential =
        credential_path_from(None, Some(relative_xdg.clone()), Some(home.clone()), &cwd).unwrap();
    let standard = standard_config_directory(Some(relative_xdg), Some(home)).unwrap();
    let config_directory = directory.path().join("computer-config");
    std::fs::create_dir(&config_directory).unwrap();

    // The daemon's hidden roots cover the directory the CLI actually
    // writes, even though XDG_CONFIG_HOME was relative.
    let hidden = hidden_roots_for(config_directory, Some(standard.clone()), &credential).unwrap();
    assert!(
        hidden.iter().any(|root| credential.starts_with(root)),
        "the credential {credential:?} must sit under a hidden root: {hidden:?}"
    );
    let denied = denied_paths_from(None, credential.clone(), Some(standard));
    assert!(
        denied
            .iter()
            .any(|path| credential == *path || credential.starts_with(path)),
        "the credential must be denied to the file tools: {denied:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_bot_cannot_read_the_credential_the_cli_saved() {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return;
    }
    let directory = outside_tempdir();
    let cwd = directory.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    // NEHA_CREDENTIAL is relative and neither HOME nor XDG_CONFIG_HOME is
    // set: the production resolver still gives an absolute credential
    // path, and the production hidden roots must cover its parent.
    let credential = credential_path_from(
        Some(PathBuf::from("session/credential.json")),
        None,
        None,
        &cwd,
    )
    .unwrap();
    write_private(&credential, b"session-secret-must-not-leak").unwrap();
    let config_directory = directory.path().join("computer-config");
    std::fs::create_dir(&config_directory).unwrap();
    let hidden = hidden_roots_for(config_directory, None, &credential).unwrap();
    let denied = denied_paths_from(None, credential.clone(), None);
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = LocalRuntime::machine_with_hidden(workspace, hidden)
        .expect("NEHA_TEST_BWRAP is set but bubblewrap cannot start a sandbox")
        .with_denied(denied);

    // The Bot's shell cannot read the credential the CLI actually uses.
    let shell = runtime
        .execute(Action::Shell {
            command: format!("cat '{}'", credential.display()),
        })
        .await;
    assert!(
        !shell.output.text.contains("session-secret-must-not-leak"),
        "the shell must not read the CLI credential: {}",
        shell.output.text
    );

    // Neither can the file tools.
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
    assert!(!read.output.text.contains("session-secret-must-not-leak"));
    let error = runtime
        .read_bytes(&credential.to_string_lossy())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the path is hidden from this Computer"),
        "{error}"
    );
    assert!(!error.contains("session-secret-must-not-leak"));

    // The credential is still there on the host, private to the user.
    assert!(credential.is_file());
}

#[test]
fn pairing_needs_a_terminal_or_an_explicit_yes() {
    // --yes always pairs without asking, terminal or not.
    assert!(!confirmation_needed(true, false).unwrap());
    assert!(!confirmation_needed(true, true).unwrap());
    // A terminal asks, and the answer decides.
    assert!(confirmation_needed(false, true).unwrap());
    // A non-interactive run must not silently confirm.
    let error = confirmation_needed(false, false).unwrap_err().to_string();
    assert!(error.contains("--yes"), "{error}");
    assert!(error.contains("not a terminal"), "{error}");
}

#[test]
fn a_confirmation_answer_is_yes_only_when_it_says_so() {
    for answer in ["y", "Y", "yes", " YES \n"] {
        assert!(answer_is_yes(answer), "{answer:?}");
    }
    for answer in ["", "n", "no", "later", "y n"] {
        assert!(!answer_is_yes(answer), "{answer:?}");
    }
}

#[test]
fn a_space_url_is_trimmed_and_must_be_http() {
    assert_eq!(
        normalized_space_url(" http://localhost:7337/ ").unwrap(),
        "http://localhost:7337"
    );
    assert!(normalized_space_url("").is_err());
    assert!(normalized_space_url("ftp://127.0.0.1:7337").is_err());
}

#[test]
fn a_space_url_must_stay_on_this_machine() {
    for url in [
        "http://127.0.0.1:7337",
        "http://localhost:7337",
        "https://[::1]:7337",
    ] {
        assert!(normalized_space_url(url).is_ok(), "{url}");
    }
    for url in [
        "http://10.0.0.5:7337",
        "http://127.0.0.2:7337",
        "https://space.example",
    ] {
        let error = normalized_space_url(url).unwrap_err().to_string();
        assert!(
            error.contains("127.0.0.1"),
            "a remote Space must be refused clearly: {error}"
        );
    }
    assert_eq!(
        socket_url("http://[::1]:7337").unwrap(),
        "ws://[::1]:7337/api/computer/socket"
    );
}

#[test]
fn the_channel_speaks_ws_or_wss_to_the_space() {
    assert_eq!(
        socket_url("http://127.0.0.1:7337/").unwrap(),
        "ws://127.0.0.1:7337/api/computer/socket"
    );
    assert_eq!(
        socket_url("https://localhost:7337").unwrap(),
        "wss://localhost:7337/api/computer/socket"
    );
    assert!(socket_url("ftp://127.0.0.1:7337").is_err());
}

#[test]
fn file_bytes_travel_as_url_safe_base64_without_padding() {
    assert_eq!(encode_bytes(b"hello"), "aGVsbG8");
    assert_eq!(encode_bytes(&[0xfb, 0xff]), "-_8");
    assert_eq!(decode_bytes("aGVsbG8").unwrap(), b"hello");
    assert_eq!(decode_bytes("-_8").unwrap(), vec![0xfb, 0xff]);
    assert!(
        decode_bytes("aGk=").is_err(),
        "the pinned alphabet has no padding"
    );
    assert!(decode_bytes("not base64!").is_err());
}

#[test]
fn a_bot_folder_is_named_by_its_identifier() {
    let workspace = Path::new("/home/ada/.neha/workspace");
    let bot = Id::new();

    assert_eq!(
        folder_for(workspace, &bot.to_string()).unwrap(),
        workspace.join(bot.to_string())
    );
    for bot in ["", ".", "..", "a/b", "../escape", "not-an-identifier"] {
        assert!(folder_for(workspace, bot).is_err(), "{bot:?}");
    }
}

#[test]
fn a_write_is_acknowledged_with_a_succeeded_result() {
    assert_eq!(
        write_receipt(7, "out/report.bin", 5),
        DeviceFrame::Result {
            id: 7,
            status: ActionStatus::Succeeded,
            output: ToolOutput::full("wrote 5 bytes to out/report.bin"),
        }
    );
}

#[tokio::test]
async fn a_ping_is_answered_with_a_pong() {
    let config = config_at(PathBuf::from("/home/ada/.neha/workspace"));
    let runtimes = empty_runtimes();
    let current = Mutex::new(None);

    assert_eq!(
        execute_frame(&config, &[], &[], &runtimes, &current, HostFrame::Ping).await,
        DeviceFrame::Pong
    );
    assert!(runtimes_is_empty(&runtimes));
    assert!(current.lock().unwrap().is_none());
}

#[tokio::test]
async fn an_action_for_a_bot_that_is_not_an_identifier_is_refused() {
    let config = config_at(PathBuf::from("/home/ada/.neha/workspace"));
    let runtimes = empty_runtimes();
    let current = Mutex::new(None);

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &current,
        HostFrame::Action {
            id: 3,
            bot: "../escape".to_owned(),
            action: WireAction::Shell {
                command: "true".to_owned(),
            },
            working_dir: None,
            access: Default::default(),
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;

    match answer {
        DeviceFrame::Failed { id, message } => {
            assert_eq!(id, 3);
            assert!(message.contains("not an identifier"), "{message}");
        }
        other => panic!("a Bot that is not an identifier must be refused: {other:?}"),
    }
    assert!(runtimes_is_empty(&runtimes));
}

/// Serves a fake Space on the other end of the daemon's channel.
///
/// It greets nothing, sends a shell that starts and then sleeps, queues a
/// second action behind it, and leaves once the first shell has started.
/// A shell that survives the closed channel writes its later markers.
async fn fake_space(listener: tokio::net::TcpListener, folder: PathBuf, bot: Id) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let greeting = socket.next().await.unwrap().unwrap();
    assert!(matches!(greeting, Message::Text(_)));
    let started = folder.join("started");
    let first = HostFrame::Action {
        id: 1,
        bot: bot.to_string(),
        action: WireAction::Shell {
            command: format!(
                "echo started > {}; sleep 0.6; echo done > {}",
                started.display(),
                folder.join("done").display()
            ),
        },
        working_dir: None,
        access: Default::default(),
        env: Vec::new(),
        traceparent: None,
    };
    let second = HostFrame::Action {
        id: 2,
        bot: bot.to_string(),
        action: WireAction::Shell {
            command: format!("echo second > {}", folder.join("second").display()),
        },
        working_dir: None,
        access: Default::default(),
        env: Vec::new(),
        traceparent: None,
    };
    for frame in [first, second] {
        socket
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .unwrap();
    }
    for _ in 0..300 {
        if started.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(socket);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_channel_cancels_the_running_action() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let folder = workspace.path().join(bot.to_string());
    std::fs::create_dir_all(&folder).unwrap();
    let runtimes = runtimes_with(&bot.to_string(), false, LocalRuntime::new(&folder).unwrap());
    let config = config_at(workspace.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let space = tokio::spawn(fake_space(listener, folder.clone(), bot));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/computer/socket"))
            .await
            .unwrap();
    // The greeting is the handshake `connect_and_serve` does before it
    // serves; this test starts at the serving step.
    let hello = DeviceFrame::Hello {
        version: CHANNEL_VERSION,
        computer: config.computer().to_owned(),
        workspace: config.workspace().to_string_lossy().into_owned(),
        // Windows runs a managed folder without an OS fence. The Space
        // accepts a truthful false only when its operator opted in.
        sandbox: !cfg!(windows),
        // A Windows managed runtime has no OS fence, so it serves neither
        // granted folders nor a run directory nor full access.
        supports_grants: Some(!cfg!(windows)),
        supports_full_access: Some(!cfg!(windows)),
        origin: None,
    };
    socket
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();

    let error = serve_channel(&config, &[], &[], socket, runtimes)
        .await
        .expect_err("a closed channel ends the session");
    assert!(error.to_string().contains("closed"), "{error}");

    // The shell sleeps 0.6s before its second marker; wait past that.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(
        folder.join("started").is_file(),
        "the action ran before its Space left"
    );
    assert!(
        !folder.join("done").exists(),
        "a lost channel kills the action it was running"
    );
    assert!(
        !folder.join("second").exists(),
        "a frame queued behind the killed action never starts"
    );
    let _ = space.await;
}

/// A real channel close while a full-access action is starting ends the
/// action's whole process tree. The worker owns the child, so dropping the
/// execute future cannot leave a descendant in its own process group.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_channel_ends_a_full_access_actions_process_tree() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let folder = workspace.path().join(bot.to_string());
    std::fs::create_dir_all(&folder).unwrap();
    let started = folder.join("started");
    let alive = folder.join("alive");
    let config = channel_config(workspace.path());
    let (listener, address) = space_listener().await;
    let space = tokio::spawn({
        let started = started.clone();
        let alive = alive.clone();
        let bot = bot.to_string();
        async move {
            let mut socket = accept_space(listener).await;
            send_space_frame(
                &mut socket,
                HostFrame::Action {
                    id: 1,
                    bot,
                    action: WireAction::Shell {
                        command: format!(
                            "setsid sh -c 'sleep 0.6; echo alive > {}' >/dev/null 2>&1 & \
                             echo started > {}; sleep 30",
                            alive.display(),
                            started.display()
                        ),
                    },
                    working_dir: None,
                    access: AccessFrame {
                        full_access: true,
                        write_roots: Vec::new(),
                    },
                    env: Vec::new(),
                    traceparent: None,
                },
            )
            .await;
            // Leave once the shell is running, so the close races the
            // action rather than arriving before it started.
            for _ in 0..400 {
                if started.is_file() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop(socket);
        }
    });
    let daemon = connect_daemon(address, workspace.path()).await;
    let error = serve_channel(&config, &[], &[], daemon, empty_runtimes())
        .await
        .expect_err("a closed channel ends the session");
    let _ = space.await;
    assert!(error.to_string().contains("closed"), "{error}");
    assert!(started.exists(), "the action started before the Space left");

    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !alive.exists(),
        "a lost channel ends a full-access action's whole process tree"
    );
}

/// The accepting side of a fake Space's channel.
type SpaceSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Starts a fake Space's listener on loopback.
async fn space_listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    (listener, address)
}

/// Accepts the daemon's channel and reads its greeting.
async fn accept_space(listener: tokio::net::TcpListener) -> SpaceSocket {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let greeting = socket.next().await.unwrap().unwrap();
    assert!(matches!(greeting, Message::Text(_)));

    socket
}

/// Connects the daemon end of the channel and announces this Computer.
async fn connect_daemon(address: std::net::SocketAddr, workspace: &Path) -> TestSocket {
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/computer/socket"))
            .await
            .unwrap();
    let hello = DeviceFrame::Hello {
        version: CHANNEL_VERSION,
        computer: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
        workspace: workspace.to_string_lossy().into_owned(),
        sandbox: true,
        supports_grants: Some(true),
        supports_full_access: Some(true),
        origin: None,
    };
    socket
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();

    socket
}

/// Sends one frame as the Space would.
async fn send_space_frame(socket: &mut SpaceSocket, frame: HostFrame) {
    socket
        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();
}

/// A workspace holding one managed Bot runtime, ready for the daemon.
fn managed_bot(workspace: &Path, bot: Id) -> (PathBuf, TestRuntimes) {
    let folder = workspace.join(bot.to_string());
    std::fs::create_dir_all(&folder).unwrap();
    let runtimes = runtimes_with(&bot.to_string(), false, LocalRuntime::new(&folder).unwrap());

    (folder, runtimes)
}

fn channel_config(workspace: &Path) -> TestConfig {
    config_at(workspace.to_path_buf())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_that_sends_faster_than_the_worker_is_disconnected() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    let config = channel_config(workspace.path());
    let (listener, address) = space_listener().await;
    let space = tokio::spawn({
        let folder = folder.clone();
        async move {
            let mut socket = accept_space(listener).await;
            // One action keeps the worker busy while the Space floods the
            // channel with frames it cannot run yet.
            send_space_frame(
                &mut socket,
                HostFrame::Action {
                    id: 1,
                    bot: bot.to_string(),
                    action: WireAction::Shell {
                        command: format!(
                            "sleep 0.5; echo done > {}",
                            folder.join("done").display()
                        ),
                    },
                    working_dir: None,
                    access: Default::default(),
                    env: Vec::new(),
                    traceparent: None,
                },
            )
            .await;
            for id in 2..=200 {
                let frame = HostFrame::Action {
                    id,
                    bot: "not a bot".to_owned(),
                    action: WireAction::Shell {
                        command: "true".to_owned(),
                    },
                    working_dir: None,
                    access: Default::default(),
                    env: Vec::new(),
                    traceparent: None,
                };
                if socket
                    .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    let daemon = connect_daemon(address, workspace.path()).await;

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        serve_channel(&config, &[], &[], daemon, runtimes),
    )
    .await
    .expect("a flooding Space must not hang the daemon")
    .expect_err("a flooding Space is disconnected");
    assert!(error.to_string().contains("faster"), "{error}");

    space.abort();
    // The action the abandoned channel was running is killed.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !folder.join("done").exists(),
        "an abandoned action is killed with its channel"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_that_sends_huge_frames_is_disconnected() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    let config = channel_config(workspace.path());
    let (listener, address) = space_listener().await;
    let space = tokio::spawn({
        let folder = folder.clone();
        async move {
            let mut socket = accept_space(listener).await;
            send_space_frame(
                &mut socket,
                HostFrame::Action {
                    id: 1,
                    bot: bot.to_string(),
                    action: WireAction::Shell {
                        command: format!(
                            "sleep 0.5; echo done > {}",
                            folder.join("done").display()
                        ),
                    },
                    working_dir: None,
                    access: Default::default(),
                    env: Vec::new(),
                    traceparent: None,
                },
            )
            .await;
            // Each frame alone fits; together they exceed the byte budget
            // long before the worker can run them.
            let huge = "x".repeat(600 * 1024);
            for id in 2..=4 {
                let frame = HostFrame::Action {
                    id,
                    bot: "not a bot".to_owned(),
                    action: WireAction::Shell {
                        command: huge.clone(),
                    },
                    working_dir: None,
                    access: Default::default(),
                    env: Vec::new(),
                    traceparent: None,
                };
                send_space_frame(&mut socket, frame).await;
            }
        }
    });
    let daemon = connect_daemon(address, workspace.path()).await;

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        serve_channel(&config, &[], &[], daemon, runtimes),
    )
    .await
    .expect("a Space stuffing huge frames must not hang the daemon")
    .expect_err("a Space stuffing huge frames is disconnected");
    assert!(error.to_string().contains("more than"), "{error}");

    space.abort();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !folder.join("done").exists(),
        "an abandoned action is killed with its channel"
    );
}

#[tokio::test]
async fn a_file_read_uses_the_runs_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    std::fs::write(folder.join("report.txt"), "from the workspace").unwrap();
    let directory = folder.join("run");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("report.txt"), "from the run").unwrap();
    let config = channel_config(workspace.path());

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Read {
            id: 1,
            bot: bot.to_string(),
            path: "report.txt".to_owned(),
            working_dir: Some(directory.to_string_lossy().into_owned()),
            access: AccessFrame::default(),
        },
    )
    .await;
    let DeviceFrame::Bytes { bytes, .. } = answer else {
        panic!("expected bytes, got {answer:?}");
    };
    assert_eq!(decode_bytes(&bytes).unwrap(), b"from the run");
}

/// The run's directory reaches the runtime as its write root: a relative
/// write from a channel action lands there.
#[cfg(unix)]
#[tokio::test]
async fn a_run_directory_is_a_write_root_through_the_channel() {
    if std::env::var_os("NEHA_TEST_BWRAP").is_none() {
        eprintln!("skipping the real-Computer test: NEHA_TEST_BWRAP is not set");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    std::fs::create_dir(workspace.path().join(bot.to_string())).unwrap();
    let run_directory = outside_tempdir();
    let config = channel_config(workspace.path());
    let runtimes = empty_runtimes();

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::WriteFile {
                path: "note.txt".to_owned(),
                content: "from the run".to_owned(),
            },
            working_dir: Some(run_directory.path().to_string_lossy().into_owned()),
            access: AccessFrame {
                full_access: false,
                write_roots: vec![run_directory.path().to_string_lossy().into_owned()],
            },
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;
    let DeviceFrame::Result { status, .. } = answer else {
        panic!("expected a result, got {answer:?}");
    };
    assert_eq!(status, ActionStatus::Succeeded);
    assert_eq!(
        std::fs::read_to_string(run_directory.path().join("note.txt")).unwrap(),
        "from the run"
    );
}

/// A full-access Bot's run opens its folder without the fence's scan or
/// probe: what the Bot made itself, like a symlink, does not refuse it.
#[cfg(unix)]
#[tokio::test]
async fn a_full_access_action_opens_without_the_fence() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let folder = workspace.path().join(bot.to_string());
    std::fs::create_dir(&folder).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", folder.join("link")).unwrap();
    let outside = outside_tempdir();
    let target = outside.path().join("written.txt");
    let config = channel_config(workspace.path());
    let runtimes = empty_runtimes();

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::Shell {
                command: format!("printf full > {}", target.display()),
            },
            working_dir: None,
            access: AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;
    let DeviceFrame::Result { status, output, .. } = answer else {
        panic!("expected a result, got {answer:?}");
    };
    assert_eq!(status, ActionStatus::Succeeded, "{output:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "full");
    // The cache holds a full-access base for this Bot; no granted one.
    assert_eq!(runtimes_len(&runtimes), 1);
    assert!(runtimes_all_full_access(&runtimes));
}

/// Switching a Bot from full access to granted access builds a granted base
/// instead of reusing the cached full-access one: the folder the Bot made
/// links in is scanned and refused.
#[cfg(unix)]
#[tokio::test]
async fn a_granted_run_does_not_reuse_a_full_access_base() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let folder = workspace.path().join(bot.to_string());
    std::fs::create_dir(&folder).unwrap();
    let config = channel_config(workspace.path());
    let runtimes = empty_runtimes();
    let current = Mutex::new(None);

    let full = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &current,
        HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::Shell {
                command: format!("ln -s /etc/hostname {}/link", folder.display()),
            },
            working_dir: None,
            access: AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;
    assert!(
        matches!(
            full,
            DeviceFrame::Result {
                status: ActionStatus::Succeeded,
                ..
            }
        ),
        "{full:?}"
    );

    let granted = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &current,
        HostFrame::Action {
            id: 2,
            bot: bot.to_string(),
            action: WireAction::Shell {
                command: "true".to_owned(),
            },
            working_dir: None,
            access: AccessFrame::default(),
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;
    match granted {
        DeviceFrame::Failed { message, .. } => {
            assert!(message.contains("symlink"), "{message}");
        }
        other => panic!("a granted run must scan the folder: {other:?}"),
    }
    // The failed granted open is not cached; the cached base stays the
    // full-access one, never silently serving a granted run.
    assert_eq!(runtimes_len(&runtimes), 1);
    assert!(runtimes_all_full_access(&runtimes));
}

/// A read that verifies an interrupted full-access run's write uses the
/// run's own access, so it reaches a path only full access can read.
#[cfg(unix)]
#[tokio::test]
async fn a_full_access_read_reaches_a_hidden_path() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    std::fs::create_dir(workspace.path().join(bot.to_string())).unwrap();
    let hidden = outside_tempdir();
    let secret = hidden.path().join("computer.json");
    std::fs::write(&secret, "dummy-credential").unwrap();
    let config = channel_config(workspace.path());
    let runtimes = empty_runtimes();

    let answer = execute_frame(
        &config,
        &[hidden.path().to_path_buf()],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Read {
            id: 1,
            bot: bot.to_string(),
            path: secret.to_string_lossy().into_owned(),
            working_dir: None,
            access: AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
        },
    )
    .await;
    let DeviceFrame::Bytes { bytes, .. } = answer else {
        panic!("expected bytes, got {answer:?}");
    };
    assert_eq!(decode_bytes(&bytes).unwrap(), b"dummy-credential");
}

/// A File delivery is chosen by the Space, so full access never widens it
/// beyond the Bot's folder.
#[cfg(unix)]
#[tokio::test]
async fn a_full_access_file_delivery_stays_inside_the_bot_folder() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let folder = workspace.path().join(bot.to_string());
    std::fs::create_dir(&folder).unwrap();
    let outside = outside_tempdir();
    let target = outside.path().join("delivered.bin");
    let config = channel_config(workspace.path());
    let runtimes = empty_runtimes();
    let bytes = encode_bytes(b"hello");

    let refused = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Write {
            id: 1,
            bot: bot.to_string(),
            path: target.to_string_lossy().into_owned(),
            bytes: bytes.clone(),
            access: AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
        },
    )
    .await;
    assert!(
        matches!(refused, DeviceFrame::Failed { .. }),
        "a delivery outside the folder is refused: {refused:?}"
    );
    assert!(!target.exists());

    let delivered = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Write {
            id: 2,
            bot: bot.to_string(),
            path: folder.join("delivered.bin").to_string_lossy().into_owned(),
            bytes,
            access: AccessFrame {
                full_access: true,
                write_roots: Vec::new(),
            },
        },
    )
    .await;
    assert!(
        matches!(
            delivered,
            DeviceFrame::Result {
                status: ActionStatus::Succeeded,
                ..
            }
        ),
        "{delivered:?}"
    );
    assert_eq!(
        std::fs::read(folder.join("delivered.bin")).unwrap(),
        b"hello"
    );
}

/// The run directory from the frame names the folder it says, trailing
/// space included: the daemon does not trim a path the Space normalized.
#[cfg(unix)]
#[tokio::test]
async fn a_run_directory_keeps_the_trailing_space_of_its_name() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    let run = folder.join("run");
    let run_space = folder.join("run ");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&run_space).unwrap();
    let config = channel_config(workspace.path());

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::WriteFile {
                path: "note.txt".to_owned(),
                content: "space".to_owned(),
            },
            working_dir: Some(run_space.to_string_lossy().into_owned()),
            access: AccessFrame::default(),
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;
    assert!(
        matches!(
            answer,
            DeviceFrame::Result {
                status: ActionStatus::Succeeded,
                ..
            }
        ),
        "{answer:?}"
    );
    assert_eq!(
        std::fs::read_to_string(run_space.join("note.txt")).unwrap(),
        "space"
    );
    assert!(
        !run.join("note.txt").exists(),
        "the trailing space must not be trimmed into the other folder"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_that_stops_reading_is_disconnected() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    std::fs::write(folder.join("big.bin"), vec![7_u8; 4 * 1024 * 1024]).unwrap();
    let config = channel_config(workspace.path());
    let (listener, address) = space_listener().await;
    let space = tokio::spawn({
        async move {
            let mut socket = accept_space(listener).await;
            for id in 1..=5 {
                send_space_frame(
                    &mut socket,
                    HostFrame::Read {
                        id,
                        bot: bot.to_string(),
                        path: "big.bin".to_owned(),
                        working_dir: None,
                        access: AccessFrame::default(),
                    },
                )
                .await;
            }
            // This Space never reads an answer.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    let daemon = connect_daemon(address, workspace.path()).await;

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        serve_channel(&config, &[], &[], daemon, runtimes),
    )
    .await
    .expect("a Space that stops reading must not hang the daemon")
    .expect_err("a Space that stops reading is disconnected");
    assert!(error.to_string().contains("writer"), "{error}");

    space.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_that_stops_sending_ends_the_channel() {
    let workspace = tempfile::tempdir().unwrap();
    let config = channel_config(workspace.path());
    let (listener, address) = space_listener().await;
    let space = tokio::spawn(async move {
        let socket = accept_space(listener).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(socket);
    });
    let daemon = connect_daemon(address, workspace.path()).await;

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        serve_channel(&config, &[], &[], daemon, empty_runtimes()),
    )
    .await
    .expect("a quiet Space must not hang the daemon")
    .expect_err("a closed channel ends the session");
    assert!(error.to_string().contains("closed"), "{error}");

    let _ = space.await;
}

/// A skill script the Space delivers runs through the channel: the daemon
/// runs the delivered bytes and answers with the shell's output.
#[cfg(unix)]
#[tokio::test]
async fn a_skill_script_runs_through_the_channel() {
    let workspace = tempfile::tempdir().unwrap();
    let bot = Id::new();
    let (folder, runtimes) = managed_bot(workspace.path(), bot);
    let config = channel_config(workspace.path());
    let content = "#!/bin/sh\nprintf 'hello %s' \"$1\"\n";

    let answer = execute_frame(
        &config,
        &[],
        &[],
        &runtimes,
        &Mutex::new(None),
        HostFrame::Action {
            id: 1,
            bot: bot.to_string(),
            action: WireAction::RunScript {
                name: "greet.sh".to_owned(),
                content: content.to_owned(),
                arguments: vec!["space".to_owned()],
            },
            working_dir: None,
            access: AccessFrame::default(),
            env: Vec::new(),
            traceparent: None,
        },
    )
    .await;

    let DeviceFrame::Result { status, output, .. } = answer else {
        panic!("expected a result, got {answer:?}");
    };
    assert_eq!(status, ActionStatus::Succeeded, "{output:?}");
    assert!(output.text.contains("hello space"), "{output:?}");
    // A staged script is unique per action and gone once it ran.
    let staging = folder.join(".neha/skills");
    assert!(
        !staging.exists() || std::fs::read_dir(&staging).unwrap().next().is_none(),
        "the delivered script leaves nothing behind"
    );
}
