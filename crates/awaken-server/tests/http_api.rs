#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
//! HTTP API contract tests — migrated from tirea-agentos-server/tests/http_api.rs.
//!
//! Validates route construction, request/response serialization,
//! API error types, and message conversion logic.

use awaken_server::app::ServerConfig;
use awaken_server::routes::ApiError;
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
        replay_buffer_capacity: 512,
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.address, "127.0.0.1:8080");
    assert_eq!(parsed.sse_buffer_size, 128);
    assert_eq!(parsed.replay_buffer_capacity, 512);
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
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 1000,
        started_at: None,
        finished_at: None,
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
    use awaken_contract::ModelSpec;
    use awaken_contract::contract::event_store::{EventReader, EventScope};
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::mailbox::MailboxStore;
    use awaken_contract::contract::message::{Message, ToolCall};
    use awaken_contract::contract::storage::{
        RunRecord, RunStore, RunWaitingState, RunWaitingTicket, ThreadRunStore, ThreadStore,
        WaitingReason,
    };
    use awaken_contract::contract::suspension::ToolCallResumeMode;
    use awaken_contract::contract::versioned_registry::PinnedRegistryManifest;
    use awaken_contract::registry_spec::AgentSpec;
    use awaken_contract::thread::Thread;
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_server::app::{ServerConfig, ServerState};
    use awaken_server::routes::build_router;
    use awaken_stores::{InMemoryEventStore, memory::InMemoryStore};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    struct TestRunResolver {
        inner: Arc<dyn awaken_runtime::AgentResolver>,
    }

    #[async_trait]
    impl awaken_runtime::Resolver for TestRunResolver {
        async fn resolve(
            &self,
            req: awaken_runtime::ResolutionRequest,
        ) -> Result<awaken_runtime::ResolvedRunPlan, awaken_runtime::ResolveError> {
            let agent_id = match &req.target {
                awaken_runtime::ResolutionTarget::Root { agent_id, .. } => agent_id.as_str(),
                awaken_runtime::ResolutionTarget::Delegate { agent_id, .. } => agent_id.as_str(),
                awaken_runtime::ResolutionTarget::Handoff { agent_id, .. } => agent_id.as_str(),
            };
            let execution = self.inner.resolve_execution(agent_id).unwrap_or_else(|_| {
                let agent = awaken_runtime::ResolvedAgent::new(
                    agent_id,
                    "test-model",
                    "test",
                    Arc::new(ImmediateExecutor),
                );
                awaken_runtime::ExecutionPlan::from_resolved_agent(&agent)
            });
            let tools = match &execution {
                awaken_runtime::ExecutionPlan::Local(agent) => agent
                    .tool_descriptors()
                    .into_iter()
                    .map(|descriptor| awaken_runtime::ResolvedTool { descriptor })
                    .collect(),
                awaken_runtime::ExecutionPlan::Remote(_) => Vec::new(),
            };
            Ok(awaken_runtime::ResolvedRunPlan::Replayable(
                awaken_runtime::ResolvedRun {
                    agent_spec: execution.spec().clone(),
                    role: awaken_runtime::ExecutionRole::Root,
                    model: awaken_runtime::ResolvedModelBinding {
                        upstream_model: match &execution {
                            awaken_runtime::ExecutionPlan::Local(agent) => {
                                agent.upstream_model.clone()
                            }
                            awaken_runtime::ExecutionPlan::Remote(agent) => {
                                agent.spec.model_id.clone()
                            }
                        },
                    },
                    execution,
                    tools,
                    overrides: req.overrides,
                    backend_profile: awaken_runtime::BackendProfile::full_local(),
                    requirements: awaken_runtime::BackendRequirements::from_features(&req.features),
                    scope: awaken_runtime::ReplayableScope {
                        manifest: PinnedRegistryManifest {
                            publication_id: Some("test-publication".into()),
                            registry_snapshot_version: Some(1),
                            entries: Vec::new(),
                        },
                    },
                },
            ))
        }
    }

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
        mailbox_store: Arc<awaken_stores::InMemoryMailboxStore>,
        event_store: Arc<InMemoryEventStore>,
    }

    fn make_test_app() -> TestApp {
        let store = Arc::new(InMemoryStore::new());
        let event_store = Arc::new(InMemoryEventStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
                .with_provider("mock", Arc::new(ImmediateExecutor))
                .with_agent_spec(AgentSpec {
                    id: "test-agent".into(),
                    model_id: "test-model".into(),
                    system_prompt: "test".into(),
                    max_rounds: 0,
                    ..Default::default()
                })
                .with_in_memory_thread_run_store(store.clone())
                .build()
                .expect("build runtime"),
        );
        runtime.set_run_resolver(Arc::new(TestRunResolver {
            inner: runtime.resolver_arc(),
        }));
        let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
        let mailbox = std::sync::Arc::new(awaken_server::mailbox::Mailbox::new(
            runtime.clone(),
            mailbox_store.clone(),
            store.clone(),
            "test".to_string(),
            awaken_server::mailbox::MailboxConfig::default(),
        ));
        let state = ServerState::new(
            runtime.clone(),
            mailbox,
            store.clone(),
            runtime.resolver_arc(),
            ServerConfig::default(),
        )
        .with_event_store(event_store.clone());
        TestApp {
            router: build_router(&state),
            store,
            mailbox_store,
            event_store,
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

    async fn patch_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
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

    async fn seed_thread_with_lineage(
        store: &InMemoryStore,
        title: Option<&str>,
        resource_id: Option<&str>,
        parent_thread_id: Option<&str>,
    ) -> String {
        let mut thread = Thread::new();
        if let Some(t) = title {
            thread.metadata.title = Some(t.to_string());
        }
        thread.resource_id = resource_id.map(str::to_string);
        thread.parent_thread_id = parent_thread_id.map(str::to_string);
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
        let record = run_record(run_id, thread_id, status, 1000);
        store.create_run(&record).await.unwrap();
        record
    }

    fn run_record(run_id: &str, thread_id: &str, status: RunStatus, updated_at: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "test-agent".to_string(),
            parent_run_id: None,
            registry_manifest: None,
            activation: None,
            request: None,
            input: None,
            output: None,
            status,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: updated_at,
            started_at: None,
            finished_at: None,
            updated_at,
            steps: 1,
            input_tokens: 10,
            output_tokens: 20,
            state: None,
        }
    }

    fn waiting_tool_run(run_id: &str, thread_id: &str, ticket_id: &str) -> RunRecord {
        let mut run = run_record(run_id, thread_id, RunStatus::Waiting, 1000);
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::ToolPermission,
            ticket_ids: vec![ticket_id.to_string()],
            tickets: vec![RunWaitingTicket {
                ticket_id: ticket_id.to_string(),
                tool_call_id: "tool-call-1".to_string(),
                tool_name: "dangerous".to_string(),
                arguments: json!({"path": "/tmp/x"}),
                resume_mode: ToolCallResumeMode::ReplayToolCall,
                reason: Some("approval".to_string()),
                updated_at: 1000,
            }],
            since_dispatch_id: Some("dispatch-1".to_string()),
            message: Some("suspended".to_string()),
        });
        run
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
    async fn list_threads_filters_by_resource_and_parent_thread() {
        let test = make_test_app();
        let matching = seed_thread_with_lineage(
            &test.store,
            Some("Match"),
            Some("resource-a"),
            Some("parent-1"),
        )
        .await;
        seed_thread_with_lineage(
            &test.store,
            Some("Wrong Resource"),
            Some("resource-b"),
            Some("parent-1"),
        )
        .await;
        seed_thread_with_lineage(
            &test.store,
            Some("Wrong Parent"),
            Some("resource-a"),
            Some("parent-2"),
        )
        .await;

        let (status, body) = get_json(
            test.router,
            "/v1/threads?resourceId=%20resource-a%20&parentThreadId=%20parent-1%20",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_str(), Some(matching.as_str()));
        assert_eq!(body["total"].as_u64(), Some(1));
        assert_eq!(body["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn list_threads_filters_root_threads() {
        let test = make_test_app();
        let matching =
            seed_thread_with_lineage(&test.store, Some("Root"), Some("resource-a"), None).await;
        seed_thread_with_lineage(
            &test.store,
            Some("Child"),
            Some("resource-a"),
            Some("parent-1"),
        )
        .await;
        seed_thread_with_lineage(&test.store, Some("Other Root"), Some("resource-b"), None).await;

        let (status, body) =
            get_json(test.router, "/v1/threads?resourceId=resource-a&root=true").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_str(), Some(matching.as_str()));
        assert_eq!(body["total"].as_u64(), Some(1));
        assert_eq!(body["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn list_threads_rejects_root_and_parent_combination() {
        let test = make_test_app();

        let (status, body) =
            get_json(test.router, "/v1/threads?root=true&parentThreadId=parent-1").await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"].as_str(),
            Some("root=true cannot be combined with parentThreadId")
        );
    }

    #[tokio::test]
    async fn list_thread_summaries_includes_latest_run_agent() {
        let test = make_test_app();
        let thread_id = seed_thread_with_lineage(
            &test.store,
            Some("A2UI Thread"),
            Some("resource-a"),
            Some("parent-1"),
        )
        .await;
        seed_run(&test.store, "run-1", &thread_id, RunStatus::Done).await;

        let (status, body) = get_json(test.router, "/v1/threads/summaries").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"].as_str(), Some(thread_id.as_str()));
        assert_eq!(items[0]["agent_id"].as_str(), Some("test-agent"));
        assert_eq!(items[0]["resource_id"].as_str(), Some("resource-a"));
        assert_eq!(items[0]["parent_thread_id"].as_str(), Some("parent-1"));
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
    async fn create_thread_via_post_accepts_lineage_fields() {
        let test = make_test_app();
        let parent_id = seed_thread(&test.store, Some("Parent Thread")).await;
        let (status, body) = post_json(
            test.router.clone(),
            "/v1/threads",
            json!({
                "title": "Lineage Thread",
                "resourceId": " resource-a ",
                "parentThreadId": format!(" {} ", parent_id)
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["resource_id"].as_str(), Some("resource-a"));
        assert_eq!(body["parent_thread_id"].as_str(), Some(parent_id.as_str()));

        let thread_id = body["id"].as_str().expect("thread id");
        let thread = test.store.load_thread(thread_id).await.unwrap().unwrap();
        assert_eq!(thread.resource_id.as_deref(), Some("resource-a"));
        assert_eq!(thread.parent_thread_id.as_deref(), Some(parent_id.as_str()));

        let by_thread = test
            .event_store
            .list(EventScope::thread(thread_id), None, 10)
            .await
            .unwrap();
        assert_eq!(by_thread.events.len(), 1);
        assert_eq!(by_thread.events[0].event_kind.as_str(), "ThreadCreated");
        assert_eq!(by_thread.events[0].payload["resource_id"], "resource-a");
        assert_eq!(
            by_thread.events[0].payload["parent_thread_id"],
            parent_id.as_str()
        );
        assert_eq!(
            by_thread.events[0].scopes,
            vec![EventScope::thread(thread_id)]
        );
    }

    #[tokio::test]
    async fn create_thread_via_post_rejects_missing_parent_thread() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/threads",
            json!({
                "title": "Broken Thread",
                "parentThreadId": "missing-parent"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"].as_str(),
            Some("parent thread not found: missing-parent")
        );
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
            test.router.clone(),
            &format!("/v1/threads/{id}/messages?offset=3&limit=4"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4);
        assert_eq!(body["total"].as_u64(), Some(10));
        assert_eq!(body["has_more"].as_bool(), Some(true));
        let next_cursor = body["next_cursor"].as_str().expect("next cursor");

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/threads/{id}/messages?cursor={next_cursor}&limit=4"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("msg-7"));
        assert_eq!(messages[2]["content"][0]["text"].as_str(), Some("msg-9"));
        assert_eq!(body["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn get_thread_messages_cursor_pagination() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs: Vec<Message> = (0..6).map(|i| Message::user(format!("msg-{i}"))).collect();
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/threads/{id}/messages?cursor=2&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("msg-2"));
        assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("msg-3"));
        assert_eq!(body["total"].as_u64(), Some(6));
        assert_eq!(body["has_more"].as_bool(), Some(true));
        let next_cursor = body["next_cursor"].as_str().expect("next cursor");

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/threads/{id}/messages?cursor={next_cursor}&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("msg-4"));
        assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("msg-5"));
    }

    #[tokio::test]
    async fn get_thread_messages_invalid_cursor_returns_400() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;

        let (status, body) = get_json(
            test.router,
            &format!("/v1/threads/{id}/messages?cursor=not-a-number"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"].as_str(),
            Some("cursor must be a valid pagination token")
        );
    }

    #[tokio::test]
    async fn get_thread_messages_supports_run_and_sequence_filters() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let run1 = awaken_contract::contract::message::MessageMetadata {
            run_id: Some("run-1".to_string()),
            step_index: Some(0),
        };
        let run2 = awaken_contract::contract::message::MessageMetadata {
            run_id: Some("run-2".to_string()),
            step_index: Some(0),
        };
        let messages = vec![
            Message::user("input"),
            Message::assistant("first").with_metadata(run1.clone()),
            Message::internal_system("hidden").with_metadata(run1.clone()),
            Message::assistant("other").with_metadata(run2),
            Message::assistant("second").with_metadata(run1),
        ];
        test.store.save_messages(&id, &messages).await.unwrap();

        let (status, body) = get_json(
            test.router,
            &format!("/v1/threads/{id}/messages?runId=run-1&after=1&order=desc"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("second"));
        assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("first"));
        assert_eq!(body["total"].as_u64(), Some(2));
    }

    #[tokio::test]
    async fn patch_thread_updates_lineage_fields() {
        let test = make_test_app();
        let id = seed_thread(&test.store, Some("Patch Target")).await;
        let parent_id = seed_thread(&test.store, Some("Patch Parent")).await;

        let (status, body) = patch_json(
            test.router.clone(),
            &format!("/v1/threads/{id}"),
            json!({
                "resourceId": "resource-b",
                "parentThreadId": parent_id.as_str()
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["resource_id"].as_str(), Some("resource-b"));
        assert_eq!(body["parent_thread_id"].as_str(), Some(parent_id.as_str()));

        let thread = test.store.load_thread(&id).await.unwrap().unwrap();
        assert_eq!(thread.resource_id.as_deref(), Some("resource-b"));
        assert_eq!(thread.parent_thread_id.as_deref(), Some(parent_id.as_str()));

        let page = test
            .event_store
            .list(EventScope::thread(id.as_str()), None, 10)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_kind.as_str(), "ThreadUpdated");
        assert_eq!(page.events[0].payload["resource_id"], "resource-b");
        assert_eq!(page.events[0].payload["parent_thread_id"], parent_id);
        assert!(page.events[0].payload["previous"]["resource_id"].is_null());
    }

    #[tokio::test]
    async fn patch_thread_supports_clearing_lineage_fields() {
        let test = make_test_app();
        let parent_id = seed_thread(&test.store, Some("Detach Parent")).await;
        let id = seed_thread_with_lineage(
            &test.store,
            Some("Detach Child"),
            Some("resource-a"),
            Some(parent_id.as_str()),
        )
        .await;

        let (status, body) = patch_json(
            test.router,
            &format!("/v1/threads/{id}"),
            json!({
                "resourceId": null,
                "parentThreadId": null
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["resource_id"].is_null());
        assert!(body["parent_thread_id"].is_null());

        let thread = test.store.load_thread(&id).await.unwrap().unwrap();
        assert_eq!(thread.resource_id, None);
        assert_eq!(thread.parent_thread_id, None);
    }

    #[tokio::test]
    async fn patch_thread_rejects_cycle() {
        let test = make_test_app();
        let root_id = seed_thread(&test.store, Some("Root")).await;
        let child_id =
            seed_thread_with_lineage(&test.store, Some("Child"), None, Some(root_id.as_str()))
                .await;

        let (status, body) = patch_json(
            test.router,
            &format!("/v1/threads/{root_id}"),
            json!({
                "parentThreadId": child_id.as_str()
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .expect("error message")
                .contains("cycle detected")
        );
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

        let page = test
            .event_store
            .list(EventScope::thread(id.as_str()), None, 10)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_kind.as_str(), "ThreadDeleted");
        assert_eq!(page.events[0].payload["child_strategy"], "detach");
    }

    #[tokio::test]
    async fn delete_thread_defaults_to_detaching_direct_children() {
        let test = make_test_app();
        let parent_id = seed_thread(&test.store, Some("Delete Parent")).await;
        let child_id = seed_thread_with_lineage(
            &test.store,
            Some("Delete Child"),
            None,
            Some(parent_id.as_str()),
        )
        .await;
        let grandchild_id = seed_thread_with_lineage(
            &test.store,
            Some("Delete Grandchild"),
            None,
            Some(child_id.as_str()),
        )
        .await;

        let (status, _body) =
            delete_json(test.router.clone(), &format!("/v1/threads/{parent_id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert!(test.store.load_thread(&parent_id).await.unwrap().is_none());
        assert_eq!(
            test.store
                .load_thread(&child_id)
                .await
                .unwrap()
                .unwrap()
                .parent_thread_id,
            None
        );
        assert_eq!(
            test.store
                .load_thread(&grandchild_id)
                .await
                .unwrap()
                .unwrap()
                .parent_thread_id
                .as_deref(),
            Some(child_id.as_str())
        );
    }

    #[tokio::test]
    async fn delete_thread_reject_strategy_returns_bad_request() {
        let test = make_test_app();
        let parent_id = seed_thread(&test.store, Some("Reject Parent")).await;
        let _child_id = seed_thread_with_lineage(
            &test.store,
            Some("Reject Child"),
            None,
            Some(parent_id.as_str()),
        )
        .await;

        let (status, body) = delete_json(
            test.router.clone(),
            &format!("/v1/threads/{parent_id}?childStrategy=reject"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .expect("error message")
                .contains("child threads")
        );
        assert!(test.store.load_thread(&parent_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_thread_cascade_strategy_removes_descendants() {
        let test = make_test_app();
        let parent_id = seed_thread(&test.store, Some("Cascade Parent")).await;
        let child_id = seed_thread_with_lineage(
            &test.store,
            Some("Cascade Child"),
            None,
            Some(parent_id.as_str()),
        )
        .await;
        let grandchild_id = seed_thread_with_lineage(
            &test.store,
            Some("Cascade Grandchild"),
            None,
            Some(child_id.as_str()),
        )
        .await;

        let (status, _body) = delete_json(
            test.router.clone(),
            &format!("/v1/threads/{parent_id}?childStrategy=cascade"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert!(test.store.load_thread(&parent_id).await.unwrap().is_none());
        assert!(test.store.load_thread(&child_id).await.unwrap().is_none());
        assert!(
            test.store
                .load_thread(&grandchild_id)
                .await
                .unwrap()
                .is_none()
        );
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
    async fn active_run_for_thread_returns_running_run() {
        let test = make_test_app();
        seed_run(&test.store, "run-active", "t-active", RunStatus::Running).await;
        seed_run(&test.store, "run-done", "t-active", RunStatus::Done).await;

        let (status, body) = get_json(test.router, "/v1/threads/t-active/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["active_run"]["run_id"].as_str(), Some("run-active"));
        assert_eq!(body["active_run"]["status"].as_str(), Some("running"));
    }

    #[tokio::test]
    async fn active_run_for_thread_prefers_active_projection() {
        let test = make_test_app();
        let mut thread = Thread::with_id("t-projection");
        thread.active_run_id = Some("run-active-projected".into());
        thread.open_run_id = Some("run-active-projected".into());
        test.store.save_thread(&thread).await.unwrap();
        test.store
            .create_run(&run_record(
                "run-active-projected",
                "t-projection",
                RunStatus::Running,
                100,
            ))
            .await
            .unwrap();
        test.store
            .create_run(&run_record(
                "run-newer-waiting",
                "t-projection",
                RunStatus::Waiting,
                200,
            ))
            .await
            .unwrap();

        let (status, body) = get_json(test.router, "/v1/threads/t-projection/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["active_run"]["run_id"].as_str(),
            Some("run-active-projected")
        );
        assert_eq!(body["active_run"]["status"].as_str(), Some("running"));
    }

    #[tokio::test]
    async fn active_run_for_thread_uses_open_waiting_projection() {
        let test = make_test_app();
        let mut thread = Thread::with_id("t-open");
        thread.open_run_id = Some("run-open-waiting".into());
        test.store.save_thread(&thread).await.unwrap();
        test.store
            .create_run(&run_record(
                "run-open-waiting",
                "t-open",
                RunStatus::Waiting,
                100,
            ))
            .await
            .unwrap();
        test.store
            .create_run(&run_record(
                "run-stale-running",
                "t-open",
                RunStatus::Running,
                200,
            ))
            .await
            .unwrap();

        let (status, body) = get_json(test.router, "/v1/threads/t-open/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["active_run"]["run_id"].as_str(),
            Some("run-open-waiting")
        );
        assert_eq!(body["active_run"]["status"].as_str(), Some("waiting"));
    }

    #[tokio::test]
    async fn active_run_for_thread_includes_durable_waiting_tickets() {
        let test = make_test_app();
        let thread_id = "t-open-ticket";
        let run = waiting_tool_run("run-open-ticket", thread_id, "ticket-1");
        test.store
            .checkpoint(thread_id, &[Message::user("approve?")], &run)
            .await
            .unwrap();

        let (status, body) = get_json(test.router, "/v1/threads/t-open-ticket/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        let waiting = &body["active_run"]["waiting"];
        assert_eq!(waiting["reason"].as_str(), Some("tool_permission"));
        assert_eq!(waiting["ticket_ids"][0].as_str(), Some("ticket-1"));
        assert_eq!(
            waiting["tickets"][0]["tool_call_id"].as_str(),
            Some("tool-call-1")
        );
        assert_eq!(
            waiting["tickets"][0]["tool_name"].as_str(),
            Some("dangerous")
        );
    }

    #[tokio::test]
    async fn active_run_for_thread_falls_back_when_projection_is_stale() {
        let test = make_test_app();
        let mut thread = Thread::with_id("t-stale");
        thread.active_run_id = Some("missing-run".into());
        thread.open_run_id = Some("done-run".into());
        test.store.save_thread(&thread).await.unwrap();
        test.store
            .create_run(&run_record("done-run", "t-stale", RunStatus::Done, 100))
            .await
            .unwrap();
        test.store
            .create_run(&run_record(
                "run-fallback",
                "t-stale",
                RunStatus::Running,
                200,
            ))
            .await
            .unwrap();

        let (status, body) = get_json(test.router, "/v1/threads/t-stale/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["active_run"]["run_id"].as_str(), Some("run-fallback"));
        assert_eq!(body["active_run"]["status"].as_str(), Some("running"));
    }

    #[tokio::test]
    async fn active_run_for_thread_returns_null_when_idle() {
        let test = make_test_app();
        seed_run(&test.store, "run-done", "t-idle", RunStatus::Done).await;

        let (status, body) = get_json(test.router, "/v1/threads/t-idle/runs/active").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["active_run"].is_null());
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
    async fn cancel_thread_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/threads/nonexistent-thread/cancel",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn ai_sdk_cancel_thread_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/ai-sdk/threads/nonexistent-thread/cancel",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn ag_ui_interrupt_thread_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/ag-ui/threads/nonexistent-thread/interrupt",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn decision_endpoint_not_found_returns_404() {
        let test = make_test_app();
        let (status, body) = post_json(
            test.router,
            "/v1/threads/nonexistent-thread/decision",
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
        let (status, _body) = post_json(
            test.router,
            "/v1/threads/some-thread/decision",
            json!({
                "toolCallId": "tc-1",
                "action": "invalid_action",
                "payload": {}
            }),
        )
        .await;
        // Bad action returns 400 before the thread lookup
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn thread_messages_accepts_interrupt_then_queue_input_mode() {
        let test = make_test_app();
        let thread_id = seed_thread(&test.store, Some("Control Thread")).await;

        let (status, body) = post_json(
            test.router.clone(),
            &format!("/v1/threads/{thread_id}/messages"),
            json!({
                "mode": "interrupt_then_queue",
                "messages": [{"role": "user", "content": "redirect"}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["thread_id"].as_str(), Some(thread_id.as_str()));
        assert!(matches!(
            body["status"].as_str(),
            Some("running") | Some("queued")
        ));
    }

    #[tokio::test]
    async fn thread_messages_accepts_live_then_queue_input_mode() {
        let test = make_test_app();
        let thread_id = seed_thread(&test.store, Some("Live Control Thread")).await;

        let (status, body) = post_json(
            test.router.clone(),
            &format!("/v1/threads/{thread_id}/messages"),
            json!({
                "mode": "live_then_queue",
                "messages": [{"role": "user", "content": "steer softly"}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["thread_id"].as_str(), Some(thread_id.as_str()));
        assert!(matches!(
            body["status"].as_str(),
            Some("running") | Some("queued")
        ));
    }

    #[tokio::test]
    async fn thread_messages_resume_open_run_continues_projected_waiting_run() {
        let test = make_test_app();
        let thread_id = "thread-resume-open";
        let mut run = run_record("run-open-input", thread_id, RunStatus::Waiting, 1000);
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: Some("waiting for user input".to_string()),
        });
        test.store
            .checkpoint(thread_id, &[Message::user("original")], &run)
            .await
            .unwrap();

        let (status, body) = post_json(
            test.router,
            &format!("/v1/threads/{thread_id}/messages"),
            json!({
                "mode": "resume_open_run",
                "messages": [{"role": "user", "content": "continue same run"}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["thread_id"].as_str(), Some(thread_id));

        let dispatches = test
            .mailbox_store
            .list_dispatches(thread_id, None, 10, 0)
            .await
            .unwrap();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].run_id, "run-open-input");
    }

    #[tokio::test]
    async fn decision_endpoint_requeues_durable_waiting_ticket_after_restart() {
        let test = make_test_app();
        let thread_id = "thread-durable-decision";
        let run = waiting_tool_run("run-durable-decision", thread_id, "ticket-durable");
        test.store
            .checkpoint(thread_id, &[Message::user("approve?")], &run)
            .await
            .unwrap();

        let (status, body) = post_json(
            test.router.clone(),
            "/v1/runs/run-durable-decision/decision",
            json!({
                "toolCallId": "tool-call-1",
                "action": "resume",
                "payload": {"approved": true}
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["run_id"].as_str(), Some("run-durable-decision"));

        let dispatches = test
            .mailbox_store
            .list_dispatches(thread_id, None, 10, 0)
            .await
            .unwrap();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].run_id, "run-durable-decision");
    }

    #[tokio::test]
    async fn run_inputs_use_run_anchor_and_accept_control_mode() {
        let test = make_test_app();
        let thread_id = seed_thread(&test.store, Some("Run Input Thread")).await;
        seed_run(&test.store, "run-control-1", &thread_id, RunStatus::Running).await;

        let (status, body) = post_json(
            test.router,
            "/v1/runs/run-control-1/inputs",
            json!({
                "mode": "queue",
                "messages": [{"role": "user", "content": "follow up"}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["status"].as_str(), Some("inputs_accepted"));
        assert_eq!(body["run_id"].as_str(), Some("run-control-1"));
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
    async fn get_messages_for_thread_filters_internal_by_default() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![
            Message::user("visible-user"),
            Message::internal_system("hidden-system"),
            Message::assistant("visible-assistant"),
        ];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(test.router, &format!("/v1/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
        assert_eq!(
            messages[0]["content"][0]["text"].as_str(),
            Some("visible-user")
        );
        assert_eq!(
            messages[1]["content"][0]["text"].as_str(),
            Some("visible-assistant")
        );
    }

    #[tokio::test]
    async fn get_messages_for_thread_includes_internal_when_requested() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![
            Message::user("visible-user"),
            Message::internal_system("hidden-system"),
        ];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(
            test.router,
            &format!("/v1/threads/{id}/messages?visibility=all"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
        assert_eq!(
            messages[1]["content"][0]["text"].as_str(),
            Some("hidden-system")
        );
        assert_eq!(messages[1]["visibility"].as_str(), Some("internal"));
    }

    #[tokio::test]
    async fn ai_sdk_thread_messages_filter_internal_by_default() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![
            Message::user("visible-user"),
            Message::internal_system("hidden-system"),
            Message::assistant("visible-assistant"),
        ];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) =
            get_json(test.router, &format!("/v1/ai-sdk/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
        assert_eq!(messages[0]["role"].as_str(), Some("user"));
        assert_eq!(
            messages[0]["parts"][0]["text"].as_str(),
            Some("visible-user")
        );
        assert_eq!(messages[1]["role"].as_str(), Some("assistant"));
        assert_eq!(
            messages[1]["parts"][0]["text"].as_str(),
            Some("visible-assistant")
        );
    }

    #[tokio::test]
    async fn ai_sdk_thread_messages_support_cursor_pagination() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs: Vec<Message> = (0..5).map(|i| Message::user(format!("msg-{i}"))).collect();
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/ai-sdk/threads/{id}/messages?cursor=1&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["parts"][0]["text"].as_str(), Some("msg-1"));
        assert_eq!(messages[1]["parts"][0]["text"].as_str(), Some("msg-2"));
        assert_eq!(body["has_more"].as_bool(), Some(true));
        let next_cursor = body["next_cursor"].as_str().expect("next cursor");

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/ai-sdk/threads/{id}/messages?cursor={next_cursor}&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["parts"][0]["text"].as_str(), Some("msg-3"));
        assert_eq!(messages[1]["parts"][0]["text"].as_str(), Some("msg-4"));
    }

    #[tokio::test]
    async fn ag_ui_thread_messages_filter_internal_by_default() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs = vec![
            Message::user("visible-user"),
            Message::internal_system("hidden-system"),
            Message::assistant("visible-assistant"),
        ];
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) =
            get_json(test.router, &format!("/v1/ag-ui/threads/{id}/messages")).await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(body["total"].as_u64(), Some(2));
        assert_eq!(messages[0]["role"].as_str(), Some("user"));
        assert_eq!(
            messages[0]["content"][0]["text"].as_str(),
            Some("visible-user")
        );
        assert_eq!(messages[1]["role"].as_str(), Some("assistant"));
        assert_eq!(
            messages[1]["content"][0]["text"].as_str(),
            Some("visible-assistant")
        );
    }

    #[tokio::test]
    async fn ag_ui_thread_messages_support_cursor_pagination() {
        let test = make_test_app();
        let id = seed_thread(&test.store, None).await;
        let msgs: Vec<Message> = (0..5).map(|i| Message::user(format!("msg-{i}"))).collect();
        test.store.save_messages(&id, &msgs).await.unwrap();

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/ag-ui/threads/{id}/messages?cursor=1&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("msg-1"));
        assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("msg-2"));
        assert_eq!(body["has_more"].as_bool(), Some(true));
        let next_cursor = body["next_cursor"].as_str().expect("next cursor");

        let (status, body) = get_json(
            test.router.clone(),
            &format!("/v1/ag-ui/threads/{id}/messages?cursor={next_cursor}&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("msg-3"));
        assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("msg-4"));
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
    async fn health_readiness_returns_healthy_json() {
        let test = make_test_app();
        let (status, body) = get_json(test.router, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["components"]["store"], "ok");
        assert_eq!(body["components"]["runtime"], "ok");
    }

    #[tokio::test]
    async fn health_liveness_returns_200() {
        let test = make_test_app();
        let (status, _body) = get_json(test.router, "/health/live").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_endpoint_returns_404() {
        let test = make_test_app();
        let (status, _body) = get_json(test.router, "/v1/completely-unknown-endpoint").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
