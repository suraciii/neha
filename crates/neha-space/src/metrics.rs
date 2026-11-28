//! The Space's metrics endpoint.
//!
//! Instruments are OpenTelemetry counters, gauges, and histograms collected by
//! `opentelemetry-prometheus` into one [`prometheus::Registry`]. The
//! instruments are process-wide: a Space process serves one Space, so the
//! registry a scrape reads is that Space's. Labels are fixed enums; an
//! identifier, a path, a tool name, or an error string is never a label.
//!
//! `NEHA_METRICS_TOKEN` disables the endpoint when unset and must name a
//! 32-character-or-longer value without whitespace when set; a malformed value
//! disables it and the start logs `metrics.disabled`.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, MeterProvider as _};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{Encoder as _, TextEncoder};

/// The HTTP histogram's fixed boundaries, in seconds.
const HTTP_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
/// The Model histogram's fixed boundaries, in seconds.
const MODEL_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];
/// The run histogram's fixed boundaries, in seconds.
const RUN_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 900.0];
/// The action/MCP histogram's fixed boundaries, in seconds.
const ACTION_BUCKETS: &[f64] = &[
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

static METRICS: OnceLock<Metrics> = OnceLock::new();
static PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();
static TOKEN: OnceLock<Option<String>> = OnceLock::new();
static TOKEN_PROBLEM: OnceLock<Option<&'static str>> = OnceLock::new();
static STARTED: OnceLock<std::time::Instant> = OnceLock::new();

/// The token the endpoint requires, when it is enabled.
pub fn token() -> Option<String> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(value) = TEST_TOKEN.lock().expect("metrics test token lock").clone() {
        return value;
    }

    TOKEN
        .get_or_init(|| {
            let value = std::env::var("NEHA_METRICS_TOKEN").ok()?;
            let problem = if value.len() < 32 {
                Some("short")
            } else if value.chars().any(char::is_whitespace) {
                Some("whitespace")
            } else {
                None
            };
            let _ = TOKEN_PROBLEM.set(problem);
            if problem.is_some() { None } else { Some(value) }
        })
        .clone()
}

/// Sets the token for an integration test; `None` restores the environment.
#[cfg(any(test, feature = "test-support"))]
pub fn set_token_for_test(value: Option<&str>) {
    *TEST_TOKEN.lock().expect("metrics test token lock") =
        value.map(|value| Some(value.to_owned()));
}

/// Disables the endpoint for an integration test, whatever the environment.
#[cfg(any(test, feature = "test-support"))]
pub fn disable_token_for_test() {
    *TEST_TOKEN.lock().expect("metrics test token lock") = Some(None);
}

/// Restores the environment token for a test.
#[cfg(any(test, feature = "test-support"))]
pub fn restore_token_for_test() {
    *TEST_TOKEN.lock().expect("metrics test token lock") = None;
}

#[cfg(any(test, feature = "test-support"))]
static TEST_TOKEN: std::sync::Mutex<Option<Option<String>>> = std::sync::Mutex::new(None);

/// Why a configured token disabled the endpoint, when it did.
pub fn token_problem() -> Option<&'static str> {
    let _ = token();
    TOKEN_PROBLEM.get().and_then(|problem| *problem)
}

