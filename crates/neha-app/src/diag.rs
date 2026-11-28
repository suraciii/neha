//! Process diagnostics: structured logs, panic capture, startup reports, and
//! the one place that installs them.
//!
//! The binary calls [`init`] once, after parsing the command line and before a
//! command touches the home. A library never calls it; a crate that emits
//! events stays silent when no subscriber is installed.
//!
//! `eng/diagnostics.md` states the event catalog and the field allowlist. This
//! module implements the process-wide rules: `NEHA_LOG`, `NEHA_LOG_FORMAT`,
//! `NEHA_ENV`, the JSON line shape, and the private startup report.

use std::cell::Cell;
use std::fs;
use std::io::Write as _;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde_json::json;
use tracing_subscriber::Layer as _;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, Targets, filter_fn};
use tracing_subscriber::layer::Filter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The way this process is used, which selects its service name and default
/// log level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// `neha serve`: a long-lived Space.
    Serve,
    /// `neha computer run` and `neha computer worker`: a long-lived daemon.
    Computer,
    /// Any one-shot command; its output is the result, not a log stream.
    Cli,
}

impl Mode {
    fn service(self) -> &'static str {
        match self {
            Self::Serve => "neha-space",
            Self::Computer => "neha-computer",
            Self::Cli => "neha-cli",
        }
    }

    fn default_level(self) -> &'static str {
        match self {
            Self::Serve | Self::Computer => "info",
            Self::Cli => "warn",
        }
    }
}

/// The trace provider stays alive for the process so a span created in a
/// library has a valid context; no exporter is attached.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

thread_local! {
    static IN_PANIC_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Installs the subscriber, the panic hook, and the JSON line fields.
///
/// An invalid `NEHA_LOG`, `NEHA_LOG_FORMAT`, or `NEHA_ENV` fails before a
/// command opens its home, so a typo cannot leave a Space half started.
pub fn init(mode: Mode) -> Result<()> {
    let filters = match std::env::var("NEHA_LOG") {
        Ok(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                bail!("NEHA_LOG is empty; name a level or directives");
            }
            value
        }
        Err(std::env::VarError::NotPresent) => mode.default_level().to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("NEHA_LOG is not valid UTF-8");
        }
    };
    let format = match std::env::var("NEHA_LOG_FORMAT") {
        Ok(value) if value == "json" => Format::Json,
        Ok(value) if value == "text" => Format::Text,
        Ok(value) => bail!("NEHA_LOG_FORMAT must be `json` or `text`, not `{value}`"),
        Err(std::env::VarError::NotPresent) => Format::Json,
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("NEHA_LOG_FORMAT is not valid UTF-8");
        }
    };
    let environment = environment()?;
    let filter = directive_filter(&filters)?;

    let service = mode.service();
    let version = env!("CARGO_PKG_VERSION");
    let provider = SdkTracerProvider::builder().build();
    TRACER_PROVIDER
        .set(provider)
        .map_err(|_| anyhow!("diagnostics were initialized twice"))?;

    match format {
        Format::Json => {
            let otel_layer = tracer_layer();
            let mut json_layer = json_subscriber::fmt::layer()
                .with_current_span(false)
                .with_span_list(false)
                .flatten_event(true)
                .with_writer(std::io::stderr);
            {
                let inner = json_layer.inner_layer_mut();
                inner.with_opentelemetry_ids(true);
                inner.add_static_field("service", json!(service));
                inner.add_static_field("service_version", json!(version));
                inner.add_static_field("deployment_environment", json!(environment));
            }
            tracing_subscriber::registry()
                .with(json_layer.with_filter(filter_fn(diagnostic_target).and(filter)))
                .with(otel_layer.with_filter(filter_fn(diagnostic_target)))
                .try_init()
                .map_err(|error| anyhow!("cannot install the log subscriber: {error}"))?;
        }
        Format::Text => {
            let otel_layer = tracer_layer();
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_target(true);
            tracing_subscriber::registry()
                .with(fmt_layer.with_filter(filter_fn(diagnostic_target).and(filter)))
                .with(otel_layer.with_filter(filter_fn(diagnostic_target)))
                .try_init()
                .map_err(|error| anyhow!("cannot install the log subscriber: {error}"))?;
        }
    }

    install_panic_hook();
    tracing::debug!(
        target: "neha::diag",
        event = "diag.start",
        log_format = ?format,
        "diagnostics initialized"
    );

    Ok(())
}

