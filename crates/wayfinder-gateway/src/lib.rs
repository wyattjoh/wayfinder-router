use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::to_bytes;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as JsonValue};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use toml::Value;
use wayfinder_internal_core::complexity::{
    explain_score, recommend_tier, score_complexity, ComplexityScore, RoutingConfig, Tier,
};
use wayfinder_internal_core::config::{dump_routing_toml, routing_config_from_toml, CONFIG_FILE};
use wayfinder_internal_core::feedback::{read_labels, record_label, DEFAULT_LOG};
use wayfinder_internal_core::pricing::{
    estimate_tokens, price_table, table_version, turn_cost, usage_tokens, Date, SavingsLedger,
    TurnCost,
};
use wayfinder_internal_core::profiles::PROFILES;
use wayfinder_internal_core::vkeys;
use wayfinder_internal_core::{DEFAULT_HOST, DEFAULT_PORT};

pub mod bootstrap;
pub mod recalibrate;
pub mod reliability;

pub const COMMAND_NAME: &str = "serve";

const DEMO_HTML: &str = include_str!("assets/demo.html");
const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
const AUTO_DIRECTIVE: &str = "auto";
const PREFER_LOCAL_DIRECTIVE: &str = "prefer-local";
const PREFER_HOSTED_DIRECTIVE: &str = "prefer-hosted";
const PREFER_CLOUD_DIRECTIVE: &str = "prefer-cloud";
const DEFAULT_CACHE_TTL: f64 = 300.0;
const DEFAULT_CACHE_MAX_ENTRIES: usize = 1024;
const DEFAULT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_RATE_LIMIT_WINDOW: f64 = 60.0;
const DEFAULT_RETRIES: usize = 2;
const DEFAULT_BREAKER_THRESHOLD: usize = 5;
const DEFAULT_BREAKER_COOLDOWN: f64 = 30.0;
const DEFAULT_FAILOVER: &str = "same-tier";
const RECENT_LIMIT: usize = 200;
const FEEDBACK_TOKEN_ENV: &str = "WAYFINDER_ROUTER_FEEDBACK_TOKEN";
const SAVINGS_FILE_ENV: &str = "WAYFINDER_ROUTER_SAVINGS_FILE";
const SAVINGS_FILE: &str = "wayfinder-savings.json";
const CONFIG_BODY_LIMIT: usize = 1024 * 1024;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Wayfinder routing</title>
  <style>
    :root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, sans-serif; }
    body { margin: 2rem; max-width: 780px; }
    pre { padding: 1rem; border: 1px solid color-mix(in srgb, currentColor 20%, transparent); border-radius: 6px; overflow: auto; }
  </style>
</head>
<body>
  <h1>Wayfinder routing</h1>
  <p>Recent routing decisions are exposed as metadata at <code>/router/recent</code>.</p>
  <pre id="recent">Loading...</pre>
  <script>
    async function refresh() {
      const response = await fetch('/router/recent?limit=50');
      document.getElementById('recent').textContent = JSON.stringify(await response.json(), null, 2);
    }
    refresh();
    setInterval(refresh, 2000);
  </script>
</body>
</html>"#;

#[derive(Debug, Clone, PartialEq)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub dry_run: bool,
    pub timeout_seconds: Option<f64>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            dry_run: false,
            timeout_seconds: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    message: String,
}

impl GatewayError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for GatewayError {}

impl From<std::io::Error> for GatewayError {
    fn from(err: std::io::Error) -> Self {
        Self::new(err.to_string())
    }
}

#[derive(Clone)]
struct AppState {
    options: ServeOptions,
    config_path: PathBuf,
    feedback_path: PathBuf,
    routing: RoutingConfig,
    gateway: GatewayConfig,
    model_ids: Vec<String>,
    price_table: BTreeMap<String, f64>,
    priced: bool,
    runtime: Arc<GatewayRuntime>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GatewayConfig {
    pub models: BTreeMap<String, GatewayModel>,
    pub offline: bool,
    pub retries: usize,
    pub breaker_threshold: usize,
    pub breaker_cooldown: f64,
    pub failover: String,
    cache: Option<CacheConfig>,
    rate_limit: Option<RateLimitConfig>,
    keys: BTreeMap<String, VirtualKeyConfig>,
}

/// An upstream endpoint a recommended model name maps to (from `[gateway.models]`).
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayModel {
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub api_key_cmd: Option<String>,
    pub cost_per_1k: Option<f64>,
    pub fallbacks: Vec<String>,
    pub context_window: Option<usize>,
}

/// One OpenAI-style chat message handed to the in-process relay.
///
/// The relay (`invoke_messages` / `stream_messages`) sends these as the upstream
/// `messages` array, so a caller that has a `Vec<{role, content}>` conversation can
/// reach a configured model without standing up the axum server (WF-DESIGN-0001).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayMessage {
    pub role: String,
    pub content: String,
}

impl RelayMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// A failure relaying a call to an upstream model (the relay's public error surface).
///
/// `Transport` covers a connection-level failure (timeout, connection refused, the
/// equivalent of the gateway being unavailable). `Status` carries a non-success HTTP
/// status and the upstream's body. `Shape` means the reply could not be parsed into the
/// expected OpenAI completion shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamError {
    Transport(String),
    Status { status: u16, body: String },
    Shape(String),
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "upstream transport failed: {message}"),
            Self::Status { status, body } => {
                write!(f, "upstream returned {status}: {body}")
            }
            Self::Shape(message) => write!(f, "upstream returned an unexpected shape: {message}"),
        }
    }
}

impl Error for UpstreamError {}

#[derive(Clone, Debug, PartialEq)]
struct CacheConfig {
    enabled: bool,
    ttl: f64,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct RateLimitConfig {
    rpm: Option<u64>,
    tpm: Option<u64>,
    window: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VirtualKeyConfig {
    hash: String,
}

struct GatewayRuntime {
    cache: Mutex<ResponseCache>,
    rate_limiter: Mutex<RateLimiter>,
    breaker: Mutex<reliability::CircuitBreaker>,
    metrics: Mutex<Metrics>,
    recent: Mutex<VecDeque<RecentDecision>>,
    ledger: Mutex<SavingsLedger>,
    savings_path: PathBuf,
}

#[derive(Default)]
struct ResponseCache {
    config: Option<CacheConfig>,
    entries: BTreeMap<String, CacheEntry>,
    lru: VecDeque<String>,
    bytes: usize,
}

#[derive(Clone)]
struct CacheEntry {
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
    stored_at: Instant,
    prompt_tokens: usize,
    completion_tokens: usize,
    estimated: bool,
    avoided_cost: f64,
}

struct RateLimiter {
    config: Option<RateLimitConfig>,
    clock: Clock,
    window_started: Option<Instant>,
    requests: u64,
    tokens: u64,
}

struct RateAdmission {
    allowed: bool,
    limit: &'static str,
    retry_after: u64,
}

struct RateSnapshot {
    limit: u64,
    remaining: u64,
    reset: u64,
}

#[derive(Default)]
struct Metrics {
    requests: BTreeMap<(String, String), u64>,
    upstream_errors: BTreeMap<String, u64>,
    failovers: BTreeMap<(String, String), u64>,
    rate_limited: BTreeMap<String, u64>,
    key_requests: BTreeMap<String, u64>,
    cache_hits: u64,
    cache_misses: u64,
    cache_avoided_cost: f64,
    realized_cost: f64,
    baseline_cost: f64,
    decision_latency_sum: f64,
    decision_latency_count: u64,
    upstream_latency: BTreeMap<String, LatencyTotals>,
}

#[derive(Default)]
struct LatencyTotals {
    sum: f64,
    count: u64,
}

#[derive(Clone, Serialize)]
struct RecentDecision {
    request_id: String,
    model: String,
    served_by: String,
    score: f64,
    mode: String,
    ts: u64,
    cost: RecentCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
}

#[derive(Clone, Serialize)]
struct RecentCost {
    realized: f64,
    baseline: f64,
    saved: f64,
    tokens: usize,
    unit: &'static str,
    estimated: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    models: Vec<String>,
    offline: bool,
}

#[derive(Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelEntry>,
}

#[derive(Serialize)]
struct ModelEntry {
    id: String,
    object: &'static str,
    created: u64,
    owned_by: &'static str,
}

#[derive(Deserialize)]
struct SavingsQuery {
    period: Option<String>,
}

#[derive(Deserialize)]
struct FeedbackRequest {
    text: Option<String>,
    label: Option<String>,
}

#[derive(Serialize)]
struct RouterModelEntry {
    name: String,
    endpoint: String,
    model: String,
    api_key_env: Option<String>,
    key_ok: bool,
}

pub fn serve_summary(options: &ServeOptions) -> String {
    let mode = if options.dry_run {
        "dry-run"
    } else {
        "forwarding-disabled"
    };
    let timeout = options
        .timeout_seconds
        .map(|value| format!("{value}s timeout"))
        .unwrap_or_else(|| "default timeout".to_owned());
    format!(
        "wayfinder-router serve listening on http://{}:{} ({mode}, {timeout})",
        options.host, options.port
    )
}

pub fn build_app(options: ServeOptions) -> Result<Router, GatewayError> {
    build_app_from_dir(options, std::env::current_dir()?)
}

pub fn build_app_from_dir(
    options: ServeOptions,
    start_dir: impl AsRef<Path>,
) -> Result<Router, GatewayError> {
    let state = AppState::load(options, start_dir.as_ref(), system_clock())?;
    Ok(router_with_state(state))
}

pub fn build_app_from_dir_with_clock<F>(
    options: ServeOptions,
    start_dir: impl AsRef<Path>,
    clock: F,
) -> Result<Router, GatewayError>
where
    F: Fn() -> Instant + Send + Sync + 'static,
{
    let state = AppState::load(options, start_dir.as_ref(), Arc::new(clock))?;
    Ok(router_with_state(state))
}

fn router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/models", get(list_models))
        .route("/v1/feedback", post(record_feedback))
        .route("/v1/savings", get(savings))
        .route("/savings", get(savings))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/messages", post(anthropic_messages))
        .route("/metrics", get(metrics))
        .route("/router/recent", get(router_recent))
        .route("/router/profiles", get(router_profiles))
        .route("/router/models", get(router_models))
        .route("/router", get(router_dashboard))
        .route("/demo", get(demo_page))
        .route(
            "/router/config",
            get(router_config).post(write_router_config),
        )
        .with_state(state)
}

pub async fn serve(options: ServeOptions) -> Result<(), GatewayError> {
    let app = build_app(options.clone())?;
    let listener = TcpListener::bind((options.host.as_str(), options.port)).await?;
    eprintln!("{}", serve_summary(&options));
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn serve_blocking(options: ServeOptions) -> Result<(), GatewayError> {
    tokio::runtime::Runtime::new()
        .map_err(GatewayError::from)?
        .block_on(serve(options))
}

impl AppState {
    fn load(options: ServeOptions, start_dir: &Path, clock: Clock) -> Result<Self, GatewayError> {
        let loaded = load_config(start_dir)?;
        let model_ids = model_ids(&loaded.routing, &loaded.gateway);
        let tier_ladder = loaded
            .routing
            .tiers
            .iter()
            .map(|tier| tier.model.as_str())
            .collect::<Vec<_>>();
        let model_costs = loaded
            .gateway
            .models
            .iter()
            .map(|(name, model)| (name.as_str(), model.cost_per_1k));
        let (costs, priced) = price_table(model_costs, tier_ladder);
        let feedback_path = start_dir.join(DEFAULT_LOG);
        let savings_path = std::env::var_os(SAVINGS_FILE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| start_dir.join(SAVINGS_FILE));
        let runtime = Arc::new(GatewayRuntime::new(
            &loaded.gateway,
            priced,
            savings_path.clone(),
            clock,
        ));
        Ok(Self {
            options,
            config_path: loaded.config_path,
            feedback_path,
            routing: loaded.routing,
            gateway: loaded.gateway,
            model_ids,
            price_table: costs,
            priced,
            runtime,
        })
    }
}

struct LoadedConfig {
    config_path: PathBuf,
    routing: RoutingConfig,
    gateway: GatewayConfig,
}

fn load_config(start_dir: &Path) -> Result<LoadedConfig, GatewayError> {
    let Some(path) = find_config(start_dir) else {
        return Ok(LoadedConfig {
            config_path: start_dir.join(CONFIG_FILE),
            routing: RoutingConfig::default(),
            gateway: GatewayConfig {
                models: BTreeMap::new(),
                offline: false,
                retries: DEFAULT_RETRIES,
                breaker_threshold: DEFAULT_BREAKER_THRESHOLD,
                breaker_cooldown: DEFAULT_BREAKER_COOLDOWN,
                failover: DEFAULT_FAILOVER.to_owned(),
                cache: None,
                rate_limit: None,
                keys: BTreeMap::new(),
            },
        });
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| GatewayError::new(format!("{}: {err}", path.display())))?;
    let where_ = path.to_string_lossy();
    let routing = routing_config_from_toml(&text, &where_)
        .map_err(|err| GatewayError::new(err.to_string()))?;
    let gateway = parse_gateway_config(&text, &where_)?;
    Ok(LoadedConfig {
        config_path: path,
        routing,
        gateway,
    })
}

/// Read `[gateway.models.<name>]` from the nearest `wayfinder-router.toml`.
///
/// Mirrors the Python `load_gateway_config(start_dir).models`: it walks up from
/// `start_dir` for the config file, parses the model map, and returns it for
/// out-of-crate consumers (the TUI, bootstrap). Returns an empty map when no
/// config file is found.
pub fn load_gateway_models(
    start_dir: &Path,
) -> Result<BTreeMap<String, GatewayModel>, GatewayError> {
    let Some(path) = find_config(start_dir) else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| GatewayError::new(format!("{}: {err}", path.display())))?;
    let where_ = path.to_string_lossy();
    Ok(parse_gateway_config(&text, &where_)?.models)
}

