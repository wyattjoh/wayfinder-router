use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::Value;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};
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

async fn post_chat(
    dir: &std::path::Path,
    options: ServeOptions,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let app = build_app_from_dir(options, dir).expect("app should build");
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
}

#[derive(Clone, Default)]
struct UpstreamState {
    requests: Arc<Mutex<Vec<UpstreamRequest>>>,
    status: StatusCode,
    stream: bool,
}

#[derive(Clone, Debug)]
struct UpstreamRequest {
    body: Value,
    authorization: Option<String>,
}

struct FakeUpstream {
    base_url: String,
    requests: Arc<Mutex<Vec<UpstreamRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    async fn start(status: StatusCode, stream: bool) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = UpstreamState {
            requests: requests.clone(),
            status,
            stream,
        };
        let app = Router::new()
            .route("/chat/completions", post(fake_chat_completion))
            .with_state(state);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            task,
        }
    }

    fn calls(&self) -> Vec<UpstreamRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Default)]
struct HangingStreamState {
    requests: Arc<Mutex<Vec<UpstreamRequest>>>,
    sender: Arc<Mutex<Option<mpsc::Sender<Result<String, Infallible>>>>>,
    notify_sender: Arc<Notify>,
}

struct HangingStreamUpstream {
    base_url: String,
    requests: Arc<Mutex<Vec<UpstreamRequest>>>,
    sender: Arc<Mutex<Option<mpsc::Sender<Result<String, Infallible>>>>>,
    notify_sender: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl HangingStreamUpstream {
    async fn start() -> Self {
        let state = HangingStreamState::default();
        let requests = state.requests.clone();
        let sender = state.sender.clone();
        let notify_sender = state.notify_sender.clone();
        let app = Router::new()
            .route("/chat/completions", post(hanging_stream_chat_completion))
            .with_state(state);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            sender,
            notify_sender,
            task,
        }
    }

    async fn wait_for_stream(&self) {
        loop {
            if self.sender.lock().unwrap().is_some() {
                return;
            }
            self.notify_sender.notified().await;
        }
    }

    async fn send_chunk(&self, chunk: &str) {
        self.wait_for_stream().await;
        let sender = self.sender.lock().unwrap().clone().unwrap();
        sender.send(Ok(chunk.to_owned())).await.unwrap();
    }

    fn calls(&self) -> Vec<UpstreamRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for HangingStreamUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_chat_completion(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.requests.lock().unwrap().push(UpstreamRequest {
        body: body.clone(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    if state.status != StatusCode::OK {
        return (
            state.status,
            Json(serde_json::json!({
                "error": {
                    "message": "upstream unavailable",
                    "type": "upstream_error"
                }
            })),
        )
            .into_response();
    }
    if state.stream {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
data: [DONE]\n\n",
        )
            .into_response();
    }
    Json(serde_json::json!({
        "id": "upstream-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "hello from upstream"
            },
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

async fn hanging_stream_chat_completion(
    State(state): State<HangingStreamState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.requests.lock().unwrap().push(UpstreamRequest {
        body,
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    let (sender, receiver) = mpsc::channel::<Result<String, Infallible>>(8);
    *state.sender.lock().unwrap() = Some(sender);
    state.notify_sender.notify_waiters();
    let body = Body::from_stream(stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|item| (item, receiver))
    }));
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
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

#[tokio::test]
async fn chat_completions_dry_run_returns_decision_headers_and_debug_payload() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        r#"
[routing]
threshold = 0.5
"#,
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions {
            dry_run: true,
            ..ServeOptions::default()
        },
        "/v1/chat/completions",
        &[("X-Wayfinder-Debug", "true")],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(headers["x-wayfinder-router-mode"], "scored");
    assert_eq!(headers["x-wayfinder-router-score"], "0.00");
    assert!(headers.contains_key("x-wayfinder-router-request-id"));
    assert_eq!(body["wayfinder"]["model"], "local");
    assert_eq!(body["wayfinder"]["dry_run"], true);
    assert_eq!(body["wayfinder"]["features"]["word_count"], 2);
}

#[tokio::test]
async fn chat_completions_scores_and_forwards_to_configured_upstream_model_with_auth() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let dir = tempdir().unwrap();
    std::env::set_var("WAYFINDER_TEST_CLOUD_KEY", "secret-test-key");
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.0

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"

[gateway.models.cloud]
base_url = "{base_url}"
model = "cloud-upstream"
api_key_env = "WAYFINDER_TEST_CLOUD_KEY"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[("X-Wayfinder-Debug", "true")],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "cloud");
    assert_eq!(headers["x-wayfinder-router-served-by"], "cloud");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from upstream"
    );
    assert_eq!(body["wayfinder"]["model"], "cloud");
    assert_eq!(body["wayfinder"]["mode"], "scored");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["model"], "cloud-upstream");
    assert_eq!(calls[0].body["messages"][0]["content"], "Say hello.");
    assert_eq!(
        calls[0].authorization.as_deref(),
        Some("Bearer secret-test-key")
    );
}

#[tokio::test]
async fn chat_completions_pins_exact_model_and_bare_path() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.0

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"

[gateway.models.cloud]
base_url = "{base_url}"
model = "cloud-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, _) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/chat/completions",
        &[],
        serde_json::json!({
            "model": "local",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(headers["x-wayfinder-router-mode"], "pinned");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["model"], "local-upstream");
}

#[tokio::test]
async fn chat_completions_threshold_override_can_reselect_binary_route() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 1.0

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"

[gateway.models.cloud]
base_url = "{base_url}"
model = "cloud-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, _) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[("X-Wayfinder-Threshold", "0.0")],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "cloud");
    assert_eq!(headers["x-wayfinder-router-mode"], "threshold-override");
    assert_eq!(calls[0].body["model"], "cloud-upstream");
}