/// Flushes anything a future sink holds.
///
/// The stderr sink is unbuffered; the function exists so callers have one
/// shutdown point when a file or network sink joins the design.
pub fn shutdown() {
    tracing::debug!(target: "neha::diag", event = "diag.shutdown", "diagnostics stopping");
}

/// Writes the private report a failed start leaves behind.
///
/// The file lives under `<home>/logs`, is readable only by the user, and
/// carries the build, the phase, and a message scrubbed by
/// [`neha_space::diagnostics::scrub_error`]. A write failure is returned to the
/// caller, which prints the report instead of claiming a saved path.
pub fn write_startup_failure(
    home: &Path,
    phase: &str,
    error_kind: &str,
    message: &str,
) -> std::io::Result<PathBuf> {
    let directory = home.join("logs");
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let path = directory.join(format!("startup-{stamp}-{}.log", std::process::id()));
    let report = serde_json::to_vec_pretty(&json!({
        "created_unix_ms": stamp,
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": neha_space::SCHEMA_VERSION,
        "protocol_version": neha::surface::PROTOCOL_VERSION,
        "phase": phase,
        "error_kind": error_kind,
        "error": message,
        "warning": "raw diagnostics can contain configuration or credential values; review before sharing",
    }))
    .map_err(std::io::Error::other)?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(&report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    Ok(path)
}

/// Converts a start error into a phase, a kind, and a typed summary.
///
/// A startup event and the private report carry the summary, never the
/// original display text: a configuration error can quote the offending value,
/// and a record or remote error can carry a path or a response. The caller
/// keeps the original error for its own exit message.
#[must_use]
pub fn startup_failure(error: &neha_space::ServerError) -> (&'static str, &'static str, String) {
    use neha_space::ServerError;

    match error {
        ServerError::Config(neha_space::ConfigError::Read { .. }) => (
            "config",
            "config",
            "the configuration file cannot be read".to_owned(),
        ),
        ServerError::Config(neha_space::ConfigError::Parse { line, column, .. }) => (
            "config",
            "config",
            format!("the configuration file is not valid TOML at line {line}, column {column}"),
        ),
        ServerError::Config(neha_space::ConfigError::Invalid(_)) => (
            "config",
            "config",
            "the configuration names an unsupported value".to_owned(),
        ),
        ServerError::PublicUrl(_) => (
            "config",
            "public_url",
            "the configured public origin is invalid".to_owned(),
        ),
        ServerError::Store(_) => ("store", "store", "the record cannot be opened".to_owned()),
        ServerError::Identity(_) => (
            "identity",
            "identity",
            "the identity data cannot be opened".to_owned(),
        ),
        ServerError::Secrets(_) => (
            "secrets",
            "secrets",
            "the sealed secrets cannot be read".to_owned(),
        ),
        ServerError::Blobs(_) | ServerError::FileStore { .. } => {
            ("blobs", "blobs", "the File store cannot be used".to_owned())
        }
        ServerError::Io(_) => ("bind", "io", "the listener cannot be bound".to_owned()),
        ServerError::Ticket(_) => (
            "ticket",
            "ticket",
            "the local pairing ticket cannot be written".to_owned(),
        ),
        ServerError::Ui(_) => (
            "ui",
            "ui",
            "the browser client has no index.html".to_owned(),
        ),
        ServerError::UiOverlapsHome(_) | ServerError::UiOverlapsConfiguration(_) => (
            "ui",
            "ui",
            "the browser client directory overlaps a private directory".to_owned(),
        ),
        ServerError::Model(_) => (
            "workers",
            "model",
            "the model client cannot be built".to_owned(),
        ),
    }
}

/// The typed summary for a home that cannot be opened.
#[must_use]
pub fn home_failure(error: &neha_space::HomeError) -> (&'static str, String) {
    use neha_space::HomeError;

    match error {
        HomeError::MissingEnv => ("home", "NEHA_HOME is not set".to_owned()),
        HomeError::Io(_) => ("home", "the Space home cannot be opened".to_owned()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Json,
    Text,
}

/// Targets a Neha diagnostic may use. A dependency target is never enabled,
/// because a dependency logs its own request and message payloads.
const TARGET_PREFIX: &str = "neha::";

fn diagnostic_target(meta: &tracing::Metadata<'_>) -> bool {
    meta.target().starts_with(TARGET_PREFIX)
}

fn environment() -> Result<String> {
    const ALLOWED: [&str; 4] = ["development", "staging", "production", "test"];

    match std::env::var("NEHA_ENV") {
        Ok(value) if ALLOWED.contains(&value.as_str()) => Ok(value),
        Ok(value) => {
            bail!("NEHA_ENV must be one of development, staging, production, test, not `{value}`")
        }
        Err(std::env::VarError::NotPresent) => Ok("production".to_owned()),
        Err(std::env::VarError::NotUnicode(_)) => bail!("NEHA_ENV is not valid UTF-8"),
    }
}

/// Builds the level filter from `RUST_LOG`-style directives.
///
/// A plain list of levels and `target=level` pairs goes through [`Targets`],
/// which keeps no state per span. A directive that names a span or field needs
/// [`EnvFilter`], the only filter that understands it. The function refuses an
/// invalid directive instead of silently dropping it.
fn directive_filter(directives: &str) -> Result<Box<dyn Filter<Registry> + Send + Sync>> {
    if directives
        .split(',')
        .any(|directive| directive.trim().is_empty())
    {
        bail!("NEHA_LOG has an empty directive");
    }

    if plain_directives(directives) {
        let targets = directives
            .parse::<Targets>()
            .map_err(|error| anyhow!("invalid NEHA_LOG directive `{directives}`: {error}"))?;
        return Ok(Box::new(targets));
    }

    let env_filter = EnvFilter::try_new(directives)
        .map_err(|error| anyhow!("invalid NEHA_LOG directive `{directives}`: {error}"))?;
    Ok(Box::new(env_filter))
}

fn plain_directives(directives: &str) -> bool {
    directives.split(',').all(|directive| {
        let directive = directive.trim();
        !directive.is_empty()
            && !directive.starts_with('=')
            && !directive.ends_with('=')
            && directive.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-' | '=')
            })
    })
}

fn tracer_layer<S>()
-> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let provider = TRACER_PROVIDER
        .get()
        .expect("the trace provider is installed before the subscriber");
    tracing_opentelemetry::layer().with_tracer(provider.tracer("neha"))
}