/// Parse `[gateway]` TOML from file text without touching the environment.
pub fn gateway_config_from_toml(text: &str, where_: &str) -> Result<GatewayConfig, GatewayError> {
    parse_gateway_config(text, where_)
}

/// Validate `[gateway]` TOML for UI callers that only need the error surface.
pub fn validate_gateway_toml(text: &str, where_: &str) -> Result<(), GatewayError> {
    gateway_config_from_toml(text, where_).map(|_| ())
}

/// Serialize parsed gateway config back to TOML without resolving secrets.
pub fn dump_gateway_toml(gateway: &GatewayConfig) -> String {
    let mut blocks = Vec::new();
    if gateway.retries != DEFAULT_RETRIES
        || gateway.offline
        || gateway.breaker_threshold != DEFAULT_BREAKER_THRESHOLD
        || gateway.breaker_cooldown != DEFAULT_BREAKER_COOLDOWN
        || gateway.failover != DEFAULT_FAILOVER
    {
        let mut lines = vec!["[gateway]".to_owned()];
        if gateway.offline {
            lines.push("offline = true".to_owned());
        }
        if gateway.retries != DEFAULT_RETRIES {
            lines.push(format!("retries = {}", gateway.retries));
        }
        if gateway.breaker_threshold != DEFAULT_BREAKER_THRESHOLD {
            lines.push(format!("breaker_threshold = {}", gateway.breaker_threshold));
        }
        if gateway.breaker_cooldown != DEFAULT_BREAKER_COOLDOWN {
            lines.push(format!(
                "breaker_cooldown = {}",
                python_float_repr(gateway.breaker_cooldown)
            ));
        }
        if gateway.failover != DEFAULT_FAILOVER {
            lines.push(format!("failover = \"{}\"", gateway.failover));
        }
        blocks.push(lines.join("\n"));
    }
    if let Some(cache) = &gateway.cache {
        let mut lines = vec![
            "[gateway.cache]".to_owned(),
            format!("enabled = {}", cache.enabled),
        ];
        if cache.ttl != DEFAULT_CACHE_TTL {
            lines.push(format!("ttl = {}", python_float_repr(cache.ttl)));
        }
        if cache.max_entries != DEFAULT_CACHE_MAX_ENTRIES {
            lines.push(format!("max_entries = {}", cache.max_entries));
        }
        if cache.max_bytes != DEFAULT_CACHE_MAX_BYTES {
            lines.push(format!("max_bytes = {}", cache.max_bytes));
        }
        blocks.push(lines.join("\n"));
    }
    if let Some(rate_limit) = &gateway.rate_limit {
        let mut lines = vec!["[gateway.rate_limit]".to_owned()];
        if let Some(rpm) = rate_limit.rpm {
            lines.push(format!("rpm = {rpm}"));
        }
        if let Some(tpm) = rate_limit.tpm {
            lines.push(format!("tpm = {tpm}"));
        }
        if rate_limit.window != DEFAULT_RATE_LIMIT_WINDOW {
            lines.push(format!("window = {}", python_float_repr(rate_limit.window)));
        }
        blocks.push(lines.join("\n"));
    }
    for (id, key) in &gateway.keys {
        blocks.push(format!("[gateway.keys.{id}]\nhash = \"{}\"", key.hash));
    }
    for (name, model) in &gateway.models {
        let mut lines = vec![
            format!("[gateway.models.{name}]"),
            format!("base_url = \"{}\"", model.base_url),
            format!("model = \"{}\"", model.model),
        ];
        if let Some(api_key_env) = &model.api_key_env {
            lines.push(format!("api_key_env = \"{api_key_env}\""));
        }
        if let Some(api_key_cmd) = &model.api_key_cmd {
            lines.push(format!("api_key_cmd = \"{api_key_cmd}\""));
        }
        if let Some(cost_per_1k) = model.cost_per_1k {
            lines.push(format!("cost_per_1k = {}", python_float_repr(cost_per_1k)));
        }
        if !model.fallbacks.is_empty() {
            let rendered = model
                .fallbacks
                .iter()
                .map(|fallback| format!("\"{fallback}\""))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("fallbacks = [{rendered}]"));
        }
        if let Some(context_window) = model.context_window {
            lines.push(format!("context_window = {context_window}"));
        }
        blocks.push(lines.join("\n"));
    }
    blocks.join("\n\n")
}

fn python_float_repr(value: f64) -> String {
    let rounded = (value * 1_000_000.0).round() / 1_000_000.0;
    let mut rendered = format!("{rounded:.6}");
    while rendered.contains('.') && rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.push('0');
    }
    rendered
}

fn find_config(start_dir: &Path) -> Option<PathBuf> {
    for dir in start_dir.ancestors() {
        let path = dir.join(CONFIG_FILE);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn parse_gateway_config(text: &str, where_: &str) -> Result<GatewayConfig, GatewayError> {
    let data: Value = text
        .parse()
        .map_err(|err| GatewayError::new(format!("{where_}: invalid TOML: {err}")))?;
    let Some(gateway_value) = data.get("gateway") else {
        return Ok(GatewayConfig {
            models: BTreeMap::new(),
            offline: false,
            retries: DEFAULT_RETRIES,
            breaker_threshold: DEFAULT_BREAKER_THRESHOLD,
            breaker_cooldown: DEFAULT_BREAKER_COOLDOWN,
            failover: DEFAULT_FAILOVER.to_owned(),
            cache: None,
            rate_limit: None,
            keys: BTreeMap::new(),
        });
    };
    let gateway_table = gateway_value
        .as_table()
        .ok_or_else(|| GatewayError::new(format!("{where_}: '[gateway]' must be a table")))?;
    let retries = match gateway_table.get("retries") {
        Some(value) => non_negative_usize(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.retries' must be a non-negative integer"
            ))
        })?,
        None => DEFAULT_RETRIES,
    };
    let offline = match gateway_table.get("offline") {
        Some(Value::Boolean(value)) => *value,
        Some(_) => {
            return Err(GatewayError::new(format!(
                "{where_}: 'gateway.offline' must be a boolean"
            )));
        }
        None => false,
    };
    let breaker_threshold = match gateway_table.get("breaker_threshold") {
        Some(value) => positive_usize_value(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.breaker_threshold' must be a positive integer"
            ))
        })?,
        None => DEFAULT_BREAKER_THRESHOLD,
    };
    let breaker_cooldown = match gateway_table.get("breaker_cooldown") {
        Some(value) => non_negative_number(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.breaker_cooldown' must be a non-negative number"
            ))
        })?,
        None => DEFAULT_BREAKER_COOLDOWN,
    };
    let failover = match gateway_table.get("failover") {
        Some(value) => string_field(Some(value)).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.failover' must be one of {}",
                reliability::FAILOVER_POLICIES.join(", ")
            ))
        })?,
        None => DEFAULT_FAILOVER.to_owned(),
    };
    if !reliability::FAILOVER_POLICIES.contains(&failover.as_str()) {
        return Err(GatewayError::new(format!(
            "{where_}: 'gateway.failover' must be one of {}",
            reliability::FAILOVER_POLICIES.join(", ")
        )));
    }
    let cache = parse_cache_config(gateway_table.get("cache"), where_)?;
    let rate_limit = parse_rate_limit_config(gateway_table.get("rate_limit"), where_)?;
    let keys = parse_keys_config(gateway_table.get("keys"), where_)?;
    let Some(models_value) = gateway_table.get("models") else {
        return Ok(GatewayConfig {
            models: BTreeMap::new(),
            offline,
            retries,
            breaker_threshold,
            breaker_cooldown,
            failover,
            cache,
            rate_limit,
            keys,
        });
    };
    let Some(models) = models_value.as_table() else {
        return Err(GatewayError::new(format!(
            "{where_}: '[gateway.models]' must be a table"
        )));
    };
    let mut parsed = BTreeMap::new();
    for (name, value) in models {
        let Some(table) = value.as_table() else {
            return Err(GatewayError::new(format!(
                "{where_}: '[gateway.models.{name}]' must be a table"
            )));
        };
        let base_url = string_field(table.get("base_url")).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.models.{name}.base_url' must be a string"
            ))
        })?;
        let model = string_field(table.get("model")).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.models.{name}.model' must be a string"
            ))
        })?;
        let api_key_env = match table.get("api_key_env") {
            Some(value) => Some(string_field(Some(value)).ok_or_else(|| {
                GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.api_key_env' must be a non-empty string"
                ))
            })?),
            None => None,
        };
        let api_key_cmd = match table.get("api_key_cmd") {
            Some(value) => Some(string_field(Some(value)).ok_or_else(|| {
                GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.api_key_cmd' must be a non-empty string"
                ))
            })?),
            None => None,
        };
        if api_key_cmd.is_some() && api_key_env.is_none() {
            // The command fills a named variable; without one there is nowhere to put the key.
            return Err(GatewayError::new(format!(
                "{where_}: 'gateway.models.{name}.api_key_cmd' needs 'api_key_env' to name \
                 the variable it fills"
            )));
        }
        let cost_per_1k = match table.get("cost_per_1k") {
            Some(value) => Some(non_negative_number(value).ok_or_else(|| {
                GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.cost_per_1k' must be a non-negative number"
                ))
            })?),
            None => None,
        };
        let fallbacks = match table.get("fallbacks") {
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    string_field(Some(value)).ok_or_else(|| {
                        GatewayError::new(format!(
                            "{where_}: 'gateway.models.{name}.fallbacks' must be a list of model names"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.fallbacks' must be a list of model names"
                )));
            }
            None => Vec::new(),
        };
        let context_window = match table.get("context_window") {
            Some(value) => Some(positive_usize_value(value).ok_or_else(|| {
                GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.context_window' must be a positive integer"
                ))
            })?),
            None => None,
        };
        parsed.insert(
            name.clone(),
            GatewayModel {
                base_url,
                model,
                api_key_env,
                api_key_cmd,
                cost_per_1k,
                fallbacks,
                context_window,
            },
        );
    }
    for (name, model) in &parsed {
        for fallback in &model.fallbacks {
            if fallback == name {
                return Err(GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.fallbacks' cannot include itself"
                )));
            }
            if !parsed.contains_key(fallback) {
                return Err(GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.fallbacks' names unknown model '{fallback}'"
                )));
            }
        }
    }
    Ok(GatewayConfig {
        models: parsed,
        offline,
        retries,
        breaker_threshold,
        breaker_cooldown,
        failover,
        cache,
        rate_limit,
        keys,
    })
}

fn string_field(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn parse_cache_config(
    value: Option<&Value>,
    where_: &str,
) -> Result<Option<CacheConfig>, GatewayError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(GatewayError::new(format!(
            "{where_}: '[gateway.cache]' must be a table"
        )));
    };
    let enabled = match table.get("enabled") {
        Some(Value::Boolean(value)) => *value,
        Some(_) => {
            return Err(GatewayError::new(format!(
                "{where_}: 'gateway.cache.enabled' must be a boolean"
            )));
        }
        None => false,
    };
    let ttl = match table.get("ttl") {
        Some(value) => non_negative_number(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.cache.ttl' must be a non-negative number"
            ))
        })?,
        None => DEFAULT_CACHE_TTL,
    };
    let max_entries = match table.get("max_entries") {
        Some(value) => positive_usize(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.cache.max_entries' must be a positive integer"
            ))
        })?,
        None => DEFAULT_CACHE_MAX_ENTRIES,
    };
    let max_bytes = match table.get("max_bytes") {
        Some(value) => positive_usize(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.cache.max_bytes' must be a positive integer"
            ))
        })?,
        None => DEFAULT_CACHE_MAX_BYTES,
    };
    Ok(Some(CacheConfig {
        enabled,
        ttl,
        max_entries,
        max_bytes,
    }))
}

fn parse_rate_limit_config(
    value: Option<&Value>,
    where_: &str,
) -> Result<Option<RateLimitConfig>, GatewayError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(GatewayError::new(format!(
            "{where_}: '[gateway.rate_limit]' must be a table"
        )));
    };
    let rpm = match table.get("rpm") {
        Some(value) => Some(positive_u64(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.rate_limit.rpm' must be a positive integer"
            ))
        })?),
        None => None,
    };
    let tpm = match table.get("tpm") {
        Some(value) => Some(positive_u64(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.rate_limit.tpm' must be a positive integer"
            ))
        })?),
        None => None,
    };
    if rpm.is_none() && tpm.is_none() {
        return Err(GatewayError::new(format!(
            "{where_}: '[gateway.rate_limit]' must set 'rpm' and/or 'tpm'"
        )));
    }
    let window = match table.get("window") {
        Some(value) => positive_number(value).ok_or_else(|| {
            GatewayError::new(format!(
                "{where_}: 'gateway.rate_limit.window' must be a positive number"
            ))
        })?,
        None => DEFAULT_RATE_LIMIT_WINDOW,
    };
    Ok(Some(RateLimitConfig { rpm, tpm, window }))
}

