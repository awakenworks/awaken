//! Deterministic model adapter over the production split-Control composition.
//!
//! This module adds no alternate Control service. It loads the same typed
//! deployment as the product command and supplies only the existing model
//! publication SPI so a provider-free cluster can exercise the real
//! Control-to-Coordinator boundaries.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Json;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;

use crate::model_publication::{DistributedProviderPublicationResolver, scenario_model_catalog};

pub async fn build_distributed_control_router() -> Router {
    // The scenario process is a thin composition adapter, not a second config
    // owner. Its deployment fixture supplies the same explicit file used by the
    // production Control migration command; omission retains the normal default
    // path for focused unit tests.
    let config_path = std::env::var_os("AWAKEN_SCENARIO_CONFIG").map(std::path::PathBuf::from);
    let deployment =
        awaken_cli::config::ResolvedDeployment::load(awaken_cli::config::ConfigOverrides {
            config_path,
            ..Default::default()
        })
        .unwrap_or_else(|error| panic!("distributed Control deployment configuration: {error}"));
    assert_eq!(
        deployment.role,
        awaken_cli::config::Role::Control,
        "distributed Control scenario requires role = \"control\""
    );
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("distributed Control seal key: {error}"));
    let resolver = Arc::new(DistributedProviderPublicationResolver::new(
        scenario_model_catalog("adr71-echo").await,
    ));
    awaken_cli::build_control_router_with_publication_resolver(&deployment, &key, resolver)
        .await
        .unwrap_or_else(|error| panic!("assemble distributed Control scenario: {error}"))
}

/// Hermetic Anthropic Messages edge used by the distributed-role cluster proof.
/// It is a protocol fixture only: publication, credential selection/materialization,
/// Worker execution, and commit all remain on their production paths.
pub fn build_distributed_provider_router() -> Router {
    Router::new().route("/v1/messages", post(distributed_provider_message))
}

async fn distributed_provider_message(
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Response<Body> {
    if headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        != Some("adr71-provider-secret")
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "invalid key" }
            })),
        )
            .into_response();
    }
    let text = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .and_then(|message| message.get("content"))
        .map(message_text)
        .unwrap_or_default();
    if text.contains("ADR71-CHAOS-SLOW") {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let reply = format!("ADR71-PROVIDER:{text}");
    if request.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
        return anthropic_text_stream(&request, &reply);
    }
    Json(serde_json::json!({
        "id": "msg_adr71",
        "type": "message",
        "role": "assistant",
        "model": request.get("model").and_then(serde_json::Value::as_str).unwrap_or("adr71-echo"),
        "content": [{ "type": "text", "text": reply }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    }))
    .into_response()
}

fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type") == Some(&serde_json::Value::String("text".into())))
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn anthropic_text_stream(request: &serde_json::Value, reply: &str) -> Response<Body> {
    let frame =
        |event: &str, value: serde_json::Value| format!("event: {event}\ndata: {value}\n\n");
    let model = request
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("adr71-echo");
    let body = [
        frame(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_adr71", "type": "message", "role": "assistant",
                    "model": model, "content": [], "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 1, "output_tokens": 0 }
                }
            }),
        ),
        frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        frame(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": reply }
            }),
        ),
        frame(
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        ),
        frame(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 1 }
            }),
        ),
        frame(
            "message_stop",
            serde_json::json!({ "type": "message_stop" }),
        ),
    ]
    .concat();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .expect("static Anthropic response")
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;

    #[tokio::test]
    async fn distributed_provider_requires_the_projected_secret_and_speaks_streaming_messages() {
        // Cause/effect decision table: P1 absent/wrong projected key -> 401 and no
        // model response; P2 exact key + streaming Messages request -> one valid
        // Anthropic SSE terminal ladder carrying the input marker. This fixture
        // never substitutes for Credential or Runtime behavior.
        let app = build_distributed_provider_router();
        let denied = app
            .clone()
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "P1");

        let accepted = app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-api-key", "adr71-provider-secret")
                    .body(Body::from(
                        r#"{"model":"adr71-echo","stream":true,"messages":[{"role":"user","content":"marker"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK, "P2");
        let body = to_bytes(accepted.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("ADR71-PROVIDER:marker"), "P2: {body}");
        assert!(body.contains("event: message_stop"), "P2: {body}");
    }
}
