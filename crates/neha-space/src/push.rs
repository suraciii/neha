//! Web Push delivery for the installed browser client.
//!
//! A browser subscription is a device, not a record event. This module keeps
//! those devices in a private state file and delivers one notification for
//! each event the notification contract names: a finished run, or an approval
//! that waits for its User. The VAPID key is sealed with the Space's other
//! secrets.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::Uri;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha::record::{ApprovalRequested, Event, EventKind, RunFinished, RunStatus};
use neha::seq::Seq;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use web_push_native::jwt_simple::algorithms::ECDSAP256PublicKeyLike as _;
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::p256::PublicKey;
use web_push_native::{Auth, WebPushBuilder};

use crate::home::Home;
use crate::secrets::Secrets;
use crate::server::SpaceServer;
use crate::state::State;

const MAX_SUBSCRIPTIONS: usize = 10;
const MAX_ENDPOINT_BYTES: usize = 2_048;
const PUSH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One browser's push subscription.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Subscription {
    endpoint: String,
    p256dh: String,
    auth: String,
    account: Id,
}

/// The JSON state file beside the record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    subscriptions: Vec<Subscription>,
    /// Unix seconds. A User mutes the whole Space for a time.
    #[serde(default)]
    muted_until: Option<u64>,
}

/// The public answer to `GET /api/push`.
pub(crate) struct Status {
    pub(crate) key: String,
    pub(crate) muted_until: Option<u64>,
}

/// A notification the record asked for, before it is addressed to a device.
struct Notice {
    account: Id,
    title: String,
    kind: &'static str,
    url: String,
    tag: String,
}

#[derive(Serialize)]
struct Payload<'a> {
    kind: &'a str,
    title: &'a str,
    url: &'a str,
    tag: &'a str,
}

enum Delivery {
    Sent,
    Gone,
    Failed(u16),
}

/// The Space's push devices and signing key.
pub(crate) struct Push {
    path: PathBuf,
    store: Mutex<Store>,
    key: Option<[u8; 32]>,
    public: Option<String>,
    /// The VAPID contact URI: the public origin, or a local placeholder.
    contact: String,
    client: reqwest::Client,
}