fn parse_keys_config(
    value: Option<&Value>,
    where_: &str,
) -> Result<BTreeMap<String, VirtualKeyConfig>, GatewayError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Value::Table(table) = value else {
        return Err(GatewayError::new(format!(
            "{where_}: '[gateway.keys]' must be a table"
        )));
    };
    let mut keys = BTreeMap::new();
    for (id, value) in table {
        let Some(entry) = value.as_table() else {
            return Err(GatewayError::new(format!(
                "{where_}: '[gateway.keys.{id}]' must be a table"
            )));
        };
        let Some(hash) = string_field(entry.get("hash")) else {
            return Err(GatewayError::new(format!(
                "{where_}: 'gateway.keys.{id}.hash' must be a SHA-256 hex digest"
            )));
        };
        if !is_sha256_hex(&hash) {
            return Err(GatewayError::new(format!(
                "{where_}: 'gateway.keys.{id}.hash' must be a SHA-256 hex digest"
            )));
        }
        keys.insert(
            id.clone(),
            VirtualKeyConfig {
                hash: hash.to_ascii_lowercase(),
            },
        );
    }
    Ok(keys)
}

fn non_negative_number(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(value) if *value >= 0 => Some(*value as f64),
        Value::Float(value) if *value >= 0.0 => Some(*value),
        _ => None,
    }
}

fn positive_number(value: &Value) -> Option<f64> {
    non_negative_number(value).filter(|value| *value > 0.0)
}

fn positive_usize(value: &Value) -> Option<usize> {
    positive_usize_value(value)
}

fn positive_usize_value(value: &Value) -> Option<usize> {
    let Value::Integer(value) = value else {
        return None;
    };
    usize::try_from(*value).ok().filter(|value| *value > 0)
}

fn non_negative_usize(value: &Value) -> Option<usize> {
    let Value::Integer(value) = value else {
        return None;
    };
    usize::try_from(*value).ok()
}

fn positive_u64(value: &Value) -> Option<u64> {
    let Value::Integer(value) = value else {
        return None;
    };
    u64::try_from(*value).ok().filter(|value| *value > 0)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn model_ids(routing: &RoutingConfig, gateway: &GatewayConfig) -> Vec<String> {
    if !gateway.models.is_empty() {
        return gateway.models.keys().cloned().collect();
    }
    let mut ids = if let Some(classifier) = &routing.classifier {
        classifier.models.clone()
    } else {
        routing
            .tiers
            .iter()
            .map(|tier| tier.model.clone())
            .collect::<Vec<_>>()
    };
    ids.dedup();
    ids
}

impl GatewayRuntime {
    fn new(gateway: &GatewayConfig, priced: bool, savings_path: PathBuf, clock: Clock) -> Self {
        let ledger = SavingsLedger::load(&savings_path)
            .map(|mut ledger| {
                ledger.priced = priced;
                ledger
            })
            .unwrap_or_else(|_| SavingsLedger::new(priced));
        Self {
            cache: Mutex::new(ResponseCache::new(gateway.cache.clone())),
            rate_limiter: Mutex::new(RateLimiter::new(gateway.rate_limit.clone(), clock)),
            breaker: Mutex::new(reliability::CircuitBreaker::new(
                gateway.breaker_threshold,
                Duration::from_secs_f64(gateway.breaker_cooldown),
            )),
            metrics: Mutex::new(Metrics::default()),
            recent: Mutex::new(VecDeque::new()),
            ledger: Mutex::new(ledger),
            savings_path,
        }
    }
}

impl ResponseCache {
    fn new(config: Option<CacheConfig>) -> Self {
        Self {
            config: config.filter(|config| config.enabled),
            entries: BTreeMap::new(),
            lru: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &str) -> Option<CacheEntry> {
        let config = self.config.as_ref()?;
        let entry = self.entries.get(key)?;
        if config.ttl > 0.0 && entry.stored_at.elapsed() >= Duration::from_secs_f64(config.ttl) {
            self.drop_key(key);
            return None;
        }
        let entry = self.entries.get(key)?.clone();
        self.touch(key);
        Some(entry)
    }

    fn put(&mut self, key: String, entry: CacheEntry) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let size = entry.body.len();
        if size > config.max_bytes {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.body.len());
            self.lru.retain(|item| item != &key);
        }
        self.bytes += size;
        self.entries.insert(key.clone(), entry);
        self.lru.push_back(key);
        self.evict(&config);
    }

    fn touch(&mut self, key: &str) {
        self.lru.retain(|item| item != key);
        self.lru.push_back(key.to_owned());
    }

    fn drop_key(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.body.len());
        }
        self.lru.retain(|item| item != key);
    }

    fn evict(&mut self, config: &CacheConfig) {
        while self.entries.len() > config.max_entries || self.bytes > config.max_bytes {
            let Some(key) = self.lru.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry.body.len());
            }
        }
    }
}

impl RateLimiter {
    fn new(config: Option<RateLimitConfig>, clock: Clock) -> Self {
        Self {
            config,
            clock,
            window_started: None,
            requests: 0,
            tokens: 0,
        }
    }

    fn admit(&mut self) -> RateAdmission {
        let Some(config) = self.config.clone() else {
            return RateAdmission {
                allowed: true,
                limit: "",
                retry_after: 0,
            };
        };
        self.roll_window(&config);
        let retry_after = self.retry_after(&config);
        if config.rpm.is_some_and(|rpm| self.requests >= rpm) {
            return RateAdmission {
                allowed: false,
                limit: "rpm",
                retry_after,
            };
        }
        if config.tpm.is_some_and(|tpm| self.tokens >= tpm) {
            return RateAdmission {
                allowed: false,
                limit: "tpm",
                retry_after,
            };
        }
        self.requests += 1;
        RateAdmission {
            allowed: true,
            limit: "",
            retry_after: 0,
        }
    }

    fn add_tokens(&mut self, tokens: usize) {
        let Some(config) = self.config.clone() else {
            return;
        };
        if config.tpm.is_none() {
            return;
        }
        self.roll_window(&config);
        self.tokens = self.tokens.saturating_add(tokens as u64);
    }

    fn snapshot(&mut self) -> Option<RateSnapshot> {
        let config = self.config.clone()?;
        let rpm = config.rpm?;
        self.roll_window(&config);
        Some(RateSnapshot {
            limit: rpm,
            remaining: rpm.saturating_sub(self.requests),
            reset: self.retry_after(&config),
        })
    }

    fn roll_window(&mut self, config: &RateLimitConfig) {
        let now = (self.clock)();
        let window = Duration::from_secs_f64(config.window);
        match self.window_started {
            Some(started) if now.duration_since(started) < window => {}
            _ => {
                self.window_started = Some(now);
                self.requests = 0;
                self.tokens = 0;
            }
        }
    }

    fn retry_after(&self, config: &RateLimitConfig) -> u64 {
        let window = Duration::from_secs_f64(config.window);
        let Some(started) = self.window_started else {
            return config.window.ceil().max(1.0) as u64;
        };
        let elapsed = (self.clock)().duration_since(started);
        if elapsed >= window {
            1
        } else {
            (window - elapsed).as_secs_f64().ceil().max(1.0) as u64
        }
    }
}

impl Metrics {
    fn observe_decision(&mut self, model: &str, mode: &str, latency: Duration) {
        *self
            .requests
            .entry((model.to_owned(), mode.to_owned()))
            .or_default() += 1;
        self.decision_latency_count += 1;
        self.decision_latency_sum += latency.as_secs_f64();
    }

    fn observe_upstream(&mut self, model: &str, latency: Duration) {
        let totals = self.upstream_latency.entry(model.to_owned()).or_default();
        totals.count += 1;
        totals.sum += latency.as_secs_f64();
    }

    fn observe_upstream_error(&mut self, model: &str) {
        *self.upstream_errors.entry(model.to_owned()).or_default() += 1;
    }

    fn observe_failover(&mut self, chosen: &str, served_by: &str) {
        *self
            .failovers
            .entry((chosen.to_owned(), served_by.to_owned()))
            .or_default() += 1;
    }

    fn observe_cost(&mut self, realized: f64, baseline: f64) {
        self.realized_cost = round_cost(self.realized_cost + realized);
        self.baseline_cost = round_cost(self.baseline_cost + baseline);
    }

    fn observe_cache_hit(&mut self, avoided_cost: f64) {
        self.cache_hits += 1;
        self.cache_avoided_cost = round_cost(self.cache_avoided_cost + avoided_cost.max(0.0));
    }

    fn observe_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

    fn observe_rate_limited(&mut self, limit: &str) {
        *self.rate_limited.entry(limit.to_owned()).or_default() += 1;
    }

    fn observe_key_request(&mut self, key_id: &str) {
        *self.key_requests.entry(key_id.to_owned()).or_default() += 1;
    }

    fn render(&self, version: &str, dry_run: bool, recent_total: usize) -> String {
        let mut lines = Vec::new();
        lines.push("# HELP wayfinder_router_build_info Build and runtime metadata.".to_owned());
        lines.push("# TYPE wayfinder_router_build_info gauge".to_owned());
        lines.push(format!(
            "wayfinder_router_build_info{{version=\"{}\",dry_run=\"{}\"}} 1",
            label_escape(version),
            dry_run
        ));
        lines.push(
            "# HELP wayfinder_router_recent_decisions_total Number of routing decisions retained in memory."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_recent_decisions_total gauge".to_owned());
        lines.push(format!(
            "wayfinder_router_recent_decisions_total {recent_total}"
        ));
        lines.push(
            "# HELP wayfinder_router_requests_total Routed requests by model and mode.".to_owned(),
        );
        lines.push("# TYPE wayfinder_router_requests_total counter".to_owned());
        for ((model, mode), count) in &self.requests {
            lines.push(format!(
                "wayfinder_router_requests_total{{model=\"{}\",mode=\"{}\"}} {count}",
                label_escape(model),
                label_escape(mode)
            ));
        }
        lines.push(
            "# HELP wayfinder_router_upstream_errors_total Upstream failures by model.".to_owned(),
        );
        lines.push("# TYPE wayfinder_router_upstream_errors_total counter".to_owned());
        for (model, count) in &self.upstream_errors {
            lines.push(format!(
                "wayfinder_router_upstream_errors_total{{model=\"{}\"}} {count}",
                label_escape(model)
            ));
        }
        if !self.failovers.is_empty() {
            lines.push(
                "# HELP wayfinder_router_failovers_total Requests served by a fallback target."
                    .to_owned(),
            );
            lines.push("# TYPE wayfinder_router_failovers_total counter".to_owned());
            for ((chosen, served_by), count) in &self.failovers {
                lines.push(format!(
                    "wayfinder_router_failovers_total{{model=\"{}\",served_by=\"{}\"}} {count}",
                    label_escape(chosen),
                    label_escape(served_by)
                ));
            }
        }
        lines.push(
            "# HELP wayfinder_router_cache_hits_total Exact-match response cache hits.".to_owned(),
        );
        lines.push("# TYPE wayfinder_router_cache_hits_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_cache_hits_total {}",
            self.cache_hits
        ));
        lines.push(
            "# HELP wayfinder_router_cache_misses_total Cacheable response cache misses."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_cache_misses_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_cache_misses_total {}",
            self.cache_misses
        ));
        lines.push(
            "# HELP wayfinder_router_cache_avoided_cost_total Upstream cost avoided by cache hits."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_cache_avoided_cost_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_cache_avoided_cost_total {}",
            self.cache_avoided_cost
        ));
        lines.push(
            "# HELP wayfinder_router_rate_limited_total Requests rejected with 429 by limit."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_rate_limited_total counter".to_owned());
        for (limit, count) in &self.rate_limited {
            lines.push(format!(
                "wayfinder_router_rate_limited_total{{limit=\"{}\"}} {count}",
                label_escape(limit)
            ));
        }
        if !self.key_requests.is_empty() {
            lines.push(
                "# HELP wayfinder_router_key_requests_total Requests by virtual-key id.".to_owned(),
            );
            lines.push("# TYPE wayfinder_router_key_requests_total counter".to_owned());
            for (key, count) in &self.key_requests {
                lines.push(format!(
                    "wayfinder_router_key_requests_total{{key=\"{}\"}} {count}",
                    label_escape(key)
                ));
            }
        }
        lines.push("# HELP wayfinder_router_realized_cost_total Cumulative realized spend on the chosen tier.".to_owned());
        lines.push("# TYPE wayfinder_router_realized_cost_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_realized_cost_total {}",
            self.realized_cost
        ));
        lines.push(
            "# HELP wayfinder_router_baseline_cost_total Cumulative always-frontier baseline cost."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_baseline_cost_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_baseline_cost_total {}",
            self.baseline_cost
        ));
        lines.push(
            "# HELP wayfinder_router_savings_cost_total Cumulative savings versus baseline."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_savings_cost_total counter".to_owned());
        lines.push(format!(
            "wayfinder_router_savings_cost_total {}",
            round_cost(self.baseline_cost - self.realized_cost)
        ));
        lines.push(
            "# HELP wayfinder_router_decision_latency_seconds Time spent scoring and routing."
                .to_owned(),
        );
        lines.push("# TYPE wayfinder_router_decision_latency_seconds summary".to_owned());
        lines.push(format!(
            "wayfinder_router_decision_latency_seconds_sum {}",
            self.decision_latency_sum
        ));
        lines.push(format!(
            "wayfinder_router_decision_latency_seconds_count {}",
            self.decision_latency_count
        ));
        lines.push("# HELP wayfinder_router_upstream_latency_seconds Upstream model round-trip time by model.".to_owned());
        lines.push("# TYPE wayfinder_router_upstream_latency_seconds summary".to_owned());
        for (model, totals) in &self.upstream_latency {
            lines.push(format!(
                "wayfinder_router_upstream_latency_seconds_sum{{model=\"{}\"}} {}",
                label_escape(model),
                totals.sum
            ));
            lines.push(format!(
                "wayfinder_router_upstream_latency_seconds_count{{model=\"{}\"}} {}",
                label_escape(model),
                totals.count
            ));
        }
        lines.join("\n") + "\n"
    }
}

