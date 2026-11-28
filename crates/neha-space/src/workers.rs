//! Provisioning and lifecycle for worker-backed Hosted Computers.
//!
//! A containerized Hosted Space has no bubblewrap and no Docker socket. A
//! Supervisor owns the Docker API, creates one worker container per Hosted
//! Computer, and starts or stops it on this process's request. The Space
//! records the same Computer either way; only the runtime behind it changes.
//!
//! A worker gets a Computer token from the Space's private state. The token is
//! generated once, stored beside the record, and handed to the Supervisor with
//! a Workspace path relative to the deployment's data directory.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use neha::id::Id;
use serde_json::json;

use crate::diagnostics::scrub_error;
use crate::server::SpaceServer;

/// Returns the Supervisor's base URL when the deployment named a usable one.
fn supervisor_url_from_env() -> Option<String> {
    let value = std::env::var("NEHA_WORKER_SUPERVISOR_URL").ok()?;
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }

    Some(value.to_owned())
}

/// The environment settings of a worker-backed Hosted Space.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    supervisor_url: String,
    supervisor_token: String,
    always_on: bool,
    idle: Duration,
}

impl WorkerConfig {
    /// Reads the deployment's worker settings, when it has any.
    ///
    /// Without a Supervisor URL the Space keeps the process-local Hosted
    /// Computer runtime: a Local Space and the desktop app stay unchanged.
    pub fn from_env() -> Option<Self> {
        let supervisor_url = supervisor_url_from_env()?;
        let supervisor_token = std::env::var("NEHA_WORKER_SUPERVISOR_TOKEN").unwrap_or_default();
        let always_on = std::env::var("NEHA_WORKER_ALWAYS_ON")
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let idle = std::env::var("NEHA_WORKER_IDLE_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(600));

        Some(Self {
            supervisor_url,
            supervisor_token,
            always_on,
            idle,
        })
    }

    /// Builds settings for a test Space with an explicit Supervisor.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_test(
        supervisor_url: impl Into<String>,
        supervisor_token: impl Into<String>,
        always_on: bool,
        idle: Duration,
    ) -> Self {
        Self {
            supervisor_url: supervisor_url.into(),
            supervisor_token: supervisor_token.into(),
            always_on,
            idle,
        }
    }
}

/// The small internal API the Supervisor exposes.
struct SupervisorClient {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl SupervisorClient {
    fn new(config: &WorkerConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "the worker Supervisor client cannot be built".to_owned())?;

        Ok(Self {
            client,
            base: config.supervisor_url.clone(),
            token: config.supervisor_token.clone(),
        })
    }

    async fn create(&self, computer: Id, workspace: &str, token: &str) -> Result<(), String> {
        self.client
            .post(format!("{}/v1/workers", self.base))
            .bearer_auth(&self.token)
            .json(&json!({
                "computer": computer,
                "workspace": workspace,
                "token": token,
            }))
            .send()
            .await
            .map_err(|error| supervisor_failure("create", &error))?
            .error_for_status()
            .map_err(|error| supervisor_failure("create", &error))?;
        Ok(())
    }

    async fn start(&self, computer: Id) -> Result<(), String> {
        self.post(computer, "start").await
    }

    async fn stop(&self, computer: Id) -> Result<(), String> {
        self.post(computer, "stop").await
    }