#[tokio::test]
async fn chat_completions_prefer_cloud_alias_pins_highest_tier() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[[routing.tiers]]
min_score = 0.0
model = "small"

[[routing.tiers]]
min_score = 0.5
model = "large"

[gateway.models.small]
base_url = "{base_url}"
model = "small-upstream"

[gateway.models.large]
base_url = "{base_url}"
model = "large-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, _) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "prefer-cloud",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "large");
    assert_eq!(headers["x-wayfinder-router-mode"], "pinned");
    assert_eq!(calls[0].body["model"], "large-upstream");
}

#[tokio::test]
async fn chat_completions_reports_missing_model_config() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        r#"
[routing]
threshold = 0.0

[gateway.models.local]
base_url = "http://127.0.0.1:9"
model = "local-upstream"
"#,
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers["x-wayfinder-router-model"], "cloud");
    assert_eq!(body["error"]["type"], "wayfinder_router_misconfigured");
}

#[tokio::test]
async fn chat_completions_shapes_upstream_errors() {
    let upstream = FakeUpstream::start(StatusCode::SERVICE_UNAVAILABLE, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(body["error"]["type"], "wayfinder_router_upstream_error");
}

#[tokio::test]
async fn chat_completions_passes_through_upstream_client_errors() {
    let upstream = FakeUpstream::start(StatusCode::BAD_REQUEST, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": false
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(body["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn chat_completions_passes_through_streaming_upstream_client_errors() {
    let upstream = FakeUpstream::start(StatusCode::UNAUTHORIZED, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": true
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(body["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn chat_completions_relays_streaming_sse() {
    let upstream = FakeUpstream::start(StatusCode::OK, true).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/chat/completions",
        &[],
        serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": true
        }),
    )
    .await;
    let body = String::from_utf8(body).unwrap();
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert_eq!(headers["x-wayfinder-router-served-by"], "local");
    assert!(body.contains("data: [DONE]"));
    assert!(body.contains("\"content\":\"hel\""));
    assert_eq!(calls[0].body["stream"], true);
    assert_eq!(calls[0].body["model"], "local-upstream");
}

#[tokio::test]
async fn anthropic_messages_translates_request_and_forwards_through_chat_route() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.0

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"

[gateway.models.cloud]
base_url = "{base_url}"
model = "cloud-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/messages",
        &[],
        serde_json::json!({
            "model": "claude-3-5-haiku-latest",
            "system": [{"type": "text", "text": "Use short answers."}],
            "messages": [
                {"role": "user", "content": "Plan a route."},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "I will check."},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"city": "Calgary"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "clear"}]},
                    {"type": "text", "text": "Continue."}
                ]}
            ],
            "max_tokens": 128,
            "temperature": 0.2,
            "top_p": 0.9,
            "stop_sequences": ["END"],
            "tools": [{
                "name": "lookup",
                "description": "Find city data",
                "input_schema": {"type": "object"}
            }],
            "tool_choice": {"type": "tool", "name": "lookup"}
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "cloud");
    assert_eq!(headers["x-wayfinder-router-served-by"], "cloud");
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["model"], "claude-3-5-haiku-latest");
    assert_eq!(body["content"][0]["text"], "hello from upstream");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["model"], "cloud-upstream");
    assert_eq!(
        calls[0].body["messages"],
        serde_json::json!([
            {"role": "system", "content": "Use short answers."},
            {"role": "user", "content": "Plan a route."},
            {
                "role": "assistant",
                "content": "I will check.",
                "tool_calls": [{
                    "id": "toolu_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"city\":\"Calgary\"}"}
                }]
            },
            {"role": "tool", "tool_call_id": "toolu_1", "content": "clear"},
            {"role": "user", "content": "Continue."}
        ])
    );
    assert_eq!(calls[0].body["max_tokens"], 128);
    assert_eq!(calls[0].body["temperature"], 0.2);
    assert_eq!(calls[0].body["top_p"], 0.9);
    assert_eq!(calls[0].body["stop"], serde_json::json!(["END"]));
    assert_eq!(calls[0].body["tools"][0]["function"]["name"], "lookup");
    assert_eq!(
        calls[0].body["tool_choice"],
        serde_json::json!({"type": "function", "function": {"name": "lookup"}})
    );
}