impl Push {
    /// Loads the subscriptions and the VAPID key.
    ///
    /// Push is optional: a broken key or state file disables notifications
    /// instead of refusing to open the Space.
    pub(crate) fn open(home: &Home, secrets: &Secrets, contact: String) -> Arc<Self> {
        let path = home.root().join("state").join("push.json");
        let store = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("neha: the push subscriptions are not readable: {error}");
                    Store::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(error) => {
                eprintln!("neha: the push subscriptions could not be read: {error}");
                Store::default()
            }
        };
        let key = match secrets.vapid_private_key() {
            Ok(key) => Some(key),
            Err(error) => {
                eprintln!("neha: web push is disabled: {error}");
                None
            }
        };
        let public = key
            .as_ref()
            .and_then(|bytes| ES256KeyPair::from_bytes(bytes).ok())
            .and_then(|pair| public_key(&pair));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Arc::new(Self {
            path,
            store: Mutex::new(store),
            key,
            public,
            contact,
            client,
        })
    }

    /// Returns the public key and the current mute, when push is configured.
    pub(crate) fn status(&self) -> Option<Status> {
        let key = self.public.clone()?;
        let store = self.store.lock().ok()?;
        Some(Status {
            key,
            muted_until: store.muted_until.filter(|until| *until > now()),
        })
    }

    /// Stores or updates one browser subscription.
    pub(crate) fn subscribe(
        &self,
        account: Id,
        endpoint: String,
        p256dh: String,
        auth: String,
    ) -> Result<(), SubscribeError> {
        validate_subscription(&endpoint, &p256dh, &auth)?;
        let mut store = self.store.lock().map_err(|_| SubscribeError::Store)?;
        let existing = store
            .subscriptions
            .iter()
            .position(|subscription| subscription.endpoint == endpoint);
        if existing.is_none() && store.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return Err(SubscribeError::TooMany);
        }
        let mut next = store.clone();
        match existing {
            Some(index) => {
                next.subscriptions[index] = Subscription {
                    endpoint,
                    p256dh,
                    auth,
                    account,
                }
            }
            None => next.subscriptions.push(Subscription {
                endpoint,
                p256dh,
                auth,
                account,
            }),
        }
        self.save(&next).map_err(|_| SubscribeError::Store)?;
        *store = next;

        Ok(())
    }

    /// Forgets one browser subscription.
    pub(crate) fn unsubscribe(&self, account: Id, endpoint: &str) -> Result<(), SubscribeError> {
        let mut store = self.store.lock().map_err(|_| SubscribeError::Store)?;
        let mut next = store.clone();
        next.subscriptions.retain(|subscription| {
            subscription.endpoint != endpoint || subscription.account != account
        });
        if next.subscriptions.len() == store.subscriptions.len() {
            return Ok(());
        }
        self.save(&next).map_err(|_| SubscribeError::Store)?;
        *store = next;

        Ok(())
    }

    /// Mutes the whole Space for a number of minutes.
    pub(crate) fn mute(&self, minutes: u64) -> Result<u64, SubscribeError> {
        if !(1..=24 * 60).contains(&minutes) {
            return Err(SubscribeError::Invalid("mute from 1 to 1440 minutes"));
        }
        let until = now().saturating_add(minutes.saturating_mul(60));
        let mut store = self.store.lock().map_err(|_| SubscribeError::Store)?;
        let mut next = store.clone();
        next.muted_until = Some(until);
        self.save(&next).map_err(|_| SubscribeError::Store)?;
        *store = next;

        Ok(until)
    }

    /// Ends a mute.
    pub(crate) fn unmute(&self) -> Result<(), SubscribeError> {
        let mut store = self.store.lock().map_err(|_| SubscribeError::Store)?;
        if store.muted_until.is_none() {
            return Ok(());
        }
        let mut next = store.clone();
        next.muted_until = None;
        self.save(&next).map_err(|_| SubscribeError::Store)?;
        *store = next;

        Ok(())
    }

    /// Builds the notification an event names, if it names one this device
    /// class should receive.
    fn notice(&self, event: &Event, state: &State) -> Option<Notice> {
        self.public.as_ref()?;
        let store = self.store.lock().ok()?;
        if store.muted_until.is_some_and(|until| until > now()) {
            return None;
        }
        let acting = event.acting?;
        if !store
            .subscriptions
            .iter()
            .any(|subscription| subscription.account == acting)
        {
            return None;
        }
        match event.kind {
            EventKind::RunFinished => finished(event, state, acting),
            EventKind::ApprovalRequested => approval(event, state, acting),
            _ => None,
        }
    }

    /// Addresses one notice to the subscriptions of its User.
    fn jobs(&self, notice: &Notice) -> Option<(Vec<Subscription>, Vec<u8>)> {
        let store = self.store.lock().ok()?;
        let subscriptions: Vec<Subscription> = store
            .subscriptions
            .iter()
            .filter(|subscription| subscription.account == notice.account)
            .cloned()
            .collect();
        if subscriptions.is_empty() {
            return None;
        }
        let payload = serde_json::to_vec(&Payload {
            kind: notice.kind,
            title: &notice.title,
            url: &notice.url,
            tag: &notice.tag,
        })
        .ok()?;
        Some((subscriptions, payload))
    }

    /// Sends one encrypted push request to one browser.
    async fn deliver(&self, subscription: &Subscription, payload: &[u8]) -> Delivery {
        let Some(key) = self.key else {
            return Delivery::Failed(0);
        };
        let Ok(pair) = ES256KeyPair::from_bytes(&key) else {
            return Delivery::Failed(0);
        };
        let Ok(endpoint) = subscription.endpoint.parse::<Uri>() else {
            return Delivery::Failed(0);
        };
        let Ok(public) = URL_SAFE_NO_PAD
            .decode(&subscription.p256dh)
            .ok()
            .and_then(|bytes| PublicKey::from_sec1_bytes(&bytes).ok())
            .ok_or(())
        else {
            return Delivery::Failed(0);
        };
        let Ok(auth) = URL_SAFE_NO_PAD.decode(&subscription.auth) else {
            return Delivery::Failed(0);
        };
        if auth.len() != 16 {
            return Delivery::Failed(0);
        }
        let auth = Auth::clone_from_slice(&auth);
        let request = match WebPushBuilder::new(endpoint, public, auth)
            .with_valid_duration(PUSH_TTL)
            .with_vapid(&pair, &self.contact)
            .build(payload.to_vec())
        {
            Ok(request) => request,
            Err(_) => return Delivery::Failed(0),
        };
        let (parts, body) = request.into_parts();
        let mut outgoing = self
            .client
            .request(reqwest::Method::POST, parts.uri.to_string());
        for (name, value) in parts.headers.iter() {
            outgoing = outgoing.header(name.as_str(), value.as_bytes());
        }
        let response = match outgoing.body(body).send().await {
            Ok(response) => response,
            Err(_) => return Delivery::Failed(0),
        };
        let status = response.status().as_u16();
        match status {
            200..=299 => Delivery::Sent,
            404 | 410 => Delivery::Gone,
            _ => Delivery::Failed(status),
        }
    }

    fn remove(&self, endpoint: &str) {
        let Ok(mut store) = self.store.lock() else {
            return;
        };
        let mut next = store.clone();
        next.subscriptions
            .retain(|subscription| subscription.endpoint != endpoint);
        if next.subscriptions.len() == store.subscriptions.len() {
            return;
        }
        if self.save(&next).is_ok() {
            *store = next;
        }
    }

    fn save(&self, store: &Store) -> Result<(), std::io::Error> {
        let bytes = serde_json::to_vec_pretty(store)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("json.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        fs::rename(&temporary, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// Starts the one task that follows recorded events into notifications.
pub(crate) fn spawn(server: Arc<SpaceServer>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut receiver = server.events.subscribe();
        let mut cursor = server.store.last_seq().await.unwrap_or(Seq::ZERO);
        loop {
            match receiver.recv().await {
                Ok(event) => process(&server, event, &mut cursor),
                Err(RecvError::Lagged(_)) => catch_up(&server, &mut cursor).await,
                Err(RecvError::Closed) => break,
            }
        }
    })
}