async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    let mut models = state.model_ids.clone();
    models.sort();
    Json(HealthResponse {
        status: "ok",
        models,
        offline: state.gateway.offline,
    })
}

async fn list_models(State(state): State<AppState>) -> Json<ModelsResponse> {
    let mut ids = vec!["auto".to_owned()];
    if state.routing.classifier.is_none() && !state.routing.tiers.is_empty() {
        ids.push("prefer-local".to_owned());
        ids.push("prefer-hosted".to_owned());
    }
    ids.extend(state.model_ids.iter().cloned());
    Json(ModelsResponse {
        object: "list",
        data: ids
            .into_iter()
            .map(|id| ModelEntry {
                id,
                object: "model",
                created: 0,
                owned_by: "wayfinder",
            })
            .collect(),
    })
}

async fn record_feedback(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<FeedbackRequest>, JsonRejection>,
) -> Response<Body> {
    let expected_token = std::env::var(FEEDBACK_TOKEN_ENV).ok();
    if let Some(expected_token) = expected_token {
        let authorized = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == format!("Bearer {expected_token}"))
            .unwrap_or(false);
        if !authorized {
            return json_response(
                StatusCode::UNAUTHORIZED,
                HeaderMap::new(),
                json!({"error": "unauthorized"}),
            );
        }
    }
    let Ok(Json(payload)) = payload else {
        return json_response(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"error": "invalid JSON body"}),
        );
    };
    let Some(text) = payload.text.filter(|text| !text.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"error": "missing 'text'"}),
        );
    };
    let Some(label) = payload.label.filter(|label| !label.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"error": "missing 'label'"}),
        );
    };
    if let Err(err) = record_label(&state.feedback_path, &text, &label) {
        let status = if err.kind() == std::io::ErrorKind::InvalidInput {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        return json_response(status, HeaderMap::new(), json!({"error": err.to_string()}));
    }
    let count = read_labels(&state.feedback_path)
        .map(|labels| labels.len())
        .unwrap_or(0);
    json_response(
        StatusCode::OK,
        HeaderMap::new(),
        json!({"ok": true, "count": count}),
    )
}

async fn savings(
    State(state): State<AppState>,
    Query(query): Query<SavingsQuery>,
) -> Json<JsonValue> {
    let period = query.period.as_deref().unwrap_or("all");
    let days = match period {
        "today" => Some(1),
        "7d" => Some(7),
        "30d" => Some(30),
        _ => None,
    };
    let report = state.runtime.ledger.lock().unwrap().period(days, None);
    Json(json!({
        "period_days": report.period_days,
        "priced": report.priced,
        "unit": if report.priced { "usd" } else { "relative" },
        "requests": report.requests,
        "estimated_requests": report.estimated_requests,
        "tokens": report.tokens,
        "realized": report.realized,
        "baseline": report.baseline,
        "saved": report.saved,
        "saved_pct": report.saved_pct,
        "price_table_version": table_version(
            state.price_table.iter().map(|(model, cost)| (model.as_str(), *cost))
        )
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JsonValue>,
) -> Response<Body> {
    chat_completions_response(state, headers, body).await
}

async fn chat_completions_response(
    state: AppState,
    headers: HeaderMap,
    body: JsonValue,
) -> Response<Body> {
    let request_id = next_request_id();
    let key_id = match authorize_key(&state, &headers, &request_id) {
        Ok(key_id) => key_id,
        Err(response) => return response,
    };
    let rate_admission = {
        let mut limiter = state.runtime.rate_limiter.lock().unwrap();
        limiter.admit()
    };
    if !rate_admission.allowed {
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_rate_limited(rate_admission.limit);
        let mut headers = request_id_header(&request_id);
        headers.insert(
            "x-wayfinder-router-rate-limit",
            HeaderValue::from_static(rate_admission.limit),
        );
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&rate_admission.retry_after.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("1")),
        );
        add_rate_limit_headers(&state, &mut headers);
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            headers,
            json!({
                "error": {
                    "message": "wayfinder gateway rate limit exceeded",
                    "type": "wayfinder_router_rate_limited"
                }
            }),
        );
    }

    let decision_started = Instant::now();
    let prompt = extract_scoped_prompt(body.get("messages"), &headers);
    let decision = score_complexity(&prompt, &state.routing);
    let decision_latency = decision_started.elapsed();
    let mut route =
        match route_decision(&state, &headers, body.get("model"), &decision, &request_id) {
            Ok(route) => route,
            Err(response) => return response,
        };
    apply_sticky_route(&state, &headers, body.get("messages"), &mut route);
    let client_body = body.clone();
    let mut response_headers =
        decision_headers(&route.chosen, decision.score, &route.mode, &request_id);
    add_rate_limit_headers(&state, &mut response_headers);
    let offline = offline_enabled(&headers, &state.gateway);
    if offline {
        response_headers.insert(
            "x-wayfinder-router-offline",
            HeaderValue::from_static("true"),
        );
    }
    if let Some(key_id) = &key_id {
        response_headers.insert(
            "x-wayfinder-router-key",
            HeaderValue::from_str(key_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
        );
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_key_request(key_id);
    }
    if state.options.dry_run {
        let cost = zero_recent_cost(state.priced);
        observe_decision(
            &state,
            &route,
            &route.chosen,
            &request_id,
            decision.score,
            decision_latency,
            None,
            cost,
            key_id.clone(),
            false,
        );
        return json_response(
            StatusCode::OK,
            response_headers,
            dry_run_debug_body(&state, &decision, &route, &request_id, offline),
        );
    }

    let deliver_from = if offline {
        tier_ladder(&state.routing)
            .into_iter()
            .find(|model| state.gateway.models.contains_key(model))
            .unwrap_or_else(|| route.chosen.clone())
    } else {
        route.chosen.clone()
    };
    let Some(target) = state.gateway.models.get(&deliver_from).cloned() else {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            response_headers,
            json!({
                "error": {
                    "message": format!("no gateway endpoint configured for model '{}'", deliver_from),
                    "type": "wayfinder_router_misconfigured"
                }
            }),
        );
    };

    let debug = debug_enabled(&headers);
    let cache_key = if cache_enabled(&state) && is_cacheable_request(&client_body) && !debug {
        Some(cache_key(&target.model, &client_body))
    } else {
        None
    };
    if let Some(cache_key) = &cache_key {
        if let Some(entry) = state.runtime.cache.lock().unwrap().get(cache_key) {
            response_headers.insert("x-wayfinder-router-cache", HeaderValue::from_static("hit"));
            let cost = recent_cost_from_tokens(
                &state,
                &deliver_from,
                entry.prompt_tokens,
                entry.completion_tokens,
                entry.estimated,
            );
            {
                let mut metrics = state.runtime.metrics.lock().unwrap();
                metrics.observe_cache_hit(entry.avoided_cost);
                metrics.observe_decision(&route.chosen, &route.mode, decision_latency);
            }
            push_recent(
                &state,
                RecentDecision {
                    request_id: request_id.clone(),
                    model: route.chosen.clone(),
                    served_by: deliver_from.clone(),
                    score: round_score(decision.score),
                    mode: route.mode.clone(),
                    ts: unix_ts(),
                    cost,
                    key_id,
                },
            );
            return bytes_response(
                entry.status,
                with_content_type(
                    served_headers(response_headers, &route.chosen, &deliver_from, offline),
                    &entry.content_type,
                ),
                entry.body,
            );
        }
        response_headers.insert("x-wayfinder-router-cache", HeaderValue::from_static("miss"));
        state.runtime.metrics.lock().unwrap().observe_cache_miss();
    }
    let policy = failover_policy(&headers, &state.gateway.failover);
    let plan = delivery_plan_for(&state, &deliver_from, &prompt, policy, offline);
    if plan.is_empty() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            response_headers,
            json!({
                "error": {
                    "message": format!(
                        "no available upstream for '{}' (cooling down or context too small)",
                        deliver_from
                    ),
                    "type": "wayfinder_router_circuit_open"
                }
            }),
        );
    }
    if body.get("stream").and_then(JsonValue::as_bool) == Some(true) {
        return stream_upstream(
            state,
            plan,
            body,
            response_headers,
            decision,
            route,
            request_id,
            prompt,
            key_id,
            decision_latency,
            offline,
        )
        .await;
    }
    forward_upstream(
        state,
        plan,
        body,
        response_headers,
        debug,
        decision,
        route,
        request_id,
        prompt,
        cache_key,
        key_id,
        decision_latency,
        offline,
    )
    .await
}

async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JsonValue>,
) -> Response<Body> {
    let model = body
        .get("model")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(AUTO_DIRECTIVE)
        .to_owned();
    let openai_body = anthropic_to_openai_request(&body);
    let prompt = extract_prompt(openai_body.get("messages"));
    let response = chat_completions_response(state, headers, openai_body).await;
    anthropic_from_chat_response(response, &model, &prompt).await
}

async fn anthropic_from_chat_response(
    response: Response<Body>,
    model: &str,
    prompt: &str,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let headers = parts.headers;
    if status.is_success() && content_type.starts_with("text/event-stream") {
        return anthropic_stream_response(body, headers, model);
    }
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                headers,
                anthropic_error(StatusCode::BAD_GATEWAY, "upstream response failed"),
            );
        }
    };
    let body = bytes.to_vec();
    if !status.is_success() {
        let message = upstream_error_message(&body).unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("upstream request failed")
                .to_owned()
        });
        return json_response(status, headers, anthropic_error(status, &message));
    }
    let Ok(parsed) = serde_json::from_slice::<JsonValue>(&body) else {
        return json_response(
            StatusCode::BAD_GATEWAY,
            headers,
            anthropic_error(StatusCode::BAD_GATEWAY, "upstream response was not JSON"),
        );
    };
    if parsed.get("choices").is_none() {
        return bytes_response(status, headers, body);
    }
    json_response(
        StatusCode::OK,
        headers,
        openai_to_anthropic_response(&parsed, model, prompt),
    )
}

#[derive(Clone)]
struct RouteDecision {
    chosen: String,
    mode: String,
}

fn route_decision(
    state: &AppState,
    headers: &HeaderMap,
    model_field: Option<&JsonValue>,
    decision: &ComplexityScore,
    request_id: &str,
) -> Result<RouteDecision, Response<Body>> {
    if let Some(pinned) = resolve_pin(model_field, &state.routing, &state.gateway) {
        return Ok(RouteDecision {
            chosen: pinned,
            mode: "pinned".to_owned(),
        });
    }
    let threshold = match parse_threshold_header(headers) {
        Ok(value) => value,
        Err(message) => {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                request_id_header(request_id),
                json!({
                    "error": {
                        "message": message,
                        "type": "wayfinder_router_bad_override"
                    }
                }),
            ));
        }
    };
    if let Some(threshold) = threshold {
        let Some(tiers) = threshold_tiers(&state.routing, threshold) else {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                request_id_header(request_id),
                json!({
                    "error": {
                        "message": "x-wayfinder-threshold applies only to a binary tiered router",
                        "type": "wayfinder_router_bad_override"
                    }
                }),
            ));
        };
        return Ok(RouteDecision {
            chosen: recommend_tier(decision.score, &tiers),
            mode: "threshold-override".to_owned(),
        });
    }
    Ok(RouteDecision {
        chosen: decision.recommendation.clone(),
        mode: "scored".to_owned(),
    })
}

fn resolve_pin(
    model_field: Option<&JsonValue>,
    routing: &RoutingConfig,
    gateway: &GatewayConfig,
) -> Option<String> {
    let name = model_field?.as_str()?.trim();
    if name.is_empty() || name == AUTO_DIRECTIVE {
        return None;
    }
    if routing.classifier.is_none() && !routing.tiers.is_empty() {
        if name == PREFER_LOCAL_DIRECTIVE {
            return routing.tiers.first().map(|tier| tier.model.clone());
        }
        if name == PREFER_HOSTED_DIRECTIVE || name == PREFER_CLOUD_DIRECTIVE {
            return routing.tiers.last().map(|tier| tier.model.clone());
        }
    }
    if gateway.models.contains_key(name) {
        Some(name.to_owned())
    } else {
        None
    }
}

fn parse_threshold_header(headers: &HeaderMap) -> Result<Option<f64>, String> {
    let Some(value) = headers.get("x-wayfinder-threshold") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| "x-wayfinder-threshold must be valid UTF-8".to_owned())?;
    let threshold = value
        .parse::<f64>()
        .map_err(|_| format!("x-wayfinder-threshold must be a number in 0.0-1.0, got {value:?}"))?;
    if !(0.0..=1.0).contains(&threshold) {
        return Err(format!(
            "x-wayfinder-threshold must be in 0.0-1.0, got {threshold}"
        ));
    }
    Ok(Some(threshold))
}

fn threshold_tiers(routing: &RoutingConfig, threshold: f64) -> Option<Vec<Tier>> {
    if routing.classifier.is_some() || routing.tiers.len() != 2 {
        return None;
    }
    Some(vec![
        Tier {
            min_score: 0.0,
            model: routing.tiers[0].model.clone(),
            cost: routing.tiers[0].cost,
        },
        Tier {
            min_score: threshold,
            model: routing.tiers[1].model.clone(),
            cost: routing.tiers[1].cost,
        },
    ])
}

fn failover_policy<'a>(headers: &'a HeaderMap, configured: &'a str) -> &'a str {
    headers
        .get("x-wayfinder-failover")
        .and_then(|value| value.to_str().ok())
        .filter(|value| reliability::FAILOVER_POLICIES.contains(value))
        .unwrap_or(configured)
}

