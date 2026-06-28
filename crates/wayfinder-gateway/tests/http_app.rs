use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::tempdir;
use tower::ServiceExt;
use wayfinder_internal_gateway::{build_app_from_dir, ServeOptions};

async fn get_json(path: &str) -> (StatusCode, Value) {
    let app = build_app_from_dir(ServeOptions::default(), std::env::current_dir().unwrap())
        .expect("app should build");
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

async fn get_text(path: &str) -> (StatusCode, String, String) {
    let app = build_app_from_dir(ServeOptions::default(), std::env::current_dir().unwrap())
        .expect("app should build");
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
        .get("content-type")
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

#[tokio::test]
async fn healthz_reports_ok_and_default_models() {
    let (status, body) = get_json("/healthz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["models"], serde_json::json!(["cloud", "local"]));
}

#[tokio::test]
async fn models_returns_openai_compatible_directives_and_routes() {
    let (status, body) = get_json("/v1/models").await;
    let (_, bare) = get_json("/models").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(bare, body);
    assert_eq!(body["object"], "list");
    let ids = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["auto", "prefer-local", "prefer-hosted", "local", "cloud"]
    );
}

#[tokio::test]
async fn metrics_returns_prometheus_text() {
    let (status, content_type, body) = get_text("/metrics").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/plain"));
    assert!(body.contains("# HELP wayfinder_router_build_info"));
    assert!(body.contains("wayfinder_router_recent_decisions_total 0"));
}

#[tokio::test]
async fn router_recent_empty_shape_is_metadata_only() {
    let (status, body) = get_json("/router/recent").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({
            "total": 0,
            "by_model": {},
            "recent": []
        })
    );
}

#[tokio::test]
async fn router_dashboard_and_demo_are_served_as_html() {
    let (router_status, router_content_type, router_body) = get_text("/router").await;
    let (demo_status, demo_content_type, demo_body) = get_text("/demo").await;

    assert_eq!(router_status, StatusCode::OK);
    assert!(router_content_type.starts_with("text/html"));
    assert!(router_body.contains("Wayfinder routing"));
    assert!(router_body.contains("/router/recent"));
    assert_eq!(demo_status, StatusCode::OK);
    assert!(demo_content_type.starts_with("text/html"));
    assert!(demo_body.contains("<title>Wayfinder</title>"));
}

#[tokio::test]
async fn configured_gateway_models_are_loaded_from_toml() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        r#"
[routing]
threshold = 0.4

[gateway.models.small]
base_url = "http://localhost:11434/v1"
model = "llama3.2"

[gateway.models.frontier]
base_url = "https://api.example.com/v1"
model = "frontier"
"#,
    )
    .unwrap();

    let app = build_app_from_dir(ServeOptions::default(), dir.path()).expect("app should build");
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let ids = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        ["auto", "prefer-local", "prefer-hosted", "frontier", "small"]
    );
}
