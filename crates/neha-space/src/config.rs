//! The Space's configuration file.
//!
//! One file, `<home>/config.toml`, names where the bytes of Files are stored
//! and where the model catalog comes from. An absent file means the defaults,
//! so a Space that never needed S3 or a catalog override keeps working
//! unchanged. The file is strict: an unknown key, an S3 field beside the local
//! backend, or a malformed value refuses the start instead of being ignored.

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

use crate::home::Home;

/// The file a Space reads its configuration from.
const CONFIG_FILE: &str = "config.toml";

/// One Space's configuration.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Where the bytes of Files are stored.
    pub storage: Storage,
    /// Where the model catalog comes from and how long it stays fresh.
    pub catalog: CatalogConfig,
    /// What the Space does with the scripts a skill carries.
    pub skills: SkillsConfig,
    /// Where the Space may install a package from.
    pub plugins: PluginsConfig,
}

/// The sources a Space installs packages from.
///
/// An empty list allows none: the Admin names the repositories and directory
/// prefixes a package may come from.
#[derive(Clone, Debug, Default)]
pub struct PluginsConfig {
    /// Source prefixes a package may come from.
    pub sources: Vec<String>,
}

/// What the Space does with the scripts a skill carries.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkillsConfig {
    /// Whether the Space treats skills as instructions only, so the scripts of
    /// a skill never reach a Computer.
    pub instructions_only: bool,
}

/// The models.dev catalog.
#[derive(Clone, Debug)]
pub struct CatalogConfig {
    /// The address the catalog is fetched from.
    pub url: String,
    /// How many hours a cached catalog stays fresh; 0 disables background refresh.
    pub refresh_hours: u64,
}

/// The default refresh interval of a cached catalog.
const DEFAULT_REFRESH_HOURS: u64 = 24;

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            url: crate::catalog::DEFAULT_URL.to_owned(),
            refresh_hours: DEFAULT_REFRESH_HOURS,
        }
    }
}

impl Config {
    /// Reads `<home>/config.toml`; an absent file is the local default.
    pub fn load(home: &Home) -> Result<Self, ConfigError> {
        let path = home.root().join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let document: Document =
            toml::from_str(&text).map_err(|source| parse_failure(&path, &text, source))?;

        Ok(Self {
            storage: document.storage.into_storage()?,
            catalog: document.catalog.into_catalog()?,
            skills: SkillsConfig {
                instructions_only: document.skills.instructions_only.unwrap_or(false),
            },
            plugins: document.plugins.into_plugins()?,
        })
    }
}

/// Where the bytes of Files are stored.
#[derive(Clone, Debug, Default)]
pub enum Storage {
    /// A directory under the Space home.
    #[default]
    Local,
    /// A prefix of an S3-compatible bucket.
    S3(S3Storage),
}

/// What an S3-compatible store needs.
#[derive(Clone, Debug)]
pub struct S3Storage {
    /// The bucket that holds this Space's objects.
    pub bucket: String,
    /// The bucket's region; every S3 service names one, even outside AWS.
    pub region: String,
    /// The key prefix that belongs to this Space, without edge slashes.
    pub prefix: String,
    /// The service endpoint the bucket lives at.
    pub endpoint: String,
    /// The credential profile to read; absent means the default chain.
    pub profile: Option<String>,
    /// Whether requests use bucket-as-host addressing.
    pub virtual_host: bool,
    /// A KMS key for server-side encryption; absent means no SSE-KMS.
    pub sse_kms_key_id: Option<String>,
}

impl fmt::Display for Storage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => formatter.write_str("a directory in this Space"),
            Self::S3(storage) => {
                write!(formatter, "s3://{}", storage.bucket)?;
                if !storage.prefix.is_empty() {
                    write!(formatter, "/{}", storage.prefix)?;
                }
                write!(formatter, " at {}", storage.endpoint)
            }
        }
    }
}

/// The parsed `config.toml`; an absent section is that section's default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    storage: StorageSettings,
    #[serde(default)]
    catalog: CatalogSettings,
    #[serde(default)]
    skills: SkillsSettings,
    #[serde(default)]
    plugins: PluginsSettings,
}

/// The `[plugins]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginsSettings {
    sources: Option<Vec<String>>,
}

impl PluginsSettings {
    fn into_plugins(self) -> Result<PluginsConfig, ConfigError> {
        let Some(sources) = self.sources else {
            return Ok(PluginsConfig::default());
        };
        if sources.len() > 64 {
            return Err(ConfigError::Invalid(
                "plugins.sources names at most 64 sources".to_owned(),
            ));
        }
        for source in &sources {
            if source.is_empty() || source.len() > 512 || source.chars().any(char::is_control) {
                return Err(ConfigError::Invalid(
                    "a plugins.sources entry is 1 to 512 printable bytes".to_owned(),
                ));
            }
        }

        Ok(PluginsConfig { sources })
    }
}