/// Installs the meter provider on first use and returns the instruments.
pub fn install() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let registry = prometheus::Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .without_target_info()
            .scope_info_enabled(false)
            .without_counter_suffixes()
            .build()
            .expect("the Prometheus registry accepts the collector");
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();
        let meter = provider.meter("neha");
        let _ = PROVIDER.set(provider);
        let _ = STARTED.set(std::time::Instant::now());

        Metrics {
            registry,
            http_requests: meter
                .u64_counter("neha_http_requests_total")
                .with_description("HTTP requests")
                .build(),
            http_duration: meter
                .f64_histogram("neha_http_request_duration_seconds")
                .with_description("HTTP request duration")
                .with_boundaries(HTTP_BUCKETS.to_vec())
                .build(),
            ws_clients: meter
                .u64_gauge("neha_ws_clients")
                .with_description("Connected WebSocket clients")
                .build(),
            ws_command_errors: meter
                .u64_counter("neha_ws_command_errors_total")
                .with_description("Refused WebSocket commands")
                .build(),
            ws_events_lagged: meter
                .u64_counter("neha_ws_events_lagged_total")
                .with_description("Event-stream lagged readers")
                .build(),
            computers_connected: meter
                .u64_gauge("neha_computers_connected")
                .with_description("Connected Computers")
                .build(),
            runs_queued: meter
                .u64_gauge("neha_runs_queued")
                .with_description("Queued runs")
                .build(),
            runs_running: meter
                .u64_gauge("neha_runs_running")
                .with_description("Running runs")
                .build(),
            runs_waiting: meter
                .u64_gauge("neha_runs_waiting")
                .with_description("Runs waiting for an answer")
                .build(),
            runs_finished: meter
                .u64_counter("neha_runs_finished_total")
                .with_description("Finished runs")
                .build(),
            run_duration: meter
                .f64_histogram("neha_run_duration_seconds")
                .with_description("Run duration")
                .with_boundaries(RUN_BUCKETS.to_vec())
                .build(),
            model_calls: meter
                .u64_counter("neha_model_calls_total")
                .with_description("Model calls")
                .build(),
            model_duration: meter
                .f64_histogram("neha_model_call_duration_seconds")
                .with_description("Model call duration")
                .with_boundaries(MODEL_BUCKETS.to_vec())
                .build(),
            model_retries: meter
                .u64_counter("neha_model_retries_total")
                .with_description("Model retries")
                .build(),
            model_failures: meter
                .u64_counter("neha_model_failures_total")
                .with_description("Failed Model calls")
                .build(),
            tokens: meter
                .u64_counter("neha_tokens_total")
                .with_description("Tokens reported by Providers")
                .build(),
            mcp_calls: meter
                .u64_counter("neha_mcp_calls_total")
                .with_description("MCP calls")
                .build(),
            mcp_discovery_failures: meter
                .u64_counter("neha_mcp_discovery_failures_total")
                .with_description("MCP discovery failures")
                .build(),
            workers_running: meter
                .u64_gauge("neha_workers_running")
                .with_description("Running workers")
                .build(),
            push_deliveries: meter
                .u64_counter("neha_push_deliveries_total")
                .with_description("Push deliveries")
                .build(),
            worker_operations: meter
                .u64_counter("neha_worker_operations_total")
                .with_description("Worker Supervisor operations")
                .build(),
            store_errors: meter
                .u64_counter("neha_store_errors_total")
                .with_description("Record errors")
                .build(),
            blob_errors: meter
                .u64_counter("neha_blob_errors_total")
                .with_description("File store errors")
                .build(),
            catalog_refresh: meter
                .u64_counter("neha_catalog_refresh_total")
                .with_description("Catalog refreshes")
                .build(),
            internal_errors: meter
                .u64_counter("neha_internal_errors_total")
                .with_description("Internal errors")
                .build(),
            actions: meter
                .u64_counter("neha_actions_total")
                .with_description("Actions")
                .build(),
            action_duration: meter
                .f64_histogram("neha_action_duration_seconds")
                .with_description("Action duration")
                .with_boundaries(ACTION_BUCKETS.to_vec())
                .build(),
            build_info: meter
                .u64_gauge("neha_build_info")
                .with_description("Build identity")
                .build(),
            uptime: meter
                .u64_gauge("neha_uptime_seconds")
                .with_description("Process uptime")
                .build(),
            record_seq: meter
                .u64_gauge("neha_record_seq")
                .with_description("Last recorded sequence")
                .build(),
            record_bytes: meter
                .u64_gauge("neha_record_bytes")
                .with_description("Record database size")
                .build(),
        }
    })
}

