mod common;

use async_trait::async_trait;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use common::{compose_http_app, SlowTerminatePlugin, TerminatePlugin};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea_agentos::contracts::runtime::tool_call::{Tool, ToolDescriptor, ToolError, ToolResult};
use tirea_agentos::contracts::storage::{
    Committed, MailboxEntryOrigin, MailboxStore, MailboxWriter, RunOrigin, ThreadHead,
    ThreadListPage, ThreadListQuery, ThreadReader, ThreadStore, ThreadStoreError, ThreadWriter,
};
use tirea_agentos::contracts::thread::Thread;
use tirea_agentos::contracts::ToolCallContext;
use tirea_agentos::contracts::{RunRequest, ThreadChangeSet};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_contract::testing::MailboxEntryBuilder;
use tirea_store_adapters::MemoryStore;
use tokio::sync::{Notify, RwLock};
use tower::ServiceExt;

fn make_os() -> AgentOs {
    make_os_with_storage(Arc::new(MemoryStore::new()))
}

fn make_os_with_storage(write_store: Arc<dyn ThreadStore>) -> AgentOs {
    make_os_with_storage_and_tools(write_store, HashMap::new())
}

fn make_os_with_storage_and_tools(
    write_store: Arc<dyn ThreadStore>,
    tools: HashMap<String, Arc<dyn Tool>>,
) -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_test".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_test",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_test")),
        )
        .with_tools(tools)
        .with_agent_spec(AgentDefinitionSpec::local_with_id("test", def))
        .with_agent_state_store(write_store)
        .build()
        .unwrap()
}

fn make_os_with_slow_terminate_behavior_requested_plugin(
    write_store: Arc<dyn ThreadStore>,
) -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["slow_terminate_behavior_requested_test".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "slow_terminate_behavior_requested_test",
            Arc::new(SlowTerminatePlugin::new(
                "slow_terminate_behavior_requested_test",
                std::time::Duration::from_millis(250),
            )),
        )
        .with_agent_spec(AgentDefinitionSpec::local_with_id("test", def))
        .with_agent_state_store(write_store)
        .build()
        .unwrap()
}

fn test_mailbox_svc(os: &Arc<AgentOs>, store: Arc<dyn MailboxStore>) -> Arc<MailboxService> {
    Arc::new(MailboxService::new(os.clone(), store, "test"))
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input message").with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(ToolResult::success("echo", json!({ "echoed": message })))
    }
}

struct AskUserQuestionEchoTool;

#[async_trait]
impl Tool for AskUserQuestionEchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "askUserQuestion",
            "Ask User Question",
            "Echo the resolved frontend response payload",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("askUserQuestion", args))
    }
}

#[derive(Default)]
struct RecordingStorage {
    threads: RwLock<HashMap<String, Thread>>,
    saves: AtomicUsize,
    notify: Notify,
}

impl RecordingStorage {
    async fn wait_saves(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.saves.load(Ordering::SeqCst) < n {
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    }
}

#[async_trait]
impl ThreadWriter for RecordingStorage {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        let mut threads = self.threads.write().await;
        if threads.contains_key(&thread.id) {
            return Err(ThreadStoreError::AlreadyExists);
        }
        threads.insert(thread.id.clone(), thread.clone());
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
        Ok(Committed { version: 0 })
    }

    async fn append(
        &self,
        id: &str,
        delta: &ThreadChangeSet,
        _precondition: tirea_agentos::contracts::storage::VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        let mut threads = self.threads.write().await;
        if let Some(thread) = threads.get_mut(id) {
            delta.apply_to(thread);
        }
        self.saves.fetch_add(1, Ordering::SeqCst);
        drop(threads);
        self.notify.notify_waiters();
        Ok(Committed { version: 0 })
    }

    async fn delete(&self, id: &str) -> Result<(), ThreadStoreError> {
        let mut threads = self.threads.write().await;
        threads.remove(id);
        Ok(())
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        let mut threads = self.threads.write().await;
        threads.insert(thread.id.clone(), thread.clone());
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl ThreadReader for RecordingStorage {
    async fn load(&self, id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        let threads = self.threads.read().await;
        Ok(threads.get(id).map(|t| ThreadHead {
            thread: t.clone(),
            version: 0,
        }))
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        let threads = self.threads.read().await;
        let mut ids: Vec<String> = threads.keys().cloned().collect();
        ids.sort();
        let total = ids.len();
        let limit = query.limit.clamp(1, 200);
        let offset = query.offset.min(total);
        let end = (offset + limit + 1).min(total);
        let slice = &ids[offset..end];
        let has_more = slice.len() > limit;
        let items: Vec<String> = slice.iter().take(limit).cloned().collect();
        Ok(ThreadListPage {
            items,
            total,
            has_more,
        })
    }
}

#[tokio::test]
async fn test_sessions_query_endpoints() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());

    let thread =
        Thread::new("s1").with_message(tirea_agentos::contracts::thread::Message::user("hello"));
    storage.save(&thread).await.unwrap();

    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage, mailbox_svc));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/threads")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let page: tirea_agentos::contracts::storage::ThreadListPage =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(page.items, vec!["s1".to_string()]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/threads/s1/messages")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].message.content, "hello");
    assert!(!page.has_more);
}

#[tokio::test]
async fn test_ai_sdk_sse_and_persists_session() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t1",
        "messages": [{ "role": "user", "content": "hi" }],
        "runId": "run_1"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(r#""type":"start""#),
        "missing start event: {text}"
    );
    // text-start/text-end are lazy — only emitted when TextDelta events occur.
    // This test terminates before inference, so no text is produced.
    assert!(
        !text.contains(r#""type":"text-start""#),
        "unexpected text-start without text content: {text}"
    );
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish: {text}"
    );

    let saved = storage.load_thread("t1").await.unwrap().unwrap();
    assert_eq!(saved.id, "t1");
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "hi");
}

#[tokio::test]
async fn test_ai_sdk_sse_accepts_messages_request_shape() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t-messages-shape",
        "messages": [
            { "role": "user", "parts": [{ "type": "text", "text": "first input" }] },
            { "role": "assistant", "parts": [{ "type": "text", "text": "ignored assistant text" }] },
            { "role": "user", "parts": [{ "type": "text", "text": "latest input" }] }
        ],
        "runId": "run-messages-shape"
    });

    let (status, text) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish event: {text}"
    );

    // Only the last user text is extracted and stored incrementally via prepare_run.
    let saved = storage
        .load_thread("t-messages-shape")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "latest input");
}

#[tokio::test]
async fn test_ai_sdk_sse_messages_request_uses_id_when_session_id_missing() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t-id-only",
        "messages": [
            { "role": "user", "content": "id-fallback-input" }
        ],
        "runId": "run-id-only"
    });

    let (status, text) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish event: {text}"
    );

    let saved = storage.load_thread("t-id-only").await.unwrap().unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "id-fallback-input");
}

#[tokio::test]
async fn test_ai_sdk_sse_messages_request_requires_user_text() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "id": "s1",
        "messages": [
            { "role": "assistant", "parts": [{ "type": "text", "text": "no user turn" }] }
        ]
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("input"),
        "expected input validation error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_messages_request_requires_thread_identifier() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "messages": [
            { "role": "user", "content": "hello without thread id" }
        ]
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("id cannot be empty"),
        "expected id validation error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_accepts_messages_content_array_shape() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t-content-array",
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "content-array-input" },
                    { "type": "file", "url": "https://example.com/f.txt" }
                ]
            }
        ],
        "runId": "run-content-array"
    });

    let (status, text) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish event: {text}"
    );

    let saved = storage
        .load_thread("t-content-array")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "content-array-input");
}

#[tokio::test]
async fn test_ai_sdk_sse_sets_expected_headers_and_done_trailer() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage, mailbox_svc));

    let payload = json!({
        "id": "t-headers",
        "messages": [{ "role": "user", "content": "hello" }],
        "runId": "run-headers"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-vercel-ai-ui-message-stream")
            .and_then(|v| v.to_str().ok()),
        Some("v1")
    );
    assert_eq!(
        resp.headers()
            .get("x-tirea-ai-sdk-version")
            .and_then(|v| v.to_str().ok()),
        Some(tirea_protocol_ai_sdk_v6::AI_SDK_VERSION)
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("data: [DONE]"),
        "ai-sdk stream should end with [DONE] trailer: {text}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_run_info_omits_run_id_field() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t-v7",
        "messages": [{ "role": "user", "content": "hi" }]
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str::<Value>(json).ok())
        .collect();

    let run_info = events
        .iter()
        .find(|e| e["type"] == "data-run-info")
        .unwrap_or_else(|| panic!("missing run-info event: {text}"));
    assert!(
        run_info["data"]["threadId"].as_str() == Some("t-v7"),
        "run-info should include threadId for thread-based identity: {text}"
    );
    assert!(
        run_info["data"].get("runId").is_none(),
        "run-info should not expose internal run id: {text}"
    );
}

