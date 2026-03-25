//! Run API lifecycle tests — validates start, list, and contract behavior.
//!
//! Mirrors high-value run API tests from uncarve's tirea-agentos-server/tests/run_api.rs,
//! adapted to awaken's AppState + RunDispatcher architecture.
//!
//! NOTE: Routes using `{id}` path parameters (e.g. `/v1/runs/{id}`,
//! `/v1/runs/{id}/cancel`) are unreachable due to a mismatch between
//! axum 0.7's `:id` param syntax and the `{id}` syntax used in routes.rs.
//! Tests for those endpoints are omitted until the path params are fixed.

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::storage::{RunRecord, RunStore};
use awaken_contract::registry_spec::AgentSpec;
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

// ── Mock executor ──

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

// ── Shared helpers ──

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
    let mailbox_store = std::sync::Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = std::sync::Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    TestApp {
        router: build_router().with_state(state),
        store,
    }
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, String) {
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
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, String) {
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
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

fn extract_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| !data.is_empty())
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events.iter().find(|e| {
        e.get("event_type")
            .and_then(Value::as_str)
            .or_else(|| e.get("type").and_then(Value::as_str))
            == Some(event_type)
    })
}

// ============================================================================
// Start run (POST /v1/runs)
// ============================================================================

#[tokio::test]
async fn start_run_streams_sse_with_run_lifecycle() {
    let test = make_test_app();
    let (status, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected: {body}");

    let events = extract_sse_events(&body);
    let run_start = find_event(&events, "run_start");
    assert!(run_start.is_some(), "no run_start event in SSE: {body}");
    let run_id = run_start.unwrap()["run_id"]
        .as_str()
        .expect("run_start should have run_id");
    assert!(!run_id.is_empty());

    let run_finish = find_event(&events, "run_finish");
    assert!(run_finish.is_some(), "no run_finish event in SSE: {body}");
}

#[tokio::test]
async fn start_run_includes_thread_id_in_events() {
    let test = make_test_app();
    let (status, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "threadId": "explicit-thread",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = extract_sse_events(&body);
    let run_start = find_event(&events, "run_start").expect("run_start missing");
    assert_eq!(
        run_start["thread_id"].as_str(),
        Some("explicit-thread"),
        "thread_id should be propagated"
    );
}

#[tokio::test]
async fn start_run_generates_thread_id_when_omitted() {
    let test = make_test_app();
    let (status, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = extract_sse_events(&body);
    let run_start = find_event(&events, "run_start").expect("run_start missing");
    let thread_id = run_start["thread_id"]
        .as_str()
        .expect("thread_id should be present");
    assert!(
        !thread_id.is_empty(),
        "auto-generated thread_id should be non-empty"
    );
}

#[tokio::test]
async fn start_run_rejects_empty_agent_id() {
    let test = make_test_app();
    let (status, _body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "  ",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn start_run_rejects_empty_messages() {
    let test = make_test_app();
    let (status, _body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "messages": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn start_run_includes_step_events() {
    let test = make_test_app();
    let (_, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    let events = extract_sse_events(&body);
    let step_start = find_event(&events, "step_start");
    let step_end = find_event(&events, "step_end");
    assert!(step_start.is_some(), "step_start missing in: {body}");
    assert!(step_end.is_some(), "step_end missing in: {body}");
}

#[tokio::test]
async fn start_run_includes_inference_complete() {
    let test = make_test_app();
    let (_, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    let events = extract_sse_events(&body);
    let inference = find_event(&events, "inference_complete");
    assert!(inference.is_some(), "inference_complete missing in: {body}");
    assert_eq!(inference.unwrap()["model"].as_str(), Some("mock-model"));
}

// ============================================================================
// List runs (GET /v1/runs)
// ============================================================================

#[tokio::test]
async fn list_runs_returns_empty_initially() {
    let test = make_test_app();
    let (status, body) = get_json(test.router, "/v1/runs").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let items = payload["items"].as_array().expect("items should be array");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_runs_returns_seeded_records() {
    let test = make_test_app();
    for i in 0..3 {
        let record = RunRecord {
            run_id: format!("run-list-{i}"),
            thread_id: format!("thread-list-{i}"),
            agent_id: "test-agent".to_string(),
            parent_run_id: None,
            status: RunStatus::Done,
            termination_code: None,
            created_at: 1000 + i as u64,
            updated_at: 1000 + i as u64,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        test.store.create_run(&record).await.expect("seed run");
    }

    let (status, body) = get_json(test.router, "/v1/runs").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let items = payload["items"].as_array().expect("items should be array");
    assert_eq!(items.len(), 3);
}

#[tokio::test]
async fn list_runs_supports_status_filter() {
    let test = make_test_app();

    let done_record = RunRecord {
        run_id: "run-filter-done".to_string(),
        thread_id: "thread-filter".to_string(),
        agent_id: "test-agent".to_string(),
        parent_run_id: None,
        status: RunStatus::Done,
        termination_code: None,
        created_at: 1000,
        updated_at: 1000,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    let running_record = RunRecord {
        run_id: "run-filter-running".to_string(),
        thread_id: "thread-filter-2".to_string(),
        agent_id: "test-agent".to_string(),
        parent_run_id: None,
        status: RunStatus::Running,
        termination_code: None,
        created_at: 1001,
        updated_at: 1001,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    test.store
        .create_run(&done_record)
        .await
        .expect("seed done");
    test.store
        .create_run(&running_record)
        .await
        .expect("seed running");

    let (status, body) = get_json(test.router, "/v1/runs?status=done").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let items = payload["items"].as_array().expect("items should be array");
    assert!(
        items
            .iter()
            .all(|item| item["status"].as_str() == Some("done")),
        "all items should be done: {payload}"
    );
}

#[tokio::test]
async fn list_runs_rejects_invalid_status() {
    let test = make_test_app();
    let (status, _body) = get_json(test.router, "/v1/runs?status=invalid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ============================================================================
// RunRecord contract tests
// ============================================================================

#[test]
fn run_record_status_transitions() {
    assert!(RunStatus::Running.can_transition_to(RunStatus::Waiting));
    assert!(RunStatus::Running.can_transition_to(RunStatus::Done));
    assert!(RunStatus::Waiting.can_transition_to(RunStatus::Running));
    assert!(!RunStatus::Done.can_transition_to(RunStatus::Running));
}

#[test]
fn run_record_terminal_status() {
    assert!(!RunStatus::Running.is_terminal());
    assert!(!RunStatus::Waiting.is_terminal());
    assert!(RunStatus::Done.is_terminal());
}

#[test]
fn run_query_defaults() {
    use awaken_contract::contract::storage::RunQuery;
    let q = RunQuery::default();
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, 50);
    assert!(q.thread_id.is_none());
    assert!(q.status.is_none());
}

// ============================================================================
// Health endpoint
// ============================================================================

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let test = make_test_app();
    let (status, _body) = get_json(test.router, "/health").await;
    assert_eq!(status, StatusCode::OK);
}
