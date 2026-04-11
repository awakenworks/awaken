use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::ToolCall;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadRunStore, ThreadStore};
use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};
use awaken_contract::state::PersistedState;
use awaken_protocol_a2a::{
    Artifact, Message as A2aMessage, MessageRole, Part, SendMessageRequest, SendMessageResponse,
    Task, TaskState, TaskStatus,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::extensions::a2a::A2aBackendFactory;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_runtime::{AgentRuntime, BackendAbortRequest, ExecutionBackendFactory, RunRequest};
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

struct ImmediateExecutor;

#[async_trait]
impl LlmExecutor for ImmediateExecutor {
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

struct DelegatingExecutor {
    tool_name: String,
    call_count: AtomicUsize,
}

#[async_trait]
impl LlmExecutor for DelegatingExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            let prompt = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::User)
                .map(|message| message.text())
                .unwrap_or_else(|| "delegate".to_string());
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    "delegate-1",
                    &self.tool_name,
                    json!({"prompt": prompt}),
                )],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(StreamResult {
                content: vec![awaken_contract::contract::content::ContentBlock::text(
                    "delegation complete",
                )],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
    }

    fn name(&self) -> &str {
        "delegating"
    }
}

#[derive(Clone, Copy, Default)]
enum MockCompletionMode {
    #[default]
    Completed,
    InputRequired,
    AuthRequired,
}

#[derive(Default)]
struct MockA2aState {
    send_requests: Mutex<Vec<SendMessageRequest>>,
    cancel_requests: Mutex<Vec<String>>,
    unexpected_requests: Mutex<Vec<String>>,
    poll_count: AtomicUsize,
    subscribe_count: AtomicUsize,
    slow_poll_started: AtomicBool,
    slow_poll: bool,
    supports_subscribe: bool,
    completion_mode: MockCompletionMode,
}

struct MockA2aServer {
    base_url: String,
    state: Arc<MockA2aState>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockA2aServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn mock_send_message(
    State(state): State<Arc<MockA2aState>>,
    Json(payload): Json<SendMessageRequest>,
) -> Json<SendMessageResponse> {
    let return_immediately = payload
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.return_immediately)
        .unwrap_or(false);
    state.send_requests.lock().push(payload);
    let turn = state.send_requests.lock().len();
    let task = if return_immediately {
        working_task()
    } else {
        task_for_mode(state.completion_mode, "remote-task-1", turn)
    };
    Json(SendMessageResponse {
        task: Some(task),
        message: None,
    })
}

async fn mock_get_task(
    Path(task_path): Path<String>,
    State(state): State<Arc<MockA2aState>>,
) -> Json<Task> {
    state.poll_count.fetch_add(1, Ordering::SeqCst);
    if state.slow_poll {
        state.slow_poll_started.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let task_id = task_path.trim_end_matches(":cancel");
    let turn = state.send_requests.lock().len();
    Json(task_for_mode(state.completion_mode, task_id, turn))
}

async fn mock_task_get(
    Path(task_path): Path<String>,
    State(state): State<Arc<MockA2aState>>,
) -> Response {
    if task_path.ends_with(":subscribe") && state.supports_subscribe {
        return mock_subscribe_task(Path(task_path), State(state)).await;
    }
    Json(mock_get_task(Path(task_path), State(state)).await.0).into_response()
}

async fn mock_subscribe_task(
    Path(task_path): Path<String>,
    State(state): State<Arc<MockA2aState>>,
) -> Response {
    state.subscribe_count.fetch_add(1, Ordering::SeqCst);
    let task_id = task_path.trim_end_matches(":subscribe");
    let turn = state.send_requests.lock().len();
    let task = task_for_mode(state.completion_mode, task_id, turn);
    let body = format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "task": task
        }))
        .expect("mock stream response")
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn mock_cancel_task(
    Path(task_path): Path<String>,
    State(state): State<Arc<MockA2aState>>,
) -> Json<Task> {
    let task_id = task_path.trim_end_matches(":cancel").to_string();
    state.cancel_requests.lock().push(task_id.clone());
    Json(Task {
        id: task_id.clone(),
        context_id: "remote-ctx-1".into(),
        status: TaskStatus {
            state: TaskState::Canceled,
            message: None,
            timestamp: None,
        },
        artifacts: vec![],
        history: vec![],
        metadata: None,
    })
}

async fn mock_fallback(State(state): State<Arc<MockA2aState>>, uri: Uri) -> (StatusCode, String) {
    state.unexpected_requests.lock().push(uri.to_string());
    (StatusCode::NOT_FOUND, "not found".to_string())
}

