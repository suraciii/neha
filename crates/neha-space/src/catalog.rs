//! The models.dev model catalog.
//!
//! The catalog names the Providers a Space can connect to and the Models each
//! one offers. A build carries a seed of it, so a Space that never reached the
//! network still knows the vendors. A cache holds the catalog a fetch brought
//! back, together with the metadata that says when it was fetched and when it
//! was last confirmed. The seed is read locally; only an explicit refresh and a
//! stale cache on an existing cache reach the network.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use neha::surface::{ModelSpec, ProviderProtocol, ThinkingLevel};
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::CatalogConfig;
use crate::home::Home;

/// The address the catalog is normally fetched from.
pub const DEFAULT_URL: &str = "https://models.dev/api.json";

/// The seed shipped with the build.
const SEED: &[u8] = include_bytes!("../data/models.dev.json.gz");

/// The version of the cache file this build reads and writes.
const CACHE_VERSION: u32 = 1;

/// The directory under the home that holds the catalog cache.
const CACHE_DIRECTORY: &str = "catalog";

/// The cache file's name inside the cache directory.
const CACHE_FILE: &str = "catalog.json";

/// How long one catalog fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The context window a Model gets when the catalog does not name one.
const DEFAULT_CONTEXT_LIMIT: u32 = 128_000;

/// The output limit a Model gets when the catalog does not name one.
const DEFAULT_MAX_OUTPUT: u32 = 8_192;

/// The base address of OpenAI when the catalog does not name one.
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// The base address of Anthropic when the catalog does not name one.
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";

/// Every thinking level a Model may support, weakest first.
const ALL_THINKING_LEVELS: [ThinkingLevel; 5] = [
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Max,
];

/// Reads the catalog and keeps the last good copy in memory.
pub struct CatalogStore {
    home: Home,
    config: CatalogConfig,
    /// The loaded catalog and its cache metadata, filled on first use.
    loaded: Mutex<Option<Arc<Loaded>>>,
    /// Serializes fetches so two callers never write the cache at once.
    fetching: tokio::sync::Mutex<()>,
}

/// One loaded catalog and the metadata of the cache it came from.
struct Loaded {
    catalog: Arc<Catalog>,
    /// The cache metadata; absent means the built-in seed is in use.
    cache: Option<CacheMetadata>,
}

/// The metadata stored beside one cached catalog.
#[derive(Clone, Debug)]
struct CacheMetadata {
    etag: Option<String>,
    fetched_at: i64,
    checked_at: i64,
    source: String,
}

impl CatalogStore {
    /// Opens the catalog of one Space; nothing is read or fetched yet.
    #[must_use]
    pub fn open(home: &Home, config: &CatalogConfig) -> Self {
        Self {
            home: home.clone(),
            config: config.clone(),
            loaded: Mutex::new(None),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    /// Returns the catalog, preferring the cache over the seed once.
    ///
    /// The parsed catalog is kept in memory, so later calls are cheap.
    pub async fn catalog(&self) -> Result<Arc<Catalog>, CatalogError> {
        Ok(self.current().catalog.clone())
    }

    /// Returns the cache state and how many Providers and Models the catalog has.
    pub async fn status(&self) -> Result<CatalogStatus, CatalogError> {
        Ok(self.status_now())
    }

    /// Fetches the catalog again, keeping the last good copy when the fetch fails.
    ///
    /// Callers are serialized: a second refresh waits for the first one and
    /// then fetches on its own. The cache is written only after the body parsed
    /// and validated, so a broken answer never replaces a working cache.
    pub async fn refresh(&self) -> Result<CatalogRefresh, CatalogError> {
        let _fetching = self.fetching.lock().await;
        let loaded = self.current();
        let known_etag = loaded
            .cache
            .as_ref()
            .and_then(|metadata| metadata.etag.clone());

        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|source| CatalogError::Fetch {
                url: self.config.url.clone(),
                source,
            })?;
        let mut request = client.get(&self.config.url);
        if let Some(etag) = &known_etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|source| CatalogError::Fetch {
            url: self.config.url.clone(),
            source,
        })?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return self.confirm(loaded);
        }
        if !status.is_success() {
            return Err(CatalogError::Invalid {
                url: self.config.url.clone(),
                message: format!("the server answered {status}"),
            });
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .await
            .map_err(|source| CatalogError::Fetch {
                url: self.config.url.clone(),
                source,
            })?;
        let catalog = Catalog::from_json(&body).map_err(|message| CatalogError::Invalid {
            url: self.config.url.clone(),
            message,
        })?;

        let now = unix_now();
        let metadata = CacheMetadata {
            etag,
            fetched_at: now,
            checked_at: now,
            source: self.config.url.clone(),
        };
        self.write_cache(&catalog, &metadata)?;
        self.remember(Loaded {
            catalog: Arc::new(catalog),
            cache: Some(metadata),
        });

        Ok(CatalogRefresh {
            status: self.status_now(),
            changed: true,
        })
    }