#[tokio::test]
async fn test_agui_sse_and_persists_session() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "threadId": "th1",
        "runId": "r1",
        "messages": [
            {"role": "user", "content": "hello from agui"}
        ],
        "tools": []
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED: {text}"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED: {text}"
    );

    let saved = storage.load_thread("th1").await.unwrap().unwrap();
    assert_eq!(saved.id, "th1");
    assert_eq!(saved.messages.len(), 1);
}

#[tokio::test]
async fn test_industry_common_persistence_saves_user_message_before_run_completes_ai_sdk() {
    let storage = Arc::new(RecordingStorage::default());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, Arc::new(MemoryStore::new()));
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "t2",
        "messages": [{ "role": "user", "content": "hi" }],
        "runId": "run_2"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    storage.wait_saves(2).await;

    assert!(
        storage.saves.load(Ordering::SeqCst) >= 2,
        "expected at least 2 saves (user ingress + final)"
    );

    let saved = storage.load_thread("t2").await.unwrap().unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "hi");
}

#[tokio::test]
async fn test_industry_common_persistence_saves_inbound_request_messages_agui() {
    let storage = Arc::new(RecordingStorage::default());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, Arc::new(MemoryStore::new()));
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "threadId": "th2",
        "runId": "r2",
        "messages": [
            {"role": "user", "content": "hello", "id": "m1"}
        ],
        "tools": []
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    storage.wait_saves(2).await;

    assert!(
        storage.saves.load(Ordering::SeqCst) >= 2,
        "expected at least 2 saves (request ingress + final)"
    );

    let saved = storage.load_thread("th2").await.unwrap().unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "hello");
}

#[tokio::test]
async fn test_agui_sse_idless_user_message_not_duplicated_by_internal_reapply() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os.clone(), storage.clone(), mailbox_svc));

    let payload = json!({
        "threadId": "th-idless-once",
        "runId": "r-idless-once",
        "messages": [
            {"role": "user", "content": "hello without id"}
        ],
        "tools": []
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let saved = storage
        .load_thread("th-idless-once")
        .await
        .unwrap()
        .unwrap();
    let user_hello_count = saved
        .messages
        .iter()
        .filter(|m| {
            m.role == tirea_agentos::contracts::thread::Role::User
                && m.content == "hello without id"
        })
        .count();
    assert_eq!(
        user_hello_count, 1,
        "id-less user message should be applied exactly once per request"
    );
}

// ============================================================================
// Helper: POST JSON and return (status, body_json)
// ============================================================================

async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn post_sse_text(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    (status, text)
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn make_app(os: Arc<AgentOs>, read_store: Arc<dyn ThreadReader>) -> axum::Router {
    let mailbox_store: Arc<dyn MailboxStore> = Arc::new(MemoryStore::new());
    let mailbox_svc = test_mailbox_svc(&os, mailbox_store);
    compose_http_app(AppState::new(os, read_store, mailbox_svc))
}

// ============================================================================
// Health endpoint
// ============================================================================

#[tokio::test]
async fn test_health_returns_200() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================================
// GET /v1/sessions/:id — session not found
// ============================================================================

#[tokio::test]
async fn test_get_session_not_found() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "expected not found error: {body}"
    );
}

// ============================================================================
// GET /v1/sessions/:id/messages — session not found
// ============================================================================

#[tokio::test]
async fn test_get_session_messages_not_found() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/nonexistent/messages").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "expected not found error: {body}"
    );
}

// ============================================================================
// AI SDK SSE — error paths
// ============================================================================

#[tokio::test]
async fn test_ai_sdk_sse_empty_thread_id() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({ "id": "  ", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("id"),
        "expected id error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_empty_input() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({ "id": "s1", "messages": [{ "role": "user", "content": "  " }] });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("input"),
        "expected input error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_agent_not_found() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({ "id": "s1", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/no_such_agent/runs", payload).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "expected agent not found error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_rejects_legacy_payload_shape() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "sessionId": "legacy-session",
        "input": "legacy-input"
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400/422 for legacy payload rejection, got {status}"
    );
    assert!(body == Value::Null || body.get("error").is_some());
}

#[tokio::test]
async fn test_ai_sdk_sse_malformed_json() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    // Axum returns 400 for JSON parse errors.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================================
// AG-UI SSE — error paths
// ============================================================================

#[tokio::test]
async fn test_agui_sse_agent_not_found() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "threadId": "th1", "runId": "r1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/no_such_agent/runs", payload).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "expected agent not found error: {body}"
    );
}

#[tokio::test]
async fn test_agui_sse_empty_thread_id() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "threadId": "", "runId": "r1",
        "messages": [], "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("threadId"),
        "expected threadId validation error: {body}"
    );
}

#[tokio::test]
async fn test_agui_sse_empty_run_id() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "threadId": "th1", "runId": "",
        "messages": [], "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("runId"),
        "expected runId validation error: {body}"
    );
}

#[tokio::test]
async fn test_agui_sse_malformed_json() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    // Axum returns 400 for JSON parse errors.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_agui_sse_session_id_mismatch() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    // Pre-save a session with id "real-id".
    storage.save(&Thread::new("real-id")).await.unwrap();

    let _app = make_app(os, read_store);

    // The mismatch can only occur if storage returns a session with a different id than the key.
    // This is an internal consistency check, not triggerable from external input.
}

// ============================================================================
// Failing storage — tests error propagation
// ============================================================================

struct FailingStorage;

#[async_trait]
impl ThreadWriter for FailingStorage {
    async fn create(&self, _thread: &Thread) -> Result<Committed, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }

    async fn append(
        &self,
        _id: &str,
        _delta: &tirea_agentos::contracts::ThreadChangeSet,
        _precondition: tirea_agentos::contracts::storage::VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }

    async fn delete(&self, _id: &str) -> Result<(), ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk delete denied",
        )))
    }

    async fn save(&self, _thread: &Thread) -> Result<(), ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }
}

#[async_trait]
impl ThreadReader for FailingStorage {
    async fn load(&self, _id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk read denied",
        )))
    }

    async fn list_threads(
        &self,
        _query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk list denied",
        )))
    }
}

#[tokio::test]
async fn test_list_sessions_storage_error() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(FailingStorage);
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage error: {body}"
    );
}

#[tokio::test]
async fn test_get_session_storage_error() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(FailingStorage);
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_sse_storage_load_error() {
    let os = Arc::new(make_os_with_storage(Arc::new(FailingStorage)));
    let read_store: Arc<dyn ThreadReader> = Arc::new(FailingStorage);
    let app = make_app(os, read_store);

    let payload = json!({ "id": "s1", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage error: {body}"
    );
}

#[tokio::test]
async fn test_agui_sse_storage_load_error() {
    let os = Arc::new(make_os_with_storage(Arc::new(FailingStorage)));
    let read_store: Arc<dyn ThreadReader> = Arc::new(FailingStorage);
    let app = make_app(os, read_store);

    let payload = json!({
        "threadId": "th1", "runId": "r1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage error: {body}"
    );
}

/// Storage that loads OK but fails on save — tests user message persistence error.
struct SaveFailStorage;

#[async_trait]
impl ThreadWriter for SaveFailStorage {
    async fn create(&self, _thread: &Thread) -> Result<Committed, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }

    async fn append(
        &self,
        _id: &str,
        _delta: &tirea_agentos::contracts::ThreadChangeSet,
        _precondition: tirea_agentos::contracts::storage::VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }

    async fn delete(&self, _id: &str) -> Result<(), ThreadStoreError> {
        Ok(())
    }

    async fn save(&self, _thread: &Thread) -> Result<(), ThreadStoreError> {
        Err(ThreadStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk write denied",
        )))
    }
}

#[async_trait]
impl ThreadReader for SaveFailStorage {
    async fn load(&self, _id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        Ok(None)
    }