/// The `[skills]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillsSettings {
    /// Whether skills are instructions only, so their scripts never run.
    instructions_only: Option<bool>,
}

/// The `[catalog]` section, before its values are checked against each other.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogSettings {
    url: Option<String>,
    refresh_hours: Option<u64>,
}

impl CatalogSettings {
    fn into_catalog(self) -> Result<CatalogConfig, ConfigError> {
        let default = CatalogConfig::default();
        let url = match self.url {
            Some(url) => {
                if url.is_empty() {
                    return Err(ConfigError::Invalid(
                        "catalog.url must not be empty".to_owned(),
                    ));
                }
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(ConfigError::Invalid(
                        "catalog.url must start with \"http://\" or \"https://\"".to_owned(),
                    ));
                }
                url
            }
            None => default.url,
        };

        Ok(CatalogConfig {
            url,
            refresh_hours: self.refresh_hours.unwrap_or(default.refresh_hours),
        })
    }
}

/// The `[storage]` section, before its values are checked against each other.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageSettings {
    backend: Option<String>,
    bucket: Option<String>,
    region: Option<String>,
    prefix: Option<String>,
    endpoint: Option<String>,
    profile: Option<String>,
    virtual_host: Option<bool>,
    sse_kms_key_id: Option<String>,
}

impl StorageSettings {
    fn into_storage(self) -> Result<Storage, ConfigError> {
        match self.backend.as_deref().unwrap_or("local") {
            "local" => self.into_local(),
            "s3" => self.into_s3(),
            other => Err(ConfigError::Invalid(format!(
                "storage.backend must be \"local\" or \"s3\", not {other:?}"
            ))),
        }
    }

    fn into_local(self) -> Result<Storage, ConfigError> {
        let s3_fields = [
            ("bucket", self.bucket.is_some()),
            ("region", self.region.is_some()),
            ("prefix", self.prefix.is_some()),
            ("endpoint", self.endpoint.is_some()),
            ("profile", self.profile.is_some()),
            ("virtual_host", self.virtual_host.is_some()),
            ("sse_kms_key_id", self.sse_kms_key_id.is_some()),
        ];
        if let Some((field, _)) = s3_fields.iter().find(|(_, set)| *set) {
            return Err(ConfigError::Invalid(format!(
                "storage.{field} is only allowed with storage.backend = \"s3\""
            )));
        }

        Ok(Storage::Local)
    }

    fn into_s3(self) -> Result<Storage, ConfigError> {
        let bucket = required(self.bucket, "bucket")?;
        let region = required(self.region, "region")?;
        let endpoint = required(self.endpoint, "endpoint")?;
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(ConfigError::Invalid(
                "storage.endpoint must start with \"http://\" or \"https://\"".to_owned(),
            ));
        }
        let prefix = prefix(self.prefix.unwrap_or_default())?;

        Ok(Storage::S3(S3Storage {
            bucket,
            region,
            prefix,
            endpoint,
            profile: self.profile,
            virtual_host: self.virtual_host.unwrap_or(false),
            sse_kms_key_id: self.sse_kms_key_id,
        }))
    }
}

/// Returns one required, non-empty S3 value.
fn required(value: Option<String>, field: &str) -> Result<String, ConfigError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        _ => Err(ConfigError::Invalid(format!(
            "storage.{field} is required when storage.backend = \"s3\""
        ))),
    }
}

/// Returns the normalized key prefix, refusing values that could escape it.
///
/// The leading slash is checked before trailing slashes are trimmed, so a
/// prefix that is only slashes cannot become the empty prefix.
fn prefix(value: String) -> Result<String, ConfigError> {
    if value.starts_with('/') {
        return Err(ConfigError::Invalid(
            "storage.prefix must not start with '/'".to_owned(),
        ));
    }
    let prefix = value.trim_end_matches('/');
    if prefix.contains("..") {
        return Err(ConfigError::Invalid(
            "storage.prefix must not contain \"..\"".to_owned(),
        ));
    }
    if prefix.contains('\\') {
        return Err(ConfigError::Invalid(
            "storage.prefix must not contain a backslash".to_owned(),
        ));
    }

    Ok(prefix.to_owned())
}

