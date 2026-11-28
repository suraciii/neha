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

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha::record::{ApprovalRequested, Event, EventKind, RunFinished, RunStatus};
use neha::seq::Seq;
use p256::PublicKey;
use sad_web_push::request_builder::try_build_request;
use sad_web_push::{
    ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

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

enum Readiness {
    Ready,
    Muted,
    Gone,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Resolves only addresses the Space may reach: public push services, and
/// loopback for a unit test's fake device. Redirects are disabled separately,
/// so a public service cannot hand the Space a private target.
struct PublicResolver {
    allow_loopback: bool,
}

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| -> BoxError { Box::new(error) })?;
            let public: Vec<std::net::SocketAddr> = addresses
                .filter(|address| {
                    public_address(address.ip()) || (allow_loopback && address.ip().is_loopback())
                })
                .collect();
            if public.is_empty() {
                return Err(Box::new(std::io::Error::other(
                    "the push endpoint has no public address",
                )) as BoxError);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The Space's push devices and signing key.
pub(crate) struct Push {
    path: PathBuf,
    store: Mutex<Store>,
    /// A read or parse failure. The file is left untouched until a human
    /// repairs it; a successful write must not replace other devices' state.
    storage_error: Option<String>,
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
        let (store, storage_error) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(store) => (store, None),
                Err(error) => {
                    let message = format!("the push subscriptions are not readable: {error}");
                    tracing::warn!(
                        target: "neha::push",
                        event = "push.disabled",
                        reason = "subscriptions_unreadable",
                        error_kind = "parse",
                        "web push is disabled for this start"
                    );
                    (Store::default(), Some(message))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Store::default(), None),
            Err(error) => {
                let message = format!("the push subscriptions could not be read: {error}");
                tracing::warn!(
                    target: "neha::push",
                    event = "push.disabled",
                    reason = "subscriptions_read_failed",
                    error_kind = "io",
                    "web push is disabled for this start"
                );
                (Store::default(), Some(message))
            }
        };
        let key = match secrets.vapid_private_key() {
            Ok(key) => Some(key),
            Err(_) => {
                tracing::warn!(
                    target: "neha::push",
                    event = "push.disabled",
                    reason = "vapid_key",
                    error_kind = "secrets",
                    "web push is disabled for this start"
                );
                None
            }
        };
        let public = key.as_ref().and_then(vapid_public);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            // A proxy would resolve the endpoint itself, outside the resolver
            // below; the Space talks to the push service directly.
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(PublicResolver {
                allow_loopback: cfg!(any(test, feature = "test-support")),
            }))
            .build()
            .expect("a push client");

        Arc::new(Self {
            path,
            store: Mutex::new(store),
            storage_error,
            key,
            public,
            contact,
            client,
        })
    }

    /// Returns the public key and the current mute, when push is configured.
    pub(crate) fn status(&self) -> Option<Status> {
        if self.storage_error.is_some() {
            return None;
        }
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
        self.writable()?;
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
        self.writable()?;
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
        self.writable()?;
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
        self.writable()?;
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

    fn writable(&self) -> Result<(), SubscribeError> {
        if self.storage_error.is_some() {
            return Err(SubscribeError::Store);
        }
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

    /// Whether this exact device may still receive the notice. A mute stops
    /// every remaining device; a removed or re-subscribed device is skipped.
    fn readiness(&self, notice: &Notice, sent: &Subscription) -> Readiness {
        if self.storage_error.is_some() {
            return Readiness::Muted;
        }
        let Ok(store) = self.store.lock() else {
            return Readiness::Muted;
        };
        if store.muted_until.is_some_and(|until| until > now()) {
            return Readiness::Muted;
        }
        if store.subscriptions.iter().any(|subscription| {
            subscription.account == notice.account
                && subscription.endpoint == sent.endpoint
                && subscription.p256dh == sent.p256dh
                && subscription.auth == sent.auth
        }) {
            Readiness::Ready
        } else {
            Readiness::Gone
        }
    }

    /// Sends one encrypted push request to one browser.
    async fn deliver(&self, subscription: &Subscription, payload: &[u8]) -> Delivery {
        let Some(key) = self.key else {
            return Delivery::Failed(0);
        };
        let private = URL_SAFE_NO_PAD.encode(key);
        let info = SubscriptionInfo::new(
            subscription.endpoint.clone(),
            subscription.p256dh.clone(),
            subscription.auth.clone(),
        );
        // A unit test's fake device is plain HTTP on loopback; the subscription
        // route and the production resolver refuse that outside tests.
        let insecure = cfg!(any(test, feature = "test-support"))
            && subscription.endpoint.starts_with("http://");
        let mut vapid = match VapidSignatureBuilder::from_base64(&private, &info) {
            Ok(vapid) => vapid,
            Err(_) => return Delivery::Failed(0),
        };
        vapid.add_claim("sub", self.contact.as_str());
        vapid.allow_insecure_endpoint(insecure);
        let signature = match vapid.build() {
            Ok(signature) => signature,
            Err(_) => return Delivery::Failed(0),
        };
        let mut message = WebPushMessageBuilder::new(&info);
        message.set_ttl(PUSH_TTL.as_secs() as u32);
        message.set_payload(ContentEncoding::Aes128Gcm, payload);
        message.set_vapid_signature(signature);
        message.allow_insecure_endpoint(insecure);
        let message = match message.build() {
            Ok(message) => message,
            Err(_) => return Delivery::Failed(0),
        };
        let request = match try_build_request::<Vec<u8>>(message) {
            Ok(request) => request,
            Err(_) => return Delivery::Failed(0),
        };
        let (parts, body) = request.into_parts();
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::POST);
        let mut outgoing = self.client.request(method, parts.uri.to_string());
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

    fn remove(&self, sent: &Subscription) {
        let Ok(mut store) = self.store.lock() else {
            return;
        };
        let mut next = store.clone();
        next.subscriptions.retain(|subscription| {
            !(subscription.endpoint == sent.endpoint
                && subscription.p256dh == sent.p256dh
                && subscription.auth == sent.auth)
        });
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

/// Starts the task that turns new record events into notifications.
///
/// `cursor` is the last sequence the caller counted before starting work the
/// task must see. The task subscribes first, then catches up through the store,
/// because an event recorded between the capture and the subscription is in
/// neither the channel nor before the cursor.
pub(crate) fn spawn(server: Arc<SpaceServer>, mut cursor: Seq) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut receiver = server.events.subscribe();
        catch_up(&server, &mut cursor).await;
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if event.seq <= cursor {
                        continue;
                    }
                    // Publishers append and then broadcast independently, so a
                    // later sequence may arrive first. Drain the ordered record
                    // instead of advancing the cursor over the missing event.
                    if cursor.next() != Some(event.seq) {
                        catch_up(&server, &mut cursor).await;
                        continue;
                    }
                    process(&server, event, &mut cursor).await;
                }
                Err(RecvError::Lagged(_)) => catch_up(&server, &mut cursor).await,
                Err(RecvError::Closed) => break,
            }
        }
    })
}