fn offline_enabled(headers: &HeaderMap, gateway: &GatewayConfig) -> bool {
    gateway.offline
        || headers
            .get("x-wayfinder-offline")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                )
            })
            .unwrap_or(false)
}

fn tier_ladder(routing: &RoutingConfig) -> Vec<String> {
    let mut tiers = routing.tiers.clone();
    tiers.sort_by(|a, b| {
        a.min_score
            .partial_cmp(&b.min_score)
            .unwrap_or(CmpOrdering::Equal)
    });
    tiers.into_iter().map(|tier| tier.model).collect()
}

fn delivery_plan_for(
    state: &AppState,
    chosen: &str,
    prompt: &str,
    policy: &str,
    offline: bool,
) -> Vec<String> {
    let Some(target) = state.gateway.models.get(chosen) else {
        return Vec::new();
    };
    let ladder = tier_ladder(&state.routing);
    let mut candidates = target.fallbacks.clone();
    if !offline {
        candidates.extend(reliability::failover_candidates(chosen, &ladder, policy));
    }
    let estimated_tokens = estimate_tokens(prompt);
    let now = reliability_now();
    let breaker = state.runtime.breaker.lock().unwrap();
    reliability::delivery_plan_at(chosen, candidates, Some(&breaker), now, |name| {
        state
            .gateway
            .models
            .get(name)
            .map(|model| reliability::precheck_ok(estimated_tokens, model.context_window))
            .unwrap_or(true)
    })
}

fn reliability_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn retry_jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

fn retry_delays_for(retries: usize) -> Vec<Duration> {
    reliability::retry_delays(
        retries,
        Duration::from_millis(200),
        Duration::from_secs(5),
        retry_jitter,
    )
}

fn retry_delay(delays: &[Duration], attempt: usize) -> Duration {
    delays.get(attempt).copied().unwrap_or_default()
}

fn served_headers(
    mut headers: HeaderMap,
    chosen: &str,
    served_by: &str,
    offline: bool,
) -> HeaderMap {
    headers.insert(
        "x-wayfinder-router-served-by",
        HeaderValue::from_str(served_by).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    if served_by != chosen && !offline {
        headers.insert(
            "x-wayfinder-router-failover",
            HeaderValue::from_static("true"),
        );
    }
    headers
}

async fn forward_upstream(
    state: AppState,
    plan: Vec<String>,
    body: JsonValue,
    headers: HeaderMap,
    debug: bool,
    decision: ComplexityScore,
    route: RouteDecision,
    request_id: String,
    prompt: String,
    cache_key_hint: Option<String>,
    key_id: Option<String>,
    decision_latency: Duration,
    offline: bool,
) -> Response<Body> {
    let client = upstream_client(&state.options);
    let mut last_error = "no upstream available".to_owned();
    for served_by in plan {
        let Some(target) = state.gateway.models.get(&served_by).cloned() else {
            continue;
        };
        let delays = retry_delays_for(state.gateway.retries);
        for attempt in 0..=state.gateway.retries {
            let mut upstream_body = body.clone();
            upstream_body["model"] = JsonValue::String(target.model.clone());
            let upstream_started = Instant::now();
            let response = client
                .post(chat_url(&target.base_url))
                .headers(upstream_headers(&target))
                .json(&upstream_body)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_error = err.to_string();
                    state
                        .runtime
                        .metrics
                        .lock()
                        .unwrap()
                        .observe_upstream_error(&served_by);
                    if attempt < state.gateway.retries {
                        tokio::time::sleep(retry_delay(&delays, attempt)).await;
                    }
                    continue;
                }
            };
            let upstream_latency = upstream_started.elapsed();
            let status = response.status();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/json")
                .to_owned();
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    last_error = err.to_string();
                    state
                        .runtime
                        .metrics
                        .lock()
                        .unwrap()
                        .observe_upstream_error(&served_by);
                    if attempt < state.gateway.retries {
                        tokio::time::sleep(retry_delay(&delays, attempt)).await;
                    }
                    continue;
                }
            };
            if !reliability::is_retryable(Some(status.as_u16())) {
                if status.is_success() {
                    state.runtime.breaker.lock().unwrap().record(
                        &served_by,
                        true,
                        reliability_now(),
                    );
                    let served_headers =
                        served_headers(headers, &route.chosen, &served_by, offline);
                    let parsed = serde_json::from_slice::<JsonValue>(&bytes).ok();
                    let completion = parsed
                        .as_ref()
                        .map(extract_completion_text)
                        .unwrap_or_default();
                    let usage = parsed
                        .as_ref()
                        .map(|parsed| usage_tokens(parsed, &prompt, &completion))
                        .unwrap_or_else(|| usage_tokens(&JsonValue::Null, &prompt, &completion));
                    let turn_cost = turn_cost_from_tokens(
                        &state,
                        &served_by,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.estimated,
                    );
                    let cost = recent_cost_from_turn(&state, &turn_cost);
                    state
                        .runtime
                        .rate_limiter
                        .lock()
                        .unwrap()
                        .add_tokens(usage.prompt_tokens + usage.completion_tokens);
                    record_savings(&state, &turn_cost);
                    {
                        let mut metrics = state.runtime.metrics.lock().unwrap();
                        metrics.observe_decision(&route.chosen, &route.mode, decision_latency);
                        metrics.observe_upstream(&served_by, upstream_latency);
                        if served_by != route.chosen && !offline {
                            metrics.observe_failover(&route.chosen, &served_by);
                        }
                        metrics.observe_cost(turn_cost.realized, turn_cost.baseline);
                    }
                    push_recent(
                        &state,
                        RecentDecision {
                            request_id: request_id.clone(),
                            model: route.chosen.clone(),
                            served_by: served_by.clone(),
                            score: round_score(decision.score),
                            mode: route.mode.clone(),
                            ts: unix_ts(),
                            cost: cost.clone(),
                            key_id: key_id.clone(),
                        },
                    );
                    if let Some(parsed) = parsed.as_ref() {
                        if cache_key_hint.is_some()
                            && is_storable_response(status, &content_type, parsed)
                        {
                            state.runtime.cache.lock().unwrap().put(
                                cache_key(&target.model, &body),
                                CacheEntry {
                                    status,
                                    content_type: content_type.clone(),
                                    body: bytes.to_vec(),
                                    stored_at: Instant::now(),
                                    prompt_tokens: usage.prompt_tokens,
                                    completion_tokens: usage.completion_tokens,
                                    estimated: usage.estimated,
                                    avoided_cost: cost.realized,
                                },
                            );
                        }
                    }
                    if debug && content_type.contains("json") {
                        if let Some(mut parsed) = parsed {
                            if let Some(object) = parsed.as_object_mut() {
                                object.insert(
                                    "wayfinder".to_owned(),
                                    debug_payload(&decision, &route, &request_id, offline),
                                );
                                return bytes_response(
                                    StatusCode::OK,
                                    with_content_type(served_headers, "application/json"),
                                    serde_json::to_vec(&parsed).unwrap_or_else(|_| bytes.to_vec()),
                                );
                            }
                        }
                    }
                    return bytes_response(
                        status,
                        with_content_type(served_headers, &content_type),
                        bytes.to_vec(),
                    );
                }
                if reliability::is_auth_failure(Some(status.as_u16())) {
                    state
                        .runtime
                        .metrics
                        .lock()
                        .unwrap()
                        .observe_upstream_error(&served_by);
                    state.runtime.breaker.lock().unwrap().record(
                        &served_by,
                        false,
                        reliability_now(),
                    );
                } else {
                    state.runtime.breaker.lock().unwrap().record(
                        &served_by,
                        true,
                        reliability_now(),
                    );
                }
                return bytes_response(
                    status,
                    with_content_type(
                        served_headers(headers, &route.chosen, &served_by, offline),
                        &content_type,
                    ),
                    bytes.to_vec(),
                );
            }
            last_error = format!("upstream returned {status}");
            state
                .runtime
                .metrics
                .lock()
                .unwrap()
                .observe_upstream_error(&served_by);
            if attempt < state.gateway.retries {
                tokio::time::sleep(retry_delay(&delays, attempt)).await;
            }
        }
        state
            .runtime
            .breaker
            .lock()
            .unwrap()
            .record(&served_by, false, reliability_now());
    }
    upstream_error(headers, &last_error)
}

