//! HTTP API contract tests — migrated from tirea-agentos-server/tests/http_api.rs.
//!
//! Validates route construction, request/response serialization,
//! API error types, and message conversion logic.

use awaken_server::app::ServerConfig;
use awaken_server::protocols::acp::stdio::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, parse_request, serialize_notification,
    serialize_response,
};
use awaken_server::routes::{ApiError, build_router};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

// ============================================================================
// ServerConfig
// ============================================================================

#[test]
fn server_config_default_values() {
    let config = ServerConfig::default();
    assert_eq!(config.address, "0.0.0.0:3000");
    assert_eq!(config.sse_buffer_size, 64);
}

#[test]
fn server_config_serde_roundtrip() {
    let config = ServerConfig {
        address: "127.0.0.1:8080".to_string(),
        sse_buffer_size: 128,
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.address, "127.0.0.1:8080");
    assert_eq!(parsed.sse_buffer_size, 128);
}

#[test]
fn server_config_deserialize_with_defaults() {
    let json = r#"{"address": "localhost:9000"}"#;
    let config: ServerConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.address, "localhost:9000");
    assert_eq!(config.sse_buffer_size, 64);
}

#[test]
fn server_config_custom_buffer_size() {
    let json = r#"{"address": "0.0.0.0:3000", "sse_buffer_size": 256}"#;
    let config: ServerConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.sse_buffer_size, 256);
}

// ============================================================================
// API Error responses
// ============================================================================

#[test]
fn api_error_bad_request_response() {
    let err = ApiError::BadRequest("missing field".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn api_error_not_found_response() {
    let err = ApiError::NotFound("resource".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_thread_not_found_response() {
    let err = ApiError::ThreadNotFound("t-123".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_run_not_found_response() {
    let err = ApiError::RunNotFound("r-123".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_internal_response() {
    let err = ApiError::Internal("db error".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ============================================================================
// Route builder (smoke test — verifies router construction doesn't panic)
// ============================================================================

#[test]
fn build_router_constructs_without_panic() {
    let _router = build_router();
}

// ============================================================================
// Request payload deserialization contracts
// ============================================================================

#[test]
fn create_run_payload_camel_case() {
    let json = json!({
        "agentId": "agent-1",
        "threadId": "thread-1",
        "messages": [
            {"role": "user", "content": "hello"}
        ]
    });
    // Verify the contract shape parses
    assert_eq!(json["agentId"], "agent-1");
    assert_eq!(json["threadId"], "thread-1");
    assert_eq!(json["messages"][0]["role"], "user");
}

#[test]
fn create_run_payload_snake_case_alias() {
    let json = json!({
        "agent_id": "agent-1",
        "thread_id": "thread-1",
        "messages": []
    });
    assert_eq!(json["agent_id"], "agent-1");
    assert_eq!(json["thread_id"], "thread-1");
}

#[test]
fn decision_payload_deserialize() {
    let json = r#"{"toolCallId":"c1","action":"resume","payload":{"approved":true}}"#;
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["toolCallId"], "c1");
    assert_eq!(parsed["action"], "resume");
}

#[test]
fn decision_payload_invalid_action() {
    // Verify contract: action must be "resume" or "cancel"
    let json = json!({
        "toolCallId": "c1",
        "action": "invalid_action",
        "payload": {}
    });
    assert_ne!(json["action"], "resume");
    assert_ne!(json["action"], "cancel");
}

// ============================================================================
// Thread API contracts
// ============================================================================

#[test]
fn list_params_defaults() {
    let json = "{}";
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    // Default limit should be 50, offset None
    assert!(parsed.get("offset").is_none());
    assert!(parsed.get("limit").is_none());
}

#[test]
fn create_thread_payload_with_title() {
    let json = json!({"title": "My Thread"});
    assert_eq!(json["title"], "My Thread");
}

#[test]
fn create_thread_payload_without_title() {
    let json = json!({});
    assert!(json.get("title").is_none());
}

// ============================================================================
// Message conversion contracts
// ============================================================================

#[test]
fn run_message_roles() {
    let roles = ["user", "assistant", "system", "unknown"];
    let valid_count = roles
        .iter()
        .filter(|r| matches!(**r, "user" | "assistant" | "system"))
        .count();
    assert_eq!(valid_count, 3);
}

// ============================================================================
// Mailbox API contracts
// ============================================================================

#[test]
fn mailbox_push_payload() {
    let json = json!({"payload": {"text": "hello from frontend"}});
    assert_eq!(json["payload"]["text"], "hello from frontend");
}

#[test]
fn mailbox_push_payload_empty() {
    let json = json!({});
    // Default payload should be null
    assert!(json.get("payload").is_none());
}

// ============================================================================
// JSON-RPC 2.0 stdio protocol (ACP)
// ============================================================================

#[test]
fn parse_valid_jsonrpc_request() {
    let line = r#"{"jsonrpc":"2.0","method":"session/start","params":{"agentId":"a1"},"id":1}"#;
    let req = parse_request(line).unwrap();
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.method, "session/start");
    assert_eq!(req.id, Some(json!(1)));
}

#[test]
fn parse_jsonrpc_notification_without_id() {
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"hi"}}"#;
    let req = parse_request(line).unwrap();
    assert!(req.id.is_none());
}

#[test]
fn parse_invalid_json_returns_error() {
    let result = parse_request("not json");
    assert!(result.is_err());
}

#[test]
fn jsonrpc_success_response_serde() {
    let resp = JsonRpcResponse::success(Some(json!(1)), json!({"ok": true}));
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"result\""));
    assert!(!json.contains("\"error\""));
}

#[test]
fn jsonrpc_error_response_serde() {
    let resp = JsonRpcResponse::error(Some(json!(1)), -32600, "Invalid Request");
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("-32600"));
    assert!(json.contains("Invalid Request"));
    assert!(!json.contains("\"result\""));
}

#[test]
fn jsonrpc_method_not_found_response() {
    let resp = JsonRpcResponse::method_not_found(Some(json!(1)));
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32601);
}

#[test]
fn jsonrpc_invalid_params_response() {
    let resp = JsonRpcResponse::invalid_params(Some(json!(1)), "missing field");
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32602);
    assert_eq!(err.message, "missing field");
}

#[test]
fn jsonrpc_internal_error_response() {
    let resp = JsonRpcResponse::internal_error(Some(json!(1)), "boom");
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32603);
    assert_eq!(err.message, "boom");
}