fn process(server: &Arc<SpaceServer>, event: Event, cursor: &mut Seq) {
    if event.seq <= *cursor {
        return;
    }
    *cursor = event.seq;
    let notice = {
        let Ok(state) = server.state.lock() else {
            return;
        };
        server.push.notice(&event, &state)
    };
    let Some(notice) = notice else {
        return;
    };
    let Some((subscriptions, payload)) = server.push.jobs(&notice) else {
        return;
    };
    let payload = Arc::new(payload);
    for subscription in subscriptions {
        let push = Arc::clone(&server.push);
        let payload = Arc::clone(&payload);
        tokio::spawn(async move {
            match push.deliver(&subscription, &payload).await {
                Delivery::Sent => {}
                Delivery::Gone => push.remove(&subscription.endpoint),
                Delivery::Failed(status) => {
                    let host = endpoint_host(&subscription.endpoint);
                    eprintln!("neha: a push to {host} failed ({status})");
                }
            }
        });
    }
}

async fn catch_up(server: &Arc<SpaceServer>, cursor: &mut Seq) {
    let reader = server.store.reader();
    loop {
        let before = *cursor;
        let Ok(page) = reader.events_from(before, 256).await else {
            return;
        };
        for event in page.events {
            process(server, event, cursor);
        }
        if page.next <= before {
            return;
        }
        *cursor = page.next;
    }
}

fn finished(event: &Event, state: &State, acting: Id) -> Option<Notice> {
    let data: RunFinished = serde_json::from_value(event.data.clone()).ok()?;
    let run = state.runs.get(&data.run)?;
    if run.acting != acting || state.chats.get(&run.chat)?.owner != acting {
        return None;
    }
    let kind = match data.status {
        RunStatus::Succeeded => "finished",
        RunStatus::Failed => "failed",
        _ => return None,
    };
    let title = state
        .bots
        .get(&run.bot)
        .map(|bot| bot.name.clone())
        .unwrap_or_else(|| "Neha".to_owned());
    let mut url = format!("/?chat={}", run.chat);
    if let Some(message) = data.reply.or(run.reply) {
        url.push_str(&format!("&message={message}"));
    }

    Some(Notice {
        account: acting,
        title,
        kind,
        url,
        tag: format!("chat-{}", run.chat),
    })
}

fn approval(event: &Event, state: &State, acting: Id) -> Option<Notice> {
    let data: ApprovalRequested = serde_json::from_value(event.data.clone()).ok()?;
    if data.acting != acting || state.chats.get(&data.chat)?.owner != acting {
        return None;
    }
    let title = state
        .bots
        .get(&data.bot)
        .map(|bot| bot.name.clone())
        .unwrap_or_else(|| "Neha".to_owned());

    Some(Notice {
        account: acting,
        title,
        kind: "approval",
        url: format!("/?chat={}&approval={}", data.chat, data.approval),
        tag: format!("chat-{}", data.chat),
    })
}

