//! The CLI client's integration tests: pairing, the command socket, and the
//! run wait.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use neha::file::MAX_FILE_BYTES;
use neha::id::Id;
use neha::surface::PROTOCOL_VERSION;
use tokio_tungstenite::tungstenite::Message;

use neha_app::client::test_support::{
    disposition_name, pair_body, pair_url, read_for_upload, socket_url, ticket_url,
    wait_for_run_within,
};
use neha_app::client::{Credential, PairedComputer, SignedIn, pair_with_ticket, request_ticket};

#[test]
fn a_public_origin_uses_wss() {
    assert_eq!(
        socket_url("https://space.example.com"),
        "wss://space.example.com"
    );
    assert_eq!(
        socket_url("https://space.example.com:8443"),
        "wss://space.example.com:8443"
    );
    assert_eq!(socket_url("http://127.0.0.1:7337"), "ws://127.0.0.1:7337");
}

/// The CLI's credential for a Space at this address.
fn credential_at(address: std::net::SocketAddr) -> Credential {
    Credential {
        url: format!("http://{address}"),
        token: "token".to_owned(),
        account: SignedIn {
            id: Id::new(),
            display_name: "Ada".to_owned(),
            is_admin: false,
        },
    }
}

/// Waits for one run against a Space that never answers, and returns the
/// error the wait produced.
async fn timed_out_run_wait(credential: &Credential) -> anyhow::Error {
    tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_run_within(credential, Id::new(), Duration::from_millis(300), |_| {}),
    )
    .await
    .expect("the wait must end")
    .expect_err("a Space that never answers must end the wait")
}

/// Serves a Space that greets the CLI and then never answers a command.
async fn silent_space(listener: tokio::net::TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    loop {
        let Some(Ok(message)) = socket.next().await else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        if frame["type"] == "hello" {
            let greeting = serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION});
            socket
                .send(Message::Text(greeting.to_string().into()))
                .await
                .unwrap();
        }
    }
}

/// Serves a Space that answers every command with a waiting run view.
async fn waiting_space(listener: tokio::net::TcpListener, run: Id, approval: Id) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    loop {
        let Some(Ok(message)) = socket.next().await else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        match frame["type"].as_str() {
            Some("hello") => {
                let greeting = serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION});
                socket
                    .send(Message::Text(greeting.to_string().into()))
                    .await
                    .unwrap();
            }
            Some("command") => {
                let result = serde_json::json!({
                    "id": run,
                    "status": "waiting",
                    "approval": approval,
                });
                let reply = serde_json::json!({
                    "type": "confirmed",
                    "id": frame["id"].clone(),
                    "result": result,
                });
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_waiting_run_returns_before_the_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let run = Id::new();
    let approval = Id::new();
    let space = tokio::spawn(waiting_space(listener, run, approval));
    let started = std::time::Instant::now();

    let view = wait_for_run_within(&credential_at(address), run, Duration::from_secs(5), |_| {})
        .await
        .unwrap();

    assert_eq!(view["status"], "waiting");
    assert_eq!(
        view["approval"].as_str(),
        Some(approval.to_string().as_str())
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a waiting run ends the wait immediately, not at the deadline"
    );
    space.abort();
}

#[test]
fn a_file_larger_than_the_limit_is_refused_before_upload() {
    let directory = tempfile::tempdir().unwrap();
    let big = directory.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    std::fs::write(directory.path().join("small.txt"), "hi").unwrap();

    let error = read_for_upload(&big).unwrap_err().to_string();
    assert!(error.contains("at most"), "{error}");
    assert_eq!(
        read_for_upload(&directory.path().join("small.txt")).unwrap(),
        b"hi"
    );
    assert!(read_for_upload(directory.path()).is_err());
}

#[test]
fn the_local_pairing_paths_and_body_are_the_pinned_ones() {
    assert_eq!(
        ticket_url("http://127.0.0.1:7337/"),
        "http://127.0.0.1:7337/api/computer/ticket"
    );
    assert_eq!(
        pair_url("http://127.0.0.1:7337/"),
        "http://127.0.0.1:7337/api/computer/pair"
    );
    assert_eq!(
        pair_body("ticket-a"),
        serde_json::json!({"ticket": "ticket-a"}),
        "the pairing body carries the ticket and nothing else"
    );
}

