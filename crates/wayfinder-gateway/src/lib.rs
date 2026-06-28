use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::to_bytes;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{stream, StreamExt};
use serde::Serialize;
use serde_json::{json, Map, Value as JsonValue};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use toml::Value;
use wayfinder_internal_core::complexity::{
    recommend_tier, score_complexity, ComplexityScore, RoutingConfig, Tier,
};
use wayfinder_internal_core::config::{routing_config_from_toml, CONFIG_FILE};
use wayfinder_internal_core::pricing::{estimate_tokens, price_table, turn_cost, usage_tokens};
use wayfinder_internal_core::vkeys;
use wayfinder_internal_core::{DEFAULT_HOST, DEFAULT_PORT};

pub const COMMAND_NAME: &str = "serve";

const DEMO_HTML: &str = include_str!("../../../wayfinder_router/demo.html");
const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
const AUTO_DIRECTIVE: &str = "auto";
const PREFER_LOCAL_DIRECTIVE: &str = "prefer-local";
const PREFER_HOSTED_DIRECTIVE: &str = "prefer-hosted";
const PREFER_CLOUD_DIRECTIVE: &str = "prefer-cloud";
const DEFAULT_CACHE_TTL: f64 = 300.0;
const DEFAULT_CACHE_MAX_ENTRIES: usize = 1024;
const DEFAULT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_RATE_LIMIT_WINDOW: f64 = 60.0;
const RECENT_LIMIT: usize = 200;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

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
    routing: RoutingConfig,
    gateway: GatewayConfig,
    model_ids: Vec<String>,
    price_table: BTreeMap<String, f64>,
    priced: bool,
    runtime: Arc<GatewayRuntime>,
}

#[derive(Clone, Debug, PartialEq)]
struct GatewayConfig {
    models: BTreeMap<String, GatewayModel>,
    cache: Option<CacheConfig>,
    rate_limit: Option<RateLimitConfig>,
    keys: BTreeMap<String, VirtualKeyConfig>,
}

#[derive(Clone, Debug, PartialEq)]
struct GatewayModel {
    base_url: String,
    model: String,
    api_key_env: Option<String>,
    cost_per_1k: Option<f64>,
}

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
    metrics: Mutex<Metrics>,
    recent: Mutex<VecDeque<RecentDecision>>,
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

