use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value as JsonValue};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tower::ServiceExt;
use wayfinder_internal_core::complexity::FEATURE_ORDER;
use wayfinder_internal_core::feedback::DEFAULT_LOG;
use wayfinder_internal_gateway::UpstreamError;
use wayfinder_internal_ui::{build_app_from_dir, build_app_from_dir_with_invoker};

const TRIVIAL: &str = "hi";
const COMPLEX: &str = "# Plan\n\n## Steps\n\n- step 0\n- step 1\n- step 2\n- step 3\n- step 4\n- step 5\n- step 6\n- step 7\n- step 8\n- step 9\n- step 10\n- step 11\n\n## Refs\n\n[a](https://x) [b](https://y)\n\n```py\nx=1\n```\n| a | b |\n| - | - |\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("ui crate lives under crates/wayfinder-ui")
        .to_path_buf()
}

fn contract_fixture(path: &str) -> JsonValue {
    let path = repo_root().join("tests/fixtures/contracts").join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()))
}

fn dataset() -> String {
    let local = format!(r#"{{"text":{TRIVIAL:?},"label":"local"}}"#);
    let cloud = format!(r#"{{"text":{COMPLEX:?},"label":"cloud"}}"#);
    std::iter::repeat_n(local, 4)
        .chain(std::iter::repeat_n(cloud, 4))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn get_text(dir: &TempDir, path: &str) -> (StatusCode, String, String) {
    let app = build_app_from_dir(dir.path()).expect("app should build");
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

async fn get_json(dir: &TempDir, path: &str) -> (StatusCode, JsonValue) {
    let app = build_app_from_dir(dir.path()).expect("app should build");
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn post_json(dir: &TempDir, path: &str, body: JsonValue) -> (StatusCode, JsonValue) {
    let app = build_app_from_dir(dir.path()).expect("app should build");
    post_json_app(app, path, body).await
}

async fn post_json_app(app: axum::Router, path: &str, body: JsonValue) -> (StatusCode, JsonValue) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn write_gateway_config(dir: &TempDir) {
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        r#"[gateway.models.cloud]
base_url = "http://cloud.example/v1"
model = "cloud-model"

[gateway.models.local]
base_url = "http://local.example/v1"
model = "local-model"
"#,
    )
    .expect("gateway config should be writable");
}

fn write_gateway_config_from_fixture(dir: &TempDir, fixture: &JsonValue) {
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        fixture["gateway_config"].as_str().unwrap(),
    )
    .expect("gateway config should be writable");
}

fn write_feedback_log_from_fixture(dir: &TempDir, fixture: &JsonValue) {
    let lines = fixture["feedback_log_lines"]
        .as_array()
        .expect("feedback_log_lines should be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("feedback log line should be a string")
        })
        .collect::<Vec<_>>();
    std::fs::write(dir.path().join(DEFAULT_LOG), lines.join("\n"))
        .expect("feedback log should write");
}

#[tokio::test]
async fn index_serves_the_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (status, content_type, body) = get_text(&dir, "/").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"));
    assert!(body.contains("Wayfinder"));
}

#[tokio::test]
async fn api_score_returns_python_payload_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (status, body) = post_json(&dir, "/api/score", json!({ "prompt": COMPLEX })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema_version"], "3");
    assert!(body["score"].is_number());
    assert!(matches!(
        body["recommendation"].as_str(),
        Some("local" | "cloud")
    ));
    assert!(body["tiers"].is_array());
    assert_eq!(
        body["contributions"].as_array().unwrap().len(),
        FEATURE_ORDER.len()
    );
    assert_eq!(body["contributions"][0]["name"], "word_count");
}

#[tokio::test]
async fn api_score_threshold_override_changes_routing() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (_, low) = post_json(
        &dir,
        "/api/score",
        json!({ "prompt": TRIVIAL, "threshold": 0.0 }),
    )
    .await;
    assert_eq!(low["recommendation"], "cloud");

    let (_, high) = post_json(
        &dir,
        "/api/score",
        json!({ "prompt": TRIVIAL, "threshold": 1.0 }),
    )
    .await;
    assert_eq!(high["recommendation"], "local");
}