#[test]
fn a_disposition_names_the_file() {
    assert_eq!(
        disposition_name("attachment; filename*=UTF-8''report.pdf").as_deref(),
        Some("report.pdf")
    );
    assert_eq!(
        disposition_name("attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A%20v2.txt").as_deref(),
        Some("报告 v2.txt")
    );
    assert_eq!(disposition_name("attachment").as_deref(), None);
    assert_eq!(
        disposition_name("attachment; filename*=UTF-8''%zz").as_deref(),
        None
    );
}

#[tokio::test]
async fn a_space_that_never_answers_ends_the_run_wait() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let space = tokio::spawn(silent_space(listener));

    let started = std::time::Instant::now();
    let error = timed_out_run_wait(&credential_at(address)).await;

    assert!(error.to_string().contains("did not end"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait took {:?}",
        started.elapsed()
    );
    space.abort();
}

#[tokio::test]
async fn a_space_that_never_opens_ends_the_run_wait() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let space = tokio::spawn(async move {
        // Accept the connection and never answer the opening handshake.
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let started = std::time::Instant::now();
    let error = timed_out_run_wait(&credential_at(address)).await;

    assert!(error.to_string().contains("did not end"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait took {:?}",
        started.elapsed()
    );
    space.abort();
}

/// Serves a 302 to `location` for every request.
async fn redirecting_space(
    location: String,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::http::header::LOCATION;
    use axum::response::IntoResponse;
    use axum::routing::any;

    let app = Router::new().fallback(any(move || {
        let location = location.clone();
        async move { (StatusCode::FOUND, [(LOCATION, location)]).into_response() }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (address, task)
}

#[tokio::test]
async fn pairing_requests_do_not_follow_redirects() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A redirect target that counts every connection it receives.
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let target_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = target.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    let (address, space) = redirecting_space(format!("http://{target_address}")).await;
    let space_url = format!("http://{address}");

    let ticket = request_ticket(&space_url, "token").await;
    assert!(
        ticket.is_err(),
        "a redirecting ticket request must not be accepted"
    );
    let paired = pair_with_ticket(&space_url, "ticket-a").await;
    assert!(
        paired.is_err(),
        "a redirecting pairing must not be accepted"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "pairing requests must not follow a redirect"
    );

    space.abort();
    target_task.abort();
}

/// Runs only as the child half of `pairing_requests_bypass_a_system_proxy`.
///
/// The parent starts a fake Space and points the proxy variables at a
/// listener it counts; this child then pairs through the real client. It
/// returns at once when the normal suite runs it.
#[tokio::test]
async fn a_local_pairing_client_child_pairs_around_the_proxy() {
    let Ok(space) = std::env::var("NEHA_PAIRING_CHILD_SPACE") else {
        return;
    };

    let ticket = request_ticket(&space, "token")
        .await
        .expect("the direct ticket request must succeed");
    assert_eq!(ticket, "ticket-from-space");
    assert_eq!(
        pair_with_ticket(&space, "ticket-from-space").await.unwrap(),
        PairedComputer {
            computer: "computer-from-space".to_owned(),
            credential: "credential-from-space".to_owned(),
        }
    );
}

#[test]
fn pairing_requests_bypass_a_system_proxy() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::routing::post;
    use serde_json::json;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // A proxy that counts every connection it receives.
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let proxy = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = proxy_listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });

        // A fake Space on loopback that answers both local pairing routes.
        let app = Router::new()
            .route(
                "/api/computer/ticket",
                post(|| async { Json(json!({"ticket": "ticket-from-space"})) }),
            )
            .route(
                "/api/computer/pair",
                post(|| async {
                    Json(json!({
                        "computer": "computer-from-space",
                        "credential": "credential-from-space",
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let space_address = listener.local_addr().unwrap();
        let space = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let proxy_url = format!("http://{proxy_address}");
        let space_url = format!("http://{space_address}");
        let child = tokio::task::spawn_blocking(move || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("client::a_local_pairing_client_child_pairs_around_the_proxy")
                .arg("--nocapture")
                .env("HTTP_PROXY", &proxy_url)
                .env("HTTPS_PROXY", &proxy_url)
                .env("ALL_PROXY", &proxy_url)
                .env("NEHA_PAIRING_CHILD_SPACE", &space_url)
                .output()
                .expect("the child test process starts")
        })
        .await
        .unwrap();

        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            child.status.success() && stdout.contains("1 passed"),
            "the child pairing failed: {stdout}\n{stderr}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the pairing client must not touch the system proxy"
        );
        space.abort();
        proxy.abort();
    });
}