    async fn post(&self, computer: Id, action: &str) -> Result<(), String> {
        self.client
            .post(format!("{}/v1/workers/{computer}/{action}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| supervisor_failure(action, &error))?
            .error_for_status()
            .map_err(|error| supervisor_failure(action, &error))?;
        Ok(())
    }
}

/// A Supervisor failure without its display text.
///
/// Reqwest formats a custom HTTP reason phrase into its error, and a proxy or
/// Supervisor controls that phrase. Only the fixed operation and a status
/// code or transport class leave the Space.
fn supervisor_failure(operation: &str, error: &reqwest::Error) -> String {
    if let Some(status) = error.status() {
        return format!("the Supervisor {operation} request answered {status}");
    }
    if error.is_timeout() {
        return format!("the Supervisor {operation} request timed out");
    }
    if error.is_connect() {
        return format!("the Supervisor {operation} request could not connect");
    }
    format!("the Supervisor {operation} request failed")
}

/// Starts the background lifecycle manager for one worker-backed Space.
pub fn spawn(server: Arc<SpaceServer>, config: WorkerConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match SupervisorClient::new(&config) {
            Ok(client) => client,
            Err(_) => {
                tracing::error!(
                    target: "neha::worker",
                    event = "worker.disabled",
                    reason = "supervisor_client",
                    error_kind = "client",
                    "cannot build the worker Supervisor client"
                );
                return;
            }
        };
        let mut provisioned: HashSet<Id> = HashSet::new();
        let mut last_active: HashMap<Id, Instant> = HashMap::new();
        let mut stopped: HashSet<Id> = HashSet::new();
        let mut warned: HashSet<Id> = HashSet::new();

        while !server.is_closed() {
            let (computers, active) = snapshot(&server);
            for (computer, workspace) in computers {
                if !provisioned.contains(&computer) {
                    match token_for(&server, computer).await {
                        Ok(token) => match client.create(computer, &workspace, &token).await {
                            Ok(()) => {
                                provisioned.insert(computer);
                            }
                            Err(error) => warn_once(&mut warned, computer, &error),
                        },
                        Err(error) => warn_once(&mut warned, computer, &error),
                    }
                }
                if !provisioned.contains(&computer) {
                    continue;
                }
                let now = Instant::now();
                if config.always_on || active.contains(&computer) {
                    last_active.insert(computer, now);
                    stopped.remove(&computer);
                    if let Err(error) = client.start(computer).await {
                        // The container may have been removed outside this
                        // manager. Forget the provisioning so the next tick
                        // asks the Supervisor to create it again.
                        provisioned.remove(&computer);
                        stopped.remove(&computer);
                        warn_once(&mut warned, computer, &error);
                    }
                } else {
                    let since = *last_active.entry(computer).or_insert(now);
                    if !stopped.contains(&computer) && now.duration_since(since) >= config.idle {
                        match client.stop(computer).await {
                            Ok(()) => {
                                stopped.insert(computer);
                            }
                            Err(error) => {
                                provisioned.remove(&computer);
                                stopped.remove(&computer);
                                warn_once(&mut warned, computer, &error);
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

/// Returns the Hosted Computers and the Computers with active work.
fn snapshot(server: &SpaceServer) -> (Vec<(Id, String)>, HashSet<Id>) {
    let Ok(state) = server.state.lock() else {
        return (Vec::new(), HashSet::new());
    };
    let computers = state
        .computers
        .values()
        .filter(|computer| computer.kind == "hosted")
        .map(|computer| {
            (
                computer.id,
                format!("workspaces/{computer_id}", computer_id = computer.id),
            )
        })
        .collect();
    let active = state
        .runs
        .values()
        .filter(|run| {
            matches!(
                run.status,
                crate::state::RunStatus::Queued
                    | crate::state::RunStatus::Running
                    | crate::state::RunStatus::Waiting
            )
        })
        .filter_map(|run| {
            run.computer
                .or_else(|| state.bots.get(&run.bot).map(|bot| bot.computer))
        })
        .collect();

    (computers, active)
}

/// Reads or creates the one Computer token a worker keeps.
async fn token_for(server: &SpaceServer, computer: Id) -> Result<String, String> {
    let directory = server.home.root().join("state").join("workers");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create the worker state directory: {}", error.kind()))?;
    let path = directory.join(format!("{computer}.token"));
    match std::fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => {
            server
                .identity
                .ensure_computer_token(computer, token.trim())
                .await
                .map_err(|_| "the worker identity cannot be saved".to_owned())?;
            Ok(token.trim().to_owned())
        }
        Ok(_) => {
            // An empty file cannot authenticate anything. Replace it instead
            // of creating a second name beside it.
            remove_unusable_token(&path)?;
            new_token(server, computer, &path).await
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            new_token(server, computer, &path).await
        }
        Err(error) => Err(format!("cannot read the worker token: {}", error.kind())),
    }
}

/// Removes an empty or truncated token file before a fresh one is written.
fn remove_unusable_token(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot replace the worker token: {}", error.kind())),
    }
}

async fn new_token(
    server: &SpaceServer,
    computer: Id,
    path: &std::path::Path,
) -> Result<String, String> {
    let token = crate::ticket::secret();
    write_private(path, token.as_bytes())?;
    server
        .identity
        .ensure_computer_token(computer, &token)
        .await
        .map_err(|_| "the worker identity cannot be saved".to_owned())?;
    Ok(token)
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("cannot write the worker token: {}", error.kind()))?;
        file.write_all(bytes)
            .map_err(|error| format!("cannot write the worker token: {}", error.kind()))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
            .map_err(|error| format!("cannot write the worker token: {}", error.kind()))
    }
}

fn warn_once(warned: &mut HashSet<Id>, computer: Id, error: &str) {
    if warned.insert(computer) {
        tracing::warn!(
            target: "neha::worker",
            event = "worker.fail",
            computer_id = %computer,
            operation = "worker",
            error_message = scrub_error(error).as_str(),
            "a worker operation failed"
        );
    }
}