#[derive(Default)]
struct RateLimiter {
    config: Option<RateLimitConfig>,
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
    let state = AppState::load(options, start_dir.as_ref())?;
    Ok(Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/messages", post(anthropic_messages))
        .route("/metrics", get(metrics))
        .route("/router/recent", get(router_recent))
        .route("/router", get(router_dashboard))
        .route("/demo", get(demo_page))
        .route("/router/config", post(router_config_stub))
        .with_state(state))
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
    fn load(options: ServeOptions, start_dir: &Path) -> Result<Self, GatewayError> {
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
        let runtime = Arc::new(GatewayRuntime::new(&loaded.gateway));
        Ok(Self {
            options,
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
    routing: RoutingConfig,
    gateway: GatewayConfig,
}

fn load_config(start_dir: &Path) -> Result<LoadedConfig, GatewayError> {
    let Some(path) = find_config(start_dir) else {
        return Ok(LoadedConfig {
            routing: RoutingConfig::default(),
            gateway: GatewayConfig {
                models: BTreeMap::new(),
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
    Ok(LoadedConfig { routing, gateway })
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
    let Some(models) = data
        .get("gateway")
        .and_then(|gateway| gateway.get("models"))
        .and_then(Value::as_table)
    else {
        return Ok(GatewayConfig {
            models: BTreeMap::new(),
            cache: None,
            rate_limit: None,
            keys: BTreeMap::new(),
        });
    };
    let gateway_table = data
        .get("gateway")
        .and_then(Value::as_table)
        .ok_or_else(|| GatewayError::new(format!("{where_}: '[gateway]' must be a table")))?;
    let cache = parse_cache_config(gateway_table.get("cache"), where_)?;
    let rate_limit = parse_rate_limit_config(gateway_table.get("rate_limit"), where_)?;
    let keys = parse_keys_config(gateway_table.get("keys"), where_)?;
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
        let cost_per_1k = match table.get("cost_per_1k") {
            Some(value) => Some(non_negative_number(value).ok_or_else(|| {
                GatewayError::new(format!(
                    "{where_}: 'gateway.models.{name}.cost_per_1k' must be a non-negative number"
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
                cost_per_1k,
            },
        );
    }
    Ok(GatewayConfig {
        models: parsed,
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
    let Value::Integer(value) = value else {
        return None;
    };
    usize::try_from(*value).ok().filter(|value| *value > 0)
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
    fn new(gateway: &GatewayConfig) -> Self {
        Self {
            cache: Mutex::new(ResponseCache::new(gateway.cache.clone())),
            rate_limiter: Mutex::new(RateLimiter::new(gateway.rate_limit.clone())),
            metrics: Mutex::new(Metrics::default()),
            recent: Mutex::new(VecDeque::new()),
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
    fn new(config: Option<RateLimitConfig>) -> Self {
        Self {
            config,
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
        let now = Instant::now();
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
        let elapsed = Instant::now().duration_since(started);
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
    mut body: JsonValue,
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
    let prompt = extract_prompt(body.get("messages"));
    let decision = score_complexity(&prompt, &state.routing);
    let decision_latency = decision_started.elapsed();
    let route = match route_decision(&state, &headers, body.get("model"), &decision, &request_id) {
        Ok(route) => route,
        Err(response) => return response,
    };
    let client_body = body.clone();
    let mut response_headers =
        decision_headers(&route.chosen, decision.score, &route.mode, &request_id);
    add_rate_limit_headers(&state, &mut response_headers);
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
        );
        return json_response(
            StatusCode::OK,
            response_headers,
            json!({
                "wayfinder": {
                    "model": route.chosen,
                    "score": round_score(decision.score),
                    "mode": route.mode,
                    "request_id": request_id,
                    "features": decision.features,
                    "tiers": decision.tiers,
                    "dry_run": true
                }
            }),
        );
    }

    let Some(target) = state.gateway.models.get(&route.chosen).cloned() else {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            response_headers,
            json!({
                "error": {
                    "message": format!("no gateway endpoint configured for model '{}'", route.chosen),
                    "type": "wayfinder_router_misconfigured"
                }
            }),
        );
    };

    let served_by = route.chosen.clone();
    response_headers.insert(
        "x-wayfinder-router-served-by",
        HeaderValue::from_str(&served_by).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
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
                &route.chosen,
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
                    served_by: served_by.clone(),
                    score: round_score(decision.score),
                    mode: route.mode.clone(),
                    ts: unix_ts(),
                    cost,
                    key_id,
                },
            );
            return bytes_response(
                entry.status,
                with_content_type(response_headers, &entry.content_type),
                entry.body,
            );
        }
        response_headers.insert("x-wayfinder-router-cache", HeaderValue::from_static("miss"));
        state.runtime.metrics.lock().unwrap().observe_cache_miss();
    }
    body["model"] = JsonValue::String(target.model.clone());
    if body.get("stream").and_then(JsonValue::as_bool) == Some(true) {
        return stream_upstream(
            state,
            target,
            body,
            response_headers,
            decision,
            route,
            request_id,
            prompt,
            key_id,
            decision_latency,
        )
        .await;
    }
    forward_upstream(
        state,
        target,
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

async fn forward_upstream(
    state: AppState,
    target: GatewayModel,
    body: JsonValue,
    headers: HeaderMap,
    debug: bool,
    decision: ComplexityScore,
    route: RouteDecision,
    request_id: String,
    prompt: String,
    cache_key: Option<String>,
    key_id: Option<String>,
    decision_latency: Duration,
) -> Response<Body> {
    let client = upstream_client(&state.options);
    let url = chat_url(&target.base_url);
    let upstream_started = Instant::now();
    let response = client
        .post(url)
        .headers(upstream_headers(&target))
        .json(&body)
        .send()
        .await;
    let Ok(response) = response else {
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_upstream_error(&route.chosen);
        return upstream_error(headers, "upstream request failed");
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
        Err(_) => {
            state
                .runtime
                .metrics
                .lock()
                .unwrap()
                .observe_upstream_error(&route.chosen);
            return upstream_error(headers, "upstream response failed");
        }
    };
    if status.is_client_error() {
        return bytes_response(
            status,
            with_content_type(headers, &content_type),
            bytes.to_vec(),
        );
    }
    if !status.is_success() {
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_upstream_error(&route.chosen);
        return upstream_error(headers, &format!("upstream returned {status}"));
    }
    let parsed = serde_json::from_slice::<JsonValue>(&bytes).ok();
    let completion = parsed
        .as_ref()
        .map(extract_completion_text)
        .unwrap_or_default();
    let usage = parsed
        .as_ref()
        .map(|parsed| usage_tokens(parsed, &prompt, &completion))
        .unwrap_or_else(|| usage_tokens(&JsonValue::Null, &prompt, &completion));
    let cost = recent_cost_from_tokens(
        &state,
        &route.chosen,
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.estimated,
    );
    state
        .runtime
        .rate_limiter
        .lock()
        .unwrap()
        .add_tokens(usage.prompt_tokens + usage.completion_tokens);
    {
        let mut metrics = state.runtime.metrics.lock().unwrap();
        metrics.observe_decision(&route.chosen, &route.mode, decision_latency);
        metrics.observe_upstream(&route.chosen, upstream_latency);
        metrics.observe_cost(cost.realized, cost.baseline);
    }
    push_recent(
        &state,
        RecentDecision {
            request_id: request_id.clone(),
            model: route.chosen.clone(),
            served_by: route.chosen.clone(),
            score: round_score(decision.score),
            mode: route.mode.clone(),
            ts: unix_ts(),
            cost: cost.clone(),
            key_id: key_id.clone(),
        },
    );
    if let (Some(cache_key), Some(parsed)) = (&cache_key, parsed.as_ref()) {
        if is_storable_response(status, &content_type, parsed) {
            state.runtime.cache.lock().unwrap().put(
                cache_key.clone(),
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
                    debug_payload(&decision, &route, &request_id),
                );
                return bytes_response(
                    StatusCode::OK,
                    with_content_type(headers, "application/json"),
                    serde_json::to_vec(&parsed).unwrap_or_else(|_| bytes.to_vec()),
                );
            }
        }
    }
    bytes_response(
        status,
        with_content_type(headers, &content_type),
        bytes.to_vec(),
    )
}

async fn stream_upstream(
    state: AppState,
    target: GatewayModel,
    body: JsonValue,
    headers: HeaderMap,
    decision: ComplexityScore,
    route: RouteDecision,
    request_id: String,
    prompt: String,
    key_id: Option<String>,
    decision_latency: Duration,
) -> Response<Body> {
    let client = upstream_client(&state.options);
    let url = chat_url(&target.base_url);
    let upstream_started = Instant::now();
    let response = client
        .post(url)
        .headers(upstream_headers(&target))
        .json(&body)
        .send()
        .await;
    let Ok(response) = response else {
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_upstream_error(&route.chosen);
        return upstream_error(headers, "upstream stream request failed");
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    if status.is_client_error() {
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return upstream_error(headers, "upstream response failed"),
        };
        return bytes_response(
            status,
            with_content_type(headers, &content_type),
            bytes.to_vec(),
        );
    }
    if !status.is_success() {
        state
            .runtime
            .metrics
            .lock()
            .unwrap()
            .observe_upstream_error(&route.chosen);
        return upstream_error(headers, &format!("upstream returned {status}"));
    }
    let stream = Box::pin(response.bytes_stream());
    let accounting = StreamAccounting {
        state,
        route,
        request_id,
        prompt,
        key_id,
        score: decision.score,
        decision_latency,
        upstream_started,
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
                    Some((
                        Err(std::io::Error::new(std::io::ErrorKind::Other, err)),
                        (stream, accounting),
                    ))
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

struct StreamAccounting {
    state: AppState,
    route: RouteDecision,
    request_id: String,
    prompt: String,
    key_id: Option<String>,
    score: f64,
    decision_latency: Duration,
    upstream_started: Instant,
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
            .observe_upstream_error(&self.route.chosen);
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
        let cost = recent_cost_from_tokens(
            &self.state,
            &self.route.chosen,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.estimated,
        );
        self.state
            .runtime
            .rate_limiter
            .lock()
            .unwrap()
            .add_tokens(usage.prompt_tokens + usage.completion_tokens);
        observe_decision(
            &self.state,
            &self.route,
            &self.route.chosen,
            &self.request_id,
            self.score,
            self.decision_latency,
            Some(self.upstream_started.elapsed()),
            cost,
            self.key_id.clone(),
        );
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

fn debug_payload(decision: &ComplexityScore, route: &RouteDecision, request_id: &str) -> JsonValue {
    json!({
        "model": route.chosen,
        "score": round_score(decision.score),
        "mode": route.mode,
        "request_id": request_id,
        "features": decision.features,
        "tiers": decision.tiers
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
    let cost = turn_cost(
        route,
        prompt_tokens,
        completion_tokens,
        state
            .price_table
            .iter()
            .map(|(model, cost)| (model.as_str(), *cost)),
        estimated,
        None,
    );
    RecentCost {
        realized: cost.realized,
        baseline: cost.baseline,
        saved: cost.savings,
        tokens: prompt_tokens + completion_tokens,
        unit: if state.priced { "usd" } else { "relative" },
        estimated: cost.estimated || !state.priced,
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
) {
    {
        let mut metrics = state.runtime.metrics.lock().unwrap();
        metrics.observe_decision(&route.chosen, &route.mode, decision_latency);
        if let Some(upstream_latency) = upstream_latency {
            metrics.observe_upstream(served_by, upstream_latency);
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
                            Err(std::io::Error::new(std::io::ErrorKind::Other, err)),
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
    text_started: bool,
    text_finished: bool,
    completion: String,
    output_tokens: Option<usize>,
    finish_reason: Option<JsonValue>,
}

impl AnthropicSseTranslator {
    fn new(model: String) -> Self {
        Self {
            model,
            text_started: false,
            text_finished: false,
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
            if let Some(text) = choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(JsonValue::as_str)
            {
                if !text.is_empty() {
                    if !self.text_started {
                        self.text_started = true;
                        out.extend(sse_event(
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": 0,
                                "content_block": {"type": "text", "text": ""}
                            }),
                        ));
                    }
                    self.completion.push_str(text);
                    out.extend(sse_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": text}
                        }),
                    ));
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

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.text_started {
            self.text_started = true;
            out.extend(sse_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            ));
        }
        if !self.text_finished {
            self.text_finished = true;
            out.extend(sse_event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
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
    let dry_run = if state.options.dry_run { true } else { false };
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

async fn router_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn demo_page() -> Html<&'static str> {
    Html(DEMO_HTML)
}

async fn router_config_stub() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "status": "stub",
            "message": "Rust gateway config export is reserved for a later task"
        })),
    )
}