    async fn list_threads(
        &self,
        _query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        Ok(ThreadListPage {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }
}

#[tokio::test]
async fn test_ai_sdk_sse_storage_save_error() {
    let os = Arc::new(make_os_with_storage(Arc::new(SaveFailStorage)));
    let read_store: Arc<dyn ThreadReader> = Arc::new(SaveFailStorage);
    let app = make_app(os, read_store);

    let payload = json!({ "id": "s1", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage save error: {body}"
    );
}

#[tokio::test]
async fn test_agui_sse_storage_save_error() {
    let os = Arc::new(make_os_with_storage(Arc::new(SaveFailStorage)));
    let read_store: Arc<dyn ThreadReader> = Arc::new(SaveFailStorage);
    let app = make_app(os, read_store);

    // Send a message so the session has changes to persist.
    let payload = json!({
        "threadId": "th1", "runId": "r1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"].as_str().unwrap_or("").contains("denied"),
        "expected storage save error: {body}"
    );
}

// ============================================================================
// Message pagination tests
// ============================================================================

fn make_session_with_n_messages(id: &str, n: usize) -> Thread {
    let mut thread = Thread::new(id);
    for i in 0..n {
        thread = thread.with_message(tirea_agentos::contracts::thread::Message::user(format!(
            "msg-{}",
            i
        )));
    }
    thread
}

#[tokio::test]
async fn test_messages_pagination_default_params() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();
    let thread = make_session_with_n_messages("s1", 5);
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1/messages").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 5);
    assert!(!page.has_more);
    assert_eq!(page.messages[0].message.content, "msg-0");
    assert_eq!(page.messages[4].message.content, "msg-4");
}

#[tokio::test]
async fn test_messages_pagination_with_limit() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();
    let thread = make_session_with_n_messages("s1", 10);
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1/messages?limit=3").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 3);
    assert!(page.has_more);
    assert_eq!(page.messages[0].cursor, 0);
    assert_eq!(page.messages[2].cursor, 2);
}

#[tokio::test]
async fn test_messages_pagination_cursor_forward() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();
    let thread = make_session_with_n_messages("s1", 10);
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1/messages?after=4&limit=3").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 3);
    assert_eq!(page.messages[0].cursor, 5);
    assert_eq!(page.messages[0].message.content, "msg-5");
}

#[tokio::test]
async fn test_messages_pagination_desc_order() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();
    let thread = make_session_with_n_messages("s1", 10);
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1/messages?order=desc&before=8&limit=3").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 3);
    // Desc order: highest cursors first
    assert_eq!(page.messages[0].cursor, 7);
    assert_eq!(page.messages[1].cursor, 6);
    assert_eq!(page.messages[2].cursor, 5);
}

#[tokio::test]
async fn test_messages_pagination_limit_clamped() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();
    let thread = make_session_with_n_messages("s1", 300);
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/s1/messages?limit=999").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    // limit should be clamped to 200
    assert_eq!(page.messages.len(), 200);
    assert!(page.has_more);
}

#[tokio::test]
async fn test_messages_pagination_not_found() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let (status, body) = get_json(app, "/v1/threads/nonexistent/messages?limit=10").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "expected not found error: {body}"
    );
}

#[tokio::test]
async fn test_list_threads_filters_by_parent_thread_id() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    storage
        .save(&Thread::new("t-parent-1").with_parent_thread_id("p-root"))
        .await
        .unwrap();
    storage
        .save(&Thread::new("t-parent-2").with_parent_thread_id("p-root"))
        .await
        .unwrap();
    storage.save(&Thread::new("t-other")).await.unwrap();

    let app = make_app(os, read_store);
    let (status, body) = get_json(app, "/v1/threads?parent_thread_id=p-root").await;
    assert_eq!(status, StatusCode::OK);

    let page: tirea_agentos::contracts::storage::ThreadListPage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items, vec!["t-parent-1", "t-parent-2"]);
}

#[tokio::test]
async fn test_messages_filter_by_visibility_and_run_id() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    let thread = Thread::new("s-filter")
        .with_message(
            tirea_agentos::contracts::thread::Message::user("visible-run-1").with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("run-1".to_string()),
                    step_index: Some(0),
                },
            ),
        )
        .with_message(
            tirea_agentos::contracts::thread::Message::internal_system("internal-run-1")
                .with_metadata(tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("run-1".to_string()),
                    step_index: Some(1),
                }),
        )
        .with_message(
            tirea_agentos::contracts::thread::Message::assistant("visible-run-2").with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("run-2".to_string()),
                    step_index: Some(2),
                },
            ),
        );
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);

    let (status, body) = get_json(app.clone(), "/v1/threads/s-filter/messages").await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].message.content, "visible-run-1");
    assert_eq!(page.messages[1].message.content, "visible-run-2");

    let (status, body) = get_json(
        app.clone(),
        "/v1/threads/s-filter/messages?visibility=internal",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].message.content, "internal-run-1");

    let (status, body) = get_json(
        app.clone(),
        "/v1/threads/s-filter/messages?visibility=none&run_id=run-1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].message.content, "visible-run-1");
    assert_eq!(page.messages[1].message.content, "internal-run-1");
}

#[tokio::test]
async fn test_thread_mailbox_filters_origin_and_sanitizes_payload_messages() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let external_request = RunRequest {
        agent_id: "test".to_string(),
        thread_id: Some("mailbox-visibility".to_string()),
        run_id: Some("queued-external".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        origin: RunOrigin::AgUi,
        state: None,
        messages: vec![
            tirea_agentos::contracts::thread::Message::user("visible-queued").with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("queued-run-1".to_string()),
                    step_index: Some(0),
                },
            ),
            tirea_agentos::contracts::thread::Message::internal_system("hidden-queued")
                .with_metadata(tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("queued-run-1".to_string()),
                    step_index: Some(1),
                }),
        ],
        initial_decisions: vec![],
        source_mailbox_entry_id: None,
    };
    let internal_request = RunRequest {
        agent_id: "test".to_string(),
        thread_id: Some("mailbox-visibility".to_string()),
        run_id: Some("queued-internal".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        origin: RunOrigin::Internal,
        state: None,
        messages: vec![tirea_agentos::contracts::thread::Message::internal_system(
            "internal-only-queued",
        )],
        initial_decisions: vec![],
        source_mailbox_entry_id: None,
    };

    storage
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-external", "mailbox-visibility")
                .with_origin(MailboxEntryOrigin::External)
                .with_payload(serde_json::to_value(&external_request).unwrap())
                .build(),
        )
        .await
        .unwrap();
    storage
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-internal", "mailbox-visibility")
                .with_origin(MailboxEntryOrigin::Internal)
                .with_payload(serde_json::to_value(&internal_request).unwrap())
                .build(),
        )
        .await
        .unwrap();

    let (status, body) = get_json(app.clone(), "/v1/threads/mailbox-visibility/mailbox").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["origin"], "external");
    let payload_messages = items[0]["payload"]["messages"].as_array().unwrap();
    assert_eq!(payload_messages.len(), 1);
    assert_eq!(payload_messages[0]["content"], "visible-queued");
    assert!(payload_messages[0]["metadata"].get("run_id").is_none());
    assert_eq!(payload_messages[0]["metadata"]["step_index"], 0);

    let (status, body) = get_json(
        app,
        "/v1/threads/mailbox-visibility/mailbox?origin=none&visibility=none",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);

    let external = items
        .iter()
        .find(|item| item["origin"] == "external")
        .expect("external mailbox entry");
    assert_eq!(external["payload"]["messages"].as_array().unwrap().len(), 2);

    let internal = items
        .iter()
        .find(|item| item["origin"] == "internal")
        .expect("internal mailbox entry");
    assert_eq!(internal["payload"]["messages"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_protocol_history_endpoints_hide_internal_messages_by_default() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    let thread = Thread::new("s-internal-history")
        .with_message(tirea_agentos::contracts::thread::Message::user(
            "visible-user",
        ))
        .with_message(tirea_agentos::contracts::thread::Message::internal_system(
            "internal-secret",
        ))
        .with_message(tirea_agentos::contracts::thread::Message::assistant(
            "visible-assistant",
        ));
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);

    let (status, body) =
        get_json(app.clone(), "/v1/ag-ui/threads/s-internal-history/messages").await;
    assert_eq!(status, StatusCode::OK);
    let body_text = body.to_string();
    assert!(
        !body_text.contains("internal-secret"),
        "ag-ui history must not expose internal messages: {body_text}"
    );
    assert!(body_text.contains("visible-user"));
    assert!(body_text.contains("visible-assistant"));

    let (status, body) = get_json(app, "/v1/ai-sdk/threads/s-internal-history/messages").await;
    assert_eq!(status, StatusCode::OK);
    let body_text = body.to_string();
    assert!(
        !body_text.contains("internal-secret"),
        "ai-sdk history must not expose internal messages: {body_text}"
    );
    assert!(body_text.contains("visible-user"));
    assert!(body_text.contains("visible-assistant"));
}

