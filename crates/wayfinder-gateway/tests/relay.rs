use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;
use wayfinder_internal_gateway::{
    invoke_messages, stream_messages, GatewayModel, RelayMessage, UpstreamError,
};

#[derive(Clone)]
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
                "error": {"message": "upstream unavailable", "type": "upstream_error"}
            })),
        )
            .into_response();
    }
    if state.stream {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n\
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
            "message": {"role": "assistant", "content": "hello from upstream"},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

fn model(base_url: &str, api_key_env: Option<&str>) -> GatewayModel {
    GatewayModel {
        base_url: base_url.to_owned(),
        model: "upstream-model".to_owned(),
        api_key_env: api_key_env.map(str::to_owned),
        api_key_cmd: None,
        cost_per_1k: None,
        fallbacks: Vec::new(),
        context_window: None,
    }
}

#[tokio::test]
async fn invoke_messages_returns_assembled_reply_with_auth() {
    let upstream = FakeUpstream::start(StatusCode::OK, false).await;
    let target = model(&upstream.base_url, Some("WAYFINDER_RELAY_INVOKE_KEY"));
    std::env::set_var("WAYFINDER_RELAY_INVOKE_KEY", "secret-key");
    let messages = vec![
        RelayMessage::new("system", "be brief"),
        RelayMessage::new("user", "hi"),
    ];

    let reply = tokio::task::spawn_blocking(move || {
        invoke_messages(&target, &messages, Duration::from_secs(5))
    })
    .await
    .unwrap()
    .expect("relay should return a reply");

    assert_eq!(reply, "hello from upstream");
    let calls = upstream.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["model"], "upstream-model");
    assert_eq!(calls[0].body["messages"][1]["content"], "hi");
    assert_eq!(calls[0].body.get("stream"), None);
    assert_eq!(calls[0].authorization.as_deref(), Some("Bearer secret-key"));
}

#[tokio::test]
async fn stream_messages_yields_deltas_that_assemble_the_reply() {
    let upstream = FakeUpstream::start(StatusCode::OK, true).await;
    let target = model(&upstream.base_url, None);
    let messages = vec![RelayMessage::new("user", "hi")];

    let deltas = tokio::task::spawn_blocking(move || {
        stream_messages(&target, &messages, Duration::from_secs(5))
            .collect::<Result<Vec<String>, UpstreamError>>()
    })
    .await
    .unwrap()
    .expect("stream should succeed");

    assert_eq!(deltas.concat(), "hello");
    assert!(!deltas.is_empty());
    let calls = upstream.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["stream"], true);
}

#[tokio::test]
async fn invoke_messages_surfaces_upstream_status_errors() {
    let upstream = FakeUpstream::start(StatusCode::SERVICE_UNAVAILABLE, false).await;
    let target = model(&upstream.base_url, None);
    let messages = vec![RelayMessage::new("user", "hi")];

    let error = tokio::task::spawn_blocking(move || {
        invoke_messages(&target, &messages, Duration::from_secs(5))
    })
    .await
    .unwrap()
    .expect_err("a 503 upstream should be an error");

    match error {
        UpstreamError::Status { status, .. } => assert_eq!(status, 503),
        other => panic!("expected a status error, got {other:?}"),
    }
}

#[tokio::test]
async fn invoke_messages_surfaces_transport_errors() {
    // Port 1 never accepts connections, so the relay reports a transport failure.
    let target = model("http://127.0.0.1:1", None);
    let messages = vec![RelayMessage::new("user", "hi")];

    let error = tokio::task::spawn_blocking(move || {
        invoke_messages(&target, &messages, Duration::from_secs(2))
    })
    .await
    .unwrap()
    .expect_err("an unreachable upstream should be an error");

    assert!(matches!(error, UpstreamError::Transport(_)));
}
