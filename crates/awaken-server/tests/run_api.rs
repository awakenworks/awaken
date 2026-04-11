//! Run API lifecycle tests — validates start, list, and contract behavior.
//!
//! Mirrors high-value run API tests from uncarve's tirea-agentos-server/tests/run_api.rs,
//! adapted to awaken's AppState + Mailbox architecture.
//!
//! NOTE: Control operations (cancel, decision) are now unified under
//! `/v1/threads/:id/{cancel,decision}`. The `/v1/runs` namespace is
//! read-only (list, get).

use async_trait::async_trait;
use awaken_contract::contract::content::extract_text;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadStore};
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::registry_spec::RemoteEndpoint;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::extensions::a2a::{
    AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
    DelegateRunResult, DelegateRunStatus,
};
use awaken_runtime::registry::traits::ModelBinding;
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

struct SlowExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for SlowExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "slow"
    }
}

struct PreviewInspectorExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for PreviewInspectorExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let system = if request.system.is_empty() {
            request
                .messages
                .iter()
                .find(|message| message.role == awaken_contract::contract::message::Role::System)
                .map(|message| message.text())
                .unwrap_or_default()
        } else {
            extract_text(&request.system)
        };
        let roles = request
            .messages
            .iter()
            .map(|message| format!("{:?}", message.role).to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(",");

        Ok(StreamResult {
            content: vec![awaken_contract::contract::content::ContentBlock::text(
                format!("system={system};roles={roles}"),
            )],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "preview-inspector"
    }
}

struct StaticRemoteBackend;

#[async_trait]
impl AgentBackend for StaticRemoteBackend {
    fn capabilities(&self) -> awaken_runtime::BackendCapabilities {
        awaken_runtime::BackendCapabilities {
            cancellation: awaken_runtime::BackendCancellationCapability::RemoteAbort,
            decisions: false,
            overrides: false,
            frontend_tools: false,
            continuation: awaken_runtime::BackendContinuationCapability::None,
            waits: awaken_runtime::BackendWaitCapability::None,
            transcript: awaken_runtime::BackendTranscriptCapability::SinglePrompt,
            output: awaken_runtime::BackendOutputCapability::TextAndArtifacts,
        }
    }

    async fn execute_root(
        &self,
        request: awaken_runtime::BackendRootRunRequest<'_>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        Ok(DelegateRunResult {
            agent_id: request.agent_id.to_string(),
            status: DelegateRunStatus::Completed,
            termination: TerminationReason::NaturalEnd,
            status_reason: None,
            response: Some("hello from remote root".into()),
            output: awaken_runtime::BackendRunOutput {
                text: Some("hello from remote root".into()),
                artifacts: vec![awaken_runtime::BackendOutputArtifact {
                    id: Some("artifact-1".into()),
                    name: Some("result.json".into()),
                    media_type: Some("application/json".into()),
                    content: json!({"answer": 42}),
                }],
                raw: Some(json!({"transport": "test-remote"})),
            },
            steps: 1,
            run_id: Some("remote-child-run".into()),
            inbox: None,
            state: None,
        })
    }
}

struct StaticRemoteBackendFactory;

impl AgentBackendFactory for StaticRemoteBackendFactory {
    fn backend(&self) -> &str {
        "test-remote"
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
        if endpoint.backend != "test-remote" {
            return Err(AgentBackendFactoryError::InvalidConfig(format!(
                "unexpected backend '{}'",
                endpoint.backend
            )));
        }
        Ok(Arc::new(StaticRemoteBackend))
    }
}

// ── Shared helpers ──

struct TestApp {
    router: axum::Router,
    store: Arc<InMemoryStore>,
}

