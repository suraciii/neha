//! The Space serves a browser client on a per-Space hostname and admits it with a cookie.

use futures_util::{SinkExt as _, StreamExt as _};
use neha::surface::{ClientFrame, PROTOCOL_VERSION, ServerFrame};
use neha_space::{Home, Running, start_with_ui};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn browser_authority(running: &Running) -> String {
    format!("{}:{}", running.browser_host, running.addr.port())
}

fn browser_get(running: &Running, path: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .get(format!("{}{path}", running.url()))
        .header("host", browser_authority(running))
}

fn browser_post(running: &Running, path: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{}{path}", running.url()))
        .header("host", browser_authority(running))
        .header("origin", running.browser_url())
}

fn fresh_ui(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let ui = dir.path().join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html><title>Neha</title>").unwrap();
    ui
}

#[tokio::test(flavor = "multi_thread")]
async fn the_space_serves_the_browser_client() {
    let dir = tempfile::tempdir().unwrap();
    let ui = dir.path().join("ui");
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>Neha</title><div id=\"root\"></div>",
    )
    .unwrap();
    std::fs::write(ui.join("assets/app.js"), "console.log('neha')").unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();

    let root = browser_get(&running, "/").send().await.unwrap();
    assert_eq!(root.status(), reqwest::StatusCode::OK);
    assert!(
        root.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(root.text().await.unwrap().contains("<title>Neha"));

    let invite = browser_get(&running, "/invite").send().await.unwrap();
    assert_eq!(invite.status(), reqwest::StatusCode::OK);
    assert!(invite.text().await.unwrap().contains("<title>Neha"));

    let asset = browser_get(&running, "/assets/app.js")
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), reqwest::StatusCode::OK);
    assert!(asset.text().await.unwrap().contains("console.log"));

    let shared_alias = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{}/", running.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        shared_alias.status(),
        reqwest::StatusCode::TEMPORARY_REDIRECT
    );
    assert_eq!(
        shared_alias.headers()["location"],
        format!("{}/", running.browser_url())
    );

    running.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_out_of_the_client_directory_is_not_served() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let secret = dir.path().join("outside.txt");
    std::fs::write(&secret, "do not serve").unwrap();
    symlink(&secret, ui.join("leak.txt")).unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();

    let response = browser_get(&running, "/leak.txt").send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_guard_covers_served_pages() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();

    let response = reqwest::Client::new()
        .get(format!("{}/", running.url()))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn relative_home_paths_are_isolated_by_canonical_identity() {
    let original = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();

    let mut hosts = Vec::new();
    for cwd in [&first, &second] {
        std::env::set_current_dir(cwd).unwrap();
        let home = Home::open("space").unwrap();
        let running = start_with_ui(home, 0, None).await.unwrap();
        hosts.push(running.browser_host.clone());
        running.shutdown().await;
    }
    std::env::set_current_dir(original).unwrap();
    assert_ne!(hosts[0], hosts[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_spaces_use_different_browser_hosts() {
    let mut hosts = Vec::new();
    for _ in 0..2 {
        let dir = tempfile::tempdir().unwrap();
        let ui = fresh_ui(&dir);
        let home = Home::open(dir.path().join("home")).unwrap();
        let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
        hosts.push(running.browser_host.clone());
        running.shutdown().await;
    }
    assert_ne!(hosts[0], hosts[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_directory_that_contains_the_home_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("space");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><title>Neha</title>",
    )
    .unwrap();
    let opened = Home::open(home).unwrap();
    let result = start_with_ui(opened, 0, Some(dir.path().to_path_buf())).await;
    assert!(matches!(
        result,
        Err(neha_space::ServerError::UiOverlapsHome(_))
    ));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_point_outside_the_client_directory_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let ui = dir.path().join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    let outside = dir.path().join("outside.html");
    std::fs::write(&outside, "<!doctype html><title>Outside</title>").unwrap();
    symlink(&outside, ui.join("index.html")).unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    let result = start_with_ui(home, 0, Some(ui)).await;
    assert!(matches!(result, Err(neha_space::ServerError::Ui(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_spaces_on_one_host_use_different_session_cookie_names() {
    let mut names = Vec::new();
    for _ in 0..2 {
        let dir = tempfile::tempdir().unwrap();
        let ui = fresh_ui(&dir);
        let home = Home::open(dir.path().join("home")).unwrap();
        let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
        let secret = running
            .invite
            .clone()
            .expect("a fresh Space invites")
            .split_once('#')
            .unwrap()
            .1
            .to_owned();
        let response = browser_post(&running, "/api/join")
            .json(&json!({
                "secret": secret,
                "display_name": "Ada",
                "password": "correct horse battery",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let cookie = response.headers()["set-cookie"].to_str().unwrap();
        assert!(!cookie.contains("Domain="));
        names.push(cookie.split('=').next().unwrap().to_owned());
        running.shutdown().await;
    }
    assert_ne!(names[0], names[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_client_entry_point_is_a_start_error() {
    let dir = tempfile::tempdir().unwrap();
    let ui = dir.path().join("empty-ui");
    std::fs::create_dir_all(&ui).unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    let result = start_with_ui(home, 0, Some(ui)).await;
    assert!(matches!(result, Err(neha_space::ServerError::Ui(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_api_path_does_not_fall_back_to_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();

    for path in ["/api", "/api/", "/api/not-a-route"] {
        let response = reqwest::get(format!("{}{path}", running.url()))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["code"], "not_found");
    }

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_joins_without_a_cookie_and_keeps_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;

    let response = reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["token"].is_string());

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_same_origin_join_sets_a_host_only_session_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;

    let response = browser_post(&running, "/api/join")
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .expect("the join response sets a cookie")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.contains("neha_session_"));
    assert!(cookie.contains("HttpOnly"));
    assert!(!cookie.contains("Domain="));
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["display_name"], "Ada");
    assert!(body.get("token").is_none(), "the browser receives no token");
    let token = cookie.split(';').next().unwrap().to_owned();

    let me = browser_get(&running, "/api/me")
        .header("cookie", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), reqwest::StatusCode::OK);
    assert_eq!(
        me.json::<serde_json::Value>().await.unwrap()["display_name"],
        "Ada"
    );

    let shared_alias = reqwest::Client::new()
        .get(format!("{}/api/me", running.url()))
        .header("origin", running.url())
        .header("cookie", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(shared_alias.status(), reqwest::StatusCode::FORBIDDEN);

    let socket_url = format!("ws://{}/api/socket", running.addr);
    let mut request = socket_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("host", browser_authority(&running).parse().unwrap());
    request
        .headers_mut()
        .insert("origin", running.browser_url().parse().unwrap());
    request
        .headers_mut()
        .insert("cookie", token.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capabilities: Vec::new(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let frame: ServerFrame =
        serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert!(matches!(frame, ServerFrame::Hello { version, .. } if version == PROTOCOL_VERSION));

    socket.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_sets_the_session_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;
    reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap();

    let response = browser_post(&running, "/api/login")
        .json(&json!({"display_name": "Ada", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .expect("the login response sets a cookie")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.contains("neha_session_"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));

    let foreign = browser_get(&running, "/api/me")
        .header("origin", "http://evil.example")
        .header("cookie", cookie.split(';').next().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), reqwest::StatusCode::FORBIDDEN);

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_origin_still_needs_a_bearer_token() {
    let dir = tempfile::tempdir().unwrap();
    let ui = fresh_ui(&dir);
    let home = Home::open(dir.path().join("home")).unwrap();
    let running = start_with_ui(home, 0, Some(ui)).await.unwrap();

    let response = reqwest::Client::new()
        .get(format!("{}/healthz", running.url()))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    running.shutdown().await;
}