async fn stream_upstream(
    state: AppState,
    plan: Vec<String>,
    body: JsonValue,
    headers: HeaderMap,
    decision: ComplexityScore,
    route: RouteDecision,
    request_id: String,
    prompt: String,
    key_id: Option<String>,
    decision_latency: Duration,
    offline: bool,
) -> Response<Body> {
    let client = upstream_client(&state.options);
    let mut last_error = "no upstream available".to_owned();
    let mut opened = None;
    'targets: for served_by in plan {
        let Some(target) = state.gateway.models.get(&served_by).cloned() else {
            continue;
        };
        let delays = retry_delays_for(state.gateway.retries);
        for attempt in 0..=state.gateway.retries {
            let mut upstream_body = body.clone();
            upstream_body["model"] = JsonValue::String(target.model.clone());
            let upstream_started = Instant::now();
            let response = client
                .post(chat_url(&target.base_url))
                .headers(upstream_headers(&target))
                .json(&upstream_body)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_error = err.to_string();
                    state
                        .runtime
                        .metrics
                        .lock()
                        .unwrap()
                        .observe_upstream_error(&served_by);
                    if attempt < state.gateway.retries {
                        tokio::time::sleep(retry_delay(&delays, attempt)).await;
                    }
                    continue;
                }
            };
            let status = response.status();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/json")
                .to_owned();
            if !reliability::is_retryable(Some(status.as_u16())) {
                if status.is_success() {
                    opened = Some((served_by, response, upstream_started));
                    break 'targets;
                }
                let bytes = match response.bytes().await {
                    Ok(bytes) => bytes,
                    Err(_) => return upstream_error(headers, "upstream response failed"),
                };
                if reliability::is_auth_failure(Some(status.as_u16())) {
                    state
                        .runtime
                        .metrics
                        .lock()
                        .unwrap()
                        .observe_upstream_error(&served_by);
                    state.runtime.breaker.lock().unwrap().record(
                        &served_by,
                        false,
                        reliability_now(),
                    );
                } else {
                    state.runtime.breaker.lock().unwrap().record(
                        &served_by,
                        true,
                        reliability_now(),
                    );
                }
                return bytes_response(
                    status,
                    with_content_type(
                        served_headers(headers, &route.chosen, &served_by, offline),
                        &content_type,
                    ),
                    bytes.to_vec(),
                );
            }
            last_error = format!("upstream returned {status}");
            state
                .runtime
                .metrics
                .lock()
                .unwrap()
                .observe_upstream_error(&served_by);
            if attempt < state.gateway.retries {
                tokio::time::sleep(retry_delay(&delays, attempt)).await;
            }
        }
        state
            .runtime
            .breaker
            .lock()
            .unwrap()
            .record(&served_by, false, reliability_now());
    }
    let Some((served_by, response, upstream_started)) = opened else {
        return upstream_error(headers, &last_error);
    };
    let chosen = route.chosen.clone();
    let stream = Box::pin(response.bytes_stream());
    let accounting = StreamAccounting {
        state,
        route,
        served_by: served_by.clone(),
        request_id,
        prompt,
        key_id,
        score: decision.score,
        decision_latency,
        upstream_started,
        offline,
        completion: String::new(),
        buffer: String::new(),
        failed: false,
        finished: false,
    };
    let stream = futures_util::stream::unfold(
        (stream, accounting),
        |(mut stream, mut accounting)| async move {
            match stream.as_mut().next().await {
                Some(Ok(bytes)) => {
                    accounting.observe_chunk(&bytes);
                    Some((Ok(bytes), (stream, accounting)))
                }
                Some(Err(err)) => {
                    accounting.observe_error();
                    Some((Err(std::io::Error::other(err)), (stream, accounting)))
                }
                None => {
                    accounting.finish();
                    None
                }
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .map(|mut response| {
            *response.headers_mut() = with_content_type(
                served_headers(headers, &chosen, &served_by, offline),
                "text/event-stream",
            );
            response
        })
        .unwrap_or_else(|_| {
            bytes_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                b"response build failed".to_vec(),
            )
        })
}

struct StreamAccounting {
    state: AppState,
    route: RouteDecision,
    served_by: String,
    request_id: String,
    prompt: String,
    key_id: Option<String>,
    score: f64,
    decision_latency: Duration,
    upstream_started: Instant,
    offline: bool,
    completion: String,
    buffer: String,
    failed: bool,
    finished: bool,
}

impl StreamAccounting {
    fn observe_chunk(&mut self, bytes: &[u8]) {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let (completion, done) = drain_stream_completion(&mut self.buffer);
        self.completion.push_str(&completion);
        if done {
            self.finish();
        }
    }

    fn observe_error(&mut self) {
        self.failed = true;
        self.state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_upstream_error(&self.served_by);
        self.state.runtime.breaker.lock().unwrap().record(
            &self.served_by,
            false,
            reliability_now(),
        );
    }

    fn finish(&mut self) {
        if self.failed || self.finished {
            return;
        }
        self.finished = true;
        if !self.buffer.trim().is_empty() {
            self.completion
                .push_str(&stream_event_completion(&self.buffer));
            self.buffer.clear();
        }
        let usage = usage_tokens(&JsonValue::Null, &self.prompt, &self.completion);
        let turn_cost = turn_cost_from_tokens(
            &self.state,
            &self.served_by,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.estimated,
        );
        let cost = recent_cost_from_turn(&self.state, &turn_cost);
        self.state
            .runtime
            .rate_limiter
            .lock()
            .unwrap()
            .add_tokens(usage.prompt_tokens + usage.completion_tokens);
        record_savings(&self.state, &turn_cost);
        observe_decision(
            &self.state,
            &self.route,
            &self.served_by,
            &self.request_id,
            self.score,
            self.decision_latency,
            Some(self.upstream_started.elapsed()),
            cost,
            self.key_id.clone(),
            self.offline,
        );
        self.state
            .runtime
            .breaker
            .lock()
            .unwrap()
            .record(&self.served_by, true, reliability_now());
    }
}

fn drain_stream_completion(buffer: &mut String) -> (String, bool) {
    let mut completion = String::new();
    let mut done = false;
    while let Some(index) = buffer.find("\n\n") {
        let event = buffer.drain(..index + 2).collect::<String>();
        done |= stream_event_done(&event);
        completion.push_str(&stream_event_completion(&event));
    }
    (completion, done)
}

fn stream_event_done(event: &str) -> bool {
    event
        .lines()
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(str::trim)
        .any(|line| line == "[DONE]")
}

fn stream_event_completion(event: &str) -> String {
    event
        .lines()
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "[DONE]")
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .map(|event| {
            event
                .get("choices")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(|choice| {
                    choice
                        .get("delta")
                        .and_then(|delta| delta.get("content"))
                        .or_else(|| {
                            choice
                                .get("message")
                                .and_then(|message| message.get("content"))
                        })
                        .and_then(JsonValue::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join("")
}

fn upstream_client(options: &ServeOptions) -> reqwest::Client {
    let timeout = options
        .timeout_seconds
        .filter(|seconds| *seconds > 0.0)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn upstream_headers(target: &GatewayModel) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    if let Some(env_name) = &target.api_key_env {
        if let Ok(key) = std::env::var(env_name) {
            if !key.is_empty() {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                {
                    headers.insert(reqwest::header::AUTHORIZATION, value);
                }
            }
        }
    }
    headers
}

fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

// --- in-process relay (WF-DESIGN-0001) --------------------------------------
// The terminal chat reuses the gateway's exact forward path in-process, so it gets
// real, token-streamed replies without spawning the axum server. These blocking
// entry points share the server's request building (`upstream_headers` / `chat_url`)
// and SSE/JSON parsing (`extract_completion_text`, `drain_stream_completion`,
// `stream_event_completion`); there is one upstream/parse code path, not two.

fn relay_body(target: &GatewayModel, messages: &[RelayMessage], stream: bool) -> JsonValue {
    let messages = messages
        .iter()
        .map(|message| json!({"role": message.role, "content": message.content}))
        .collect::<Vec<_>>();
    let mut body = json!({"model": target.model, "messages": messages});
    if stream {
        body["stream"] = JsonValue::Bool(true);
    }
    body
}

fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client, UpstreamError> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| UpstreamError::Transport(err.to_string()))
}

/// Run a full OpenAI-style `messages` conversation through one upstream (BYO key).
///
/// The non-streaming relay: returns the assembled assistant reply. Blocking, so it
/// runs off the async server. Reuses the server's `upstream_headers` / `chat_url`
/// request building and `extract_completion_text` reply parsing, so the relay and the
/// axum handler share one path. Must not be called from inside the gateway's Tokio
/// runtime (the terminal chat calls it from its own thread).
pub fn invoke_messages(
    target: &GatewayModel,
    messages: &[RelayMessage],
    timeout: Duration,
) -> Result<String, UpstreamError> {
    let client = blocking_client(timeout)?;
    let body = relay_body(target, messages, false);
    let delays = retry_delays_for(DEFAULT_RETRIES);
    let mut last_transport = None;
    let mut final_response = None;
    for attempt in 0..=DEFAULT_RETRIES {
        let response = client
            .post(chat_url(&target.base_url))
            .headers(upstream_headers(target))
            .json(&body)
            .send();
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                last_transport = Some(err.to_string());
                if attempt < DEFAULT_RETRIES {
                    std::thread::sleep(retry_delay(&delays, attempt));
                    continue;
                }
                break;
            }
        };
        let status = response.status();
        if status.is_success() || !reliability::is_retryable(Some(status.as_u16())) {
            final_response = Some(response);
            break;
        }
        if attempt >= DEFAULT_RETRIES {
            final_response = Some(response);
            break;
        }
        std::thread::sleep(retry_delay(&delays, attempt));
    }
    let Some(response) = final_response else {
        return Err(UpstreamError::Transport(
            last_transport.unwrap_or_else(|| "upstream transport failed".to_owned()),
        ));
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(UpstreamError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = response
        .bytes()
        .map_err(|err| UpstreamError::Transport(err.to_string()))?;
    let parsed = serde_json::from_slice::<JsonValue>(&bytes)
        .map_err(|err| UpstreamError::Shape(err.to_string()))?;
    let has_content = parsed
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(JsonValue::as_str)
        .is_some();
    if !has_content {
        return Err(UpstreamError::Shape(format!(
            "{} returned no assistant message content",
            target.model
        )));
    }
    Ok(extract_completion_text(&parsed))
}

/// Stream assistant text deltas from one upstream over SSE (blocking; BYO key).
///
/// The streaming counterpart to [`invoke_messages`] for the terminal chat: sends
/// `stream: true` and yields `delta.content` chunks as they arrive, reusing the same
/// key/URL handling and the server's SSE parse helpers. A connection or status failure
/// surfaces as the iterator's first (and only) `Err` item; a mid-stream transport error
/// surfaces as a terminal `Err`. Blocking, so it must run off the async server.
pub fn stream_messages(
    target: &GatewayModel,
    messages: &[RelayMessage],
    timeout: Duration,
) -> impl Iterator<Item = Result<String, UpstreamError>> {
    StreamMessages::start(target, messages, timeout)
}

/// Blocking iterator backing [`stream_messages`]: reads upstream bytes and drains
/// complete SSE events through the shared parser, yielding each event's text delta.
struct StreamMessages {
    state: StreamMessagesState,
}

enum StreamMessagesState {
    /// A connection/status failure to emit once before ending.
    Failed(Option<UpstreamError>),
    /// A live upstream response being drained event by event.
    Reading {
        response: reqwest::blocking::Response,
        buffer: String,
        done: bool,
    },
    Done,
}

impl StreamMessages {
    fn start(target: &GatewayModel, messages: &[RelayMessage], timeout: Duration) -> Self {
        let state = match Self::open(target, messages, timeout) {
            Ok(response) => StreamMessagesState::Reading {
                response,
                buffer: String::new(),
                done: false,
            },
            Err(err) => StreamMessagesState::Failed(Some(err)),
        };
        Self { state }
    }

    fn open(
        target: &GatewayModel,
        messages: &[RelayMessage],
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, UpstreamError> {
        let client = blocking_client(timeout)?;
        let body = relay_body(target, messages, true);
        let delays = retry_delays_for(DEFAULT_RETRIES);
        let mut last_transport = None;
        let mut final_response = None;
        for attempt in 0..=DEFAULT_RETRIES {
            let response = client
                .post(chat_url(&target.base_url))
                .headers(upstream_headers(target))
                .json(&body)
                .send();
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_transport = Some(err.to_string());
                    if attempt < DEFAULT_RETRIES {
                        std::thread::sleep(retry_delay(&delays, attempt));
                        continue;
                    }
                    break;
                }
            };
            let status = response.status();
            if status.is_success() || !reliability::is_retryable(Some(status.as_u16())) {
                final_response = Some(response);
                break;
            }
            if attempt >= DEFAULT_RETRIES {
                final_response = Some(response);
                break;
            }
            std::thread::sleep(retry_delay(&delays, attempt));
        }
        let Some(response) = final_response else {
            return Err(UpstreamError::Transport(
                last_transport.unwrap_or_else(|| "upstream transport failed".to_owned()),
            ));
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(UpstreamError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }
}

impl Iterator for StreamMessages {
    type Item = Result<String, UpstreamError>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::io::Read;

        match &mut self.state {
            StreamMessagesState::Failed(err) => {
                let err = err.take();
                self.state = StreamMessagesState::Done;
                err.map(Err)
            }
            StreamMessagesState::Reading {
                response,
                buffer,
                done,
            } => {
                loop {
                    // Drain any events already buffered before reading more bytes.
                    let (completion, finished) = drain_stream_completion(buffer);
                    if finished {
                        *done = true;
                    }
                    if !completion.is_empty() {
                        return Some(Ok(completion));
                    }
                    if *done {
                        self.state = StreamMessagesState::Done;
                        return None;
                    }
                    let mut chunk = [0u8; 8192];
                    match response.read(&mut chunk) {
                        Ok(0) => {
                            // Connection closed: flush any trailing partial event.
                            let trailing = if buffer.trim().is_empty() {
                                String::new()
                            } else {
                                let text = stream_event_completion(buffer);
                                buffer.clear();
                                text
                            };
                            self.state = StreamMessagesState::Done;
                            return if trailing.is_empty() {
                                None
                            } else {
                                Some(Ok(trailing))
                            };
                        }
                        Ok(read) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
                        }
                        Err(err) => {
                            self.state = StreamMessagesState::Done;
                            return Some(Err(UpstreamError::Transport(err.to_string())));
                        }
                    }
                }
            }
            StreamMessagesState::Done => None,
        }
    }
}

fn upstream_error(headers: HeaderMap, message: &str) -> Response<Body> {
    json_response(
        StatusCode::BAD_GATEWAY,
        headers,
        json!({
            "error": {
                "message": message,
                "type": "wayfinder_router_upstream_error"
            }
        }),
    )
}

fn decision_headers(model: &str, score: f64, mode: &str, request_id: &str) -> HeaderMap {
    let mut headers = request_id_header(request_id);
    headers.insert(
        "x-wayfinder-router-model",
        HeaderValue::from_str(model).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers.insert(
        "x-wayfinder-router-score",
        HeaderValue::from_str(&format!("{score:.2}"))
            .unwrap_or_else(|_| HeaderValue::from_static("0.00")),
    );
    headers.insert(
        "x-wayfinder-router-mode",
        HeaderValue::from_str(mode).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers
}

fn request_id_header(request_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-wayfinder-router-request-id",
        HeaderValue::from_str(request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers
}

fn json_response(status: StatusCode, headers: HeaderMap, body: JsonValue) -> Response<Body> {
    bytes_response(
        status,
        with_content_type(headers, "application/json"),
        serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
    )
}

fn bytes_response(status: StatusCode, headers: HeaderMap, body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn with_content_type(mut headers: HeaderMap, content_type: &str) -> HeaderMap {
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers
}

fn authorize_key(
    state: &AppState,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Option<String>, Response<Body>> {
    if state.gateway.keys.is_empty() {
        return Ok(None);
    }
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let presented = vkeys::extract_bearer(authorization);
    let hashes = state
        .gateway
        .keys
        .iter()
        .map(|(id, key)| (id.as_str(), key.hash.as_str()));
    if let Some(key_id) = vkeys::match_key(presented.as_deref(), hashes) {
        return Ok(Some(key_id));
    }
    let mut headers = request_id_header(request_id);
    headers.insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer"),
    );
    Err(json_response(
        StatusCode::UNAUTHORIZED,
        headers,
        json!({
            "error": {
                "message": "missing or invalid Wayfinder virtual key",
                "type": "wayfinder_router_unauthorized"
            }
        }),
    ))
}

fn add_rate_limit_headers(state: &AppState, headers: &mut HeaderMap) {
    let snapshot = state.runtime.rate_limiter.lock().unwrap().snapshot();
    let Some(snapshot) = snapshot else {
        return;
    };
    headers.insert(
        "x-ratelimit-limit",
        HeaderValue::from_str(&snapshot.limit.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-ratelimit-remaining",
        HeaderValue::from_str(&snapshot.remaining.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_str(&snapshot.reset.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("1")),
    );
}

fn cache_enabled(state: &AppState) -> bool {
    state
        .gateway
        .cache
        .as_ref()
        .map(|config| config.enabled)
        .unwrap_or(false)
}

fn cache_key(served_model: &str, body: &JsonValue) -> String {
    let mut projected = BTreeMap::new();
    if let Some(object) = body.as_object() {
        for (key, value) in object {
            if key != "model" && key != "stream" {
                projected.insert(key.clone(), value.clone());
            }
        }
    }
    let blob = serde_json::to_vec(&json!({
        "m": served_model,
        "b": projected
    }))
    .unwrap_or_default();
    let digest = Sha256::digest(blob);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_cacheable_request(body: &JsonValue) -> bool {
    if body.get("stream").and_then(JsonValue::as_bool) == Some(true) {
        return false;
    }
    if !json_number_equals(body.get("temperature"), 0.0, true) {
        return false;
    }
    if !json_number_equals(body.get("top_p"), 1.0, true) {
        return false;
    }
    if !json_number_equals(body.get("n"), 1.0, true) {
        return false;
    }
    if body.get("seed").is_some()
        || truthy_json(body.get("tools"))
        || truthy_json(body.get("tool_choice"))
    {
        return false;
    }
    if truthy_json(body.get("logit_bias")) {
        return false;
    }
    let Some(messages) = body.get("messages").and_then(JsonValue::as_array) else {
        return false;
    };
    if messages.is_empty() {
        return false;
    }
    messages.iter().all(|message| {
        message
            .as_object()
            .and_then(|object| object.get("content"))
            .and_then(JsonValue::as_str)
            .is_some()
    })
}

fn json_number_equals(value: Option<&JsonValue>, expected: f64, missing_is_ok: bool) -> bool {
    let Some(value) = value else {
        return missing_is_ok;
    };
    value.as_f64() == Some(expected)
}

fn truthy_json(value: Option<&JsonValue>) -> bool {
    match value {
        None | Some(JsonValue::Null) => false,
        Some(JsonValue::Bool(value)) => *value,
        Some(JsonValue::Array(values)) => !values.is_empty(),
        Some(JsonValue::Object(values)) => !values.is_empty(),
        Some(JsonValue::String(value)) => !value.is_empty(),
        Some(JsonValue::Number(_)) => true,
    }
}

fn is_storable_response(status: StatusCode, content_type: &str, response: &JsonValue) -> bool {
    if status != StatusCode::OK || !content_type.contains("json") || response.get("error").is_some()
    {
        return false;
    }
    let Some(choice) = response
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first())
        .and_then(JsonValue::as_object)
    else {
        return false;
    };
    let Some(message) = choice.get("message").and_then(JsonValue::as_object) else {
        return false;
    };
    if message.get("tool_calls").is_some() {
        return false;
    }
    message
        .get("content")
        .and_then(JsonValue::as_str)
        .map(|content| !content.is_empty())
        .unwrap_or(false)
}

fn debug_enabled(headers: &HeaderMap) -> bool {
    headers
        .get("x-wayfinder-debug")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

fn debug_payload(
    decision: &ComplexityScore,
    route: &RouteDecision,
    request_id: &str,
    offline: bool,
) -> JsonValue {
    let mut payload = json!({
        "model": route.chosen,
        "score": round_score(decision.score),
        "mode": route.mode,
        "request_id": request_id,
        "features": decision.features,
        "tiers": decision.tiers
    });
    if offline {
        payload["offline"] = JsonValue::Bool(true);
    }
    payload
}

fn dry_run_debug_body(
    state: &AppState,
    decision: &ComplexityScore,
    route: &RouteDecision,
    request_id: &str,
    offline: bool,
) -> JsonValue {
    let mut body = json!({
        "id": "resp-1",
        "object": "chat.completion",
        "wayfinder": {
            "model": route.chosen,
            "score": round_score(decision.score),
            "mode": route.mode,
            "request_id": request_id,
            "features": decision.features,
            "contributions": explain_score(&decision.features, state.routing.weights),
            "tiers": decision.tiers,
            "cost": dry_run_cost(state, &route.chosen, decision.features.word_count),
            "dry_run": true
        }
    });
    if offline {
        body["wayfinder"]["offline"] = JsonValue::Bool(true);
    }
    body
}

fn dry_run_cost(state: &AppState, route: &str, word_count: usize) -> JsonValue {
    let per_1k = state.price_table.get(route).copied().unwrap_or_default();
    let baseline_per_1k = state.price_table.values().copied().fold(per_1k, f64::max);
    let units = word_count as f64 / 1000.0;
    json!({
        "per_call": round_cost(per_1k * units),
        "baseline": round_cost(baseline_per_1k * units),
        "saved": round_cost((baseline_per_1k - per_1k).max(0.0) * units),
        "unit": if state.priced { "usd / 1k tokens" } else { "relative units / 1k words" },
        "estimated": true,
        "word_count": word_count
    })
}

fn extract_completion_text(response: &JsonValue) -> String {
    response
        .get("choices")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(JsonValue::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn recent_cost_from_tokens(
    state: &AppState,
    route: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
    estimated: bool,
) -> RecentCost {
    let cost = turn_cost_from_tokens(state, route, prompt_tokens, completion_tokens, estimated);
    recent_cost_from_turn(state, &cost)
}

fn turn_cost_from_tokens(
    state: &AppState,
    route: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
    estimated: bool,
) -> TurnCost {
    turn_cost(
        route,
        prompt_tokens,
        completion_tokens,
        state
            .price_table
            .iter()
            .map(|(model, cost)| (model.as_str(), *cost)),
        estimated,
        None,
    )
}

fn recent_cost_from_turn(state: &AppState, cost: &TurnCost) -> RecentCost {
    RecentCost {
        realized: cost.realized,
        baseline: cost.baseline,
        saved: cost.savings,
        tokens: cost.prompt_tokens + cost.completion_tokens,
        unit: if state.priced { "usd" } else { "relative" },
        estimated: cost.estimated || !state.priced,
    }
}

fn record_savings(state: &AppState, cost: &TurnCost) {
    let save_result = {
        let mut ledger = state.runtime.ledger.lock().unwrap();
        ledger.priced = state.priced;
        ledger.record(cost, Date::today_utc());
        ledger.save(&state.runtime.savings_path)
    };
    if save_result.is_err() {
        // Savings persistence is best effort; request handling and metrics still succeed.
    }
}

fn zero_recent_cost(priced: bool) -> RecentCost {
    RecentCost {
        realized: 0.0,
        baseline: 0.0,
        saved: 0.0,
        tokens: 0,
        unit: if priced { "usd" } else { "relative" },
        estimated: !priced,
    }
}

fn observe_decision(
    state: &AppState,
    route: &RouteDecision,
    served_by: &str,
    request_id: &str,
    score: f64,
    decision_latency: Duration,
    upstream_latency: Option<Duration>,
    cost: RecentCost,
    key_id: Option<String>,
    offline: bool,
) {
    {
        let mut metrics = state.runtime.metrics.lock().unwrap();
        metrics.observe_decision(&route.chosen, &route.mode, decision_latency);
        if let Some(upstream_latency) = upstream_latency {
            metrics.observe_upstream(served_by, upstream_latency);
        }
        if served_by != route.chosen && !offline {
            metrics.observe_failover(&route.chosen, served_by);
        }
        metrics.observe_cost(cost.realized, cost.baseline);
    }
    push_recent(
        state,
        RecentDecision {
            request_id: request_id.to_owned(),
            model: route.chosen.clone(),
            served_by: served_by.to_owned(),
            score: round_score(score),
            mode: route.mode.clone(),
            ts: unix_ts(),
            cost,
            key_id,
        },
    );
}

fn push_recent(state: &AppState, decision: RecentDecision) {
    let mut recent = state.runtime.recent.lock().unwrap();
    recent.push_front(decision);
    while recent.len() > RECENT_LIMIT {
        recent.pop_back();
    }
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn label_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn round_cost(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn round_score(score: f64) -> f64 {
    (score * 100.0).round() / 100.0
}

fn next_request_id() -> String {
    let id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rust-{id:012x}")
}

fn extract_scoped_prompt(messages: Option<&JsonValue>, headers: &HeaderMap) -> String {
    let scope = headers
        .get("x-wayfinder-route-on")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|scope| matches!(*scope, "turn" | "last_user" | "user" | "all"))
        .unwrap_or("turn");
    if scope == "turn" {
        return extract_prompt(messages);
    }
    let Some(messages) = messages.and_then(JsonValue::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    match scope {
        "all" => {
            parts.extend(messages.iter().filter_map(extract_message_text));
        }
        "user" => {
            parts.extend(messages.iter().filter_map(|message| {
                (message.get("role").and_then(JsonValue::as_str) == Some("user"))
                    .then(|| extract_message_text(message))
                    .flatten()
            }));
        }
        "last_user" => {
            if let Some(text) = messages.iter().rev().find_map(|message| {
                (message.get("role").and_then(JsonValue::as_str) == Some("user"))
                    .then(|| extract_message_text(message))
                    .flatten()
            }) {
                parts.push(text);
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        extract_prompt(Some(&JsonValue::Array(messages.clone())))
    } else {
        parts.join("\n")
    }
}

fn apply_sticky_route(
    state: &AppState,
    headers: &HeaderMap,
    messages: Option<&JsonValue>,
    route: &mut RouteDecision,
) {
    if route.mode == "pinned" || !sticky_enabled(headers) {
        return;
    }
    let Some(messages) = messages.and_then(JsonValue::as_array) else {
        return;
    };
    let ladder = tier_ladder(&state.routing);
    if ladder.len() < 2 {
        return;
    }
    let current = ladder
        .iter()
        .position(|model| model == &route.chosen)
        .unwrap_or(0);
    let sticky = messages
        .iter()
        .filter(|message| message.get("role").and_then(JsonValue::as_str) == Some("user"))
        .filter_map(extract_message_text)
        .map(|text| score_complexity(&text, &state.routing).recommendation)
        .filter_map(|model| ladder.iter().position(|candidate| candidate == &model))
        .max()
        .unwrap_or(current);
    if sticky > current {
        route.chosen = ladder[sticky].clone();
        route.mode = "sticky".to_owned();
    }
}

fn sticky_enabled(headers: &HeaderMap) -> bool {
    headers
        .get("x-wayfinder-sticky")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

fn extract_prompt(messages: Option<&JsonValue>) -> String {
    let Some(messages) = messages.and_then(JsonValue::as_array) else {
        return String::new();
    };
    let mut system = Vec::new();
    let mut last_user = None;
    let mut last_text = None;
    for message in messages {
        let Some(object) = message.as_object() else {
            continue;
        };
        let text = message_text(object);
        if text.is_some() {
            last_text = text.clone();
        }
        match object.get("role").and_then(JsonValue::as_str) {
            Some("system") => {
                if let Some(text) = text {
                    system.push(text);
                }
            }
            Some("user") => {
                last_user = text;
            }
            _ => {}
        }
    }
    let mut selected = system;
    if let Some(text) = last_user {
        selected.push(text);
    }
    if selected.is_empty() {
        if let Some(text) = last_text {
            selected.push(text);
        }
    }
    selected.join("\n")
}

fn extract_message_text(message: &JsonValue) -> Option<String> {
    let object = message.as_object()?;
    message_text(object)
}

fn message_text(message: &Map<String, JsonValue>) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(JsonValue::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn anthropic_to_openai_request(body: &JsonValue) -> JsonValue {
    let mut out = Map::new();
    out.insert(
        "model".to_owned(),
        body.get("model")
            .cloned()
            .unwrap_or_else(|| JsonValue::String(AUTO_DIRECTIVE.to_owned())),
    );

    let mut messages = Vec::new();
    let system = flatten_anthropic_text(body.get("system"));
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    if let Some(input_messages) = body.get("messages").and_then(JsonValue::as_array) {
        for message in input_messages {
            messages.extend(translate_anthropic_message(message));
        }
    }
    out.insert("messages".to_owned(), JsonValue::Array(messages));

    if let Some(value) = body.get("max_tokens") {
        out.insert("max_tokens".to_owned(), value.clone());
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = body.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(stop) = body.get("stop_sequences") {
        if !stop.as_array().is_some_and(Vec::is_empty) {
            out.insert("stop".to_owned(), stop.clone());
        }
    }
    if body.get("stream").and_then(JsonValue::as_bool) == Some(true) {
        out.insert("stream".to_owned(), JsonValue::Bool(true));
    }
    if let Some(tools) = body.get("tools").and_then(JsonValue::as_array) {
        let translated = tools
            .iter()
            .filter_map(translate_anthropic_tool)
            .collect::<Vec<_>>();
        if !translated.is_empty() {
            out.insert("tools".to_owned(), JsonValue::Array(translated));
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        out.insert(
            "tool_choice".to_owned(),
            translate_anthropic_tool_choice(choice),
        );
    }
    JsonValue::Object(out)
}

fn translate_anthropic_message(message: &JsonValue) -> Vec<JsonValue> {
    let Some(object) = message.as_object() else {
        return Vec::new();
    };
    let role = object
        .get("role")
        .and_then(JsonValue::as_str)
        .unwrap_or("user");
    let Some(content) = object.get("content") else {
        return Vec::new();
    };
    if let Some(text) = content.as_str() {
        return vec![json!({"role": role, "content": text})];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_messages = Vec::new();
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        match block.get("type").and_then(JsonValue::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                    text_parts.push(text.to_owned());
                }
            }
            Some("tool_use") => {
                let arguments = serde_json::to_string(
                    block.get("input").unwrap_or(&JsonValue::Object(Map::new())),
                )
                .unwrap_or_else(|_| "{}".to_owned());
                tool_calls.push(json!({
                    "id": block.get("id").and_then(JsonValue::as_str).unwrap_or(""),
                    "type": "function",
                    "function": {
                        "name": block.get("name").and_then(JsonValue::as_str).unwrap_or(""),
                        "arguments": arguments
                    }
                }));
            }
            Some("tool_result") => {
                tool_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": block
                        .get("tool_use_id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or(""),
                    "content": flatten_anthropic_text(block.get("content"))
                }));
            }
            _ => {}
        }
    }

    let mut messages = tool_messages;
    if role == "assistant" {
        if !text_parts.is_empty() || !tool_calls.is_empty() {
            let mut assistant = Map::new();
            assistant.insert("role".to_owned(), JsonValue::String("assistant".to_owned()));
            assistant.insert(
                "content".to_owned(),
                if text_parts.is_empty() {
                    JsonValue::Null
                } else {
                    JsonValue::String(text_parts.join("\n"))
                },
            );
            if !tool_calls.is_empty() {
                assistant.insert("tool_calls".to_owned(), JsonValue::Array(tool_calls));
            }
            messages.push(JsonValue::Object(assistant));
        }
    } else if !text_parts.is_empty() {
        messages.push(json!({"role": role, "content": text_parts.join("\n")}));
    }
    messages
}

fn translate_anthropic_tool(tool: &JsonValue) -> Option<JsonValue> {
    let tool = tool.as_object()?;
    let mut function = Map::new();
    function.insert(
        "name".to_owned(),
        tool.get("name")
            .cloned()
            .unwrap_or_else(|| JsonValue::String(String::new())),
    );
    function.insert(
        "parameters".to_owned(),
        tool.get("input_schema")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(Map::new())),
    );
    if let Some(description) = tool.get("description").and_then(JsonValue::as_str) {
        if !description.is_empty() {
            function.insert(
                "description".to_owned(),
                JsonValue::String(description.to_owned()),
            );
        }
    }
    Some(json!({"type": "function", "function": function}))
}

fn translate_anthropic_tool_choice(choice: &JsonValue) -> JsonValue {
    if let Some(choice) = choice.as_str() {
        return JsonValue::String(choice.to_owned());
    }
    let Some(object) = choice.as_object() else {
        return JsonValue::String("auto".to_owned());
    };
    match object.get("type").and_then(JsonValue::as_str) {
        Some("auto") => JsonValue::String("auto".to_owned()),
        Some("any") => JsonValue::String("required".to_owned()),
        Some("none") => JsonValue::String("none".to_owned()),
        Some("tool") => {
            let name = object.get("name").and_then(JsonValue::as_str).unwrap_or("");
            json!({"type": "function", "function": {"name": name}})
        }
        _ => JsonValue::String("auto".to_owned()),
    }
}

fn flatten_anthropic_text(value: Option<&JsonValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = value.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            block
                .as_str()
                .or_else(|| block.get("text").and_then(JsonValue::as_str))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn openai_to_anthropic_response(response: &JsonValue, model: &str, prompt: &str) -> JsonValue {
    let choice = response
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first());
    let message = choice
        .and_then(|choice| choice.get("message"))
        .and_then(JsonValue::as_object);
    let mut content = Vec::new();
    let mut completion_text = String::new();
    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(JsonValue::as_str)
    {
        completion_text.push_str(text);
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(tool_calls) = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(JsonValue::as_array)
    {
        for (index, tool_call) in tool_calls.iter().enumerate() {
            let function = tool_call.get("function").and_then(JsonValue::as_object);
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .and_then(JsonValue::as_str)
                .unwrap_or("{}");
            completion_text.push_str(arguments);
            let input = serde_json::from_str::<JsonValue>(arguments)
                .unwrap_or_else(|_| JsonValue::Object(Map::new()));
            content.push(json!({
                "type": "tool_use",
                "id": tool_call
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("toolu_{index}")),
                "name": function
                    .and_then(|function| function.get("name"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or(""),
                "input": input
            }));
        }
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }
    let usage = usage_tokens(response, prompt, &completion_text);
    json!({
        "id": response
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("msg_wayfinder"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": anthropic_stop_reason(
            choice.and_then(|choice| choice.get("finish_reason"))
        ),
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.prompt_tokens,
            "output_tokens": usage.completion_tokens
        }
    })
}

fn anthropic_error(status: StatusCode, message: &str) -> JsonValue {
    json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(status),
            "message": message
        }
    })
}

fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 402 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 => "overloaded_error",
        _ => "api_error",
    }
}

fn upstream_error_message(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<JsonValue>(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
}

fn anthropic_stop_reason(finish_reason: Option<&JsonValue>) -> &'static str {
    match finish_reason.and_then(JsonValue::as_str) {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        _ => "end_turn",
    }
}

fn anthropic_stream_response(body: Body, headers: HeaderMap, model: &str) -> Response<Body> {
    let stream = openai_sse_to_anthropic_stream(body, model.to_owned());
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(stream))
        .map(|mut response| {
            *response.headers_mut() = with_content_type(headers, "text/event-stream");
            response
        })
        .unwrap_or_else(|_| {
            bytes_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                b"response build failed".to_vec(),
            )
        })
}

fn openai_sse_to_anthropic_stream(
    body: Body,
    model: String,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    let upstream = body.into_data_stream();
    let translator = AnthropicSseTranslator::new(model);
    let buffer = String::new();
    let pending = VecDeque::from([translator.start()]);
    stream::unfold(
        (upstream, translator, buffer, pending, false),
        |(mut upstream, mut translator, mut buffer, mut pending, mut finished)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((Ok(event), (upstream, translator, buffer, pending, finished)));
                }
                if finished {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(index) = buffer.find('\n') {
                            let line = buffer[..index].to_owned();
                            buffer = buffer[index + 1..].to_owned();
                            let (events, done) =
                                openai_sse_line_events(&mut translator, line.trim());
                            pending.extend(events);
                            if done {
                                pending.push_back(translator.finish());
                                finished = true;
                                break;
                            }
                        }
                    }
                    Some(Err(err)) => {
                        return Some((
                            Err(std::io::Error::other(err)),
                            (upstream, translator, buffer, pending, finished),
                        ));
                    }
                    None => {
                        pending.push_back(translator.finish());
                        finished = true;
                    }
                }
            }
        },
    )
}

fn openai_sse_line_events(
    translator: &mut AnthropicSseTranslator,
    line: &str,
) -> (Vec<Vec<u8>>, bool) {
    let Some(payload) = line.strip_prefix("data:") else {
        return (Vec::new(), false);
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return (Vec::new(), true);
    }
    if let Ok(chunk) = serde_json::from_str::<JsonValue>(payload) {
        let events = translator.feed(&chunk);
        if events.is_empty() {
            return (Vec::new(), false);
        }
        return (vec![events], false);
    }
    (Vec::new(), false)
}

struct AnthropicSseTranslator {
    model: String,
    next_index: usize,
    text_index: Option<usize>,
    text_finished: bool,
    tools: Vec<ToolSlot>,
    completion: String,
    output_tokens: Option<usize>,
    finish_reason: Option<JsonValue>,
}

#[derive(Debug, Default)]
struct ToolSlot {
    openai_index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl AnthropicSseTranslator {
    fn new(model: String) -> Self {
        Self {
            model,
            next_index: 0,
            text_index: None,
            text_finished: false,
            tools: Vec::new(),
            completion: String::new(),
            output_tokens: None,
            finish_reason: None,
        }
    }

    fn start(&self) -> Vec<u8> {
        sse_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_wayfinder_stream",
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        )
    }

    fn feed(&mut self, chunk: &JsonValue) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(usage) = chunk.get("usage").and_then(JsonValue::as_object) {
            if let Some(tokens) = usage
                .get("completion_tokens")
                .and_then(JsonValue::as_u64)
                .and_then(|value| usize::try_from(value).ok())
            {
                self.output_tokens = Some(tokens);
            }
        }
        let Some(choices) = chunk.get("choices").and_then(JsonValue::as_array) else {
            return out;
        };
        for choice in choices {
            let Some(delta) = choice.get("delta") else {
                if let Some(reason) = choice.get("finish_reason") {
                    if !reason.is_null() {
                        self.finish_reason = Some(reason.clone());
                    }
                }
                continue;
            };
            if let Some(text) = delta.get("content").and_then(JsonValue::as_str) {
                if !text.is_empty() {
                    let index = match self.text_index {
                        Some(index) => index,
                        None => {
                            let index = self.next_index;
                            self.next_index += 1;
                            self.text_index = Some(index);
                            out.extend(sse_event(
                                "content_block_start",
                                json!({
                                    "type": "content_block_start",
                                        "index": index,
                                    "content_block": {"type": "text", "text": ""}
                                }),
                            ));
                            index
                        }
                    };
                    self.completion.push_str(text);
                    out.extend(sse_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "text_delta", "text": text}
                        }),
                    ));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(JsonValue::as_array) {
                for call in tool_calls {
                    self.ingest_tool_call(call);
                }
            }
            if let Some(reason) = choice.get("finish_reason") {
                if !reason.is_null() {
                    self.finish_reason = Some(reason.clone());
                }
            }
        }
        out
    }

    fn ingest_tool_call(&mut self, call: &JsonValue) {
        let openai_index = call
            .get("index")
            .and_then(JsonValue::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let slot_index = match self
            .tools
            .iter()
            .position(|slot| slot.openai_index == openai_index)
        {
            Some(index) => index,
            None => {
                self.tools.push(ToolSlot {
                    openai_index,
                    ..ToolSlot::default()
                });
                self.tools.len() - 1
            }
        };
        let slot = &mut self.tools[slot_index];
        if let Some(id) = call
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
        {
            slot.id = Some(id.to_owned());
        }
        if let Some(function) = call.get("function").and_then(JsonValue::as_object) {
            if let Some(name) = function
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.is_empty())
            {
                slot.name = Some(name.to_owned());
            }
            if let Some(arguments) = function.get("arguments").and_then(JsonValue::as_str) {
                slot.arguments.push_str(arguments);
                self.completion.push_str(arguments);
            }
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.text_index.is_none() && self.tools.is_empty() {
            let index = self.next_index;
            self.next_index += 1;
            self.text_index = Some(index);
            out.extend(sse_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            ));
        }
        if !self.text_finished {
            self.text_finished = true;
            if let Some(index) = self.text_index {
                out.extend(sse_event(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
        }
        self.tools.sort_by_key(|slot| slot.openai_index);
        for tool in &self.tools {
            let index = self.next_index;
            self.next_index += 1;
            let id = tool.id.clone().unwrap_or_else(|| format!("toolu_{index}"));
            let name = tool.name.clone().unwrap_or_default();
            out.extend(sse_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            ));
            if !tool.arguments.is_empty() {
                out.extend(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": tool.arguments
                        }
                    }),
                ));
            }
            out.extend(sse_event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
        }
        let output_tokens = self
            .output_tokens
            .unwrap_or_else(|| estimate_tokens(&self.completion));
        out.extend(sse_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": anthropic_stop_reason(self.finish_reason.as_ref()),
                    "stop_sequence": null
                },
                "usage": {"output_tokens": output_tokens}
            }),
        ));
        out.extend(sse_event("message_stop", json!({"type": "message_stop"})));
        out
    }
}