fn make_test_app_with_runtime(
    runtime: Arc<awaken_runtime::AgentRuntime>,
    store: Arc<InMemoryStore>,
) -> TestApp {
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

fn make_test_app() -> TestApp {
    make_test_app_with_executor(Arc::new(ImmediateExecutor))
}

fn make_test_app_with_executor(
    executor: Arc<dyn awaken_contract::contract::executor::LlmExecutor>,
) -> TestApp {
    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_provider("mock", executor)
            .with_agent_spec(AgentSpec {
                id: "test-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "test".into(),
                max_rounds: 0,
                ..Default::default()
            })
            .with_thread_run_store(store.clone())
            .build()
            .expect("build runtime"),
    );
    make_test_app_with_runtime(runtime, store)
}

fn make_test_app_with_remote_root_agent() -> TestApp {
    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(ImmediateExecutor))
            .with_agent_spec(AgentSpec {
                id: "remote-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "remote".into(),
                endpoint: Some(RemoteEndpoint {
                    backend: "test-remote".into(),
                    base_url: "https://remote.example.com".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .with_agent_backend_factory(Arc::new(StaticRemoteBackendFactory))
            .with_thread_run_store(store.clone())
            .build()
            .expect("build runtime with remote root agent"),
    );
    make_test_app_with_runtime(runtime, store)
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
async fn concurrent_same_thread_run_returns_conflict_instead_of_server_error() {
    let test = make_test_app_with_executor(Arc::new(SlowExecutor));
    let thread_id = "thread-conflict";

    let first = post_json(
        test.router.clone(),
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "threadId": thread_id,
            "messages": [{"role": "user", "content": "first"}]
        }),
    );
    let second = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "test-agent",
            "threadId": thread_id,
            "messages": [{"role": "user", "content": "second"}]
        }),
    );

    let ((status1, body1), (status2, body2)) = tokio::join!(first, second);

    let statuses = [status1, status2];
    assert!(
        statuses.contains(&StatusCode::OK),
        "one request should still execute successfully: {status1} {body1:?}, {status2} {body2:?}"
    );
    assert!(
        statuses.contains(&StatusCode::CONFLICT),
        "the losing request should surface a conflict, not a 5xx: {status1} {body1:?}, {status2} {body2:?}"
    );
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

