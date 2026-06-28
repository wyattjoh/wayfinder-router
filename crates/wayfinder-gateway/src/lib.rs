use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use toml::Value;
use wayfinder_internal_core::complexity::RoutingConfig;
use wayfinder_internal_core::config::{routing_config_from_toml, CONFIG_FILE};
use wayfinder_internal_core::{DEFAULT_HOST, DEFAULT_PORT};

pub const COMMAND_NAME: &str = "serve";

const DEMO_HTML: &str = include_str!("../../../wayfinder_router/demo.html");
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
    model_ids: Vec<String>,
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
        let model_ids = model_ids(&loaded.routing, loaded.gateway_model_ids);
        Ok(Self {
            options,
            routing: loaded.routing,
            model_ids,
        })
    }
}

struct LoadedConfig {
    routing: RoutingConfig,
    gateway_model_ids: Vec<String>,
}

fn load_config(start_dir: &Path) -> Result<LoadedConfig, GatewayError> {
    let Some(path) = find_config(start_dir) else {
        return Ok(LoadedConfig {
            routing: RoutingConfig::default(),
            gateway_model_ids: Vec::new(),
        });
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| GatewayError::new(format!("{}: {err}", path.display())))?;
    let where_ = path.to_string_lossy();
    let routing = routing_config_from_toml(&text, &where_)
        .map_err(|err| GatewayError::new(err.to_string()))?;
    let gateway_model_ids = parse_gateway_model_ids(&text, &where_)?;
    Ok(LoadedConfig {
        routing,
        gateway_model_ids,
    })
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

fn parse_gateway_model_ids(text: &str, where_: &str) -> Result<Vec<String>, GatewayError> {
    let data: Value = text
        .parse()
        .map_err(|err| GatewayError::new(format!("{where_}: invalid TOML: {err}")))?;
    let Some(models) = data
        .get("gateway")
        .and_then(|gateway| gateway.get("models"))
        .and_then(Value::as_table)
    else {
        return Ok(Vec::new());
    };
    let mut ids = models.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

fn model_ids(routing: &RoutingConfig, gateway_model_ids: Vec<String>) -> Vec<String> {
    if !gateway_model_ids.is_empty() {
        return gateway_model_ids;
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

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let dry_run = if state.options.dry_run {
        "true"
    } else {
        "false"
    };
    let body = format!(
        "# HELP wayfinder_router_build_info Build and runtime metadata.\n\
# TYPE wayfinder_router_build_info gauge\n\
wayfinder_router_build_info{{version=\"{}\",dry_run=\"{}\"}} 1\n\
# HELP wayfinder_router_recent_decisions_total Number of routing decisions retained in memory.\n\
# TYPE wayfinder_router_recent_decisions_total gauge\n\
wayfinder_router_recent_decisions_total 0\n",
        env!("CARGO_PKG_VERSION"),
        dry_run
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

async fn router_recent() -> Json<serde_json::Value> {
    Json(json!({
        "total": 0,
        "by_model": BTreeMap::<String, usize>::new(),
        "recent": []
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