    /// Spawns a refresh when an existing cache is older than its refresh interval.
    ///
    /// A fresh home has no cache and never touches the network here: a Space
    /// that starts offline must not wait on, or fail because of, the fetch.
    pub fn spawn_background(self: Arc<Self>) {
        tokio::spawn(async move {
            if !self.cache_path().is_file() {
                return;
            }
            let loaded = self.current();
            let Some(metadata) = loaded.cache.as_ref() else {
                return;
            };
            if !is_due(&self.config, last_checked(metadata), unix_now()) {
                return;
            }
            if let Err(error) = self.refresh().await {
                eprintln!("neha: the model catalog did not refresh: {error}");
            }
        });
    }

    /// Handles a 304 answer: the cached catalog is still current.
    fn confirm(&self, loaded: Arc<Loaded>) -> Result<CatalogRefresh, CatalogError> {
        let Some(metadata) = loaded.cache.as_ref() else {
            return Err(CatalogError::Invalid {
                url: self.config.url.clone(),
                message: "the server answered 304 Not Modified for a Space with no catalog cache"
                    .to_owned(),
            });
        };
        let metadata = CacheMetadata {
            checked_at: unix_now(),
            ..metadata.clone()
        };
        self.write_cache(&loaded.catalog, &metadata)?;
        self.remember(Loaded {
            catalog: loaded.catalog.clone(),
            cache: Some(metadata),
        });

        Ok(CatalogRefresh {
            status: self.status_now(),
            changed: false,
        })
    }

    /// Returns the cache state without fetching anything.
    fn status_now(&self) -> CatalogStatus {
        let loaded = self.current();
        let now = unix_now();
        let (source, builtin, etag, fetched_at, checked_at, stale) = match &loaded.cache {
            Some(metadata) => (
                metadata.source.clone(),
                false,
                metadata.etag.clone(),
                Some(metadata.fetched_at),
                Some(metadata.checked_at),
                is_due(&self.config, last_checked(metadata), now),
            ),
            None => ("builtin".to_owned(), true, None, None, None, false),
        };
        let age_seconds = loaded
            .cache
            .as_ref()
            .map(|metadata| age_seconds(now, last_checked(metadata)));

        CatalogStatus {
            source,
            builtin,
            etag,
            fetched_at,
            checked_at,
            age_seconds,
            stale,
            provider_count: loaded.catalog.provider_count(),
            model_count: loaded.catalog.model_count(),
            cache_path: self.cache_path().display().to_string(),
        }
    }

    /// Returns the loaded catalog, reading the cache or the seed once.
    fn current(&self) -> Arc<Loaded> {
        if let Some(loaded) = self.memory() {
            return loaded;
        }
        let loaded = Arc::new(self.load());
        let mut memory = self.loaded.lock().unwrap_or_else(|poisoned| {
            // The catalog lock holds no value that a panic could corrupt.
            poisoned.into_inner()
        });
        memory.get_or_insert_with(|| loaded.clone()).clone()
    }

    /// Returns the in-memory catalog when a caller loaded it already.
    fn memory(&self) -> Option<Arc<Loaded>> {
        self.loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the in-memory catalog and its cache metadata.
    fn remember(&self, loaded: Loaded) {
        *self
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(loaded));
    }

    /// Reads the cache; anything unusable falls back to the seed.
    fn load(&self) -> Loaded {
        match self.read_cache() {
            Some((catalog, metadata)) => Loaded {
                catalog: Arc::new(catalog),
                cache: Some(metadata),
            },
            None => Loaded {
                catalog: seed(),
                cache: None,
            },
        }
    }

    /// Returns the cached catalog and its metadata when the cache is usable.
    fn read_cache(&self) -> Option<(Catalog, CacheMetadata)> {
        let bytes = std::fs::read(self.cache_path()).ok()?;
        let envelope: CacheEnvelope = serde_json::from_slice(&bytes).ok()?;
        if envelope.version != CACHE_VERSION {
            return None;
        }
        if envelope.catalog.providers.is_empty() || envelope.catalog.model_count() == 0 {
            return None;
        }

        Some((
            envelope.catalog,
            CacheMetadata {
                etag: envelope.etag,
                fetched_at: envelope.fetched_at,
                checked_at: envelope.checked_at,
                source: envelope.source,
            },
        ))
    }

    /// Writes the cache atomically through a temporary file in the same directory.
    fn write_cache(&self, catalog: &Catalog, metadata: &CacheMetadata) -> Result<(), CatalogError> {
        let path = self.cache_path();
        let directory = path.parent().unwrap_or(&path).to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|error| CatalogError::Cache {
            path: directory.clone(),
            message: error.to_string(),
        })?;

        let envelope = CacheEnvelope {
            version: CACHE_VERSION,
            etag: metadata.etag.clone(),
            fetched_at: metadata.fetched_at,
            checked_at: metadata.checked_at,
            source: metadata.source.clone(),
            catalog: catalog.clone(),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|error| CatalogError::Cache {
            path: path.clone(),
            message: error.to_string(),
        })?;
        let temporary = path.with_file_name(format!("{CACHE_FILE}.tmp"));
        if let Err(error) =
            std::fs::write(&temporary, &bytes).and_then(|()| std::fs::rename(&temporary, &path))
        {
            let _ = std::fs::remove_file(&temporary);
            return Err(CatalogError::Cache {
                path,
                message: error.to_string(),
            });
        }

        Ok(())
    }

    /// Returns the cache file this store writes.
    fn cache_path(&self) -> PathBuf {
        self.home.root().join(CACHE_DIRECTORY).join(CACHE_FILE)
    }
}

