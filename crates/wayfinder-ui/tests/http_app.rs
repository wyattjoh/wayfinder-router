use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value as JsonValue};
use tempfile::TempDir;
use tower::ServiceExt;
use wayfinder_internal_core::complexity::FEATURE_ORDER;
use wayfinder_internal_ui::build_app_from_dir;

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