fn install_panic_hook() {
    panic::set_hook(Box::new(|info| {
        if IN_PANIC_HOOK.with(|flag| flag.replace(true)) {
            return;
        }

        let location = info.location().map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        });
        let thread = std::thread::current()
            .name()
            .map(str::to_owned)
            .unwrap_or_else(|| "unnamed".to_owned());

        tracing::error!(
            target: "neha::diag",
            event = "diag.panic",
            location = location.as_deref(),
            thread = thread.as_str(),
            "the process panicked"
        );

        // The default hook prints the payload verbatim. A panic message can
        // carry content, so neither output copies it; a backtrace is printed
        // only when the operator asks for one.
        eprintln!(
            "thread '{thread}' panicked at {} (the panic message is not recorded by diagnostics)",
            location.as_deref().unwrap_or("an unknown location")
        );
        if backtrace_enabled() {
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }

        IN_PANIC_HOOK.with(|flag| flag.set(false));
    }));
}

fn backtrace_enabled() -> bool {
    std::env::var_os("RUST_BACKTRACE").is_some_and(|value| value != "0")
}

#[cfg(test)]
mod tests {
    use super::{directive_filter, plain_directives};

    #[test]
    fn plain_directives_use_the_stateless_filter() {
        assert!(plain_directives("info"));
        assert!(plain_directives("info,neha_space=debug,hyper=off"));
        assert!(!plain_directives("[span]=debug"));
        assert!(!plain_directives("info,,debug"));
        assert!(!plain_directives("=debug"));
    }

    #[test]
    fn a_directive_list_is_accepted_or_refused_as_a_whole() {
        assert!(directive_filter("info").is_ok());
        assert!(directive_filter("info,neha_space=debug").is_ok());
        assert!(directive_filter("[span]=debug").is_ok());
        assert!(directive_filter("info,").is_err());
        assert!(directive_filter("=debug").is_err());
        assert!(directive_filter("[span=debug").is_err());
    }
}