#[tokio::test]
async fn ai_sdk_agent_run_creates_thread_record() {
    let test = make_test_app();
    let thread_id = "thread-ai-sdk-persist";
    let (status, body) = post_json(
        test.router.clone(),
        "/v1/ai-sdk/agents/test-agent/runs",
        json!({
            "threadId": thread_id,
            "messages": [
                {
                    "role": "user",
                    "parts": [{ "type": "text", "text": "hello" }]
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let thread = test
        .store
        .load_thread(thread_id)
        .await
        .expect("thread lookup should succeed")
        .expect("thread should be persisted");
    assert_eq!(thread.id, thread_id);

    let messages = test
        .store
        .load_messages(thread_id)
        .await
        .expect("messages lookup should succeed")
        .expect("messages should be persisted");
    assert!(!messages.is_empty());
}

#[tokio::test]
async fn start_run_supports_remote_root_agents() {
    let test = make_test_app_with_remote_root_agent();
    let (status, body) = post_json(
        test.router.clone(),
        "/v1/runs",
        json!({
            "agentId": "remote-agent",
            "threadId": "thread-remote-start-run",
            "messages": [{"role": "user", "content": "hello remote"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let events = extract_sse_events(&body);
    assert!(
        find_event(&events, "run_start").is_some(),
        "missing run_start in: {body}"
    );
    assert!(events.iter().any(|event| {
        event.get("event_type").and_then(Value::as_str) == Some("text_delta")
            && event.get("delta").and_then(Value::as_str) == Some("hello from remote root")
    }));
    let run_finish = find_event(&events, "run_finish").expect("run_finish missing");
    assert_eq!(
        run_finish["termination"]["type"].as_str(),
        Some("natural_end"),
        "unexpected run_finish: {body}"
    );
    assert_eq!(
        run_finish["result"]["output"]["artifacts"][0]["content"],
        json!({"answer": 42}),
        "remote root output artifacts should survive runtime run_finish: {body}"
    );

    let latest_run = test
        .store
        .latest_run("thread-remote-start-run")
        .await
        .expect("latest run lookup")
        .expect("persisted run");
    assert_eq!(
        latest_run
            .state
            .as_ref()
            .and_then(|state| state.extensions.get("__runtime_backend_output"))
            .and_then(|output| output.get("artifacts"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "remote root output artifacts should survive run state persistence"
    );
}

#[tokio::test]
async fn ai_sdk_agent_run_supports_remote_root_agents() {
    let test = make_test_app_with_remote_root_agent();
    let thread_id = "thread-remote-root";
    let (status, body) = post_json(
        test.router.clone(),
        "/v1/ai-sdk/agents/remote-agent/runs",
        json!({
            "threadId": thread_id,
            "messages": [
                {
                    "role": "user",
                    "parts": [{ "type": "text", "text": "hello remote" }]
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"text-delta\""),
        "missing text-delta event: {body}"
    );
    assert!(
        body.contains("hello from remote root"),
        "remote response should be streamed through AI SDK: {body}"
    );

    let messages = test
        .store
        .load_messages(thread_id)
        .await
        .expect("messages lookup should succeed")
        .expect("messages should be persisted");
    assert!(
        messages.iter().any(|message| {
            message.role == awaken_contract::contract::message::Role::Assistant
                && message.text().contains("hello from remote root")
        }),
        "assistant reply should be persisted for remote root runs"
    );

    let run = test
        .store
        .latest_run(thread_id)
        .await
        .expect("latest run lookup should succeed")
        .expect("run record should exist");
    assert_eq!(run.agent_id, "remote-agent");
    assert_eq!(run.status, RunStatus::Done);
}

#[tokio::test]
async fn ag_ui_agent_run_supports_remote_root_agents() {
    let test = make_test_app_with_remote_root_agent();
    let (status, body) = post_json(
        test.router,
        "/v1/ag-ui/agents/remote-agent/runs",
        json!({
            "threadId": "thread-remote-agui",
            "messages": [{"role": "user", "content": "hello remote"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"RUN_STARTED\""),
        "missing RUN_STARTED: {body}"
    );
    assert!(
        body.contains("hello from remote root"),
        "remote response should be streamed through AG-UI: {body}"
    );
    assert!(
        body.contains("\"type\":\"RUN_FINISHED\""),
        "missing RUN_FINISHED: {body}"
    );
}

#[tokio::test]
async fn ai_sdk_agent_preview_runs_with_draft_system_prompt_and_history() {
    let test = make_test_app_with_executor(Arc::new(PreviewInspectorExecutor));
    let (status, body) = post_json(
        test.router,
        "/v1/ai-sdk/agent-previews/runs",
        json!({
            "agent": {
                "id": "",
                "model_id": "test-model",
                "system_prompt": "draft system prompt",
                "max_rounds": 0
            },
            "messages": [
                {
                    "id": "u1",
                    "role": "user",
                    "parts": [{ "type": "text", "text": "hello" }]
                },
                {
                    "id": "a1",
                    "role": "assistant",
                    "parts": [{ "type": "text", "text": "previous reply" }]
                },
                {
                    "id": "u2",
                    "role": "user",
                    "parts": [{ "type": "text", "text": "follow up" }]
                }
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("draft system prompt"),
        "preview should use draft spec: {body}"
    );
    assert!(
        body.contains("roles=system,user,assistant,user"),
        "preview should preserve assistant history: {body}"
    );
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
async fn health_readiness_returns_ok() {
    let test = make_test_app();
    let (status, body) = get_json(test.router, "/health").await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["status"], "healthy");
}

#[tokio::test]
async fn health_liveness_returns_ok() {
    let test = make_test_app();
    let (status, _body) = get_json(test.router, "/health/live").await;
    assert_eq!(status, StatusCode::OK);
}