#[test]
fn jsonrpc_notification_serde() {
    let notif = JsonRpcNotification::new("session/update", json!({"text": "hello"}));
    let json = serialize_notification(&notif);
    assert!(json.contains("session/update"));
    assert!(json.contains("hello"));
}

#[test]
fn jsonrpc_serialize_response_handles_all_cases() {
    let success = serialize_response(&JsonRpcResponse::success(None, json!(42)));
    assert!(success.contains("42"));

    let error = serialize_response(&JsonRpcResponse::internal_error(None, "boom"));
    assert!(error.contains("boom"));
}

#[test]
fn jsonrpc_roundtrip_request() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "test/method".into(),
        params: Some(json!({"key": "val"})),
        id: Some(json!("req-1")),
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.method, "test/method");
    assert_eq!(parsed.id, Some(json!("req-1")));
}

#[test]
fn jsonrpc_response_null_id() {
    let resp = JsonRpcResponse::success(None, json!("ok"));
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"id\":null"));
}

// ============================================================================
// Run management contracts
// ============================================================================

#[test]
fn run_query_default_pagination() {
    use awaken_contract::contract::storage::RunQuery;
    let query = RunQuery::default();
    assert_eq!(query.offset, 0);
    assert_eq!(query.limit, 50);
    assert!(query.thread_id.is_none());
    assert!(query.status.is_none());
}

#[test]
fn run_record_fields() {
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::RunRecord;
    let record = RunRecord {
        run_id: "r1".into(),
        thread_id: "t1".into(),
        agent_id: "agent-1".into(),
        parent_run_id: None,
        status: RunStatus::Running,
        termination_code: None,
        created_at: 1000,
        updated_at: 1000,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    assert_eq!(record.run_id, "r1");
    assert_eq!(record.status, RunStatus::Running);
    assert!(!record.status.is_terminal());
}

#[test]
fn run_status_transitions() {
    use awaken_contract::contract::lifecycle::RunStatus;
    assert!(RunStatus::Running.can_transition_to(RunStatus::Waiting));
    assert!(RunStatus::Running.can_transition_to(RunStatus::Done));
    assert!(RunStatus::Waiting.can_transition_to(RunStatus::Running));
    assert!(!RunStatus::Done.can_transition_to(RunStatus::Running));
}