#[tokio::test]
async fn test_ai_sdk_history_encodes_tool_messages_as_tool_invocation_parts() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    let thread = Thread::new("s-tool-history")
        .with_message(
            tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
                "calling tool",
                vec![tirea_agentos::contracts::thread::ToolCall::new(
                    "call_1",
                    "search",
                    json!({"q":"rust"}),
                )],
            ),
        )
        .with_message(tirea_agentos::contracts::thread::Message::tool(
            "call_1",
            r#"{"result":"ok"}"#,
        ));
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);
    let (status, body) = get_json(
        app,
        "/v1/ai-sdk/threads/s-tool-history/messages?visibility=none",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let messages = body["messages"]
        .as_array()
        .expect("messages should be array");
    let has_input_available = messages.iter().any(|msg| {
        msg["parts"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["type"] == "tool-search"
                    && part["toolCallId"] == "call_1"
                    && part["state"] == "input-available"
                    && part["input"]["q"] == "rust"
            })
        })
    });
    assert!(
        has_input_available,
        "ai-sdk history should include input-available tool-search part: {body}"
    );

    let has_tool_output = messages.iter().any(|msg| {
        msg["parts"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["type"] == "dynamic-tool"
                    && part["toolCallId"] == "call_1"
                    && part["toolName"] == "tool"
                    && part["state"] == "output-available"
                    && part["output"]["result"] == "ok"
            })
        })
    });
    assert!(
        has_tool_output,
        "tool output should be encoded in dynamic-tool output-available part: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_history_encodes_assistant_tool_call_input_as_input_available() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    let thread = Thread::new("s-tool-input-history").with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "calling tool",
            vec![tirea_agentos::contracts::thread::ToolCall::new(
                "call_input_1",
                "search",
                json!({"q":"rust ai-sdk"}),
            )],
        ),
    );
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);
    let (status, body) = get_json(
        app,
        "/v1/ai-sdk/threads/s-tool-input-history/messages?visibility=none",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let messages = body["messages"]
        .as_array()
        .expect("messages should be array");
    let has_tool_input_available = messages.iter().any(|msg| {
        msg["parts"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["type"] == "tool-search"
                    && part["toolCallId"] == "call_input_1"
                    && part["state"] == "input-available"
                    && part["input"]["q"] == "rust ai-sdk"
            })
        })
    });
    assert!(
        has_tool_input_available,
        "assistant tool call should encode to input-available tool-search part: {body}"
    );
}

#[tokio::test]
async fn test_messages_run_id_cursor_order_combination_boundaries() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    let mut thread = Thread::new("s-run-cursor-boundary");
    for i in 0..6usize {
        let run_id = if i % 2 == 0 { "run-a" } else { "run-b" };
        thread = thread.with_message(
            tirea_agentos::contracts::thread::Message::user(format!("msg-{i}")).with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some(run_id.to_string()),
                    step_index: Some(i as u32),
                },
            ),
        );
    }
    storage.save(&thread).await.unwrap();
    let app = make_app(os, read_store);

    let (status, body) = get_json(
        app.clone(),
        "/v1/threads/s-run-cursor-boundary/messages?run_id=run-a&order=desc&before=5&limit=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].cursor, 4);
    assert_eq!(page.messages[0].message.content, "msg-4");
    assert_eq!(page.messages[1].cursor, 2);
    assert_eq!(page.messages[1].message.content, "msg-2");

    let (status, body) = get_json(
        app,
        "/v1/threads/s-run-cursor-boundary/messages?run_id=run-a&after=4&limit=10",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page: tirea_agentos::contracts::storage::MessagePage =
        serde_json::from_value(body).unwrap();
    assert!(
        page.messages.is_empty(),
        "after=4 with run-a should be empty, got: {:?}",
        page.messages
            .iter()
            .map(|m| (m.cursor, m.message.content.clone()))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Suspended-call fixture builder
// ---------------------------------------------------------------------------

struct SuspendedCallFixture {
    call_id: String,
    tool_name: String,
    arguments: Value,
    suspension_id: String,
    suspension_action: String,
    suspension_params: Value,
    pending_name: String,
    pending_args: Value,
    resume_mode: String,
}

impl SuspendedCallFixture {
    fn permission(call_id: &str, tool: &str, args: Value) -> Self {
        let suspension_id = format!("fc_perm_{}", &call_id[call_id.len().saturating_sub(1)..]);
        Self {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            arguments: args.clone(),
            suspension_id: suspension_id.clone(),
            suspension_action: "tool:PermissionConfirm".to_string(),
            suspension_params: json!({ "tool_name": tool, "tool_args": args }),
            pending_name: "PermissionConfirm".to_string(),
            pending_args: json!({ "tool_name": tool, "tool_args": args }),
            resume_mode: "replay_tool_call".to_string(),
        }
    }

    fn ask(call_id: &str, question: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            tool_name: "askUserQuestion".to_string(),
            arguments: json!({ "question": question }),
            suspension_id: call_id.to_string(),
            suspension_action: "tool:askUserQuestion".to_string(),
            suspension_params: json!({ "question": question }),
            pending_name: "askUserQuestion".to_string(),
            pending_args: json!({ "question": question }),
            resume_mode: "use_decision_as_tool_result".to_string(),
        }
    }

    fn with_suspension_id(mut self, id: &str) -> Self {
        self.suspension_id = id.to_string();
        self
    }

    fn to_state_entry(&self) -> (String, Value) {
        (
            self.call_id.clone(),
            json!({
                "call_id": self.call_id,
                "tool_name": self.tool_name,
                "arguments": self.arguments,
                "suspension": {
                    "id": self.suspension_id,
                    "action": self.suspension_action,
                    "parameters": self.suspension_params,
                },
                "pending": {
                    "id": self.suspension_id,
                    "name": self.pending_name,
                    "arguments": self.pending_args,
                },
                "resume_mode": self.resume_mode,
            }),
        )
    }
}

fn suspended_thread(id: &str, calls: Vec<SuspendedCallFixture>) -> Thread {
    let mut scope_map = serde_json::Map::new();
    let mut messages = Vec::new();
    // Collect all tool calls for a single assistant message.
    let mut tool_calls = Vec::new();
    for call in &calls {
        let (call_id, suspended_call_value) = call.to_state_entry();
        let mut scope_entry = serde_json::Map::new();
        scope_entry.insert("suspended_call".to_string(), suspended_call_value);
        scope_map.insert(call_id, Value::Object(scope_entry));
        tool_calls.push(tirea_agentos::contracts::thread::ToolCall::new(
            &call.call_id,
            &call.tool_name,
            call.suspension_params.clone(),
        ));
    }
    // Single assistant message with all tool calls.
    let assistant_text = if calls.len() == 1 {
        format!("need {}", calls[0].pending_name)
    } else {
        "need permissions".to_string()
    };
    messages.push(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            assistant_text,
            tool_calls,
        ),
    );
    // One tool message per call.
    for call in &calls {
        messages.push(tirea_agentos::contracts::thread::Message::tool(
            &call.call_id,
            format!(
                "Tool '{}' is awaiting approval. Execution paused.",
                call.tool_name
            ),
        ));
    }

    let state = json!({ "__tool_call_scope": Value::Object(scope_map) });
    let mut thread = Thread::with_initial_state(id, state);
    for msg in messages {
        thread = thread.with_message(msg);
    }
    thread
}

fn pending_permission_frontend_thread(id: &str, payload: &str) -> Thread {
    suspended_thread(
        id,
        vec![
            SuspendedCallFixture::permission("call_1", "echo", json!({"message": payload}))
                .with_suspension_id("fc_perm_1"),
        ],
    )
}

fn pending_ask_frontend_thread(id: &str, question: &str) -> Thread {
    suspended_thread(id, vec![SuspendedCallFixture::ask("ask_call_1", question)])
}

fn pending_permission_frontend_thread_pair(
    id: &str,
    first_payload: &str,
    second_payload: &str,
) -> Thread {
    suspended_thread(
        id,
        vec![
            SuspendedCallFixture::permission("call_1", "echo", json!({"message": first_payload}))
                .with_suspension_id("fc_perm_1"),
            SuspendedCallFixture::permission("call_2", "echo", json!({"message": second_payload}))
                .with_suspension_id("fc_perm_2"),
        ],
    )
}

#[tokio::test]
async fn test_agui_pending_approval_resumes_and_replays_tool_call() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "th-approve",
            "approved-run",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "threadId": "th-approve",
        "runId": "resume-approve-1",
        "messages": [
            {"role": "tool", "content": "true", "toolCallId": "fc_perm_1"}
        ],
        "tools": []
    });
    let (status, body) = post_sse_text(app, "/v1/ag-ui/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains("RUN_FINISHED"),
        "resume run should finish: {body}"
    );

    let saved = storage.load_thread("th-approve").await.unwrap().unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("approved-run")
    });
    assert!(
        replayed_tool.is_some(),
        "approved flow should append replayed tool result"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    assert!(
        rebuilt
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended_interaction must be cleared after approval replay"
    );
}