#[tokio::test]
async fn api_calibrate_returns_fragment_summary_and_curve() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (status, body) = post_json(
        &dir,
        "/api/calibrate",
        json!({ "dataset": dataset(), "mode": "threshold" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["toml"].as_str().unwrap().contains("[[routing.tiers]]"));
    assert_eq!(body["summary"]["accuracy"], 1.0);
    assert!(body["curve"]
        .as_array()
        .unwrap()
        .iter()
        .any(|point| { point["accuracy"] == 1.0 }));
}

#[tokio::test]
async fn api_calibrate_bad_dataset_is_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (status, body) = post_json(
        &dir,
        "/api/calibrate",
        json!({ "dataset": "not json", "mode": "threshold" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid JSON"));
}

#[tokio::test]
async fn api_config_get_validate_save_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (status, body) = get_json(&dir, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["toml"].as_str().unwrap().contains("[[routing.tiers]]"));

    let (_, bad) = post_json(
        &dir,
        "/api/config/validate",
        json!({ "toml": "[routing]\nthreshold = 5\n" }),
    )
    .await;
    assert_eq!(bad["ok"], false);
    assert!(bad["error"].as_str().unwrap().contains("routing.threshold"));

    let (_, good) = post_json(
        &dir,
        "/api/config/validate",
        json!({ "toml": "[routing]\nthreshold = 0.6\n" }),
    )
    .await;
    assert_eq!(good["ok"], true);
    assert_eq!(good["error"], JsonValue::Null);

    let (status, saved) = post_json(
        &dir,
        "/api/config/save",
        json!({ "toml": "[routing]\nthreshold = 0.6\n" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved, json!({ "ok": true }));
    assert!(
        std::fs::read_to_string(dir.path().join("wayfinder-router.toml"))
            .unwrap()
            .starts_with("[routing]")
    );
}

#[tokio::test]
async fn api_config_save_rejects_invalid_without_overwriting() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (status, _) = post_json(
        &dir,
        "/api/config/save",
        json!({ "toml": "[routing]\nthreshold = 0.6\n" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(
        &dir,
        "/api/config/save",
        json!({ "toml": "[routing]\nthreshold = 9\n" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("routing.threshold"));
    assert!(
        std::fs::read_to_string(dir.path().join("wayfinder-router.toml"))
            .unwrap()
            .contains("0.6")
    );
}

#[tokio::test]
async fn api_onboard_state_lists_arms_and_label_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config(&dir);

    let (status, body) = get_json(&dir, "/api/onboard").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["arms"], json!(["cloud", "local"]));
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn api_onboard_run_returns_per_arm_outputs_from_stub_invoker() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config(&dir);
    let app = build_app_from_dir_with_invoker(dir.path(), |model, prompt| {
        Ok(format!("reply:{}:{prompt}", model.model))
    })
    .expect("app should build");

    let (status, body) = post_json_app(app, "/api/onboard/run", json!({ "prompt": "hi" })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["outputs"],
        json!({
            "cloud": "reply:cloud-model:hi",
            "local": "reply:local-model:hi",
        })
    );
}

#[tokio::test]
async fn api_onboard_run_maps_upstream_errors_to_502() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config(&dir);
    let app = build_app_from_dir_with_invoker(dir.path(), |_model, _prompt| {
        Err(UpstreamError::Status {
            status: 503,
            body: "upstream unavailable".to_owned(),
        })
    })
    .expect("app should build");

    let (status, body) = post_json_app(app, "/api/onboard/run", json!({ "prompt": "hi" })).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("upstream returned 503"));
}

#[tokio::test]
async fn api_onboard_run_missing_prompt_is_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config(&dir);

    let (status, body) = post_json(&dir, "/api/onboard/run", json!({})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "missing 'prompt'");
}

#[tokio::test]
async fn api_onboard_record_appends_label_and_dataset_returns_jsonl() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config(&dir);

    let (status, body) = post_json(
        &dir,
        "/api/onboard/record",
        json!({ "prompt": "hi", "label": "local" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true, "count": 1 }));
    let log_path = dir.path().join(DEFAULT_LOG);
    assert!(log_path.is_file());

    let (status, dataset_body) = get_json(&dir, "/api/onboard/dataset").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        dataset_body["dataset"].as_str().unwrap(),
        r#"{"text":"hi","label":"local"}"#
    );
}

#[tokio::test]
async fn api_recalibrate_writes_config_from_feedback_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(DEFAULT_LOG), dataset()).expect("feedback log should write");

    let (status, body) = post_json(&dir, "/api/recalibrate", json!({ "mode": "threshold" })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["written"], true);
    assert_eq!(body["label_count"], 8);
    assert_eq!(body["summary"]["accuracy"], 1.0);
    assert_eq!(body["reason"], JsonValue::Null);
    let config = std::fs::read_to_string(dir.path().join("wayfinder-router.toml")).unwrap();
    assert!(config.starts_with("# recalibrated from feedback: "));
    assert!(config.contains("[[routing.tiers]]"));
}

#[tokio::test]
async fn api_recalibrate_skips_empty_log() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (status, body) = post_json(&dir, "/api/recalibrate", json!({ "mode": "threshold" })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["written"], false);
    assert_eq!(body["label_count"], 0);
    assert_eq!(body["summary"], JsonValue::Null);
    assert_eq!(body["reason"], "need >= 2 labels, have 0");
}

#[tokio::test]
async fn api_onboard_and_recalibrate_match_static_contract_fixture() {
    let expected = contract_fixture("ui/onboard-recalibrate.json");
    let dir = tempfile::tempdir().expect("tempdir");
    write_gateway_config_from_fixture(&dir, &expected);

    let (status, state) = get_json(&dir, "/api/onboard").await;
    assert_eq!(
        status.as_u16(),
        expected["onboard_state"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(state, expected["onboard_state"]["body"]);

    let app = build_app_from_dir_with_invoker(dir.path(), |model, prompt| {
        Ok(format!("reply:{}:{prompt}", model.model))
    })
    .expect("app should build");
    let (status, run_body) = post_json_app(
        app,
        "/api/onboard/run",
        expected["onboard_run"]["request"].clone(),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        expected["onboard_run"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(run_body, expected["onboard_run"]["body"]);

    let (status, record_body) = post_json(
        &dir,
        "/api/onboard/record",
        expected["onboard_record"]["request"].clone(),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        expected["onboard_record"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(record_body, expected["onboard_record"]["body"]);

    let (status, dataset_body) = get_json(&dir, "/api/onboard/dataset").await;
    assert_eq!(
        status.as_u16(),
        expected["onboard_dataset"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(dataset_body, expected["onboard_dataset"]["body"]);

    write_feedback_log_from_fixture(&dir, &expected);
    let (status, recalibrate_body) = post_json(
        &dir,
        "/api/recalibrate",
        expected["recalibrate"]["request"].clone(),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        expected["recalibrate"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        recalibrate_body, expected["recalibrate"]["body"],
        "recalibrate response changed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("wayfinder-router.toml")).unwrap(),
        expected["recalibrate"]["written_config"].as_str().unwrap(),
        "recalibrate written config changed"
    );
}