fn sse_event(event: &str, data: JsonValue) -> Vec<u8> {
    format!("event: {event}\ndata: {}\n\n", data).into_bytes()
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let dry_run = state.options.dry_run;
    let recent_total = state.runtime.recent.lock().unwrap().len();
    let body = state.runtime.metrics.lock().unwrap().render(
        env!("CARGO_PKG_VERSION"),
        dry_run,
        recent_total,
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

async fn router_recent(State(state): State<AppState>) -> Json<serde_json::Value> {
    let recent = state.runtime.recent.lock().unwrap();
    let mut by_model = BTreeMap::<String, usize>::new();
    for decision in recent.iter() {
        *by_model.entry(decision.model.clone()).or_default() += 1;
    }
    Json(json!({
        "total": recent.len(),
        "by_model": by_model,
        "recent": recent.iter().cloned().collect::<Vec<_>>()
    }))
}

async fn router_profiles() -> Json<JsonValue> {
    Json(json!({
        "profiles": PROFILES.iter().map(|profile| profile.to_dict()).collect::<Vec<_>>()
    }))
}

async fn router_models(State(state): State<AppState>) -> Json<JsonValue> {
    let models = state
        .gateway
        .models
        .iter()
        .map(|(name, model)| RouterModelEntry {
            name: name.clone(),
            endpoint: model.base_url.clone(),
            model: model.model.clone(),
            api_key_env: model.api_key_env.clone(),
            key_ok: model
                .api_key_env
                .as_ref()
                .map(|env| std::env::var_os(env).is_some())
                .unwrap_or(true),
        })
        .collect::<Vec<_>>();
    Json(json!({
        "models": models,
        "dry_run": state.options.dry_run
    }))
}

async fn router_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn demo_page() -> Html<&'static str> {
    Html(DEMO_HTML)
}

async fn router_config(State(state): State<AppState>) -> Response<Body> {
    match std::fs::read_to_string(&state.config_path) {
        Ok(text) => text_response(StatusCode::OK, HeaderMap::new(), text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => text_response(
            StatusCode::OK,
            HeaderMap::new(),
            current_config_text(&state),
        ),
        Err(err) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            HeaderMap::new(),
            json!({"error": err.to_string()}),
        ),
    }
}

async fn write_router_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let bytes = match to_bytes(body, CONFIG_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                HeaderMap::new(),
                json!({"error": err.to_string()}),
            );
        }
    };
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"error": "config body must be UTF-8"}),
        );
    };
    let Some(config_text) = posted_config_text(&headers, &text, &state) else {
        return text_response(
            StatusCode::OK,
            HeaderMap::new(),
            dump_routing_toml(&state.routing),
        );
    };
    if let Err(err) = validate_full_config(&config_text, &state.config_path) {
        return json_response(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"error": err.to_string()}),
        );
    }
    if let Err(err) = std::fs::write(&state.config_path, &config_text) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            HeaderMap::new(),
            json!({"error": err.to_string()}),
        );
    }
    json_response(
        StatusCode::OK,
        HeaderMap::new(),
        json!({"ok": true, "path": state.config_path.display().to_string()}),
    )
}

fn posted_config_text(headers: &HeaderMap, text: &str, state: &AppState) -> Option<String> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false);
    if !is_json {
        return Some(text.to_owned());
    }
    let Ok(value) = serde_json::from_str::<JsonValue>(text) else {
        return Some(text.to_owned());
    };
    if let Some(toml) = value.get("toml").and_then(JsonValue::as_str) {
        return Some(toml.to_owned());
    }
    if value.as_object().is_some() {
        return None;
    }
    Some(current_config_text(state))
}

fn current_config_text(state: &AppState) -> String {
    let routing = dump_routing_toml(&state.routing);
    let gateway = dump_gateway_toml(&state.gateway);
    if gateway.is_empty() {
        routing
    } else {
        format!("{}\n\n{}\n", routing.trim_end(), gateway.trim_end())
    }
}

fn validate_full_config(text: &str, path: &Path) -> Result<(), GatewayError> {
    let where_ = path.to_string_lossy();
    routing_config_from_toml(text, &where_).map_err(|err| GatewayError::new(err.to_string()))?;
    validate_gateway_toml(text, &where_)
}

fn text_response(status: StatusCode, mut headers: HeaderMap, body: String) -> Response<Body> {
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