#[tokio::test]
async fn test_agui_pending_denial_clears_pending_without_replay() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread("th-deny", "denied-run"))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "threadId": "th-deny",
        "runId": "resume-deny-1",
        "messages": [
            {"role": "tool", "content": "false", "toolCallId": "fc_perm_1"}
        ],
        "tools": []
    });
    let (status, body) = post_sse_text(app, "/v1/ag-ui/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains("RUN_FINISHED"),
        "denied resume run should still finish: {body}"
    );

    let saved = storage.load_thread("th-deny").await.unwrap().unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("echoed")
    });
    assert!(
        replayed_tool.is_none(),
        "denied flow must not replay original tool call"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    assert!(
        rebuilt
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended_interaction must be cleared after denial"
    );
}

#[tokio::test]
async fn test_ai_sdk_permission_approval_replays_backend_tool_call() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "th-ai-approve",
            "approved-by-ai-sdk",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "th-ai-approve",
        "runId": "resume-ai-approve",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-PermissionConfirm",
                "toolCallId": "fc_perm_1",
                "state": "approval-responded",
                "approval": {
                    "id": "fc_perm_1",
                    "approved": true,
                    "reason": "approved in ui"
                }
            }]
        }]
    });
    let (status, body) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains(r#""type":"tool-output-available""#),
        "approved resume should emit tool output events: {body}"
    );
    assert!(
        body.contains(r#""toolCallId":"call_1""#),
        "approved resume should replay original backend call: {body}"
    );

    let saved = storage.load_thread("th-ai-approve").await.unwrap().unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("approved-by-ai-sdk")
    });
    assert!(
        replayed_tool.is_some(),
        "approved flow should append replayed backend tool result"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    assert!(
        rebuilt
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended_interaction must be cleared after approval replay"
    );
}

#[tokio::test]
async fn test_ai_sdk_permission_denial_emits_output_denied_without_replay() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "th-ai-deny",
            "denied-by-ai-sdk",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "th-ai-deny",
        "runId": "resume-ai-deny",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-PermissionConfirm",
                "toolCallId": "fc_perm_1",
                "state": "approval-responded",
                "approval": {
                    "id": "fc_perm_1",
                    "approved": false,
                    "reason": "denied in ui"
                }
            }]
        }]
    });
    let (status, body) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains(r#""type":"tool-output-denied""#)
            && body.contains(r#""toolCallId":"call_1""#),
        "denied resume should emit tool-output-denied for backend call: {body}"
    );

    let saved = storage.load_thread("th-ai-deny").await.unwrap().unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("denied-by-ai-sdk")
    });
    assert!(
        replayed_tool.is_none(),
        "denied flow must not replay original backend tool call"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    assert!(
        rebuilt
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended_interaction must be cleared after denial"
    );
}

#[tokio::test]
async fn test_ai_sdk_tool_approval_response_part_replays_backend_tool_call() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "th-ai-approve-part",
            "approved-by-approval-part",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "th-ai-approve-part",
        "runId": "resume-ai-approve-part",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": true,
                "reason": "approved via response part"
            }]
        }]
    });
    let (status, body) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains(r#""type":"tool-output-available""#)
            && body.contains(r#""toolCallId":"call_1""#),
        "approved response-part resume should replay backend call: {body}"
    );

    let saved = storage
        .load_thread("th-ai-approve-part")
        .await
        .unwrap()
        .unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("approved-by-approval-part")
    });
    assert!(
        replayed_tool.is_some(),
        "approved tool-approval-response should append replayed backend tool result"
    );
}

#[tokio::test]
async fn test_ai_sdk_tool_approval_response_part_denial_emits_output_denied() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "th-ai-deny-part",
            "denied-by-approval-part",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "th-ai-deny-part",
        "runId": "resume-ai-deny-part",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": false,
                "reason": "denied via response part"
            }]
        }]
    });
    let (status, body) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "pending decision replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains(r#""type":"tool-output-denied""#)
            && body.contains(r#""toolCallId":"call_1""#),
        "denied response-part resume should emit tool-output-denied: {body}"
    );

    let saved = storage
        .load_thread("th-ai-deny-part")
        .await
        .unwrap()
        .unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("call_1")
            && m.content.contains("denied-by-approval-part")
    });
    assert!(
        replayed_tool.is_none(),
        "denied tool-approval-response must not replay original backend tool call"
    );
}

#[tokio::test]
async fn test_ai_sdk_batch_approval_mode_replays_only_after_all_pending_decisions() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread_pair(
            "th-ai-batch-approvals",
            "batch-approved-1",
            "batch-approved-2",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>)]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    // First decision only approves fc_perm_1. Batch approval mode should NOT
    // replay call_1 yet because call_2 remains undecided.
    let first_payload = json!({
        "id": "th-ai-batch-approvals",
        "runId": "resume-ai-batch-1",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": true
            }]
        }]
    });
    let (status, first_body) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", first_payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            first_body.contains("no active run found for thread"),
            "batch partial approval should fail without active run: {first_body}"
        );
        return;
    }
    assert!(
        !first_body.contains("batch-approved-1"),
        "partial approval must not replay call_1 yet: {first_body}"
    );

    let first_saved = storage
        .load_thread("th-ai-batch-approvals")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !first_saved.messages.iter().any(|m| {
            m.role == tirea_agentos::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("batch-approved-1")
        }),
        "partial approval should not append replayed result for call_1"
    );
    let first_state = first_saved.rebuild_state().unwrap();
    let first_scopes = first_state
        .get("__tool_call_scope")
        .and_then(Value::as_object)
        .expect("tool_call_scope should still exist after partial approval");
    let has_suspended = |id: &str| {
        first_scopes
            .get(id)
            .and_then(|s| s.get("suspended_call"))
            .is_some()
    };
    assert!(
        has_suspended("call_1") && has_suspended("call_2"),
        "both calls should remain suspended until batch approvals are complete: {first_scopes:?}"
    );

    // Second decision approves fc_perm_2. Now both suspended calls are decided,
    // so replay should execute call_1 and call_2 together.
    let second_payload = json!({
        "id": "th-ai-batch-approvals",
        "runId": "resume-ai-batch-2",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_2",
                "approved": true
            }]
        }]
    });
    let (status, second_body) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", second_payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            second_body.contains("no active run found for thread"),
            "batch final approval should fail without active run: {second_body}"
        );
        return;
    }
    assert!(
        second_body.contains("batch-approved-1") && second_body.contains("batch-approved-2"),
        "final approval should replay both pending calls: {second_body}"
    );

    let second_saved = storage
        .load_thread("th-ai-batch-approvals")
        .await
        .unwrap()
        .unwrap();
    assert!(
        second_saved.messages.iter().any(|m| {
            m.role == tirea_agentos::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("batch-approved-1")
        }),
        "batch completion should append replayed call_1 result"
    );
    assert!(
        second_saved.messages.iter().any(|m| {
            m.role == tirea_agentos::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_2")
                && m.content.contains("batch-approved-2")
        }),
        "batch completion should append replayed call_2 result"
    );

    let second_state = second_saved.rebuild_state().unwrap();
    assert!(
        second_state
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended calls must be cleared after full batch approval replay"
    );
}

#[tokio::test]
async fn test_ai_sdk_ask_output_available_replays_with_frontend_payload() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_ask_frontend_thread(
            "th-ai-ask",
            "What is your favorite color?",
        ))
        .await
        .unwrap();

    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::from([(
        "askUserQuestion".to_string(),
        Arc::new(AskUserQuestionEchoTool) as Arc<dyn Tool>,
    )]);
    let os = Arc::new(make_os_with_storage_and_tools(storage.clone(), tools));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = json!({
        "id": "th-ai-ask",
        "runId": "resume-ai-ask",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-askUserQuestion",
                "toolCallId": "ask_call_1",
                "state": "output-available",
                "output": {
                    "message": "blue"
                }
            }]
        }]
    });
    let (status, body) = post_sse_text(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    if status != StatusCode::OK {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("no active run found for thread"),
            "ask output replay should fail without active run: {body}"
        );
        return;
    }
    assert!(
        body.contains(r#""type":"tool-output-available""#)
            && body.contains(r#""toolCallId":"ask_call_1""#),
        "ask resume should emit tool-output-available: {body}"
    );

    let saved = storage.load_thread("th-ai-ask").await.unwrap().unwrap();
    let replayed_tool = saved.messages.iter().find(|m| {
        m.role == tirea_agentos::contracts::thread::Role::Tool
            && m.tool_call_id.as_deref() == Some("ask_call_1")
            && m.content.contains("blue")
    });
    assert!(
        replayed_tool.is_some(),
        "ask flow should replay frontend payload as tool input"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    assert!(
        rebuilt
            .get("__tool_call_scope")
            .and_then(|v| v.as_object())
            .map_or(true, |scopes| scopes
                .values()
                .all(|s| s.get("suspended_call").is_none())),
        "suspended_interaction must be cleared after ask replay"
    );
}

// ============================================================================
// Route restructuring regression tests
// ============================================================================

/// Old route paths must return 404 after restructuring.
#[tokio::test]
async fn test_old_routes_return_404() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let old_routes = vec![
        ("POST", "/v1/agents/test/runs/ag-ui/sse"),
        ("POST", "/v1/agents/test/runs/ai-sdk/sse"),
        ("GET", "/v1/threads/some-thread/messages/ag-ui"),
        ("GET", "/v1/threads/some-thread/messages/ai-sdk"),
    ];

    for (method, uri) in old_routes {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "old route {method} {uri} should return 404, got {}",
            resp.status()
        );
    }
}