/// One scrape's instruments.
pub struct Metrics {
    registry: prometheus::Registry,
    /// HTTP requests by route, method, and status class.
    pub http_requests: Counter<u64>,
    /// HTTP request duration.
    pub http_duration: Histogram<f64>,
    /// Connected WebSocket clients.
    pub ws_clients: Gauge<u64>,
    /// Commands refused with a protocol error code.
    pub ws_command_errors: Counter<u64>,
    /// Event readers that lagged behind the broadcast.
    pub ws_events_lagged: Counter<u64>,
    /// Connected Computers.
    pub computers_connected: Gauge<u64>,
    /// Runs waiting for their Bot.
    pub runs_queued: Gauge<u64>,
    /// Runs working.
    pub runs_running: Gauge<u64>,
    /// Runs waiting for an answer.
    pub runs_waiting: Gauge<u64>,
    /// Runs that ended, by persisted status.
    pub runs_finished: Counter<u64>,
    /// Run duration, by persisted status.
    pub run_duration: Histogram<f64>,
    /// Model calls, by protocol and outcome.
    pub model_calls: Counter<u64>,
    /// Model call duration, by protocol.
    pub model_duration: Histogram<f64>,
    /// Model retries.
    pub model_retries: Counter<u64>,
    /// Model failures.
    pub model_failures: Counter<u64>,
    /// Tokens, by kind.
    pub tokens: Counter<u64>,
    /// MCP calls, by outcome.
    pub mcp_calls: Counter<u64>,
    /// MCP discovery failures, by transport.
    pub mcp_discovery_failures: Counter<u64>,
    /// Running worker containers.
    pub workers_running: Gauge<u64>,
    /// Push deliveries, by outcome.
    pub push_deliveries: Counter<u64>,
    /// Supervisor operations, by operation and outcome.
    pub worker_operations: Counter<u64>,
    /// Record errors, by operation.
    pub store_errors: Counter<u64>,
    /// File store errors, by operation.
    pub blob_errors: Counter<u64>,
    /// Catalog refreshes, by outcome.
    pub catalog_refresh: Counter<u64>,
    /// Internal errors, by component.
    pub internal_errors: Counter<u64>,
    /// Actions, by outcome.
    pub actions: Counter<u64>,
    /// Action duration, by outcome.
    pub action_duration: Histogram<f64>,
    /// Build identity.
    pub build_info: Gauge<u64>,
    /// Process uptime.
    pub uptime: Gauge<u64>,
    /// Last recorded sequence.
    pub record_seq: Gauge<u64>,
    /// Record database size.
    pub record_bytes: Gauge<u64>,
}

impl Metrics {
    /// Records one HTTP request.
    pub fn http_request(&self, route: &str, method: &str, status: u16, seconds: f64) {
        let status = match status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        let labels = [
            KeyValue::new("route", route.to_owned()),
            KeyValue::new("method", method.to_owned()),
            KeyValue::new("status_class", status),
        ];
        self.http_requests.add(1, &labels);
        self.http_duration.record(seconds, &labels[..2]);
    }

    /// Records one internal error.
    pub fn internal_error(&self, component: &str) {
        self.internal_errors
            .add(1, &[KeyValue::new("component", component.to_owned())]);
    }

    /// Records one refused command.
    pub fn ws_command_error(&self, code: &str) {
        self.ws_command_errors
            .add(1, &[KeyValue::new("code", code.to_owned())]);
    }

    /// Records one finished run.
    pub fn run_finished(&self, status: &str, seconds: f64) {
        let labels = [KeyValue::new("status", status.to_owned())];
        self.runs_finished.add(1, &labels);
        self.run_duration.record(seconds, &labels);
    }

    /// Records a finished run whose start this process did not observe.
    ///
    /// After a restart the start time is gone; inventing a zero would bias the
    /// duration histogram, so only the counter moves.
    pub fn run_finished_without_duration(&self, status: &str) {
        self.runs_finished
            .add(1, &[KeyValue::new("status", status.to_owned())]);
    }

