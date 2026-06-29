use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use wayfinder_internal_core::calibrate::{
    calibrate, parse_dataset, sweep_curve, CalibrationError, CalibrationOptions,
};
use wayfinder_internal_core::complexity::{
    binary_tiers, explain_score, score_complexity, RoutingConfig,
};
use wayfinder_internal_core::config::{
    dump_routing_toml, load_routing_config, routing_config_from_toml, CONFIG_FILE,
};
use wayfinder_internal_gateway::validate_gateway_toml;

pub mod page;

pub const COMMAND_NAME: &str = "ui";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8099;

const MODES: [&str; 3] = ["threshold", "tiers", "classifier"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiOptions {
    pub host: String,
    pub port: u16,
}

impl Default for UiOptions {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiError {
    message: String,
}

impl UiError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for UiError {}

impl From<std::io::Error> for UiError {
    fn from(err: std::io::Error) -> Self {
        Self::new(err.to_string())
    }
}

#[derive(Clone)]
struct AppState {
    start_dir: PathBuf,
}

pub fn build_app() -> Result<Router, UiError> {
    build_app_from_dir(std::env::current_dir()?)
}

pub fn build_app_from_dir(start_dir: impl AsRef<Path>) -> Result<Router, UiError> {
    let state = AppState {
        start_dir: start_dir.as_ref().to_path_buf(),
    };
    Ok(Router::new()
        .route("/", get(index))
        .route("/api/score", post(api_score))
        .route("/api/calibrate", post(api_calibrate))
        .route("/api/config", get(api_get_config))
        .route("/api/config/validate", post(api_validate_config))
        .route("/api/config/save", post(api_save_config))
        .with_state(state))
}

pub async fn serve(options: UiOptions) -> Result<(), UiError> {
    let app = build_app()?;
    let listener = TcpListener::bind((options.host.as_str(), options.port)).await?;
    eprintln!("{}", serve_summary(&options));
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn serve_blocking(options: UiOptions) -> Result<(), UiError> {
    tokio::runtime::Runtime::new()
        .map_err(UiError::from)?
        .block_on(serve(options))
}

pub fn serve_summary(options: &UiOptions) -> String {
    format!(
        "wayfinder-router ui listening on http://{}:{}",
        options.host, options.port
    )
}

pub fn score_payload(
    prompt: &str,
    start_dir: impl AsRef<Path>,
    threshold: Option<f64>,
) -> Result<JsonValue, UiError> {
    let mut config =
        load_routing_config(start_dir.as_ref()).map_err(|err| UiError::new(err.to_string()))?;
    if let Some(threshold) = threshold {
        config = RoutingConfig {
            weights: config.weights,
            tiers: binary_tiers(threshold),
            classifier: None,
            lexicon: Default::default(),
        };
    }

    let result = score_complexity(prompt, &config);
    let mut payload = serde_json::to_value(&result).map_err(|err| UiError::new(err.to_string()))?;
    payload["contributions"] =
        serde_json::to_value(explain_score(&result.features, config.weights))
            .map_err(|err| UiError::new(err.to_string()))?;
    Ok(payload)
}

pub fn calibrate_payload(
    dataset_text: &str,
    mode: &str,
    models: Option<Vec<String>>,
) -> Result<JsonValue, CalibrationError> {
    let samples = parse_dataset(dataset_text, "<dataset>")?;
    let result = calibrate(
        &samples,
        mode,
        CalibrationOptions {
            models_order: models,
            ..CalibrationOptions::default()
        },
    )?;
    let mut payload = json!({
        "toml": result.toml,
        "summary": result.summary,
    });
    if mode == "threshold" {
        payload["curve"] = JsonValue::Array(
            sweep_curve(&samples)?
                .into_iter()
                .map(|(threshold, accuracy)| {
                    json!({
                        "threshold": threshold,
                        "accuracy": accuracy,
                    })
                })
                .collect(),
        );
    }
    Ok(payload)
}

pub fn current_config_text(start_dir: impl AsRef<Path>) -> Result<String, UiError> {
    let Some(path) = find_config_file(start_dir.as_ref()) else {
        return Ok(dump_routing_toml(&RoutingConfig::default()));
    };
    fs::read_to_string(&path)
        .map_err(|err| UiError::new(format!("cannot read {}: {err}", path.display())))
}

pub fn validate_config_text(text: &str) -> Option<String> {
    if let Err(err) = routing_config_from_toml(text, CONFIG_FILE) {
        return Some(err.to_string());
    }
    validate_gateway_toml(text, CONFIG_FILE)
        .err()
        .map(|err| err.to_string())
}

pub fn save_config_text(text: &str, start_dir: impl AsRef<Path>) -> Option<String> {
    if let Some(error) = validate_config_text(text) {
        return Some(error);
    }
    let path = start_dir.as_ref().join(CONFIG_FILE);
    fs::write(&path, text).err().map(|err| err.to_string())
}

async fn index() -> Html<&'static str> {
    Html(page::PAGE)
}

async fn api_score(State(state): State<AppState>, Json(body): Json<JsonValue>) -> Response {
    let prompt = body
        .get("prompt")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let threshold = body.get("threshold").and_then(JsonValue::as_f64);
    match score_payload(prompt, &state.start_dir, threshold) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn api_calibrate(Json(body): Json<JsonValue>) -> Response {
    let dataset = body
        .get("dataset")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let raw_mode = body.get("mode").and_then(JsonValue::as_str);
    let mode = raw_mode
        .filter(|mode| MODES.contains(mode))
        .unwrap_or("threshold");

    match calibrate_payload(dataset, mode, models_list(body.get("models"))) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn api_get_config(State(state): State<AppState>) -> Response {
    match current_config_text(&state.start_dir) {
        Ok(toml) => Json(json!({ "toml": toml })).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn api_validate_config(Json(body): Json<JsonValue>) -> Json<JsonValue> {
    let toml = body
        .get("toml")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let error = validate_config_text(toml);
    Json(json!({
        "ok": error.is_none(),
        "error": error,
    }))
}

async fn api_save_config(State(state): State<AppState>, Json(body): Json<JsonValue>) -> Response {
    let toml = body
        .get("toml")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    match save_config_text(toml, &state.start_dir) {
        None => Json(json!({ "ok": true })).into_response(),
        Some(error) => json_error(StatusCode::BAD_REQUEST, error),
    }
}

fn models_list(value: Option<&JsonValue>) -> Option<Vec<String>> {
    match value {
        Some(JsonValue::Array(values)) => {
            let models = values
                .iter()
                .map(|value| value.as_str().unwrap_or_default().trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            (!models.is_empty()).then_some(models)
        }
        Some(JsonValue::String(value)) if !value.trim().is_empty() => {
            let models = value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            (!models.is_empty()).then_some(models)
        }
        _ => None,
    }
}

fn json_error(status: StatusCode, error: String) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn find_config_file(start_dir: &Path) -> Option<PathBuf> {
    let current = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    current.ancestors().find_map(|directory| {
        let candidate = directory.join(CONFIG_FILE);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::{calibrate_payload, current_config_text, save_config_text, validate_config_text};

    const TRIVIAL: &str = "hi";
    const COMPLEX: &str = "# Plan\n\n## Steps\n\n- step 0\n- step 1\n- step 2\n- step 3\n- step 4\n- step 5\n- step 6\n- step 7\n- step 8\n- step 9\n- step 10\n- step 11\n\n## Refs\n\n[a](https://x) [b](https://y)\n\n```py\nx=1\n```\n| a | b |\n| - | - |\n";

    fn dataset() -> String {
        let local = format!(r#"{{"text":{TRIVIAL:?},"label":"local"}}"#);
        let cloud = format!(r#"{{"text":{COMPLEX:?},"label":"cloud"}}"#);
        std::iter::repeat_n(local, 4)
            .chain(std::iter::repeat_n(cloud, 4))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn calibrate_payload_threshold_returns_curve_and_fragment() {
        let payload = calibrate_payload(&dataset(), "threshold", None).expect("payload");

        assert_eq!(payload["summary"]["accuracy"], 1.0);
        assert!(payload["toml"]
            .as_str()
            .unwrap()
            .contains("[[routing.tiers]]"));
        assert!(payload["curve"]
            .as_array()
            .unwrap()
            .iter()
            .any(|point| { point["accuracy"] == 1.0 }));
    }

    #[test]
    fn calibrate_payload_classifier_has_no_curve() {
        let payload = calibrate_payload(&dataset(), "classifier", None).expect("payload");

        assert!(payload["toml"]
            .as_str()
            .unwrap()
            .contains("[routing.classifier]"));
        assert!(payload.get("curve").is_none());
    }

    #[test]
    fn config_helpers_default_validate_and_save() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(current_config_text(dir.path())
            .unwrap()
            .contains("[[routing.tiers]]"));
        assert_eq!(validate_config_text("[routing]\nthreshold = 0.6\n"), None);
        assert!(validate_config_text("[routing]\nthreshold = 2.0\n").is_some());

        assert_eq!(
            save_config_text("[routing]\nthreshold = 0.7\n", dir.path()),
            None
        );
        let saved = std::fs::read_to_string(dir.path().join("wayfinder-router.toml"))
            .expect("saved config");
        assert!(saved.starts_with("[routing]"));

        assert!(save_config_text("[routing]\nthreshold = 9\n", dir.path()).is_some());
        let preserved = std::fs::read_to_string(dir.path().join("wayfinder-router.toml"))
            .expect("saved config");
        assert!(preserved.contains("0.7"));
    }
}