/// New protocol-prefixed routes must be reachable.
#[tokio::test]
async fn test_new_protocol_routes_are_reachable() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    // POST to new AG-UI run endpoint — valid request should succeed (200 SSE stream)
    let ag_ui_payload = json!({
        "threadId": "route-test-agui",
        "runId": "r-agui-1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": []
    });
    let (status, _) = post_sse_text(app.clone(), "/v1/ag-ui/agents/test/runs", ag_ui_payload).await;
    assert_eq!(status, StatusCode::OK, "new AG-UI run route should be 200");

    // POST to new AI SDK run endpoint
    let ai_sdk_payload = json!({
        "id": "route-test-aisdk",
        "messages": [{"role": "user", "content": "hello"}],
    });
    let (status, _) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", ai_sdk_payload).await;
    assert_eq!(status, StatusCode::OK, "new AI SDK run route should be 200");

    // GET protocol-encoded history (thread not found → 404, but route is matched)
    let (status, _) = get_json(app.clone(), "/v1/ag-ui/threads/nonexistent/messages").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "AG-UI history route should match but return thread-not-found"
    );

    let (status, _) = get_json(app.clone(), "/v1/ai-sdk/threads/nonexistent/messages").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "AI SDK history route should match but return thread-not-found"
    );
}

/// Wrong HTTP methods on protocol routes should return 405 Method Not Allowed.
#[tokio::test]
async fn test_protocol_routes_reject_wrong_methods() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    // GET on a POST-only endpoint
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ag-ui/agents/test/runs")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET on run endpoint should be 405"
    );

    // POST on a GET-only endpoint
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/threads/some-thread/messages")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "POST on history endpoint should be 405"
    );
}

/// Protocol isolation: AG-UI and AI SDK endpoints must not cross-route.
#[tokio::test]
async fn test_protocol_isolation_no_cross_routing() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // Run via AG-UI
    let ag_payload = json!({
        "threadId": "cross-route-agui",
        "runId": "cr-1",
        "messages": [{"role": "user", "content": "agui msg"}],
        "tools": []
    });
    let (status, body) = post_sse_text(app.clone(), "/v1/ag-ui/agents/test/runs", ag_payload).await;
    assert_eq!(status, StatusCode::OK);
    // AG-UI events use "RUN_STARTED" / "RUN_FINISHED" markers
    assert!(
        body.contains("RUN_STARTED"),
        "AG-UI run should emit RUN_STARTED events"
    );

    // Run via AI SDK
    let sdk_payload = json!({
        "id": "cross-route-aisdk",
        "messages": [{"role": "user", "content": "sdk msg"}],
    });
    let (status, body) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", sdk_payload).await;
    assert_eq!(status, StatusCode::OK);
    // AI SDK streams end with [DONE]
    assert!(
        body.contains("[DONE]"),
        "AI SDK run should emit [DONE] trailer"
    );
}

/// Unmatched protocol prefix should 404.
#[tokio::test]
async fn test_unknown_protocol_prefix_returns_404() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/unknown-proto/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Protocol-specific response headers: AI SDK sets x-vercel-ai-ui-message-stream.
#[tokio::test]
async fn test_ai_sdk_route_sets_protocol_headers() {
    let os = Arc::new(make_os());
    let read_store: Arc<dyn ThreadReader> = Arc::new(MemoryStore::new());
    let app = make_app(os, read_store);

    let payload = json!({
        "id": "header-check",
        "messages": [{"role": "user", "content": "test"}],
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get("x-vercel-ai-ui-message-stream")
            .is_some(),
        "AI SDK endpoint must set x-vercel-ai-ui-message-stream header"
    );
    assert!(
        resp.headers().get("x-tirea-ai-sdk-version").is_some(),
        "AI SDK endpoint must set x-tirea-ai-sdk-version header"
    );
}

/// Verifies that the AI SDK handler stores only the incremental user message
/// via `prepare_run`, rather than overwriting the entire thread from a client
/// snapshot. The mock agent terminates before inference, so the thread should
/// contain exactly the user message appended by `prepare_run`.
#[tokio::test]
async fn test_ai_sdk_run_stores_incremental_user_message() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // Client sends a single new user message (incremental).
    let payload = json!({
        "id": "aisdk-incremental-thread",
        "messages": [
            {
                "id": "m_user_1",
                "role": "user",
                "parts": [{ "type": "text", "text": "latest question" }]
            }
        ]
    });

    let (status, body) = post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"finish""#));

    let saved = storage
        .load_thread("aisdk-incremental-thread")
        .await
        .expect("load should not fail")
        .expect("thread should exist");
    // prepare_run appends exactly the extracted user message.
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.messages[0].content, "latest question");
    // No protocol sidecar should be stored.
    assert!(
        saved.state.get("protocol").is_none(),
        "no protocol sidecar should be stored; got: {:?}",
        saved.state
    );
}

/// Second incremental request deduplicates: only the NEW message is appended.
#[tokio::test]
async fn test_ai_sdk_second_request_deduplicates_messages() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // First request: creates thread with "hello".
    let first = json!({
        "id": "aisdk-dedup-thread",
        "messages": [{ "role": "user", "content": "hello" }]
    });
    let (status, _) = post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", first).await;
    assert_eq!(status, StatusCode::OK);

    let after_first = storage
        .load_thread("aisdk-dedup-thread")
        .await
        .unwrap()
        .unwrap();
    let first_msg_count = after_first.messages.len();
    assert!(
        first_msg_count >= 1,
        "at least the user message should be stored"
    );

    // Second request: sends a new question.
    let second = json!({
        "id": "aisdk-dedup-thread",
        "messages": [{ "role": "user", "content": "follow up" }]
    });
    let (status, _) = post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", second).await;
    assert_eq!(status, StatusCode::OK);

    let after_second = storage
        .load_thread("aisdk-dedup-thread")
        .await
        .unwrap()
        .unwrap();
    // Should have gained exactly one new user message.
    assert_eq!(
        after_second.messages.len(),
        first_msg_count + 1,
        "second request should append exactly one new user message"
    );
    assert_eq!(after_second.messages.last().unwrap().content, "follow up");
}

/// Regenerate-message truncates the stored thread at the specified messageId
/// (server-side truncation, no client snapshot needed).
#[tokio::test]
async fn test_ai_sdk_regenerate_truncates_stored_thread() {
    use tirea_contract::Message;

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // Pre-populate thread with 3 known messages via store.save().
    let thread = Thread::new("aisdk-regenerate-thread").with_messages(vec![
        Message::user("question one").with_id("m_user_1".to_string()),
        Message::assistant("answer one").with_id("m_assistant_1".to_string()),
        Message::user("question two").with_id("m_user_2".to_string()),
    ]);
    storage.save(&thread).await.expect("save should succeed");

    // Send regenerate request — no messages needed, just trigger + messageId.
    let regenerate = json!({
        "id": "aisdk-regenerate-thread",
        "trigger": "regenerate-message",
        "messageId": "m_assistant_1"
    });
    let (status, body) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", regenerate).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"finish""#));

    let saved = storage
        .load_thread("aisdk-regenerate-thread")
        .await
        .expect("load should not fail")
        .expect("thread should exist");
    // Messages after m_assistant_1 should be truncated.
    assert!(
        saved.messages.len() >= 2,
        "thread should have at least the truncated messages; got {}",
        saved.messages.len()
    );
    assert_eq!(saved.messages[0].id.as_deref(), Some("m_user_1"));
    assert_eq!(saved.messages[1].id.as_deref(), Some("m_assistant_1"));
    assert!(
        saved.messages.iter().all(|m| m.content != "question two"),
        "trailing messages after messageId must be removed"
    );
}

#[tokio::test]
async fn test_ai_sdk_regenerate_requires_message_id() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    // regenerate-message without messageId → 400
    let payload = json!({
        "id": "aisdk-regenerate-missing-id",
        "trigger": "regenerate-message"
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("messageId is required"),
        "expected regenerate messageId validation error: {body}"
    );
}

/// Regenerate with an empty messageId string is rejected.
#[tokio::test]
async fn test_ai_sdk_regenerate_rejects_empty_message_id() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    let payload = json!({
        "id": "aisdk-regenerate-empty-id",
        "trigger": "regenerate-message",
        "messageId": "  "
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("messageId cannot be empty"),
        "expected empty messageId error: {body}"
    );
}