    /// Records one Model call end.
    pub fn model_call(&self, protocol: &str, outcome: &str, seconds: f64) {
        let labels = [
            KeyValue::new("protocol", protocol.to_owned()),
            KeyValue::new("outcome", outcome.to_owned()),
        ];
        self.model_calls.add(1, &labels);
        self.model_duration
            .record(seconds, &[KeyValue::new("protocol", protocol.to_owned())]);
    }

    /// Records one Model retry.
    pub fn model_retry(&self, protocol: &str, error_kind: &str) {
        self.model_retries.add(
            1,
            &[
                KeyValue::new("protocol", protocol.to_owned()),
                KeyValue::new("error_kind", error_kind.to_owned()),
            ],
        );
    }

    /// Records one failed Model call.
    pub fn model_failure(&self, error_kind: &str) {
        self.model_failures
            .add(1, &[KeyValue::new("error_kind", error_kind.to_owned())]);
    }

    /// Records reported tokens in one kind.
    pub fn tokens(&self, kind: &str, count: u64) {
        self.tokens
            .add(count, &[KeyValue::new("kind", kind.to_owned())]);
    }

    /// Records one MCP call end.
    pub fn mcp_call(&self, outcome: &str) {
        self.mcp_calls
            .add(1, &[KeyValue::new("outcome", outcome.to_owned())]);
    }

    /// Records one MCP discovery failure.
    pub fn mcp_discovery_failure(&self, transport: &str) {
        self.mcp_discovery_failures
            .add(1, &[KeyValue::new("transport", transport.to_owned())]);
    }

    /// Records one push delivery end.
    pub fn push_delivery(&self, outcome: &str) {
        self.push_deliveries
            .add(1, &[KeyValue::new("outcome", outcome.to_owned())]);
    }

    /// Records one worker Supervisor operation end.
    pub fn worker_operation(&self, operation: &str, outcome: &str) {
        self.worker_operations.add(
            1,
            &[
                KeyValue::new("operation", operation.to_owned()),
                KeyValue::new("outcome", outcome.to_owned()),
            ],
        );
    }

    /// Records one record error.
    pub fn store_error(&self, operation: &str) {
        self.store_errors
            .add(1, &[KeyValue::new("operation", operation.to_owned())]);
    }

    /// Records one File store error.
    pub fn blob_error(&self, operation: &str) {
        self.blob_errors
            .add(1, &[KeyValue::new("operation", operation.to_owned())]);
    }

    /// Records one catalog refresh end.
    pub fn catalog_refresh(&self, outcome: &str) {
        self.catalog_refresh
            .add(1, &[KeyValue::new("outcome", outcome.to_owned())]);
    }

    /// Records one action end.
    pub fn action(&self, outcome: &str, seconds: f64) {
        let labels = [KeyValue::new("outcome", outcome.to_owned())];
        self.actions.add(1, &labels);
        self.action_duration.record(seconds, &labels);
    }

    /// Records one lagged event reader.
    pub fn ws_events_lagged(&self) {
        self.ws_events_lagged.add(1, &[]);
    }

    /// Records the build identity and uptime for one scrape.
    pub fn scrape(&self) {
        self.build_info.record(
            1,
            &[
                KeyValue::new("version", env!("CARGO_PKG_VERSION")),
                KeyValue::new("schema_version", crate::schema::SCHEMA_VERSION.to_string()),
                KeyValue::new(
                    "protocol_version",
                    neha::surface::PROTOCOL_VERSION.to_string(),
                ),
            ],
        );
        if let Some(started) = STARTED.get() {
            self.uptime.record(started.elapsed().as_secs(), &[]);
        }
    }

    /// Renders the current registry as Prometheus text.
    pub fn render(&self) -> Result<Vec<u8>, prometheus::Error> {
        let families = self.registry.gather();
        let mut body = Vec::new();
        TextEncoder::new().encode(&families, &mut body)?;
        Ok(body)
    }
}