/// The configuration cannot be read.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists but cannot be read.
    #[error("cannot read the configuration at {}: {source}", path.display())]
    Read {
        /// The file that cannot be read.
        path: PathBuf,
        /// Why it cannot be read.
        #[source]
        source: std::io::Error,
    },
    /// The file is not the TOML this build knows.
    #[error(
        "cannot parse the configuration at {}, line {line}, column {column}: {message}",
        path.display()
    )]
    Parse {
        /// The file that cannot be parsed.
        path: PathBuf,
        /// The 1-based line the failure points at.
        line: usize,
        /// The 1-based column the failure points at.
        column: usize,
        /// What went wrong, without the offending line's text.
        message: String,
    },
    /// A value is not valid for the store it names.
    #[error("the configuration is invalid: {0}")]
    Invalid(String),
}

/// Turns a TOML failure into an error that never carries the document text.
///
/// The file may hold a secret, and `toml::de::Error`'s `Display` quotes the
/// offending line. Only the position and the message are kept.
fn parse_failure(path: &std::path::Path, text: &str, error: toml::de::Error) -> ConfigError {
    let (line, column) = error
        .span()
        .map_or((1, 1), |span| position(text, span.start));

    ConfigError::Parse {
        path: path.to_path_buf(),
        line,
        column,
        message: error.message().to_owned(),
    }
}