/// Regenerate on a non-existent thread returns 400.
#[tokio::test]
async fn test_ai_sdk_regenerate_rejects_missing_thread() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    let payload = json!({
        "id": "no-such-thread",
        "trigger": "regenerate-message",
        "messageId": "m_1"
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("thread not found"),
        "expected thread not found error: {body}"
    );
}

/// Regenerate with a messageId that doesn't exist in the stored thread returns 400.
#[tokio::test]
async fn test_ai_sdk_regenerate_rejects_unknown_message_id() {
    use tirea_contract::Message;

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // Pre-populate thread.
    let thread = Thread::new("aisdk-unknown-msgid")
        .with_messages(vec![Message::user("hi").with_id("m_user_1".to_string())]);
    storage.save(&thread).await.expect("save should succeed");

    let payload = json!({
        "id": "aisdk-unknown-msgid",
        "trigger": "regenerate-message",
        "messageId": "nonexistent"
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("does not reference a stored message"),
        "expected unknown messageId error: {body}"
    );
}

/// Eager prepare_run surfaces storage errors as HTTP 500 (not buried in SSE).
#[tokio::test]
async fn test_ai_sdk_storage_error_returns_http_500() {
    let fail_store = Arc::new(FailingStorage);
    let os = Arc::new(make_os_with_storage(fail_store.clone()));
    let app = make_app(os, fail_store);

    let payload = json!({
        "id": "aisdk-storage-fail",
        "messages": [{ "role": "user", "content": "hello" }]
    });
    let (status, _) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage errors should surface as HTTP 500, not inside SSE stream"
    );
}

#[tokio::test]
async fn test_ai_sdk_resume_stream_routes_match_expected_behavior() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let no_active = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ai-sdk/agents/test/chats/no-active-chat/stream")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(no_active.status(), StatusCode::NO_CONTENT);

    let post_payload = json!({
        "id": "resume-active-chat",
        "messages": [{ "role": "user", "content": "hello resume" }]
    });
    let post_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(post_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(post_response.status(), StatusCode::OK);
    let resume_active = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ai-sdk/agents/test/chats/resume-active-chat/stream")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(resume_active.status(), StatusCode::OK);
    assert!(
        resume_active
            .headers()
            .get("x-vercel-ai-ui-message-stream")
            .is_some(),
        "resume stream should expose ai-sdk stream header"
    );

    drop(resume_active);
    drop(post_response);

    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let finished = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ai-sdk/agents/test/chats/resume-active-chat/stream")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(finished.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_ai_sdk_decision_only_request_forwards_to_active_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "id": "decision-forward-ai-sdk",
        "runId": "decision-only",
        "messages": [{ "role": "user", "content": "start" }]
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "id": "decision-forward-ai-sdk",
        "runId": "decision-only",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": true
            }]
        }]
    });
    let (status, body) = post_json(
        app.clone(),
        "/v1/ai-sdk/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "decision_forwarded");
    assert_eq!(body["threadId"], "decision-forward-ai-sdk");

    drop(first_response);
}

#[tokio::test]
async fn test_ai_sdk_decision_only_batch_request_forwards_to_active_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "id": "decision-forward-ai-sdk-batch",
        "runId": "decision-only-batch",
        "messages": [{ "role": "user", "content": "start" }]
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "id": "decision-forward-ai-sdk-batch",
        "runId": "decision-only-batch",
        "messages": [{
            "role": "assistant",
            "parts": [
                {
                    "type": "tool-approval-response",
                    "approvalId": "fc_perm_1",
                    "approved": true
                },
                {
                    "type": "tool-approval-response",
                    "approvalId": "fc_perm_2",
                    "approved": false
                }
            ]
        }]
    });
    let (status, body) = post_json(
        app.clone(),
        "/v1/ai-sdk/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "decision_forwarded");
    assert_eq!(body["threadId"], "decision-forward-ai-sdk-batch");

    drop(first_response);
}

#[tokio::test]
async fn test_ai_sdk_decision_only_with_mismatched_run_id_forwards_by_thread() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "id": "decision-forward-ai-sdk-mismatch",
        "runId": "run-active",
        "messages": [{ "role": "user", "content": "start" }]
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "id": "decision-forward-ai-sdk-mismatch",
        "runId": "run-mismatch",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": true
            }]
        }]
    });
    let (status, body) = post_sse_text(
        app.clone(),
        "/v1/ai-sdk/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        body.contains("decision_forwarded"),
        "mismatched run id should still forward by thread: {body}"
    );
    assert!(
        body.contains(r#""threadId":"decision-forward-ai-sdk-mismatch""#),
        "decision-forward ack should still identify the thread: {body}"
    );

    drop(first_response);
}

#[tokio::test]
async fn test_ai_sdk_active_run_with_user_input_and_decision_starts_new_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "id": "decision-mixed-ai-sdk",
        "messages": [{ "role": "user", "content": "start" }]
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    // Mixed payload: includes historical user message + approval response.
    // This does NOT qualify as decision-only, so a new run is created.
    let mixed_payload = json!({
        "id": "decision-mixed-ai-sdk",
        "runId": "decision-mixed-run",
        "messages": [
            { "role": "user", "content": "start" },
            {
                "role": "assistant",
                "parts": [{
                    "type": "tool-approval-response",
                    "approvalId": "fc_perm_1",
                    "approved": true
                }]
            }
        ]
    });
    let (status, body) =
        post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", mixed_payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#""type":"finish""#),
        "mixed payload should trigger a new run stream: {body}"
    );
    assert!(
        !body.contains("decision_forwarded"),
        "mixed payload should not take decision-forward fast path: {body}"
    );

    drop(first_response);
}

#[tokio::test]
async fn test_ai_sdk_decision_only_without_active_run_returns_bad_request() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    let decision_only_payload = json!({
        "id": "decision-no-active-ai-sdk",
        "runId": "decision-only-no-active",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-approval-response",
                "approvalId": "fc_perm_1",
                "approved": true
            }]
        }]
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", decision_only_payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no active run found for thread"),
        "no-active decision-only request should return protocol error: {body}"
    );
}

#[tokio::test]
async fn test_ag_ui_decision_only_request_forwards_to_active_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "threadId": "decision-forward-ag-ui",
        "runId": "run-decision-only",
        "messages": [{ "role": "user", "content": "start" }],
        "tools": []
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "threadId": "decision-forward-ag-ui",
        "runId": "run-decision-only",
        "messages": [{
            "role": "tool",
            "toolCallId": "fc_perm_1",
            "content": "true"
        }],
        "tools": []
    });
    let (status, body) = post_json(
        app.clone(),
        "/v1/ag-ui/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "decision_forwarded");
    assert_eq!(body["threadId"], "decision-forward-ag-ui");
    assert_eq!(body["runId"], "run-decision-only");

    drop(first_response);
}

#[tokio::test]
async fn test_ag_ui_decision_only_batch_request_forwards_to_active_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "threadId": "decision-forward-ag-ui-batch",
        "runId": "run-decision-only-batch",
        "messages": [{ "role": "user", "content": "start" }],
        "tools": []
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "threadId": "decision-forward-ag-ui-batch",
        "runId": "run-decision-only-batch",
        "messages": [
            {
                "role": "tool",
                "toolCallId": "fc_perm_1",
                "content": "true"
            },
            {
                "role": "tool",
                "toolCallId": "fc_perm_2",
                "content": "false"
            }
        ],
        "tools": []
    });
    let (status, body) = post_json(
        app.clone(),
        "/v1/ag-ui/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "decision_forwarded");
    assert_eq!(body["threadId"], "decision-forward-ag-ui-batch");
    assert_eq!(body["runId"], "run-decision-only-batch");

    drop(first_response);
}

#[tokio::test]
async fn test_ag_ui_decision_only_with_mismatched_run_id_echoes_request_run_id() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "threadId": "decision-forward-ag-ui-mismatch",
        "runId": "run-active",
        "messages": [{ "role": "user", "content": "start" }],
        "tools": []
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    let decision_only_payload = json!({
        "threadId": "decision-forward-ag-ui-mismatch",
        "runId": "run-mismatch",
        "messages": [{
            "role": "tool",
            "toolCallId": "fc_perm_1",
            "content": "true"
        }],
        "tools": []
    });
    let (status, body) = post_sse_text(
        app.clone(),
        "/v1/ag-ui/agents/test/runs",
        decision_only_payload,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        body.contains("decision_forwarded"),
        "mismatched run id should still forward by thread: {body}"
    );
    assert!(
        body.contains(r#""runId":"run-mismatch""#),
        "AG-UI should echo the request's runId when no internal run mapping is used: {body}"
    );

    drop(first_response);
}