async fn process(server: &Arc<SpaceServer>, event: Event, cursor: &mut Seq) {
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
    // Sends are sequential: two notifications for one Chat must not race and
    // let the older tag replace the newer one, and shutdown must not leave a
    // detached send behind. One slow endpoint delays the next device by at
    // most the request timeout.
    for subscription in subscriptions {
        match server.push.readiness(&notice, &subscription) {
            Readiness::Muted => break,
            Readiness::Gone => continue,
            Readiness::Ready => {}
        }
        match server.push.deliver(&subscription, &payload).await {
            Delivery::Sent => {}
            Delivery::Gone => server.push.remove(&subscription),
            Delivery::Failed(status) => {
                let host = endpoint_host(&subscription.endpoint);
                tracing::warn!(
                    target: "neha::push",
                    event = "push.fail",
                    endpoint_host = %host,
                    status = %status,
                    "a push could not be delivered"
                );
            }
        }
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
            process(server, event, cursor).await;
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
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    // `host_str` keeps the brackets around an IPv6 literal.
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let address = bare.parse::<std::net::IpAddr>().ok();
    let loopback = bare == "localhost" || address.is_some_and(|address| address.is_loopback());
    let testing = cfg!(any(test, feature = "test-support")) && url.scheme() == "http" && loopback;
    // Push services are HTTPS. Loopback HTTP exists only so a unit test can
    // stand a delivery endpoint beside the Space.
    if url.scheme() != "https" && !testing {
        return Err(SubscribeError::Invalid("the endpoint must use HTTPS"));
    }
    // The endpoint comes from a signed-in User, but it must not turn the Space
    // into a probe of its own host or private network. A public https name is
    // still resolved by the machine; redirects are disabled at delivery.
    if !testing && !public_destination(host, address) {
        return Err(SubscribeError::Invalid(
            "the endpoint must be a public push service",
        ));
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

/// Whether a push destination is a public name or address, not this host or a
/// private network. A name can still resolve privately; the Space sends only to
/// an endpoint its signed-in owner registered, and redirects stay disabled.
fn public_destination(host: &str, address: Option<std::net::IpAddr>) -> bool {
    if let Some(address) = address {
        return public_address(address);
    }
    !(host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".local"))
}

fn public_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => public_ipv4(address),
        std::net::IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4_mapped() {
                return public_ipv4(address);
            }
            // Start from global unicast (2000::/3), then remove the special
            // ranges inside it. Site-local fec0::/10 and discard-only 100::/64
            // are outside global unicast and therefore already refused.
            let segments = address.segments();
            if segments[0] & 0xe000 != 0x2000 {
                return false;
            }
            !((segments[0] == 0x2001
                && (segments[1] == 0x0db8
                    || segments[1] == 0x0000
                    || segments[1] == 0x0002
                    || (segments[1] & 0xfff0) == 0x0010
                    || (segments[1] & 0xfff0) == 0x0020))
                || segments[0] == 0x2002
                || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0))
        }
    }
}