/// The typed form of the cache file.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEnvelope {
    version: u32,
    #[serde(default)]
    etag: Option<String>,
    fetched_at: i64,
    checked_at: i64,
    source: String,
    catalog: Catalog,
}

/// The models.dev catalog: every Provider it knows, ordered by id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    providers: BTreeMap<String, CatalogProvider>,
}

impl Catalog {
    /// Returns one Provider by its catalog id.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&CatalogProvider> {
        self.providers.get(id)
    }

    /// Returns every Provider, ordered by id.
    pub fn providers(&self) -> impl Iterator<Item = &CatalogProvider> {
        self.providers.values()
    }

    /// Returns how many Providers the catalog names.
    #[must_use]
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Returns how many Models every Provider together offers.
    #[must_use]
    pub fn model_count(&self) -> usize {
        self.providers
            .values()
            .map(|provider| provider.models.len())
            .sum()
    }

    /// Parses and validates one models.dev provider map.
    fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let wire: BTreeMap<String, WireProvider> = serde_json::from_slice(bytes)
            .map_err(|error| format!("it is not a provider map: {error}"))?;
        let catalog = Self::from_wire(wire);
        if catalog.providers.is_empty() {
            return Err("it names no providers".to_owned());
        }
        if catalog.model_count() == 0 {
            return Err("it names no models".to_owned());
        }

        Ok(catalog)
    }

    /// Turns the wire provider map into the catalog.
    fn from_wire(wire: BTreeMap<String, WireProvider>) -> Self {
        let providers = wire
            .into_iter()
            .map(|(key, provider)| {
                let WireProvider {
                    id,
                    name,
                    npm,
                    api,
                    env,
                    doc,
                    models,
                } = provider;
                let id = if id.is_empty() { key } else { id };
                let name = if name.is_empty() { id.clone() } else { name };
                let models = models
                    .into_iter()
                    .map(|(key, model)| {
                        let model = model.into_catalog_model(key);
                        (model.id.clone(), model)
                    })
                    .collect();
                let provider = CatalogProvider {
                    id: id.clone(),
                    name,
                    npm,
                    api: api.filter(|value| !value.is_empty()),
                    env,
                    doc: doc.filter(|value| !value.is_empty()),
                    models,
                };

                (id, provider)
            })
            .collect();

        Self { providers }
    }
}

/// One Provider the catalog knows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogProvider {
    /// The provider's catalog id.
    pub id: String,
    /// The provider's display name.
    pub name: String,
    /// The AI SDK package that speaks the provider's protocol.
    pub npm: String,
    /// The base address the catalog names; absent means the protocol's preset.
    pub api: Option<String>,
    /// The environment variables that hold the provider's API key.
    pub env: Vec<String>,
    /// The provider's documentation address.
    pub doc: Option<String>,
    /// The Models the provider offers, ordered by id.
    pub models: BTreeMap<String, CatalogModel>,
}

impl CatalogProvider {
    /// Returns the protocol the Space can speak to this Provider, if any.
    #[must_use]
    pub fn protocol(&self) -> Option<ProviderProtocol> {
        match self.npm.as_str() {
            "@ai-sdk/openai" => Some(ProviderProtocol::OpenAiResponses),
            "@ai-sdk/anthropic" => Some(ProviderProtocol::AnthropicMessages),
            "@ai-sdk/openai-compatible" => Some(ProviderProtocol::ChatCompletions),
            _ => None,
        }
    }

    /// Returns the base address the Provider's protocol expects.
    ///
    /// The catalog's own address wins, verbatim; only a missing address falls
    /// back to a protocol preset, and only the protocols that have one.
    #[must_use]
    pub fn base_url(&self) -> Option<String> {
        let preset = match self.npm.as_str() {
            "@ai-sdk/openai" => Some(OPENAI_BASE_URL),
            "@ai-sdk/anthropic" => Some(ANTHROPIC_BASE_URL),
            "@ai-sdk/openai-compatible" => None,
            _ => return None,
        };
        self.api.clone().or_else(|| preset.map(str::to_owned))
    }

    /// Returns the provider's Models as the Space declares them.
    #[must_use]
    pub fn models_for_space(&self) -> Vec<ModelSpec> {
        self.models
            .values()
            .map(|model| ModelSpec {
                name: model.id.clone(),
                context_limit: model.context_limit,
                max_output: model.max_output,
                reads_images: model.attachment,
                thinks: model.reasoning,
                tools: model.tool_call,
                thinking_levels: model.thinking_levels.clone(),
            })
            .collect()
    }

    /// Returns the Model a fresh connection should use.
    ///
    /// A Model that calls tools is preferred; among those the newest release
    /// date wins, and when none names a date the lexicographically latest id.
    /// Without a tool-calling Model the first Model stands in.
    #[must_use]
    pub fn default_model_name(&self) -> Option<String> {
        let active = |model: &&CatalogModel| model.status.as_deref() != Some("deprecated");
        let mut candidates: Vec<&CatalogModel> = self
            .models
            .values()
            .filter(|model| model.tool_call && active(model))
            .collect();
        if candidates.is_empty() {
            candidates = self
                .models
                .values()
                .filter(|model| model.tool_call)
                .collect();
        }
        if candidates.is_empty() {
            return self.models.values().next().map(|model| model.id.clone());
        }

        let newest = candidates
            .iter()
            .copied()
            .filter(|model| model.release_date.is_some())
            .max_by(|left, right| {
                left.release_date
                    .cmp(&right.release_date)
                    .then_with(|| left.id.cmp(&right.id))
            });
        if let Some(newest) = newest {
            return Some(newest.id.clone());
        }

        candidates
            .iter()
            .map(|model| model.id.as_str())
            .max()
            .map(str::to_owned)
    }
}

