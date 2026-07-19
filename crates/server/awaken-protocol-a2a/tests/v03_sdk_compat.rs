//! Wire-contract checks mirrored from the official `@a2a-js/sdk` v0.3 client.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_protocol_a2a::router;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Runtime;

#[async_trait]
impl ProtocolRuntime for Runtime {
    async fn run(
        &self,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: Vec::new(),
            terminal: Terminal::Finished,
        })
    }
    async fn resume(&self, _: &str, _: &str, _: Resume) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: Vec::new(),
            terminal: Terminal::Finished,
        })
    }
    async fn pending(&self, _: &str) -> Option<Pending> {
        None
    }
    async fn history(&self, _: &str) -> Vec<Message> {
        Vec::new()
    }
    fn model(&self) -> String {
        "sdk-compat".into()
    }
}

async fn request(app: Router, method: &str, uri: &str, body: Value) -> (u16, String, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

fn send_params(context: &str) -> Value {
    json!({
        "message": {
            "kind": "message", "messageId": "m-1", "contextId": context,
            "role": "user", "parts": [{ "kind": "text", "text": "hello" }]
        }
    })
}

#[tokio::test]
async fn jsonrpc_stream_is_the_exact_sdk_envelope() {
    let app = router(Arc::new(Runtime));
    let (_, content_type, body) = request(app, "POST", "/v1/a2a", json!({
        "jsonrpc": "2.0", "id": 41, "method": "message/stream", "params": send_params("stream-context")
    })).await;
    assert!(content_type.starts_with("text/event-stream"));
    let frames = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .collect::<Vec<_>>();
    assert!(frames.len() >= 2, "{body}");
    assert!(
        frames
            .iter()
            .all(|frame| frame["jsonrpc"] == "2.0" && frame["id"] == 41)
    );
    assert_eq!(frames[0]["result"]["kind"], "task", "{body}");
    assert_eq!(
        frames.last().unwrap()["result"]["kind"],
        "status-update",
        "{body}"
    );
}

#[tokio::test]
async fn rest_stream_uses_the_stream_response_oneof_wrapper() {
    let app = router(Arc::new(Runtime));
    let (_, _, body) = request(
        app,
        "POST",
        "/v1/message:stream",
        send_params("rest-context"),
    )
    .await;
    let first = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let frame: Value = serde_json::from_str(first).unwrap();
    assert_eq!(frame["task"]["kind"], "task", "{frame}");
    assert!(frame.get("jsonrpc").is_none(), "{frame}");
}

#[tokio::test]
async fn sdk_push_config_set_get_list_delete_round_trip() {
    let app = router(Arc::new(Runtime));
    let (_, _, sent) = request(app.clone(), "POST", "/v1/a2a", json!({
        "jsonrpc": "2.0", "id": 1, "method": "message/send", "params": send_params("push-context")
    })).await;
    let sent: Value = serde_json::from_str(&sent).unwrap();
    let task_id = sent["result"]["id"].as_str().unwrap();
    let config = json!({
        "taskId": task_id,
        "pushNotificationConfig": {
            "id": "cfg-1", "url": "https://example.invalid/a2a-hook", "token": "secret",
            "authentication": { "schemes": ["Bearer"], "credentials": "credential" }
        }
    });
    let (_, _, set) = request(app.clone(), "POST", "/v1/a2a", json!({
        "jsonrpc": "2.0", "id": 2, "method": "tasks/pushNotificationConfig/set", "params": config
    })).await;
    let set: Value = serde_json::from_str(&set).unwrap();
    assert_eq!(
        set["result"]["pushNotificationConfig"]["authentication"]["schemes"][0],
        "Bearer"
    );
    assert!(
        set["result"]["pushNotificationConfig"]
            .get("taskId")
            .is_none()
    );
    assert!(
        set["result"]["pushNotificationConfig"]
            .get("token")
            .is_none(),
        "secrets are masked"
    );

    let (_, _, list) = request(app.clone(), "POST", "/v1/a2a", json!({
        "jsonrpc": "2.0", "id": 3, "method": "tasks/pushNotificationConfig/list", "params": { "id": task_id }
    })).await;
    let list: Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list["result"].as_array().unwrap().len(), 1);

    let (_, _, deleted) = request(
        app,
        "POST",
        "/v1/a2a",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tasks/pushNotificationConfig/delete",
            "params": { "id": task_id, "pushNotificationConfigId": "cfg-1" }
        }),
    )
    .await;
    let deleted: Value = serde_json::from_str(&deleted).unwrap();
    assert!(deleted["result"].is_null(), "{deleted}");
}