fn validate_subscription(endpoint: &str, p256dh: &str, auth: &str) -> Result<(), SubscribeError> {
    if endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(SubscribeError::Invalid("the endpoint is too long"));
    }
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return Err(SubscribeError::Invalid("the endpoint is not a URL"));
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SubscribeError::Invalid("the endpoint carries credentials"));
    }
    let host = url.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    // Push services are HTTPS. Loopback HTTP exists only so a unit test can
    // stand a delivery endpoint beside the Space.
    if url.scheme() != "https" && !(cfg!(test) && url.scheme() == "http" && loopback) {
        return Err(SubscribeError::Invalid("the endpoint must use HTTPS"));
    }
    let Ok(public) = URL_SAFE_NO_PAD.decode(p256dh) else {
        return Err(SubscribeError::Invalid("the p256dh key is not base64url"));
    };
    if public.len() != 65 || PublicKey::from_sec1_bytes(&public).is_err() {
        return Err(SubscribeError::Invalid("the p256dh key is not P-256"));
    }
    let Ok(auth) = URL_SAFE_NO_PAD.decode(auth) else {
        return Err(SubscribeError::Invalid("the auth secret is not base64url"));
    };
    if auth.len() != 16 {
        return Err(SubscribeError::Invalid("the auth secret must be 16 bytes"));
    }

    Ok(())
}