/// One Model the catalog knows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// The model name the provider expects.
    pub id: String,
    /// The model's display name.
    pub name: String,
    /// Whether the Model can think before it answers.
    pub reasoning: bool,
    /// Whether the Model can call tools.
    pub tool_call: bool,
    /// Whether the Model reads images.
    pub attachment: bool,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// The thinking levels the Model supports; empty means it cannot think.
    pub thinking_levels: Vec<ThinkingLevel>,
    /// The day the vendor released the Model, when the catalog names one.
    ///
    /// `default_model_name` orders the Models by it; the wire model drops the
    /// field otherwise, so it is kept beside the declared capabilities.
    pub release_date: Option<String>,
    /// The lifecycle status the catalog names, such as `deprecated`.
    #[serde(default)]
    pub status: Option<String>,
}

/// The cache state of one Space's catalog.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogStatus {
    /// `"builtin"` when the seed is in use, otherwise the cache's source URL.
    pub source: String,
    /// Whether the seed is in use instead of a cache.
    pub builtin: bool,
    /// The entity tag of the cached catalog, when the server named one.
    pub etag: Option<String>,
    /// When the cached catalog was fetched, in unix seconds.
    pub fetched_at: Option<i64>,
    /// When the cache was last confirmed current, in unix seconds.
    pub checked_at: Option<i64>,
    /// How long ago the cache was last confirmed, in seconds.
    pub age_seconds: Option<u64>,
    /// Whether the cache is older than the configured refresh interval.
    pub stale: bool,
    /// How many Providers the in-use catalog names.
    pub provider_count: usize,
    /// How many Models the in-use catalog names.
    pub model_count: usize,
    /// The cache file the store reads and writes.
    pub cache_path: String,
}

/// The result of one refresh.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogRefresh {
    /// The cache state after the refresh.
    pub status: CatalogStatus,
    /// Whether the fetch replaced the catalog.
    pub changed: bool,
}

/// Why a catalog fetch or cache write failed.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The request to the catalog's address did not finish.
    #[error("cannot fetch the catalog at {url}: {source}")]
    Fetch {
        /// The address the request went to.
        url: String,
        /// Why the request failed.
        #[source]
        source: reqwest::Error,
    },
    /// The server answered with something this build cannot use.
    #[error("the catalog at {url} is unusable: {message}")]
    Invalid {
        /// The address the answer came from.
        url: String,
        /// What is wrong with the answer.
        message: String,
    },
    /// The cache file cannot be written.
    #[error("cannot write the catalog cache at {}: {message}", path.display())]
    Cache {
        /// The file or directory the write touched.
        path: PathBuf,
        /// Why the write failed.
        message: String,
    },
}

/// Returns the seed catalog, parsed once per process.
fn seed() -> Arc<Catalog> {
    static SEED_CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();

    SEED_CATALOG
        .get_or_init(|| {
            let mut json = Vec::new();
            GzDecoder::new(SEED)
                .read_to_end(&mut json)
                .expect("the embedded catalog decompresses");
            Arc::new(Catalog::from_json(&json).expect("the embedded catalog is valid"))
        })
        .clone()
}

/// Returns how long a cache may stay fresh, or `None` when background refresh is off.
fn refresh_interval(config: &CatalogConfig) -> Option<u64> {
    (config.refresh_hours > 0).then_some(config.refresh_hours.saturating_mul(60 * 60))
}

/// Returns whether a cache last confirmed at `checked_at` needs a refresh.
fn is_due(config: &CatalogConfig, checked_at: i64, now: i64) -> bool {
    let Some(interval) = refresh_interval(config) else {
        return false;
    };

    age_seconds(now, checked_at) >= interval
}

/// Returns the instant a cache was last confirmed, tolerating an absent check.
fn last_checked(metadata: &CacheMetadata) -> i64 {
    if metadata.checked_at > 0 {
        metadata.checked_at
    } else {
        metadata.fetched_at
    }
}

/// Returns the whole seconds between `now` and `since`, saturating at zero.
fn age_seconds(now: i64, since: i64) -> u64 {
    u64::try_from(now.saturating_sub(since)).unwrap_or(0)
}