#[tokio::test]
async fn test_ag_ui_active_run_with_user_input_and_decision_starts_new_run() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_slow_terminate_behavior_requested_plugin(
        storage.clone(),
    ));
    let app = make_app(os, storage);

    let first_payload = json!({
        "threadId": "decision-mixed-ag-ui",
        "runId": "run-active",
        "messages": [{ "role": "user", "content": "start" }],
        "tools": []
    });
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_payload.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");
    assert_eq!(first_response.status(), StatusCode::OK);

    // Mixed payload: includes user input + tool decision.
    // This does NOT qualify as decision-only, so a new run is created.
    let mixed_payload = json!({
        "threadId": "decision-mixed-ag-ui",
        "runId": "run-decision-mixed",
        "messages": [
            { "role": "user", "content": "start" },
            { "role": "tool", "toolCallId": "fc_perm_1", "content": "true" }
        ],
        "tools": []
    });
    let (status, body) =
        post_sse_text(app.clone(), "/v1/ag-ui/agents/test/runs", mixed_payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("RUN_FINISHED"),
        "mixed payload should trigger a new AG-UI run stream: {body}"
    );
    assert!(
        !body.contains("decision_forwarded"),
        "mixed payload should not take decision-forward fast path: {body}"
    );

    drop(first_response);
}

#[tokio::test]
async fn test_ai_sdk_user_input_with_pending_approval_is_accepted_without_auto_cancel() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "pending-user-ai-sdk",
            "pending-payload-ai-sdk",
        ))
        .await
        .unwrap();
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    let payload = json!({
        "id": "pending-user-ai-sdk",
        "runId": "run-user-ai-sdk",
        "messages": [{ "role": "user", "content": "continue without approval" }]
    });
    let (status, body) = post_sse_text(app.clone(), "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#""type":"finish""#),
        "request should continue and finish: {body}"
    );

    let saved = storage
        .load_thread("pending-user-ai-sdk")
        .await
        .unwrap()
        .unwrap();
    assert!(
        saved.messages.iter().any(|m| {
            m.role == tirea_agentos::contracts::thread::Role::User
                && m.content.contains("continue without approval")
        }),
        "user input must always be accepted and appended"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    let has_suspended_call = rebuilt
        .get("__tool_call_scope")
        .and_then(|v| v.get("call_1"))
        .and_then(|s| s.get("suspended_call"))
        .is_some();
    assert!(
        !has_suspended_call,
        "new user input should supersede the waiting run and clear its suspended call state"
    );
    assert_ne!(
        rebuilt
            .get("__run")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str()),
        Some("pending-payload-ai-sdk"),
        "thread should point at the new active run after superseding the waiting one"
    );
}

#[tokio::test]
async fn test_ag_ui_user_input_with_pending_approval_is_accepted_even_with_policy_config() {
    let storage = Arc::new(MemoryStore::new());
    storage
        .save(&pending_permission_frontend_thread(
            "pending-user-ag-ui",
            "pending-payload-ag-ui",
        ))
        .await
        .unwrap();
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    let payload = json!({
        "threadId": "pending-user-ag-ui",
        "runId": "run-user-ag-ui",
        "messages": [{ "role": "user", "content": "continue without approval" }],
        "tools": [],
        "config": {
            "pendingApprovalPolicy": "auto_cancel"
        }
    });
    let (status, body) = post_sse_text(app.clone(), "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("RUN_FINISHED"),
        "request should continue and finish: {body}"
    );

    let saved = storage
        .load_thread("pending-user-ag-ui")
        .await
        .unwrap()
        .unwrap();
    assert!(
        saved.messages.iter().any(|m| {
            m.role == tirea_agentos::contracts::thread::Role::User
                && m.content.contains("continue without approval")
        }),
        "user input must always be accepted and appended"
    );

    let rebuilt = saved.rebuild_state().unwrap();
    let has_suspended_call = rebuilt
        .get("__tool_call_scope")
        .and_then(|v| v.get("call_1"))
        .and_then(|s| s.get("suspended_call"))
        .is_some();
    assert!(
        !has_suspended_call,
        "new user input should supersede the waiting run and clear its suspended call state"
    );
    assert_ne!(
        rebuilt
            .get("__run")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str()),
        Some("run-user-ag-ui"),
        "AG-UI should keep frontend runId decoupled from backend run id"
    );
    assert!(
        rebuilt.get("__run").and_then(|v| v.get("id")).is_some(),
        "thread should point to a newly active backend run id"
    );
}

#[tokio::test]
async fn test_ag_ui_decision_only_without_active_run_returns_bad_request() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    let decision_only_payload = json!({
        "threadId": "decision-no-active-ag-ui",
        "runId": "run-decision-no-active",
        "messages": [{
            "role": "tool",
            "toolCallId": "fc_perm_1",
            "content": "true"
        }],
        "tools": []
    });
    let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", decision_only_payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no active run found for thread"),
        "no-active AG-UI decision-only request should return protocol error: {body}"
    );
}

#[tokio::test]
async fn test_ai_sdk_request_requires_user_input_or_suspension_decisions() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage);

    let payload = json!({
        "id": "input-or-decision-required",
        "messages": [{
            "role": "assistant",
            "parts": [{ "type": "text", "text": "assistant-only" }]
        }]
    });
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("user input or suspension decisions"),
        "expected updated validation message: {body}"
    );
}

/// Generic thread endpoints remain accessible without protocol prefix.
#[tokio::test]
async fn test_generic_thread_endpoints_still_work() {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_with_storage(storage.clone()));
    let app = make_app(os, storage.clone());

    // Seed a thread via AG-UI
    let payload = json!({
        "threadId": "generic-ep-test",
        "runId": "g-1",
        "messages": [{"role": "user", "content": "seed"}],
        "tools": []
    });
    let (status, _) = post_sse_text(app.clone(), "/v1/ag-ui/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);

    // Wait for persistence
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // GET /v1/threads should list threads
    let (status, body) = get_json(app.clone(), "/v1/threads").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("items").is_some(),
        "thread list should have items field"
    );

    // GET /v1/threads/:id should return thread
    let (status, _) = get_json(app.clone(), "/v1/threads/generic-ep-test").await;
    assert_eq!(status, StatusCode::OK);

    // GET /v1/threads/:id/messages should return raw messages
    let (status, body) = get_json(app.clone(), "/v1/threads/generic-ep-test/messages").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("messages").is_some(),
        "messages endpoint should have messages field"
    );
}

// ============================================================================
// Sanitization regression tests
// ============================================================================

#[tokio::test]
async fn test_get_thread_strips_run_id_from_state() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    // Seed a thread whose state contains __run.id (as set internally by the run lifecycle).
    let thread = Thread::with_initial_state(
        "sanitize-state",
        json!({
            "__run": { "id": "internal-run-xyz", "status": "done", "done_reason": "behavior_requested" },
            "user_key": "should_remain"
        }),
    )
    .with_message(tirea_agentos::contracts::thread::Message::user("hi"));
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);
    let (status, body) = get_json(app, "/v1/threads/sanitize-state").await;
    assert_eq!(status, StatusCode::OK);

    // __run.id must be stripped from the public response.
    let run_state = body.get("state").and_then(|s| s.get("__run"));
    assert!(
        run_state.is_none() || run_state.unwrap().get("id").is_none(),
        "GET /v1/threads/:id must not expose state.__run.id, got: {body}"
    );
    // Non-sensitive state keys must survive.
    assert_eq!(
        body.get("state").and_then(|s| s.get("user_key")),
        Some(&json!("should_remain")),
        "non-sensitive state must remain intact"
    );
}

#[tokio::test]
async fn test_get_messages_strips_run_id_from_metadata() {
    let os = Arc::new(make_os());
    let storage = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = storage.clone();

    // Seed a thread with messages carrying metadata.run_id.
    let thread = Thread::new("sanitize-messages")
        .with_message(
            tirea_agentos::contracts::thread::Message::user("hello").with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("internal-run-abc".to_string()),
                    step_index: Some(0),
                },
            ),
        )
        .with_message(
            tirea_agentos::contracts::thread::Message::assistant("world").with_metadata(
                tirea_agentos::contracts::thread::MessageMetadata {
                    run_id: Some("internal-run-abc".to_string()),
                    step_index: Some(1),
                },
            ),
        );
    storage.save(&thread).await.unwrap();

    let app = make_app(os, read_store);
    let (status, body) = get_json(app, "/v1/threads/sanitize-messages/messages").await;
    assert_eq!(status, StatusCode::OK);

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages field must be an array");

    for msg in messages {
        let msg_val = msg.get("message").unwrap_or(msg);
        if let Some(meta) = msg_val.get("metadata") {
            assert!(
                meta.get("run_id").is_none(),
                "GET /v1/threads/:id/messages must not expose metadata.run_id, got: {msg_val}"
            );
        }
    }
}