fn working_task() -> Task {
    Task {
        id: "remote-task-1".into(),
        context_id: "remote-ctx-1".into(),
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        },
        artifacts: vec![],
        history: vec![],
        metadata: None,
    }
}

fn completed_task(task_id: &str, text: &str) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: "remote-ctx-1".into(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: Some(A2aMessage {
                task_id: Some(task_id.to_string()),
                context_id: Some("remote-ctx-1".into()),
                message_id: format!("status-{task_id}"),
                role: MessageRole::Agent,
                parts: vec![Part::text(text)],
                metadata: None,
            }),
            timestamp: None,
        },
        artifacts: vec![Artifact {
            artifact_id: format!("artifact-{task_id}"),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
        }],
        history: vec![],
        metadata: None,
    }
}

fn interrupted_task(task_id: &str, state: TaskState, text: &str) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: "remote-ctx-1".into(),
        status: TaskStatus {
            state,
            message: Some(A2aMessage {
                task_id: Some(task_id.to_string()),
                context_id: Some("remote-ctx-1".into()),
                message_id: format!("status-{task_id}"),
                role: MessageRole::Agent,
                parts: vec![Part::text(text)],
                metadata: None,
            }),
            timestamp: None,
        },
        artifacts: vec![],
        history: vec![],
        metadata: None,
    }
}

fn task_for_mode(mode: MockCompletionMode, task_id: &str, turn: usize) -> Task {
    match mode {
        MockCompletionMode::Completed => {
            completed_task(task_id, &format!("hello from remote root turn {turn}"))
        }
        MockCompletionMode::InputRequired => interrupted_task(
            task_id,
            TaskState::InputRequired,
            &format!("need more input for turn {turn}"),
        ),
        MockCompletionMode::AuthRequired => interrupted_task(
            task_id,
            TaskState::AuthRequired,
            &format!("authentication required for turn {turn}"),
        ),
    }
}

async fn spawn_mock_a2a_server(
    completion_mode: MockCompletionMode,
    slow_poll: bool,
    supports_subscribe: bool,
) -> MockA2aServer {
    let state = Arc::new(MockA2aState {
        slow_poll,
        supports_subscribe,
        completion_mode,
        ..Default::default()
    });
    let router = Router::new()
        .route("/v1/a2a/message:send", post(mock_send_message))
        .route(
            "/v1/a2a/tasks/*task_path",
            get(mock_task_get).post(mock_cancel_task),
        )
        .fallback(mock_fallback)
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock a2a server");
    let addr = listener.local_addr().expect("mock server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("mock server");
    });

    MockA2aServer {
        base_url: format!("http://127.0.0.1:{}/v1/a2a", addr.port()),
        state,
        handle,
    }
}

struct TestApp {
    router: Router,
    store: Arc<InMemoryStore>,
    runtime: Arc<AgentRuntime>,
}