/// Returns the 1-based line and column of a byte offset.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(1, |line| line.chars().count() + 1);

    (line, column)
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, Storage};
    use crate::home::Home;

    fn home_with(document: &str) -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), document).unwrap();
        let home = Home::open(dir.path()).unwrap();

        (dir, home)
    }

    fn s3_storage(home: &Home) -> super::S3Storage {
        let config = Config::load(home).unwrap();
        let Storage::S3(storage) = config.storage else {
            panic!("expected the S3 store, got the local one");
        };

        storage
    }

    #[test]
    fn an_absent_file_is_the_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();

        let config = Config::load(&home).unwrap();

        assert!(matches!(config.storage, Storage::Local));
        assert_eq!(config.storage.to_string(), "a directory in this Space");
    }

    #[test]
    fn local_refuses_the_fields_of_s3() {
        let (_dir, home) = home_with("[storage]\nbackend = \"local\"\nbucket = \"neha\"\n");

        let error = Config::load(&home).unwrap_err();

        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
        assert!(error.to_string().contains("storage.bucket"), "{error}");
    }

    #[test]
    fn s3_requires_a_bucket_and_a_region() {
        let (_dir, home) = home_with("[storage]\nbackend = \"s3\"\nregion = \"us-east-1\"\n");
        let error = Config::load(&home).unwrap_err();
        assert!(error.to_string().contains("storage.bucket"), "{error}");

        let (_dir, home) = home_with("[storage]\nbackend = \"s3\"\nbucket = \"neha\"\n");
        let error = Config::load(&home).unwrap_err();
        assert!(error.to_string().contains("storage.region"), "{error}");
    }

    #[test]
    fn s3_requires_an_endpoint() {
        let (_dir, home) =
            home_with("[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"us-east-1\"\n");

        let error = Config::load(&home).unwrap_err();

        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
        assert!(error.to_string().contains("storage.endpoint"), "{error}");
    }

    #[test]
    fn an_endpoint_must_be_a_url() {
        for endpoint in ["s3.eu-central-1.amazonaws.com", "localhost:9000"] {
            let (_dir, home) = home_with(&format!(
                "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"us-east-1\"\n\
                 endpoint = \"{endpoint}\"\n"
            ));

            let error = Config::load(&home).unwrap_err();

            assert!(
                error.to_string().contains("storage.endpoint"),
                "{endpoint:?}: {error}"
            );
            assert!(error.to_string().contains("http://"), "{error}");
        }
    }

    #[test]
    fn a_trailing_slash_is_trimmed_from_a_prefix() {
        let (_dir, home) = home_with(
            "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"us-east-1\"\n\
             endpoint = \"https://s3.eu-central-1.amazonaws.com\"\n\
             prefix = \"spaces/acme///\"\n",
        );

        assert_eq!(s3_storage(&home).prefix, "spaces/acme");
    }

    #[test]
    fn a_parse_error_names_the_position_without_the_secret() {
        let (_dir, home) = home_with(
            "[storage]\nbackend = \"local\"\nsecret_access_key = \"REVIEW_SECRET_MARKER\"\n",
        );

        let error = Config::load(&home).unwrap_err();

        let message = error.to_string();
        assert!(!message.contains("REVIEW_SECRET_MARKER"), "{message}");
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("column"), "{message}");
    }

    #[test]
    fn s3_reads_the_store_it_names() {
        let (_dir, home) = home_with(
            "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"eu-central-1\"\n\
             prefix = \"spaces/acme/\"\nendpoint = \"https://s3.eu-central-1.amazonaws.com\"\n\
             profile = \"org-sso\"\nvirtual_host = true\nsse_kms_key_id = \"arn:aws:kms:key/1\"\n",
        );

        let storage = s3_storage(&home);

        assert_eq!(storage.bucket, "neha");
        assert_eq!(storage.region, "eu-central-1");
        assert_eq!(storage.prefix, "spaces/acme");
        assert_eq!(storage.endpoint, "https://s3.eu-central-1.amazonaws.com");
        assert_eq!(storage.profile.as_deref(), Some("org-sso"));
        assert!(storage.virtual_host);
        assert_eq!(storage.sse_kms_key_id.as_deref(), Some("arn:aws:kms:key/1"));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let (_dir, home) = home_with("[storage]\nbackend = \"local\"\nroot = \"/tmp\"\n");
        let error = Config::load(&home).unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
        assert!(error.to_string().contains("root"), "{error}");

        let (_dir, home) = home_with("[record]\npath = \"db\"\n");
        let error = Config::load(&home).unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
        assert!(error.to_string().contains("record"), "{error}");
    }

    #[test]
    fn an_unknown_backend_is_refused() {
        let (_dir, home) = home_with("[storage]\nbackend = \"ftp\"\n");

        let error = Config::load(&home).unwrap_err();

        assert!(error.to_string().contains("storage.backend"), "{error}");
        assert!(error.to_string().contains("local"), "{error}");
        assert!(error.to_string().contains("s3"), "{error}");
    }

    #[test]
    fn a_prefix_cannot_leave_its_space() {
        for prefix in ["/spaces/acme", "/", "///", "spaces/../acme", "spaces\\acme"] {
            // A TOML literal string passes the prefix through unchanged, so a
            // backslash reaches the validator instead of the TOML parser.
            let (_dir, home) = home_with(&format!(
                "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"us-east-1\"\n\
                 endpoint = \"https://s3.example.com\"\nprefix = '{prefix}'\n"
            ));

            let error = Config::load(&home).unwrap_err();

            assert!(
                error.to_string().contains("storage.prefix"),
                "{prefix:?}: {error}"
            );
        }
    }

    #[test]
    fn display_names_the_store() {
        assert_eq!(Storage::Local.to_string(), "a directory in this Space");

        let (_dir, home) = home_with(
            "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"eu-central-1\"\n\
             prefix = \"spaces/acme\"\nendpoint = \"https://s3.eu-central-1.amazonaws.com\"\n",
        );
        assert_eq!(
            Config::load(&home).unwrap().storage.to_string(),
            "s3://neha/spaces/acme at https://s3.eu-central-1.amazonaws.com"
        );

        let (_dir, home) = home_with(
            "[storage]\nbackend = \"s3\"\nbucket = \"neha\"\nregion = \"eu-central-1\"\n\
             endpoint = \"https://s3.eu-central-1.amazonaws.com\"\n",
        );
        assert_eq!(
            Config::load(&home).unwrap().storage.to_string(),
            "s3://neha at https://s3.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn an_absent_catalog_section_is_the_public_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();

        let config = Config::load(&home).unwrap();

        assert_eq!(config.catalog.url, crate::catalog::DEFAULT_URL);
        assert_eq!(config.catalog.refresh_hours, 24);
        assert_eq!(Config::default().catalog.url, crate::catalog::DEFAULT_URL);
    }

    #[test]
    fn the_catalog_section_names_its_source_and_interval() {
        let (_dir, home) = home_with(
            "[catalog]\nurl = \"https://catalog.example.com/api.json\"\nrefresh_hours = 0\n",
        );

        let config = Config::load(&home).unwrap();

        assert_eq!(config.catalog.url, "https://catalog.example.com/api.json");
        assert_eq!(config.catalog.refresh_hours, 0);
    }

    #[test]
    fn a_catalog_url_must_be_a_web_address() {
        for url in ["", "models.dev/api.json", "ftp://models.dev/api.json"] {
            let (_dir, home) = home_with(&format!("[catalog]\nurl = \"{url}\"\n"));

            let error = Config::load(&home).unwrap_err();

            assert!(matches!(error, ConfigError::Invalid(_)), "{url:?}: {error}");
            assert!(
                error.to_string().contains("catalog.url"),
                "{url:?}: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_catalog_key_is_refused() {
        let (_dir, home) = home_with("[catalog]\nrefresh_minutes = 5\n");

        let error = Config::load(&home).unwrap_err();

        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
        assert!(error.to_string().contains("refresh_minutes"), "{error}");
    }

    #[test]
    fn a_malformed_catalog_refresh_interval_is_refused() {
        for value in ["\"daily\"", "-1", "1.5"] {
            let (_dir, home) = home_with(&format!("[catalog]\nrefresh_hours = {value}\n"));

            let error = Config::load(&home).unwrap_err();

            assert!(
                matches!(error, ConfigError::Parse { .. }),
                "{value}: {error}"
            );
        }
    }
}