#[tokio::test]
async fn anthropic_messages_bare_path_dry_run_delegates_to_router() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        r#"
[routing]
threshold = 0.5
"#,
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions {
            dry_run: true,
            ..ServeOptions::default()
        },
        "/messages",
        &[],
        serde_json::json!({
            "model": "claude-3-5-haiku-latest",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hello."}]}],
            "stream": true
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(headers["x-wayfinder-router-mode"], "scored");
    assert_eq!(body["wayfinder"]["dry_run"], true);
    assert_eq!(body["wayfinder"]["features"]["word_count"], 2);
}

#[tokio::test]
async fn anthropic_messages_shapes_upstream_client_errors() {
    let upstream = FakeUpstream::start(StatusCode::BAD_REQUEST, false).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/messages",
        &[],
        serde_json::json!({
            "model": "claude-3-5-haiku-latest",
            "messages": [{"role": "user", "content": "Say hello."}]
        }),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "upstream unavailable");
}

#[tokio::test]
async fn anthropic_messages_translates_streaming_text_sequence() {
    let upstream = FakeUpstream::start(StatusCode::OK, true).await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();

    let (status, headers, body) = post_chat(
        dir.path(),
        ServeOptions::default(),
        "/v1/messages",
        &[],
        serde_json::json!({
            "model": "claude-3-5-haiku-latest",
            "messages": [{"role": "user", "content": "Say hello."}],
            "stream": true
        }),
    )
    .await;
    let body = String::from_utf8(body).unwrap();
    let calls = upstream.calls();

    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert_eq!(headers["x-wayfinder-router-model"], "local");
    assert_eq!(calls[0].body["stream"], true);
    assert_eq!(calls[0].body["model"], "local-upstream");
    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: content_block_start"));
    assert!(body.contains("\"type\":\"text_delta\""));
    assert!(body.contains("\"text\":\"hel\""));
    assert!(body.contains("\"text\":\"lo\""));
    assert!(body.contains("event: content_block_stop"));
    assert!(body.contains("event: message_delta"));
    assert!(body.contains("event: message_stop"));
}

#[tokio::test]
async fn anthropic_messages_streaming_starts_before_upstream_finishes() {
    let upstream = HangingStreamUpstream::start().await;
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("wayfinder-router.toml"),
        format!(
            r#"
[routing]
threshold = 0.5

[gateway.models.local]
base_url = "{base_url}"
model = "local-upstream"
"#,
            base_url = upstream.base_url
        ),
    )
    .unwrap();
    let app = build_app_from_dir(ServeOptions::default(), dir.path()).expect("app should build");
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({
                "model": "claude-3-5-haiku-latest",
                "messages": [{"role": "user", "content": "Say hello."}],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();
    let response_task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });

    upstream.wait_for_stream().await;
    let mut response = tokio::time::timeout(Duration::from_millis(250), response_task)
        .await
        .expect("streaming response should start before upstream EOF")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));

    let start = tokio::time::timeout(Duration::from_millis(250), response.body_mut().frame())
        .await
        .expect("message_start should be emitted immediately")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let start = String::from_utf8(start.to_vec()).unwrap();
    assert!(start.contains("event: message_start"));

    upstream
        .send_chunk("data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n")
        .await;
    let mut delta = String::new();
    for _ in 0..2 {
        let frame = tokio::time::timeout(Duration::from_millis(250), response.body_mut().frame())
            .await
            .expect("text delta should arrive while upstream remains open")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        delta.push_str(&String::from_utf8(frame.to_vec()).unwrap());
        if delta.contains("event: content_block_delta") && delta.contains("\"text\":\"hel\"") {
            break;
        }
    }
    assert!(delta.contains("event: content_block_delta"));
    assert!(delta.contains("\"text\":\"hel\""));
    assert_eq!(upstream.calls()[0].body["stream"], true);
}
