//! Worker-backed Hosted Computers: the Space's provisioning calls.
//!
//! A fake Supervisor records the calls the Space makes. A real worker is not
//! started here: the worker channel and its tool execution are the
//! container-deployment test, and this test proves the control-plane order.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use neha::id::Id;
use neha::surface::{
    BotCreate, ClientFrame, CommandCall, MessageSend, PROTOCOL_VERSION, ServerFrame,
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use neha_space::home::Home;
use neha_space::start_with_worker_config;
use neha_space::workers::WorkerConfig;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
struct Call {
    path: String,
    authorization: String,
    body: Option<Value>,
}

async fn open_socket(url: &str, token: &str) -> Socket {
    let mut request = format!("ws://{url}/api/socket")
        .replace("http://", "")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
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
    let _ = next_frame(&mut socket).await;
    socket
}

async fn next_frame(socket: &mut Socket) -> ServerFrame {
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            continue;
        };
        return serde_json::from_str(text.as_str()).unwrap();
    }
}

async fn command(socket: &mut Socket, command: CommandCall) -> Value {
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Command {
                id: Id::new(),
                command,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    loop {
        match next_frame(socket).await {
            ServerFrame::Confirmed { result, .. } => return result,
            ServerFrame::Error { message, .. } => panic!("{message}"),
            ServerFrame::Heartbeat { .. }
            | ServerFrame::Accepted { .. }
            | ServerFrame::Committed { .. }
            | ServerFrame::Event { .. }
            | ServerFrame::Snapshot { .. } => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn fake_supervisor() -> (String, Arc<Mutex<Vec<Call>>>, tokio::task::JoinHandle<()>) {
    fake_supervisor_with(false).await
}

async fn fake_supervisor_with(
    fail_first_start: bool,
) -> (String, Arc<Mutex<Vec<Call>>>, tokio::task::JoinHandle<()>) {
    use axum::Json;
    use axum::Router;
    use axum::routing::post;

    let calls = Arc::new(Mutex::new(Vec::new()));
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = Arc::clone(&calls);
    let second = Arc::clone(&calls);
    let starts_for_action = Arc::clone(&starts);
    let app = Router::new()
        .route(
            "/v1/workers",
            post(
                move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let recorded = Arc::clone(&recorded);
                    async move {
                        recorded.lock().unwrap().push(Call {
                            path: "/v1/workers".to_owned(),
                            authorization: headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                            body: Some(body),
                        });
                        Json(json!({"container": "fake-worker"}))
                    }
                },
            ),
        )
        .route(
            "/v1/workers/{computer}/{action}",
            post(
                move |axum::extract::Path((computer, action)): axum::extract::Path<(
                    String,
                    String,
                )>,
                      headers: axum::http::HeaderMap| {
                    let recorded = Arc::clone(&second);
                    let starts = Arc::clone(&starts_for_action);
                    async move {
                        recorded.lock().unwrap().push(Call {
                            path: format!("/v1/workers/{computer}/{action}"),
                            authorization: headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                            body: None,
                        });
                        if action == "start"
                            && fail_first_start
                            && starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                        {
                            return axum::http::StatusCode::BAD_GATEWAY;
                        }
                        axum::http::StatusCode::NO_CONTENT
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), calls, task)
}

async fn fake_provider() -> (String, tokio::task::JoinHandle<()>) {
    use axum::Router;
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::Response;
    use axum::routing::post;

    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Response::builder()
                .header(CONTENT_TYPE, "text/event-stream")
                .body(Body::from(body))
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

fn worker_home(directory: &tempfile::TempDir) -> Home {
    neha_space::test_support::server::open_home_with_ticket_directory(
        directory.path(),
        directory.path().join("config"),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_is_created_and_started_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let (supervisor, calls, _supervisor_task) = fake_supervisor().await;
    let running = start_with_worker_config(
        worker_home(&dir),
        0,
        Some(WorkerConfig::for_test(
            supervisor,
            "supervisor-secret",
            false,
            Duration::from_secs(3600),
        )),
    )
    .await
    .unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;
    let session: Value = reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = session["token"].as_str().unwrap().to_owned();
    let mut socket = open_socket(&running.url(), &token).await;

    let (provider, _provider_task) = fake_provider().await;
    command(
        &mut socket,
        CommandCall::ProviderAdd(neha::surface::ProviderAdd {
            protocol: neha::surface::ProviderProtocol::ChatCompletions,
            base_url: provider,
            api_key: "sk-test".to_owned(),
            models: vec![neha::surface::ModelSpec {
                name: "test-model".to_owned(),
                context_limit: 128_000,
                max_output: 8_192,
                reads_images: false,
                thinks: false,
                tools: true,
                thinking_levels: Vec::new(),
            }],
            default_model: Some("test-model".to_owned()),
            thinking: None,
            vendor: None,
            discover: false,
        }),
    )
    .await;

    let bot = command(
        &mut socket,
        CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: None,
            thinking: None,
            model: None,
        }),
    )
    .await;
    let computer: Id = serde_json::from_value(bot["computer"]["id"].clone()).unwrap();
    let chat: Id = serde_json::from_value(bot["chat"]["id"].clone()).unwrap();
    assert_eq!(bot["computer"]["online"], false);

    // The manager creates the worker without starting it: a Hosted Computer
    // is offline until work wakes it.
    let created = wait_for_call(&calls, "/v1/workers").await;
    assert_eq!(created.authorization, "Bearer supervisor-secret");
    let body = created.body.expect("the create call carries a body");
    assert_eq!(body["computer"], json!(computer));
    assert_eq!(body["workspace"], json!(format!("workspaces/{computer}")));
    assert!(
        body["token"]
            .as_str()
            .is_some_and(|token| !token.is_empty()),
        "the worker token is empty"
    );
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.path.ends_with("/start")),
        "the manager started a worker with no work"
    );

    let sent = command(
        &mut socket,
        CommandCall::MessageSend(MessageSend {
            chat,
            text: "work".to_owned(),
            file: None,
            cwd: None,
            cwd_writable: None,
        }),
    )
    .await;
    assert_eq!(sent["status"], "queued");
    let run: Id = serde_json::from_value(sent["run"].clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let shown = command(&mut socket, CommandCall::RunShow { run }).await;
    assert_eq!(shown["status"], "queued");
    let started = wait_for_call(&calls, &format!("/v1/workers/{computer}/start")).await;
    assert_eq!(started.authorization, "Bearer supervisor-secret");

    // The token is private state beside the record, so a restart hands the
    // same credential to the same worker.
    let token_file = running
        .test_fields()
        .home
        .root()
        .join("state")
        .join("workers")
        .join(format!("{computer}.token"));
    assert!(token_file.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_removed_outside_the_manager_is_created_again() {
    let dir = tempfile::tempdir().unwrap();
    let (supervisor, calls, _supervisor_task) = fake_supervisor_with(true).await;
    let running = start_with_worker_config(
        worker_home(&dir),
        0,
        Some(WorkerConfig::for_test(
            supervisor,
            "supervisor-secret",
            true,
            Duration::from_secs(3600),
        )),
    )
    .await
    .unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;
    let session: Value = reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = session["token"].as_str().unwrap().to_owned();
    let mut socket = open_socket(&running.url(), &token).await;

    command(
        &mut socket,
        CommandCall::ComputerCreate(neha::surface::ComputerCreate {
            name: "Worker".to_owned(),
            sharing: neha::record::ComputerSharing::Shared,
        }),
    )
    .await;

    // The first start is refused, as when the container was removed while the
    // Space was running. The manager forgets its provisioning and creates the
    // worker again before starting it.
    let created = wait_for_call(&calls, "/v1/workers").await;
    assert_eq!(created.authorization, "Bearer supervisor-secret");
    let computer = created.body.unwrap()["computer"].clone();
    let expected = computer.as_str().unwrap().to_owned();
    // Make the recorded token unusable before the manager re-creates the
    // worker. The retry must replace it instead of failing on create_new.
    let token_path = running
        .test_fields()
        .home
        .root()
        .join("state")
        .join("workers")
        .join(format!("{expected}.token"));
    std::fs::write(&token_path, "").unwrap();
    wait_for_call(&calls, &format!("/v1/workers/{expected}/start")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.path == "/v1/workers")
        .count()
        < 2
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Space did not create the worker again"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let second = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.path == "/v1/workers")
        .nth(1)
        .cloned()
        .unwrap();
    assert!(
        second.body.unwrap()["token"]
            .as_str()
            .is_some_and(|token| !token.is_empty()),
        "the retried worker got an empty token"
    );
    assert!(!std::fs::read_to_string(&token_path).unwrap().is_empty());

    running.shutdown().await;
}

async fn wait_for_call(calls: &Arc<Mutex<Vec<Call>>>, path: &str) -> Call {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(call) = calls.lock().unwrap().iter().find(|call| call.path == path) {
            return call.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Space did not call {path}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_worker_stops_without_work() {
    let dir = tempfile::tempdir().unwrap();
    let (supervisor, calls, _supervisor_task) = fake_supervisor().await;
    let running = start_with_worker_config(
        worker_home(&dir),
        0,
        Some(WorkerConfig::for_test(
            supervisor,
            "supervisor-secret",
            false,
            Duration::ZERO,
        )),
    )
    .await
    .unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");
    let secret = invite.split_once('#').unwrap().1;
    let session: Value = reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({
            "secret": secret,
            "display_name": "Ada",
            "password": "correct horse battery",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = session["token"].as_str().unwrap().to_owned();
    let mut socket = open_socket(&running.url(), &token).await;

    let computer = command(
        &mut socket,
        CommandCall::ComputerCreate(neha::surface::ComputerCreate {
            name: "Worker".to_owned(),
            sharing: neha::record::ComputerSharing::Shared,
        }),
    )
    .await;
    let computer: Id = serde_json::from_value(computer["id"].clone()).unwrap();
    let _created = wait_for_call(&calls, "/v1/workers").await;
    let stopped = wait_for_call(&calls, &format!("/v1/workers/{computer}/stop")).await;
    assert_eq!(stopped.authorization, "Bearer supervisor-secret");
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.path.ends_with("/start")),
        "an idle worker was started"
    );

    running.shutdown().await;
}