/// The IPv4 ranges that are not globally routable. This is deliberately
/// explicit: a resolver result and a literal take the same path.
fn public_ipv4(address: std::net::Ipv4Addr) -> bool {
    let octets = address.octets();
    let shared = octets[0] == 100 && (64..=127).contains(&octets[1]);
    let protocol = octets[0] == 192 && octets[1] == 0 && (octets[2] == 0 || octets[2] == 2);
    let benchmark = octets[0] == 198 && (octets[1] == 18 || octets[1] == 19);
    let documentation = (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113);
    let relay = octets[0] == 192 && octets[1] == 88 && octets[2] == 99;
    !(octets[0] == 0
        || octets[0] == 10
        || octets[0] == 127
        || shared
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || protocol
        || benchmark
        || documentation
        || relay
        || octets[0] >= 224)
}

fn vapid_public(key: &[u8; 32]) -> Option<String> {
    let private = URL_SAFE_NO_PAD.encode(key);
    let partial = VapidSignatureBuilder::from_base64_no_sub(&private).ok()?;
    Some(URL_SAFE_NO_PAD.encode(partial.get_public_key()))
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
                skills: Vec::new(),
                mcps: Vec::new(),
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
    fn a_private_push_destination_is_refused() {
        for endpoint in [
            "https://127.0.0.1/push",
            "https://localhost/push",
            "https://10.0.0.1/push",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/push",
            "https://198.18.0.1/push",
            "https://192.0.2.1/push",
            "https://0.0.0.0/push",
            "https://[::1]/push",
            "https://[fec0::1]/push",
            "https://[100::1]/push",
            "https://[2001:db8::1]/push",
            "https://[3fff::1]/push",
            "https://[2002::1]/push",
            "https://device.local/push",
        ] {
            assert!(
                validate_subscription(endpoint, P256DH, AUTH).is_err(),
                "{endpoint} must not be a push destination"
            );
        }
        assert!(validate_subscription("https://push.example.com/a", P256DH, AUTH).is_ok());
        assert!(validate_subscription("http://127.0.0.1/push", P256DH, AUTH).is_ok());
    }

    #[tokio::test]
    async fn the_push_client_resolves_only_public_addresses() {
        use reqwest::dns::Resolve as _;

        let name = "127.0.0.1".parse::<reqwest::dns::Name>().unwrap();
        assert!(
            PublicResolver {
                allow_loopback: false
            }
            .resolve(name)
            .await
            .is_err()
        );
        let name = "127.0.0.1".parse::<reqwest::dns::Name>().unwrap();
        assert!(
            PublicResolver {
                allow_loopback: true
            }
            .resolve(name)
            .await
            .is_ok()
        );
    }

    #[test]
    fn the_address_policy_is_global_unicast() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fec0::1",
            "100::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2001::1",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "2002::1",
            "3fff::1",
            "64:ff9b::1",
        ] {
            let address = address.parse().unwrap();
            assert!(!public_address(address), "{address} must not be public");
        }
        for address in ["1.1.1.1", "2606:4700:4700::1111", "2001:4860:4860::8888"] {
            let address = address.parse().unwrap();
            assert!(public_address(address), "{address} must be public");
        }
    }

    #[test]
    fn a_corrupt_subscription_file_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();
        let secrets = Secrets::load(&home).unwrap();
        let path = home.root().join("state").join("push.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let corrupt = b"not a subscription file";
        std::fs::write(&path, corrupt).unwrap();
        let push = Push::open(&home, &secrets, "mailto:test@example.com".to_owned());
        assert!(push.status().is_none());
        assert!(
            push.subscribe(
                Id::new(),
                "https://push.example.com/a".to_owned(),
                P256DH.to_owned(),
                AUTH.to_owned(),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
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
    fn the_next_send_rechecks_mute_and_the_subscription() {
        use Readiness::{Gone, Muted, Ready};

        let (_directory, push) = push();
        let account = Id::new();
        push.subscribe(
            account,
            "https://push.example.com/first".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        push.subscribe(
            account,
            "https://push.example.com/second".to_owned(),
            P256DH.to_owned(),
            AUTH.to_owned(),
        )
        .unwrap();
        let (first, second) = {
            let store = push.store.lock().unwrap();
            (
                store.subscriptions[0].clone(),
                store.subscriptions[1].clone(),
            )
        };
        let notice = Notice {
            account,
            title: "Scout".to_owned(),
            kind: "finished",
            url: "/?chat=1".to_owned(),
            tag: "chat-1".to_owned(),
        };
        assert!(matches!(push.readiness(&notice, &first), Ready));
        push.unsubscribe(account, &second.endpoint).unwrap();
        assert!(matches!(push.readiness(&notice, &second), Gone));
        assert!(matches!(push.readiness(&notice, &first), Ready));
        push.mute(60).unwrap();
        assert!(matches!(push.readiness(&notice, &first), Muted));
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
    fn the_push_client_ignores_proxy_environment() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "push::tests::proxy_probe_child", "--nocapture"])
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .status()
            .unwrap();
        assert!(status.success(), "a proxy must not intercept push delivery");
    }

    #[test]
    fn proxy_probe_child() {
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
            });
            let (_directory, push) = push();
            let subscription = Subscription {
                endpoint: format!("http://{address}/push"),
                p256dh: P256DH.to_owned(),
                auth: AUTH.to_owned(),
                account: Id::new(),
            };
            assert!(matches!(
                push.deliver(&subscription, br#"{"kind":"finished"}"#).await,
                Delivery::Sent
            ));
            server.await.unwrap();
        });
    }

    #[test]
    fn a_push_request_has_a_vapid_and_encrypted_body() {
        use aes_gcm::aead::{Aead as _, KeyInit as _};
        use aes_gcm::{Aes128Gcm, Nonce};
        use hkdf::Hkdf;
        use p256::SecretKey;
        use p256::ecdh::diffie_hellman;
        use p256::elliptic_curve::Generate as _;
        use p256::elliptic_curve::sec1::ToSec1Point as _;
        use sha2::Sha256;

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
                let mut body = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
                        .await
                        .unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let split = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let headers = String::from_utf8_lossy(&request[..split]).to_string();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                body.extend_from_slice(&request[split..]);
                while body.len() < length {
                    let mut chunk = [0_u8; 4096];
                    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
                        .await
                        .unwrap();
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                tokio::io::AsyncWriteExt::write_all(
                    &mut socket,
                    b"HTTP/1.1 201 Created\r\ncontent-length: 0\r\n\r\n",
                )
                .await
                .unwrap();
                (headers, body)
            });

            let browser = SecretKey::generate();
            let ua_public = browser
                .public_key()
                .to_sec1_point(false)
                .as_bytes()
                .to_vec();
            let auth: [u8; 16] = rand::random();
            let payload = br#"{"kind":"finished"}"#;
            let (_directory, push) = push();
            let subscription = Subscription {
                endpoint: format!("http://{address}/push"),
                p256dh: URL_SAFE_NO_PAD.encode(&ua_public),
                auth: URL_SAFE_NO_PAD.encode(auth),
                account: Id::new(),
            };
            let delivery = push.deliver(&subscription, payload).await;
            assert!(matches!(delivery, Delivery::Sent));
            let (headers, body) = server.await.unwrap();
            assert!(headers.to_lowercase().contains("authorization: vapid t="));
            assert!(
                headers
                    .to_lowercase()
                    .contains("content-encoding: aes128gcm")
            );
            let public = push.public.as_ref().expect("a VAPID public key");
            assert!(headers.contains(&format!("k={public}")));
            assert_eq!(push.status().expect("push status").key, *public);

            // Browser-side RFC8291 decryption: prove the body carries the
            // notification, not just that it has the expected headers.
            assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 4096);
            let id_length = body[20] as usize;
            assert_eq!(id_length, 65);
            let as_public = &body[21..21 + id_length];
            let ciphertext = &body[21 + id_length..];
            let ephemeral = p256::PublicKey::from_sec1_bytes(as_public).unwrap();
            let shared = diffie_hellman(browser.to_nonzero_scalar(), ephemeral.as_affine());
            let mut key_info = Vec::with_capacity(14 + 130);
            key_info.extend_from_slice(b"WebPush: info\0");
            key_info.extend_from_slice(&ua_public);
            key_info.extend_from_slice(as_public);
            let prk_key = Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes().as_slice());
            let mut ikm = [0_u8; 32];
            prk_key.expand(&key_info, &mut ikm).unwrap();
            let prk = Hkdf::<Sha256>::new(Some(&body[..16]), &ikm);
            let mut cek = [0_u8; 16];
            prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
                .unwrap();
            let mut nonce = [0_u8; 12];
            prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
                .unwrap();
            let plaintext = Aes128Gcm::new_from_slice(&cek)
                .unwrap()
                .decrypt(Nonce::from_slice(&nonce), ciphertext)
                .unwrap();
            let end = plaintext
                .iter()
                .rposition(|byte| *byte == 0x02)
                .expect("the final record delimiter");
            assert_eq!(&plaintext[..end], payload);
            assert!(plaintext[end + 1..].iter().all(|byte| *byte == 0));
        });
    }
}