/// Returns the current time in unix seconds.
fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Returns the thinking level an `effort` value names, if any.
fn effort_level(value: &str) -> Option<ThinkingLevel> {
    match value {
        "none" | "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" | "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

/// Returns the thinking levels a Model supports, weakest first.
fn thinking_levels(reasoning: bool, options: &[WireReasoningOption]) -> Vec<ThinkingLevel> {
    if !reasoning {
        return Vec::new();
    }

    let mut levels = Vec::new();
    for option in options {
        match option.kind.as_str() {
            "effort" => {
                levels.extend(option.values.iter().filter_map(|value| effort_level(value)));
            }
            "budget_tokens" | "toggle" => levels.extend(ALL_THINKING_LEVELS),
            _ => {}
        }
    }
    if levels.is_empty() {
        return ALL_THINKING_LEVELS.to_vec();
    }
    levels.sort_unstable();
    levels.dedup();

    levels
}

/// The models.dev provider map, before it becomes the catalog.
#[derive(Debug, Deserialize)]
struct WireProvider {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    npm: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    doc: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, WireModel>,
}

/// One provider's models on the wire.
#[derive(Debug, Deserialize)]
struct WireModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: WireLimit,
    #[serde(default)]
    reasoning_options: Vec<WireReasoningOption>,
}

impl WireModel {
    /// Turns one wire model into the catalog's Model.
    fn into_catalog_model(self, key: String) -> CatalogModel {
        let Self {
            id,
            name,
            attachment,
            reasoning,
            tool_call,
            release_date,
            status,
            limit,
            reasoning_options,
        } = self;
        let id = if id.is_empty() { key } else { id };
        let name = if name.is_empty() { id.clone() } else { name };

        CatalogModel {
            thinking_levels: thinking_levels(reasoning, &reasoning_options),
            id,
            name,
            reasoning,
            tool_call,
            attachment,
            context_limit: limit
                .context
                .filter(|limit| *limit > 0)
                .unwrap_or(DEFAULT_CONTEXT_LIMIT),
            max_output: limit
                .output
                .filter(|limit| *limit > 0)
                .unwrap_or(DEFAULT_MAX_OUTPUT),
            release_date: release_date.filter(|date| !date.is_empty()),
            status: status.filter(|status| !status.is_empty()),
        }
    }
}

/// One model's limits on the wire; an absent limit is zero here.
#[derive(Debug, Default, Deserialize)]
struct WireLimit {
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    output: Option<u32>,
}

/// One reasoning option on the wire.
#[derive(Debug, Deserialize)]
struct WireReasoningOption {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    values: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::header::{ETAG, IF_NONE_MATCH};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::Response;
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    use super::{
        Catalog, CatalogConfig, CatalogStore, DEFAULT_URL, WireReasoningOption, is_due,
        thinking_levels,
    };
    use crate::home::Home;
    use neha::surface::{ModelSpec, ProviderProtocol, ThinkingLevel};

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();

        (dir, home)
    }

    fn config(url: &str) -> CatalogConfig {
        CatalogConfig {
            url: url.to_owned(),
            refresh_hours: 24,
        }
    }

    fn catalog_from(document: &str) -> Catalog {
        Catalog::from_json(document.as_bytes()).unwrap()
    }

    /// What the fake models.dev server answers once.
    #[derive(Clone)]
    struct Reply {
        status: u16,
        etag: Option<String>,
        body: String,
    }

    impl Reply {
        fn ok(etag: &str, body: &str) -> Self {
            Self {
                status: 200,
                etag: Some(etag.to_owned()),
                body: body.to_owned(),
            }
        }

        fn answer(status: u16) -> Self {
            Self {
                status,
                etag: None,
                body: String::new(),
            }
        }

        fn not_modified() -> Self {
            Self::answer(304)
        }

        fn malformed() -> Self {
            Self {
                status: 200,
                etag: None,
                body: "this is not JSON".to_owned(),
            }
        }
    }

    #[derive(Clone)]
    struct FakeState {
        replies: Arc<Mutex<VecDeque<Reply>>>,
        requests: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    async fn answer(State(state): State<FakeState>, headers: HeaderMap) -> Response {
        state.requests.fetch_add(1, Ordering::SeqCst);
        state.seen.lock().unwrap().push(
            headers
                .get(IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        );
        let reply = {
            let mut replies = state.replies.lock().unwrap();
            if replies.len() > 1 {
                replies.pop_front().unwrap()
            } else {
                replies[0].clone()
            }
        };
        let mut response = Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
        if let Some(etag) = reply.etag {
            response = response.header(ETAG, etag);
        }

        response.body(Body::from(reply.body)).unwrap()
    }

    struct FakeServer {
        url: String,
        requests: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fake_server(replies: Vec<Reply>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = FakeState {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: requests.clone(),
            seen: seen.clone(),
        };
        let app = Router::new()
            .route("/api.json", get(answer))
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        FakeServer {
            url: format!("http://{addr}/api.json"),
            requests,
            seen,
            task,
        }
    }

    fn fake_catalog() -> String {
        json!({
            "fake": {
                "id": "fake",
                "name": "Fake",
                "npm": "@ai-sdk/openai",
                "env": ["FAKE_API_KEY"],
                "models": {
                    "fake-large": {
                        "id": "fake-large",
                        "name": "Fake Large",
                        "attachment": true,
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "high"]}
                        ],
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": {"context": 200000, "output": 32000}
                    }
                }
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn the_seed_parses_offline() {
        let (_dir, home) = home();
        let store = CatalogStore::open(&home, &config("http://127.0.0.1:9/api.json"));

        let catalog = store.catalog().await.unwrap();
        assert!(
            Arc::ptr_eq(&catalog, &store.catalog().await.unwrap()),
            "the parsed catalog is kept in memory"
        );

        assert!(
            catalog.provider_count() > 300,
            "{}",
            catalog.provider_count()
        );
        assert!(catalog.model_count() > 10_000, "{}", catalog.model_count());
        assert!(catalog.provider("openai").is_some());
        assert!(
            !home.root().join("catalog").join("catalog.json").exists(),
            "reading the seed must not write a cache"
        );

        let status = store.status().await.unwrap();
        assert!(status.builtin);
        assert_eq!(status.source, "builtin");
        assert!(!status.stale);
        assert_eq!(status.etag, None);
        assert_eq!(status.fetched_at, None);
        assert_eq!(status.checked_at, None);
        assert_eq!(status.age_seconds, None);
        assert_eq!(status.provider_count, catalog.provider_count());
        assert_eq!(status.model_count, catalog.model_count());
        assert!(Path::new(&status.cache_path).ends_with("catalog/catalog.json"));
    }

    #[tokio::test]
    async fn the_gate_selects_the_providers_the_space_can_speak_to() {
        let (_dir, home) = home();
        let catalog = CatalogStore::open(&home, &config(DEFAULT_URL))
            .catalog()
            .await
            .unwrap();

        let openai = catalog.provider("openai").unwrap();
        assert_eq!(openai.protocol(), Some(ProviderProtocol::OpenAiResponses));
        assert_eq!(
            openai.base_url().as_deref(),
            Some("https://api.openai.com/v1")
        );

        let anthropic = catalog.provider("anthropic").unwrap();
        assert_eq!(
            anthropic.protocol(),
            Some(ProviderProtocol::AnthropicMessages)
        );
        assert_eq!(
            anthropic.base_url().as_deref(),
            Some("https://api.anthropic.com/v1")
        );

        let deepseek = catalog.provider("deepseek").unwrap();
        assert_eq!(deepseek.protocol(), Some(ProviderProtocol::ChatCompletions));
        assert_eq!(
            deepseek.base_url().as_deref(),
            Some("https://api.deepseek.com"),
            "a catalog address is used verbatim"
        );

        let google = catalog.provider("google").unwrap();
        assert_eq!(google.protocol(), None);
        assert_eq!(google.base_url(), None);
        assert!(catalog.provider("no-such-vendor").is_none());
    }

    #[test]
    fn models_for_space_carries_the_declared_capabilities() {
        let catalog = catalog_from(
            r#"{
                "vendor": {
                    "id": "vendor",
                    "name": "Vendor",
                    "npm": "@ai-sdk/openai",
                    "env": ["VENDOR_API_KEY"],
                    "models": {
                        "one": {
                            "id": "one",
                            "name": "One",
                            "attachment": true,
                            "reasoning": true,
                            "tool_call": true,
                            "reasoning_options": [
                                {"type": "effort", "values": ["high", "low"]}
                            ],
                            "limit": {"context": 200000, "output": 32000}
                        },
                        "two": {"id": "two", "name": "Two"}
                    }
                }
            }"#,
        );

        let models = catalog.provider("vendor").unwrap().models_for_space();

        assert_eq!(
            models,
            vec![
                ModelSpec {
                    name: "one".to_owned(),
                    context_limit: 200_000,
                    max_output: 32_000,
                    reads_images: true,
                    thinks: true,
                    tools: true,
                    thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::High],
                },
                ModelSpec {
                    name: "two".to_owned(),
                    context_limit: 128_000,
                    max_output: 8_192,
                    reads_images: false,
                    thinks: false,
                    tools: false,
                    thinking_levels: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn a_missing_limit_falls_back_to_the_defaults() {
        let catalog = catalog_from(
            r#"{
                "vendor": {
                    "id": "vendor",
                    "name": "Vendor",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {
                        "absent": {"id": "absent", "name": "Absent"},
                        "zero": {
                            "id": "zero",
                            "name": "Zero",
                            "limit": {"context": 0, "output": 0}
                        }
                    }
                }
            }"#,
        );

        let provider = catalog.provider("vendor").unwrap();
        assert_eq!(provider.models["absent"].context_limit, 128_000);
        assert_eq!(provider.models["absent"].max_output, 8_192);
        assert_eq!(provider.models["zero"].context_limit, 128_000);
        assert_eq!(provider.models["zero"].max_output, 8_192);
    }

    #[test]
    fn thinking_levels_follow_the_reasoning_options() {
        let option = |kind: &str, values: &[&str]| WireReasoningOption {
            kind: kind.to_owned(),
            values: values.iter().map(|value| (*value).to_owned()).collect(),
        };
        let all = vec![
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ];

        assert_eq!(thinking_levels(false, &[option("effort", &["high"])]), []);
        assert_eq!(thinking_levels(true, &[]), all);
        assert_eq!(
            thinking_levels(
                true,
                &[option(
                    "effort",
                    &[
                        "none", "minimal", "low", "medium", "high", "xhigh", "max", "default"
                    ]
                )]
            ),
            all,
            "every effort value maps into the five levels"
        );
        assert_eq!(
            thinking_levels(true, &[option("effort", &["high", "low"])]),
            [ThinkingLevel::Low, ThinkingLevel::High],
            "levels are ordered weakest first"
        );
        assert_eq!(thinking_levels(true, &[option("toggle", &[])]), all);
        assert_eq!(thinking_levels(true, &[option("budget_tokens", &[])]), all);
        assert_eq!(
            thinking_levels(true, &[option("toggle", &[]), option("effort", &["low"])]),
            all,
            "deduplicated when both kinds name levels"
        );
    }

    #[test]
    fn the_default_model_prefers_tools_and_the_newest_release() {
        let dated = catalog_from(
            r#"{
                "vendor": {
                    "id": "vendor",
                    "name": "Vendor",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {
                        "newest-plain": {
                            "id": "newest-plain",
                            "name": "Newest Plain",
                            "release_date": "2026-01-01",
                            "tool_call": false
                        },
                        "older-tool": {
                            "id": "older-tool",
                            "name": "Older Tool",
                            "release_date": "2025-01-01",
                            "tool_call": true
                        },
                        "newer-tool": {
                            "id": "newer-tool",
                            "name": "Newer Tool",
                            "release_date": "2026-01-01",
                            "tool_call": true
                        },
                        "deprecated-tool": {
                            "id": "deprecated-tool",
                            "name": "Deprecated Tool",
                            "release_date": "2027-01-01",
                            "tool_call": true,
                            "status": "deprecated"
                        },
                        "undated-tool": {
                            "id": "undated-tool",
                            "name": "Undated Tool",
                            "tool_call": true
                        }
                    }
                }
            }"#,
        );
        assert_eq!(
            dated
                .provider("vendor")
                .unwrap()
                .default_model_name()
                .as_deref(),
            Some("newer-tool")
        );

        let undated = catalog_from(
            r#"{
                "vendor": {
                    "id": "vendor",
                    "name": "Vendor",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {
                        "alpha-tool": {"id": "alpha-tool", "tool_call": true},
                        "beta-tool": {"id": "beta-tool", "tool_call": true}
                    }
                }
            }"#,
        );
        assert_eq!(
            undated
                .provider("vendor")
                .unwrap()
                .default_model_name()
                .as_deref(),
            Some("beta-tool"),
            "without release dates the latest id wins"
        );

        let plain = catalog_from(
            r#"{
                "vendor": {
                    "id": "vendor",
                    "name": "Vendor",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {
                        "first": {"id": "first"},
                        "second": {"id": "second"}
                    }
                }
            }"#,
        );
        assert_eq!(
            plain
                .provider("vendor")
                .unwrap()
                .default_model_name()
                .as_deref(),
            Some("first"),
            "without a tool-calling Model the first one stands in"
        );
    }

    #[test]
    fn a_cache_without_models_has_no_default_model() {
        let catalog = catalog_from(
            r#"{
                "empty": {
                    "id": "empty",
                    "name": "Empty",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {}
                },
                "full": {
                    "id": "full",
                    "name": "Full",
                    "npm": "@ai-sdk/openai",
                    "env": [],
                    "models": {"only": {"id": "only", "tool_call": true}}
                }
            }"#,
        );

        assert_eq!(
            catalog.provider("empty").unwrap().default_model_name(),
            None
        );
        assert_eq!(
            catalog
                .provider("full")
                .unwrap()
                .default_model_name()
                .as_deref(),
            Some("only")
        );
    }

    #[test]
    fn a_cache_is_due_after_its_refresh_interval() {
        let config = config(DEFAULT_URL);
        let now = 1_000_000;

        assert!(!is_due(&config, now, now));
        assert!(!is_due(&config, now - 86_399, now));
        assert!(is_due(&config, now - 86_400, now));

        let disabled = CatalogConfig {
            refresh_hours: 0,
            ..config
        };
        assert!(!is_due(&disabled, now - 10 * 86_400, now));
        assert!(!is_due(&disabled, now, now));
    }

    #[tokio::test]
    async fn a_stale_cache_is_reported_as_stale() {
        let (_dir, home) = home();
        let server = fake_server(vec![Reply::ok("v1", &fake_catalog())]).await;
        let config = config(&server.url);
        let store = CatalogStore::open(&home, &config);
        let written = store.refresh().await.unwrap();
        let cache_path = written.status.cache_path;
        let old = super::unix_now() - 2 * 24 * 60 * 60;
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
        envelope["fetched_at"] = json!(old);
        envelope["checked_at"] = json!(old);
        std::fs::write(&cache_path, serde_json::to_vec(&envelope).unwrap()).unwrap();

        let reopened = CatalogStore::open(&home, &config);
        let status = reopened.status().await.unwrap();
        assert!(status.stale);
        assert!(status.age_seconds.unwrap() >= 2 * 24 * 60 * 60);
        assert_eq!(status.checked_at, Some(old));
        assert_eq!(status.provider_count, 1);

        let never = CatalogConfig {
            refresh_hours: 0,
            ..config
        };
        let reopened = CatalogStore::open(&home, &never);
        assert!(
            !reopened.status().await.unwrap().stale,
            "zero hours disables staleness"
        );
    }

    #[tokio::test]
    async fn a_refresh_writes_the_cache_and_reports_the_change() {
        let (_dir, home) = home();
        let server = fake_server(vec![Reply::ok("v1", &fake_catalog())]).await;
        let config = config(&server.url);
        let store = CatalogStore::open(&home, &config);

        let refresh = store.refresh().await.unwrap();

        assert!(refresh.changed);
        assert!(!refresh.status.builtin);
        assert_eq!(refresh.status.source, server.url);
        assert_eq!(refresh.status.etag.as_deref(), Some("v1"));
        assert_eq!(refresh.status.provider_count, 1);
        assert_eq!(refresh.status.model_count, 1);
        assert!(refresh.status.fetched_at.is_some());
        assert_eq!(refresh.status.fetched_at, refresh.status.checked_at);
        assert!(refresh.status.age_seconds.unwrap() <= 5);
        assert!(!refresh.status.stale);
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert_eq!(server.seen.lock().unwrap()[0], None);
        let cache_path = Path::new(&refresh.status.cache_path);
        assert!(cache_path.is_file());
        let envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cache_path).unwrap()).unwrap();
        assert_eq!(envelope["version"], json!(1));
        assert_eq!(envelope["source"], json!(server.url));
        assert_eq!(envelope["etag"], json!("v1"));

        let reopened = CatalogStore::open(&home, &config);
        let cached = reopened.catalog().await.unwrap();
        assert_eq!(cached.provider_count(), 1);
        assert!(cached.provider("fake").is_some());
        let status = reopened.status().await.unwrap();
        assert!(!status.builtin);
        assert_eq!(status.source, server.url);
        assert_eq!(status.provider_count, 1);
        assert_eq!(status.model_count, 1);
        assert_eq!(status.fetched_at, refresh.status.fetched_at);
        assert_eq!(status.checked_at, refresh.status.checked_at);
    }

    #[tokio::test]
    async fn a_not_modified_answer_keeps_the_cached_catalog() {
        let (_dir, home) = home();
        let server = fake_server(vec![
            Reply::ok("abc", &fake_catalog()),
            Reply::not_modified(),
        ])
        .await;
        let store = CatalogStore::open(&home, &config(&server.url));

        let first = store.refresh().await.unwrap();
        let second = store.refresh().await.unwrap();

        assert!(!second.changed);
        assert_eq!(second.status.etag.as_deref(), Some("abc"));
        assert_eq!(second.status.provider_count, 1);
        assert!(second.status.checked_at >= first.status.checked_at);
        assert_eq!(
            server.seen.lock().unwrap()[1].as_deref(),
            Some("abc"),
            "the second request carries the cached entity tag"
        );
        assert_eq!(store.catalog().await.unwrap().provider_count(), 1);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_known_good_catalog() {
        let (_dir, home) = home();
        let server = fake_server(vec![
            Reply::ok("good", &fake_catalog()),
            Reply::answer(500),
            Reply::malformed(),
            Reply::ok("empty", "{}"),
            Reply::ok(
                "no-models",
                r#"{"fake": {"id": "fake", "npm": "@ai-sdk/openai", "env": [], "models": {}}}"#,
            ),
        ])
        .await;
        let store = CatalogStore::open(&home, &config(&server.url));

        let first = store.refresh().await.unwrap();
        let cache_path = Path::new(&first.status.cache_path);
        let written = std::fs::read(cache_path).unwrap();

        for expected in ["500", "malformed", "empty map", "no models"] {
            let error = store.refresh().await.unwrap_err();
            assert!(
                matches!(error, super::CatalogError::Invalid { .. }),
                "{expected}: {error}"
            );
            assert_eq!(
                store.catalog().await.unwrap().provider_count(),
                1,
                "{expected}: the last good catalog is kept"
            );
            assert_eq!(
                std::fs::read(cache_path).unwrap(),
                written,
                "{expected}: the cache is not touched"
            );
            assert_eq!(
                store.status().await.unwrap().checked_at,
                first.status.checked_at,
                "{expected}: the cache metadata is not touched"
            );
        }
    }

    #[tokio::test]
    async fn an_unusable_cache_falls_back_to_the_seed() {
        let (_dir, home) = home();
        let server = fake_server(vec![Reply::ok("v1", &fake_catalog())]).await;
        let config = config(&server.url);
        let store = CatalogStore::open(&home, &config);
        let written = store.refresh().await.unwrap();
        let cache_path = written.status.cache_path.clone();
        let valid = std::fs::read(&cache_path).unwrap();

        std::fs::write(&cache_path, b"this is not a cache").unwrap();
        let store = CatalogStore::open(&home, &config);
        let catalog = store.catalog().await.unwrap();
        assert!(catalog.provider_count() > 300);
        assert!(store.status().await.unwrap().builtin);

        let mut envelope: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        envelope["version"] = json!(2);
        std::fs::write(&cache_path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let store = CatalogStore::open(&home, &config);
        let catalog = store.catalog().await.unwrap();
        assert!(catalog.provider_count() > 300);
        assert!(store.status().await.unwrap().builtin);

        std::fs::write(&cache_path, &valid).unwrap();
        let store = CatalogStore::open(&home, &config);
        assert_eq!(store.catalog().await.unwrap().provider_count(), 1);
        assert!(!store.status().await.unwrap().builtin);
    }

    #[tokio::test]
    async fn a_fresh_home_does_not_fetch_in_the_background() {
        let (_dir, home) = home();
        let server = fake_server(vec![Reply::ok("v1", &fake_catalog())]).await;
        let store = Arc::new(CatalogStore::open(&home, &config(&server.url)));

        store.spawn_background();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(server.requests.load(Ordering::SeqCst), 0);
        assert!(!home.root().join("catalog").join("catalog.json").exists());
    }
}