fn make_gateway_app_with_options(
    mock_base_url: &str,
    endpoint_options: BTreeMap<String, Value>,
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
            .with_provider("mock", Arc::new(ImmediateExecutor))
            .with_agent_spec(AgentSpec {
                id: "remote-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "remote".into(),
                endpoint: Some(RemoteEndpoint {
                    backend: "a2a".into(),
                    base_url: mock_base_url.to_string(),
                    timeout_ms: 5_000,
                    options: endpoint_options,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .with_thread_run_store(store.clone())
            .build()
            .expect("build gateway runtime"),
    );

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "gateway-test".to_string(),
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
        runtime,
    }
}

fn make_delegate_gateway_app(mock_base_url: &str) -> TestApp {
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
            .with_provider(
                "mock",
                Arc::new(DelegatingExecutor {
                    tool_name: "agent_run_remote-agent".into(),
                    call_count: AtomicUsize::new(0),
                }),
            )
            .with_agent_spec(AgentSpec {
                id: "orchestrator".into(),
                model_id: "test-model".into(),
                system_prompt: "delegate".into(),
                max_rounds: 2,
                delegates: vec!["remote-agent".into()],
                ..Default::default()
            })
            .with_agent_spec(AgentSpec {
                id: "remote-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "remote".into(),
                endpoint: Some(RemoteEndpoint {
                    backend: "a2a".into(),
                    base_url: mock_base_url.to_string(),
                    timeout_ms: 5_000,
                    options: BTreeMap::from([("poll_interval_ms".into(), json!(10_u64))]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .with_thread_run_store(store.clone())
            .build()
            .expect("build delegate gateway runtime"),
    );

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "gateway-test".to_string(),
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
        runtime,
    }
}

fn make_gateway_app(mock_base_url: &str) -> TestApp {
    make_gateway_app_with_options(
        mock_base_url,
        BTreeMap::from([("poll_interval_ms".into(), json!(10_u64))]),
    )
}

async fn post_json(app: Router, uri: &str, payload: Value) -> (StatusCode, String) {
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

fn request_text(request: &SendMessageRequest) -> String {
    request
        .message
        .parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn persisted_a2a_state(
    mock_base_url: &str,
    task_id: &str,
    context_id: &str,
    last_state: &str,
) -> PersistedState {
    let target_key = format!("a2a:{}", mock_base_url.trim_end_matches('/'));
    let mut targets = serde_json::Map::new();
    targets.insert(
        target_key,
        json!({
            "version": 1,
            "task_id": task_id,
            "context_id": context_id,
            "last_state": last_state,
            "updated_at_ms": 1_u64
        }),
    );

    PersistedState {
        revision: 1,
        extensions: HashMap::from([(
            "__runtime_remote_backend".to_string(),
            json!({
                "version": 1,
                "targets": Value::Object(targets)
            }),
        )]),
    }
}

fn remote_state_entry<'a>(state: &'a PersistedState, mock_base_url: &str) -> Option<&'a Value> {
    let target_key = format!("a2a:{}", mock_base_url.trim_end_matches('/'));
    state
        .extensions
        .get("__runtime_remote_backend")
        .and_then(|value| value.get("targets"))
        .and_then(|targets| targets.get(target_key))
}

struct SeedRemoteRun<'a> {
    thread_id: &'a str,
    run_id: &'a str,
    message: &'a str,
    status: RunStatus,
    termination_code: &'a str,
    updated_at: u64,
    state: PersistedState,
}

async fn seed_remote_run(store: &Arc<InMemoryStore>, seed: SeedRemoteRun<'_>) {
    store
        .checkpoint(
            seed.thread_id,
            &[Message::user(seed.message)],
            &RunRecord {
                run_id: seed.run_id.into(),
                thread_id: seed.thread_id.into(),
                agent_id: "remote-agent".into(),
                parent_run_id: None,
                status: seed.status,
                termination_code: Some(seed.termination_code.into()),
                created_at: seed.updated_at,
                updated_at: seed.updated_at,
                steps: 1,
                input_tokens: 0,
                output_tokens: 0,
                state: Some(seed.state),
            },
        )
        .await
        .expect("seed remote run");
}

fn assert_upstream_turn(
    request: &SendMessageRequest,
    expected_task_id: &str,
    expected_context_id: &str,
    expected_text: &str,
) {
    assert_eq!(
        request.message.task_id.as_deref(),
        Some(expected_task_id),
        "unexpected upstream task id"
    );
    assert_eq!(
        request.message.context_id.as_deref(),
        Some(expected_context_id),
        "unexpected upstream context id"
    );
    assert_eq!(request_text(request), expected_text);
}

fn assert_not_remote_handle(request: &SendMessageRequest, task_id: &str, context_id: &str) {
    assert_ne!(request.message.task_id.as_deref(), Some(task_id));
    assert_ne!(request.message.context_id.as_deref(), Some(context_id));
}

#[tokio::test]
async fn run_api_gateway_reuses_remote_task_context_across_turns() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, false).await;
    let test = make_gateway_app(&mock.base_url);

    let (status, first_body) = post_json(
        test.router.clone(),
        "/v1/runs",
        json!({
            "agentId": "remote-agent",
            "threadId": "gateway-thread",
            "messages": [{"role": "user", "content": "first turn"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {first_body}");

    let (status, second_body) = post_json(
        test.router.clone(),
        "/v1/runs",
        json!({
            "agentId": "remote-agent",
            "threadId": "gateway-thread",
            "messages": [{"role": "user", "content": "second turn"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {second_body}");

    let send_requests = mock.state.send_requests.lock().clone();
    assert_eq!(send_requests.len(), 2, "expected two upstream turns");
    let unexpected_requests = mock.state.unexpected_requests.lock().clone();
    assert!(
        unexpected_requests.is_empty(),
        "unexpected upstream requests: {:?}",
        unexpected_requests
    );

    let first = &send_requests[0];
    assert!(first.message.task_id.is_some());
    assert_eq!(first.message.context_id.as_deref(), Some("gateway-thread"));
    assert_eq!(request_text(first), "first turn");

    let second = &send_requests[1];
    assert_ne!(second.message.task_id.as_deref(), Some("remote-task-1"));
    assert!(second.message.task_id.is_some());
    assert_eq!(second.message.context_id.as_deref(), Some("remote-ctx-1"));
    assert_eq!(request_text(second), "second turn");

    let messages = test
        .store
        .load_messages("gateway-thread")
        .await
        .expect("message lookup")
        .expect("messages persisted");
    let assistant_messages = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .map(|message| message.text())
        .collect::<Vec<_>>();
    assert!(
        assistant_messages
            .iter()
            .any(|text| text.contains("hello from remote root turn 1"))
    );
    assert!(
        assistant_messages
            .iter()
            .any(|text| text.contains("hello from remote root turn 2"))
    );
}

#[tokio::test]
async fn run_api_gateway_prefers_upstream_subscribe_before_poll_fallback() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, true).await;
    let test = make_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "remote-agent",
            "threadId": "gateway-subscribe",
            "messages": [{"role": "user", "content": "subscribe please"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(mock.state.subscribe_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        mock.state.poll_count.load(Ordering::SeqCst),
        0,
        "subscribe-capable upstream should not require polling"
    );
}

#[tokio::test]
async fn ai_sdk_gateway_streams_remote_a2a_response() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, false).await;
    let test = make_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router.clone(),
        "/v1/ai-sdk/agents/remote-agent/runs",
        json!({
            "threadId": "gateway-ai-sdk",
            "messages": [
                {
                    "role": "user",
                    "parts": [{"type": "text", "text": "hello remote"}]
                }
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(body.contains("\"type\":\"text-delta\""));
    assert!(body.contains("hello from remote root turn 1"));
}

#[tokio::test]
async fn ag_ui_gateway_streams_remote_a2a_response() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, false).await;
    let test = make_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router,
        "/v1/ag-ui/agents/remote-agent/runs",
        json!({
            "threadId": "gateway-ag-ui",
            "messages": [{"role": "user", "content": "hello remote"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(body.contains("\"type\":\"RUN_STARTED\""));
    assert!(body.contains("hello from remote root turn 1"));
    assert!(body.contains("\"type\":\"RUN_FINISHED\""));
}

#[tokio::test]
async fn run_api_gateway_surfaces_delegate_tool_progress_for_remote_a2a_backend() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, true).await;
    let test = make_delegate_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router,
        "/v1/runs",
        json!({
            "agentId": "orchestrator",
            "threadId": "delegate-progress-run-api",
            "messages": [{"role": "user", "content": "delegate now"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"event_type\":\"activity_snapshot\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"activity_type\":\"tool-call-progress\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"running\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"done\""),
        "unexpected body: {body}"
    );
}

#[tokio::test]
async fn ai_sdk_gateway_surfaces_delegate_tool_progress_for_remote_a2a_backend() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, true).await;
    let test = make_delegate_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router,
        "/v1/ai-sdk/agents/orchestrator/runs",
        json!({
            "threadId": "delegate-progress-ai-sdk",
            "messages": [
                {
                    "role": "user",
                    "parts": [{"type": "text", "text": "delegate now"}]
                }
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"data-activity-snapshot\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"activityType\":\"tool-call-progress\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"running\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"done\""),
        "unexpected body: {body}"
    );
}

#[tokio::test]
async fn ag_ui_gateway_surfaces_delegate_tool_progress_for_remote_a2a_backend() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, true).await;
    let test = make_delegate_gateway_app(&mock.base_url);

    let (status, body) = post_json(
        test.router,
        "/v1/ag-ui/agents/orchestrator/runs",
        json!({
            "threadId": "delegate-progress-ag-ui",
            "messages": [{"role": "user", "content": "delegate now"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"ACTIVITY_SNAPSHOT\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"activityType\":\"tool-call-progress\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"running\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"status\":\"done\""),
        "unexpected body: {body}"
    );
}

#[tokio::test]
async fn run_api_gateway_cancel_propagates_remote_a2a_cancel() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, true, false).await;
    let test = make_gateway_app(&mock.base_url);
    let router = test.router.clone();

    let run_task = tokio::spawn(async move {
        post_json(
            router,
            "/v1/runs",
            json!({
                "agentId": "remote-agent",
                "threadId": "gateway-cancel",
                "messages": [{"role": "user", "content": "cancel me"}]
            }),
        )
        .await
    });

    for _ in 0..50 {
        if mock.state.slow_poll_started.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mock.state.slow_poll_started.load(Ordering::SeqCst),
        "gateway never started polling upstream task"
    );

    let (status, cancel_body) = post_json(
        test.router.clone(),
        "/v1/threads/gateway-cancel/cancel",
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "unexpected body: {cancel_body}"
    );

    let (run_status, run_body) = tokio::time::timeout(Duration::from_secs(2), run_task)
        .await
        .expect("run request should finish after cancellation")
        .expect("join should succeed");
    assert_eq!(run_status, StatusCode::OK, "unexpected body: {run_body}");
    let events = extract_sse_events(&run_body);
    let run_finish = events
        .iter()
        .find(|event| event["event_type"].as_str() == Some("run_finish"))
        .expect("run_finish event");
    assert_eq!(
        run_finish["termination"]["type"].as_str(),
        Some("cancelled"),
        "unexpected run finish payload: {run_body}"
    );

    let cancel_requests = mock.state.cancel_requests.lock();
    assert_eq!(cancel_requests.as_slice(), ["remote-task-1"]);
}

#[tokio::test]
async fn run_api_gateway_persists_remote_handle_after_submit_before_completion() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, true, false).await;
    let test = make_gateway_app(&mock.base_url);
    let router = test.router.clone();

    let run_task = tokio::spawn(async move {
        post_json(
            router,
            "/v1/runs",
            json!({
                "agentId": "remote-agent",
                "threadId": "gateway-accepted-checkpoint",
                "messages": [{"role": "user", "content": "persist handle before poll completes"}]
            }),
        )
        .await
    });

    for _ in 0..50 {
        if mock.state.slow_poll_started.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mock.state.slow_poll_started.load(Ordering::SeqCst),
        "gateway never started polling upstream task"
    );

    let accepted = test
        .store
        .latest_run("gateway-accepted-checkpoint")
        .await
        .expect("load accepted run")
        .expect("accepted checkpoint should exist before completion");
    assert_eq!(accepted.status, RunStatus::Running);
    let state = accepted.state.as_ref().expect("accepted state");
    let remote = remote_state_entry(state, &mock.base_url).expect("remote handle state");
    assert_eq!(remote["task_id"].as_str(), Some("remote-task-1"));
    assert_eq!(remote["context_id"].as_str(), Some("remote-ctx-1"));
    assert_eq!(remote["last_state"].as_str(), Some("TASK_STATE_WORKING"));

    let (status, cancel_body) = post_json(
        test.router.clone(),
        "/v1/threads/gateway-accepted-checkpoint/cancel",
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "unexpected body: {cancel_body}"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), run_task)
        .await
        .expect("run request should finish after cancellation")
        .expect("join should succeed");
}

#[tokio::test]
async fn a2a_backend_abort_uses_persisted_interrupted_task_without_in_flight_entry() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, false).await;
    let backend = A2aBackendFactory
        .build(&RemoteEndpoint {
            backend: "a2a".into(),
            base_url: mock.base_url.clone(),
            ..Default::default()
        })
        .expect("build A2A backend");
    let persisted = persisted_a2a_state(
        &mock.base_url,
        "persisted-waiting-task",
        "persisted-context",
        "TASK_STATE_INPUT_REQUIRED",
    );
    let run_identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-without-in-flight-entry".into(),
        None,
        "remote-agent".into(),
        RunOrigin::User,
    );

    backend
        .abort(BackendAbortRequest {
            agent_id: "remote-agent",
            run_identity: &run_identity,
            parent: None,
            persisted_state: Some(&persisted),
            is_continuation: false,
        })
        .await
        .expect("abort should use persisted remote task id");

    let cancel_requests = mock.state.cancel_requests.lock();
    assert_eq!(cancel_requests.as_slice(), ["persisted-waiting-task"]);
}

#[tokio::test]
async fn gateway_continuation_uses_requested_run_remote_state_not_latest_run() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::Completed, false, false).await;
    let test = make_gateway_app(&mock.base_url);
    let old_state = persisted_a2a_state(
        &mock.base_url,
        "older-waiting-task",
        "older-context",
        "TASK_STATE_INPUT_REQUIRED",
    );
    let latest_state = persisted_a2a_state(
        &mock.base_url,
        "latest-completed-task",
        "latest-context",
        "TASK_STATE_COMPLETED",
    );

    seed_remote_run(
        &test.store,
        SeedRemoteRun {
            thread_id: "gateway-explicit-continue",
            run_id: "older-waiting-run",
            message: "old waiting turn",
            status: RunStatus::Waiting,
            termination_code: "input_required",
            updated_at: 1,
            state: old_state,
        },
    )
    .await;
    seed_remote_run(
        &test.store,
        SeedRemoteRun {
            thread_id: "gateway-explicit-continue",
            run_id: "latest-completed-run",
            message: "latest completed turn",
            status: RunStatus::Done,
            termination_code: "natural",
            updated_at: 2,
            state: latest_state,
        },
    )
    .await;

    test.runtime
        .run(
            RunRequest::new(
                "gateway-explicit-continue",
                vec![Message::user("resume the older waiting run")],
            )
            .with_agent_id("remote-agent")
            .with_continue_run_id("older-waiting-run"),
            Arc::new(NullEventSink),
        )
        .await
        .expect("explicit older continuation should run");

    let send_requests = mock.state.send_requests.lock().clone();
    assert_eq!(send_requests.len(), 1, "expected one upstream turn");
    let upstream = &send_requests[0];
    assert_upstream_turn(
        upstream,
        "older-waiting-task",
        "older-context",
        "resume the older waiting run",
    );
    assert_not_remote_handle(upstream, "latest-completed-task", "latest-context");
}

#[tokio::test]
async fn run_api_gateway_maps_remote_input_required_to_waiting() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::InputRequired, false, false).await;
    let test = make_gateway_app_with_options(
        &mock.base_url,
        BTreeMap::from([
            ("poll_interval_ms".into(), json!(10_u64)),
            ("history_length".into(), json!(2_u64)),
            ("return_immediately".into(), json!(false)),
        ]),
    );

    let (status, body) = post_json(
        test.router.clone(),
        "/v1/runs",
        json!({
            "agentId": "remote-agent",
            "threadId": "gateway-input-required",
            "messages": [{"role": "user", "content": "first turn"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    let events = extract_sse_events(&body);
    let run_finish = events
        .iter()
        .find(|event| event["event_type"].as_str() == Some("run_finish"))
        .expect("run_finish event");
    assert_eq!(
        run_finish["termination"]["type"].as_str(),
        Some("suspended"),
        "unexpected run finish payload: {body}"
    );
    assert_eq!(
        run_finish["result"]["status_reason"].as_str(),
        Some("input_required")
    );

    let latest_run = test
        .store
        .latest_run("gateway-input-required")
        .await
        .expect("latest run lookup")
        .expect("persisted run");
    assert_eq!(latest_run.status, RunStatus::Waiting);
    assert_eq!(
        latest_run.termination_code.as_deref(),
        Some("input_required")
    );

    let send_requests = mock.state.send_requests.lock();
    let cfg = send_requests[0]
        .configuration
        .as_ref()
        .expect("sendMessage configuration");
    assert_eq!(cfg.history_length, Some(2));
    assert_eq!(cfg.return_immediately, Some(false));
}

#[tokio::test]
async fn ai_sdk_gateway_preserves_suspended_finish_event_for_remote_input_required() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::InputRequired, false, false).await;
    let test = make_gateway_app_with_options(
        &mock.base_url,
        BTreeMap::from([("return_immediately".into(), json!(false))]),
    );

    let (status, body) = post_json(
        test.router.clone(),
        "/v1/ai-sdk/agents/remote-agent/runs",
        json!({
            "threadId": "gateway-ai-sdk-input-required",
            "messages": [
                {
                    "role": "user",
                    "parts": [{"type": "text", "text": "hello remote"}]
                }
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"finish\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"finishReason\":\"tool-calls\""),
        "unexpected body: {body}"
    );
}

#[tokio::test]
async fn ag_ui_gateway_preserves_run_finished_event_for_remote_auth_required() {
    let mock = spawn_mock_a2a_server(MockCompletionMode::AuthRequired, false, false).await;
    let test = make_gateway_app_with_options(
        &mock.base_url,
        BTreeMap::from([("return_immediately".into(), json!(false))]),
    );

    let (status, body) = post_json(
        test.router,
        "/v1/ag-ui/agents/remote-agent/runs",
        json!({
            "threadId": "gateway-ag-ui-auth-required",
            "messages": [{"role": "user", "content": "hello remote"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert!(
        body.contains("\"type\":\"RUN_FINISHED\""),
        "unexpected body: {body}"
    );
    assert!(
        body.contains("\"type\":\"TEXT_MESSAGE_END\""),
        "unexpected body: {body}"
    );
}