fn public_key(pair: &ES256KeyPair) -> Option<String> {
    let bytes = pair.public_key().public_key().to_bytes_uncompressed();
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn endpoint_host(endpoint: &str) -> String {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "the push service".to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// A subscription, mute, or state request was refused.
#[derive(Debug)]
pub(crate) enum SubscribeError {
    /// The request names something the Space will not store.
    Invalid(&'static str),
    /// The Space already holds its limit of devices.
    TooMany,
    /// The state file could not be written.
    Store,
}

impl SubscribeError {
    /// The message a client reads.
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::TooMany => "too many subscribed devices",
            Self::Store => "the push subscriptions could not be saved",
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use neha::record::{Author, Event, EventKind, RunStatus};
    use neha::seq::Seq;
    use neha::time::Timestamp;

    use super::*;
    use crate::home::Home;
    use crate::secrets::Secrets;
    use crate::state::{BotView, ChatView, RunView};

    const P256DH: &str =
        "BLn9b-VR0ca83knDNZ32dCHGyjJp-1riX9ZTN40MqV8K_LpQmLqxC_DoHvqvFXO_nGdAB4W9dogZb_sM-uV4JbY";
    const AUTH: &str = "_ordMnz7uTCmrpBTeUV4Bw";

    fn push() -> (tempfile::TempDir, Arc<Push>) {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let push = Push::open(&home, &secrets, "mailto:test@example.com".to_owned());
        (directory, push)
    }

    /// A state with one Bot, Chat, and acting User.
    fn state() -> (State, Id, Id, Id, Id) {
        let user = Id::new();
        let bot = Id::new();
        let chat = Id::new();
        let run = Id::new();
        let computer = Id::new();
        let mut state = State::default();
        state.bots.insert(
            bot,
            BotView {
                id: bot,
                owner: user,
                name: "Scout".to_owned(),
                computer,
                chat,
                full_access: false,
                grants: Vec::new(),
                thinking: None,
                model: None,
            },
        );
        state.chats.insert(
            chat,
            ChatView {
                id: chat,
                owner: user,
                bot,
                status: crate::state::ChatStatus::Idle,
                active_run: None,
            },
        );
        state.runs.insert(
            run,
            RunView {
                id: run,
                chat,
                bot,
                acting: user,
                command: Id::new(),
                message: Id::new(),
                seq: Seq::ZERO,
                started: None,
                status: RunStatus::Succeeded,
                reply: Some(Id::new()),
                usage: None,
                error: None,
                approval: None,
                cwd: None,
                cwd_writable: false,
                once_grant: None,
                computer: None,
                access: None,
                steps: Vec::new(),
                actions: Vec::new(),
            },
        );
        (state, user, bot, chat, run)
    }

    fn event(kind: EventKind, acting: Id, data: serde_json::Value) -> Event {
        Event {
            seq: Seq::ZERO,
            kind,
            version: 1,
            at: Timestamp::from_microseconds(0).unwrap(),
            author: Author::Bot(Id::new()),
            acting: Some(acting),
            command: None,
            data,
        }
    }

    #[test]
    fn a_vapid_key_persists_and_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let first = secrets.vapid_private_key().unwrap();
        let second = secrets.vapid_private_key().unwrap();
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            let metadata = std::fs::metadata(home.root().join("secrets/vapid.key")).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_subscription_survives_a_restart() {
        let (directory, push) = push();
        let account = Id::new();
        push.subscribe(
            account,
            "https://push.example.com/a".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        drop(push);
        let home = Home::open(directory.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let reopened = Push::open(&home, &secrets, "mailto:test@example.com".to_owned());
        // Reopening keeps the key and the device.
        let before = reopened.status().unwrap().key;
        assert!(before.len() > 80);
        assert!(
            reopened
                .store
                .lock()
                .unwrap()
                .subscriptions
                .iter()
                .any(|subscription| subscription.endpoint == "https://push.example.com/a")
        );
    }

    #[test]
    fn a_mute_survives_a_restart() {
        let (directory, push) = push();
        let until = push.mute(60).unwrap();
        drop(push);
        let home = Home::open(directory.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let reopened = Push::open(&home, &secrets, "mailto:test@example.com".to_owned());
        assert_eq!(reopened.status().unwrap().muted_until, Some(until));
        reopened.unmute().unwrap();
        assert_eq!(reopened.status().unwrap().muted_until, None);
    }

    #[test]
    fn a_finished_run_names_its_chat_and_reply() {
        let (state, user, _, chat, run) = state();
        let (_directory, push) = push();
        push.subscribe(
            user,
            "https://push.example.com/a".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        let notice = push
            .notice(
                &event(
                    EventKind::RunFinished,
                    user,
                    serde_json::json!({
                        "run": run,
                        "status": "succeeded",
                    }),
                ),
                &state,
            )
            .unwrap();
        assert_eq!(notice.account, user);
        assert_eq!(notice.kind, "finished");
        assert_eq!(notice.title, "Scout");
        assert!(notice.url.contains(&format!("chat={chat}")));
        assert!(notice.url.contains("message="));
    }

    #[test]
    fn a_muted_space_builds_no_notice() {
        let (state, user, _, _, run) = state();
        let (_directory, push) = push();
        push.subscribe(
            user,
            "https://push.example.com/a".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        push.mute(60).unwrap();
        let finished = event(
            EventKind::RunFinished,
            user,
            serde_json::json!({"run": run, "status": "succeeded"}),
        );
        assert!(push.notice(&finished, &state).is_none());
        push.unmute().unwrap();
        assert!(push.notice(&finished, &state).is_some());
    }

    #[test]
    fn an_approval_names_its_chat_and_card() {
        let (state, user, bot, chat, _) = state();
        let (_directory, push) = push();
        push.subscribe(
            user,
            "https://push.example.com/a".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        let approval = Id::new();
        let notice = push
            .notice(
                &event(
                    EventKind::ApprovalRequested,
                    user,
                    serde_json::json!({
                        "approval": approval,
                        "run": Id::new(),
                        "bot": bot,
                        "computer": Id::new(),
                        "chat": chat,
                        "acting": user,
                        "action": Id::new(),
                        "index": 0,
                        "step": 1,
                        "call": "call",
                        "tool": "shell",
                        "arguments": {},
                        "summary": "Run a command",
                        "reason": "rule",
                        "ambiguous": false,
                        "rule": null,
                        "lasting_scope": null,
                        "grant_folder": null,
                        "undoable": "no",
                    }),
                ),
                &state,
            )
            .unwrap();
        assert_eq!(notice.kind, "approval");
        assert_eq!(notice.url, format!("/?chat={chat}&approval={approval}"));
    }

    #[test]
    fn progress_and_other_users_are_not_noticed() {
        let (state, user, _, _, _) = state();
        let (_directory, push) = push();
        push.subscribe(
            user,
            "https://push.example.com/a".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        assert!(
            push.notice(
                &event(EventKind::RunStep, user, serde_json::json!({})),
                &state
            )
            .is_none()
        );
        assert!(
            push.notice(
                &event(
                    EventKind::RunFinished,
                    Id::new(),
                    serde_json::json!({"run": Id::new(), "status": "failed"}),
                ),
                &state
            )
            .is_none()
        );
    }

    #[test]
    fn a_push_request_has_a_vapid_and_encrypted_body() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
                        .await
                        .unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                tokio::io::AsyncWriteExt::write_all(
                    &mut socket,
                    b"HTTP/1.1 201 Created\r\ncontent-length: 0\r\n\r\n",
                )
                .await
                .unwrap();
                String::from_utf8_lossy(&request).to_string()
            });
            let (_directory, push) = push();
            let subscription = Subscription {
                endpoint: format!("http://{address}/push"),
                p256dh: P256DH.to_owned(),
                auth: AUTH.to_owned(),
                account: Id::new(),
            };
            let delivery = push.deliver(&subscription, br#"{"kind":"finished"}"#).await;
            assert!(matches!(delivery, Delivery::Sent));
            let request = server.await.unwrap();
            assert!(request.contains("authorization: vapid t="));
            assert!(request.contains("content-encoding: aes128gcm"));
        });
    }
}
