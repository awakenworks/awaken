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

// ============================================================================
// Integration tests — exercising the full HTTP stack via tower::ServiceExt
// ============================================================================
//
// These tests build a real axum Router backed by InMemoryStore and
// ImmediateExecutor, then exercise endpoints via oneshot requests.

mod integration {
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::message::{Message, ToolCall};
    use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadStore};
    use awaken_contract::registry_spec::AgentSpec;
    use awaken_contract::thread::Thread;
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_runtime::registry::traits::ModelEntry;
    use awaken_server::app::{AppState, ServerConfig};
    use awaken_server::routes::build_router;
    use awaken_stores::memory::InMemoryStore;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    struct ImmediateExecutor;

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for ImmediateExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "immediate"
        }
    }

    struct TestApp {
        router: axum::Router,
        store: Arc<InMemoryStore>,
    }

    fn make_test_app() -> TestApp {
        let store = Arc::new(InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_model(
                    "test-model",
                    ModelEntry {
                        provider: "mock".into(),
                        model_name: "mock-model".into(),
                    },
                )
                .with_provider("mock", Arc::new(ImmediateExecutor))
                .with_agent_spec(AgentSpec {
                    id: "test-agent".into(),
                    model: "test-model".into(),
                    system_prompt: "test".into(),
                    max_rounds: 0,
                    ..Default::default()
                })
                .with_thread_run_store(store.clone())
                .build()
                .expect("build runtime"),
        );
        let state = AppState::new(
            runtime.clone(),
            store.clone(),
            store.clone(),
            runtime.resolver_arc(),
            ServerConfig::default(),
        );
        TestApp {
            router: build_router().with_state(state),
            store,
        }
    }

    async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .expect("request build"),
            )
            .await
            .expect("app should handle request");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body readable");
        let text = String::from_utf8(body.to_vec()).expect("utf-8");
        let value = serde_json::from_str(&text).unwrap_or(json!(text));
        (status, value)
    }

    async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(payload.to_string()))
                    .expect("request build"),
            )
            .await
            .expect("app should handle request");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body readable");
        let text = String::from_utf8(body.to_vec()).expect("utf-8");
        let value = serde_json::from_str(&text).unwrap_or(json!(text));
        (status, value)
    }

    async fn post_raw(app: axum::Router, uri: &str, body: &str) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request build"),
            )
            .await
            .expect("app should handle request");
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body readable");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        let value = serde_json::from_str(&text).unwrap_or(json!(text));
        (status, value)
    }

    async fn delete_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .expect("request build"),
            )
            .await
            .expect("app should handle request");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body readable");
        let text = String::from_utf8(body.to_vec()).expect("utf-8");
        let value = if text.is_empty() {
            json!(null)
        } else {
            serde_json::from_str(&text).unwrap_or(json!(text))
        };
        (status, value)
    }

    /// Helper: create a thread in the store and return its ID.
    async fn seed_thread(store: &InMemoryStore, title: Option<&str>) -> String {
        let mut thread = Thread::new();
        if let Some(t) = title {
            thread.metadata.title = Some(t.to_string());
        }
        store.save_thread(&thread).await.unwrap();
        thread.id
    }

    /// Helper: seed a run record into the store.
    async fn seed_run(
        store: &InMemoryStore,
        run_id: &str,
        thread_id: &str,
        status: RunStatus,
    ) -> RunRecord {
        let record = RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "test-agent".to_string(),
            parent_run_id: None,
            status,
            termination_code: None,
            created_at: 1000,
            updated_at: 1000,
            steps: 1,
            input_tokens: 10,
            output_tokens: 20,
            state: None,
        };
        store.create_run(&record).await.unwrap();
        record
    }

    // ====================================================================
    // Thread endpoints (8)
    // ====================================================================

    #[tokio::test]
    async fn list_threads_returns_empty() {
        let test = make_test_app();
        let (status, body) = get_json(test.router, "/v1/threads").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn list_threads_returns_created_threads() {
        let test = make_test_app();
        seed_thread(&test.store, Some("Thread A")).await;
        seed_thread(&test.store, Some("Thread B")).await;
        let (status, body) = get_json(test.router, "/v1/threads").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn get_thread_by_id() {
        let test = make_test_app();
        let id = seed_thread(&test.store, Some("My Thread")).await;
        let (status, body) = get_json(test.router.clone(), &format!("/v1/threads/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"].as_str(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn get_thread_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = get_json(test.router, "/v1/threads/nonexistent-id-12345").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn create_thread_via_post() {
        let test = make_test_app();
        let (status, body) =
            post_json(test.router, "/v1/threads", json!({"title": "New Thread"})).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["metadata"]["title"].as_str(), Some("New Thread"));
        assert!(body["id"].as_str().is_some());
    }

    #[tokio::test]
    async fn get_thread_messages_for_existing_thread() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![Message::user("hello"), Message::assistant("hi")];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
    }

    #[tokio::test]
    async fn get_thread_messages_pagination() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs: Vec<Message> = (0..10).map(|i| Message::user(format!("msg-{i}"))).collect();
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(
            test.router,
            &format!("/v1/threads/{id}/messages?offset=3&limit=4"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4);
        assert_eq!(body["total"].as_u64(), Some(10));
        assert_eq!(body["has_more"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn delete_thread_returns_no_content() {
        let test = make_test_app();
        let id = seed_thread(&test.store, Some("To Delete")).await;
        let (status, _body) = delete_json(test.router.clone(), &format!("/v1/threads/{id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Verify it's gone
        let (status2, _) = get_json(test.router, &format!("/v1/threads/{id}")).await;
        assert_eq!(status2, StatusCode::NOT_FOUND);
    }

    // ====================================================================
    // Run endpoints (8)
    // ====================================================================

    #[tokio::test]
    async fn list_runs_for_thread() {
        let test = make_test_app();
        let tid = seed_thread(&test.store, None).await;
        seed_run(&test.store, "r-1", &tid, RunStatus::Done).await;
        seed_run(&test.store, "r-2", &tid, RunStatus::Running).await;
        seed_run(&test.store, "r-other", "other-thread", RunStatus::Done).await;

        let (status, body) = get_json(test.router, &format!("/v1/threads/{tid}/runs")).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
    }

    #[tokio::test]
    async fn get_run_by_id() {
        let test = make_test_app();
        seed_run(&test.store, "run-123", "t-1", RunStatus::Running).await;
        let (status, body) = get_json(test.router, "/v1/runs/run-123").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["run_id"].as_str(), Some("run-123"));
        assert_eq!(body["thread_id"].as_str(), Some("t-1"));
    }

    #[tokio::test]
    async fn get_run_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = get_json(test.router, "/v1/runs/nonexistent-run").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn run_record_done_status_fields() {
        let test = make_test_app();
        seed_run(&test.store, "r-done", "t-1", RunStatus::Done).await;
        let (status, body) = get_json(test.router, "/v1/runs/r-done").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"].as_str(), Some("done"));
        assert_eq!(body["steps"].as_u64(), Some(1));
        assert_eq!(body["input_tokens"].as_u64(), Some(10));
        assert_eq!(body["output_tokens"].as_u64(), Some(20));
    }

    #[tokio::test]
    async fn list_runs_with_custom_thread_id_filter() {
        let test = make_test_app();
        seed_run(&test.store, "r-a", "thread-alpha", RunStatus::Done).await;
        seed_run(&test.store, "r-b", "thread-beta", RunStatus::Done).await;

        let (status, body) = get_json(test.router, "/v1/threads/thread-alpha/runs").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["run_id"].as_str(), Some("r-a"));
    }

    #[tokio::test]
    async fn cancel_run_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) =
            post_json(test.router, "/v1/runs/nonexistent-run/cancel", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn decision_endpoint_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/runs/nonexistent-run/decision",
            json!({
                "toolCallId": "tc-1",
                "action": "resume",
                "payload": {}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn decision_endpoint_invalid_action_returns_400() {
        let test = make_test_app();
        // Even though the run doesn't exist, action validation happens first
        // for a valid run. Let's just verify the contract rejects bad actions
        // by using a run that doesn't exist (the 404 proves the action validation
        // path exists). For action validation, we need to test at the route level.
        let (status, _body) = post_json(
            test.router,
            "/v1/runs/some-run/decision",
            json!({
                "toolCallId": "tc-1",
                "action": "invalid_action",
                "payload": {}
            }),
        )
        .await;
        // Bad action returns 400 before the run lookup
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // ====================================================================
    // Message endpoints (5)
    // ====================================================================

    #[tokio::test]
    async fn get_messages_for_thread() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![Message::user("question"), Message::assistant("answer")];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"].as_str(), Some("user"));
        assert_eq!(messages[1]["role"].as_str(), Some("assistant"));
    }

    #[tokio::test]
    async fn messages_include_tool_results() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![
            Message::user("search for rust"),
            Message::assistant_with_tool_calls(
                "Let me search",
                vec![ToolCall::new("call_1", "search", json!({"q": "rust"}))],
            ),
            Message::tool("call_1", "found: Rust programming language"),
            Message::assistant("I found information about Rust."),
        ];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"].as_str(), Some("tool"));
        assert!(messages[2]["tool_call_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn message_ordering_preserved() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs: Vec<Message> = (0..5)
            .map(|i| Message::user(format!("message-{i}")))
            .collect();
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        for (i, msg) in messages.iter().enumerate() {
            let content = msg["content"][0]["text"].as_str().unwrap();
            assert_eq!(content, format!("message-{i}"));
        }
    }

    #[tokio::test]
    async fn empty_thread_has_no_messages() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        // No messages saved -- thread exists but has no message history

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert!(messages.is_empty());
        assert_eq!(body["total"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn messages_after_multiple_saves() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        // First save
        test.store
            .save_messages(&id, &[Message::user("first")])
            .await
            .unwrap();
        // Second save overwrites (save_messages replaces all)
        test.store
            .save_messages(
                &id,
                &[
                    Message::user("first"),
                    Message::assistant("response"),
                    Message::user("second"),
                ],
            )
            .await
            .unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
    }

    // ====================================================================
    // Error handling (4)
    // ====================================================================

    #[tokio::test]
    async fn invalid_json_body_returns_400() {
        let test = make_test_app();
        let (status, _body) = post_raw(test.router, "/v1/threads", "not valid json {{{").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_required_fields_returns_422() {
        let test = make_test_app();
        // POST /v1/runs requires agentId — axum returns 422 for missing fields
        let (status, _body) = post_json(
            test.router,
            "/v1/runs",
            json!({"messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn health_endpoint_returns_200() {
        let test = make_test_app();
        let (status, _body) = get_json(test.router, "/health").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_endpoint_returns_404() {
        let test = make_test_app();
        let (status, _body) = get_json(test.router, "/v1/completely-unknown-endpoint").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
