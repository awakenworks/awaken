//! ADR-0030 D7: GET /v1/traces/:run_id and GET /v1/traces.

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use awaken_ext_observability::trace_store::file::FileTraceStore;
use awaken_ext_observability::trace_store::{RunSummary, TraceStore};
use awaken_ext_observability::{GenAISpan, MetricsEvent, SpanContext};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, ServerConfig, ServerState, TraceModuleState};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_stores::InMemoryStore;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

// ── Stub executor ─────────────────────────────────────────────────────────

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

// ── Helpers ───────────────────────────────────────────────────────────────

fn test_admin_bearer() -> String {
    "test-admin-token".to_string()
}

fn sample_inference_event(run_id: &str, agent_id: &str) -> MetricsEvent {
    MetricsEvent::Inference(GenAISpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            ..SpanContext::default()
        },
        step_index: None,
        model: "test-model".to_string(),
        provider: "test-provider".to_string(),
        operation: "chat".to_string(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 100,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    })
}

fn sample_run_summary(run_id: &str, agent_id: &str) -> RunSummary {
    RunSummary {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        started_at: UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        ended_at: None,
        prompt_ids: vec![],
        experiment_id: None,
        variant_name: None,
        final_status: None,
        judge_score: None,
    }
}

async fn build_test_app(token: Option<&str>, store: Arc<dyn TraceStore>) -> axum::Router {
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "trace-test".into(),
        MailboxConfig::default(),
    ));

    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig {
            address: "127.0.0.1:0".to_string(),
            ..ServerConfig::default()
        },
    );
    state.trace = Some(TraceModuleState { trace_store: store });
    // F20 made trace routes opt-in; tests in this file exercise the
    // routes themselves, so they enable them explicitly.
    state.admin.admin_api_config = AdminApiConfig {
        expose_trace_routes: true,
        ..AdminApiConfig::default()
    };

    if let Some(tok) = token {
        state.admin.admin_api_config.bearer_token = Some(tok.into());
    }

    build_router(&state)
}

fn temp_trace_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("awaken-trace-test-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

async fn test_app_with_trace_store() -> (axum::Router, Arc<FileTraceStore>) {
    let dir = temp_trace_dir();
    let store = Arc::new(FileTraceStore::new(&dir).expect("FileTraceStore::new"));
    let app = build_test_app(Some(&test_admin_bearer()), store.clone()).await;
    (app, store)
}

async fn get(app: &axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, bytes::Bytes) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(tok) = token {
        builder = builder.header("Authorization", format!("Bearer {tok}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, body)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_trace_by_run_id_returns_ndjson() {
    let (app, store) = test_app_with_trace_store().await;

    let run_id = "01HXROUTE";
    let event = sample_inference_event(run_id, "agentX");
    store.append(run_id, &event).unwrap();

    let (status, body) = get(
        &app,
        &format!("/v1/traces/{run_id}"),
        Some(&test_admin_bearer()),
    )
    .await;

    assert_eq!(status, 200);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains(run_id), "body should contain run_id");
    assert!(text.ends_with('\n'), "NDJSON must end with newline");
}

#[tokio::test]
async fn list_traces_filters_by_agent() {
    let (app, store) = test_app_with_trace_store().await;

    // Seed two runs for two different agents.
    let run_a = "01HXAGENTA";
    let run_b = "01HXAGENTB";

    let summary_a = sample_run_summary(run_a, "agentA");
    let summary_b = sample_run_summary(run_b, "agentB");

    store
        .append(run_a, &sample_inference_event(run_a, "agentA"))
        .unwrap();
    store
        .append(run_b, &sample_inference_event(run_b, "agentB"))
        .unwrap();
    store.write_index_for_run(run_a, &summary_a).unwrap();
    store.write_index_for_run(run_b, &summary_b).unwrap();

    let (status, body) = get(
        &app,
        "/v1/traces?agent_id=agentA",
        Some(&test_admin_bearer()),
    )
    .await;

    assert_eq!(status, 200, "body: {}", String::from_utf8_lossy(&body));
    let json: Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "only one run for agentA");
    assert_eq!(runs[0]["agent_id"], "agentA");
}

#[tokio::test]
async fn get_trace_rejects_zero_limit() {
    // Regression for F17: `?limit=0` would otherwise return an empty
    // page whose `x-trace-next-offset` equals `offset`, freezing the
    // client in an infinite pagination loop.
    let (app, store) = test_app_with_trace_store().await;
    let run_id = "01HXLIMITZERO";
    store
        .append(run_id, &sample_inference_event(run_id, "a"))
        .unwrap();

    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/traces/{run_id}?limit=0"))
        .header("Authorization", format!("Bearer {}", test_admin_bearer()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn get_trace_pages_with_offset_and_limit() {
    let (app, store) = test_app_with_trace_store().await;

    let run_id = "01HXPAGE";
    for _ in 0..3 {
        store
            .append(run_id, &sample_inference_event(run_id, "agent"))
            .unwrap();
    }

    // First page: limit=2 → returns 2 lines, x-trace-next-offset=2.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/traces/{run_id}?offset=0&limit=2"))
        .header("Authorization", format!("Bearer {}", test_admin_bearer()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let total = resp
        .headers()
        .get("x-trace-total-events")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let next = resp
        .headers()
        .get("x-trace-next-offset")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let lines: Vec<&[u8]> = body
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "first page returns exactly two events");
    assert_eq!(total, "3");
    assert_eq!(next.as_deref(), Some("2"));

    // Last page: offset=2 → 1 line, no next.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/traces/{run_id}?offset=2"))
        .header("Authorization", format!("Bearer {}", test_admin_bearer()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("x-trace-next-offset").is_none(),
        "no next-offset on last page"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let lines: Vec<&[u8]> = body
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 1);
}

#[tokio::test]
async fn list_traces_since_rejects_invalid_rfc3339() {
    let (app, _store) = test_app_with_trace_store().await;

    let (status, body) = get(
        &app,
        "/v1/traces?since=not-a-timestamp",
        Some(&test_admin_bearer()),
    )
    .await;

    assert_eq!(status, 400, "body: {}", String::from_utf8_lossy(&body));
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"].as_str().unwrap_or("").contains("since"),
        "error message must name the offending parameter: {json}"
    );
}

#[tokio::test]
async fn list_traces_since_filters_older_runs() {
    let (app, store) = test_app_with_trace_store().await;

    let recent_id = "01HXSINCE_RECENT";
    let old_id = "01HXSINCE_OLD";

    // Index files carry started_at; everything else is irrelevant for this
    // filter assertion.
    let recent = RunSummary {
        run_id: recent_id.into(),
        agent_id: "a".into(),
        started_at: std::time::SystemTime::now(),
        ended_at: None,
        prompt_ids: vec![],
        experiment_id: None,
        variant_name: None,
        final_status: None,
        judge_score: None,
    };
    let old = RunSummary {
        run_id: old_id.into(),
        agent_id: "a".into(),
        started_at: UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        ended_at: None,
        prompt_ids: vec![],
        experiment_id: None,
        variant_name: None,
        final_status: None,
        judge_score: None,
    };
    store
        .append(recent_id, &sample_inference_event(recent_id, "a"))
        .unwrap();
    store
        .append(old_id, &sample_inference_event(old_id, "a"))
        .unwrap();
    store.write_index_for_run(recent_id, &recent).unwrap();
    store.write_index_for_run(old_id, &old).unwrap();

    // Cutoff: 2024-01-01 — old run (2023-Nov) is excluded, recent (now) is
    // included.
    let (status, body) = get(
        &app,
        "/v1/traces?since=2024-01-01T00:00:00Z",
        Some(&test_admin_bearer()),
    )
    .await;
    assert_eq!(status, 200, "body: {}", String::from_utf8_lossy(&body));
    let json: Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "only the recent run should pass `since`");
    assert_eq!(runs[0]["run_id"], recent_id);
}

#[tokio::test]
async fn get_trace_unknown_returns_404() {
    let (app, _store) = test_app_with_trace_store().await;

    let (status, body) = get(
        &app,
        "/v1/traces/does-not-exist",
        Some(&test_admin_bearer()),
    )
    .await;

    assert_eq!(status, 404, "body: {}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn trace_routes_require_admin_auth() {
    let (app, _store) = test_app_with_trace_store().await;

    // No Authorization header — should get 401 or 403.
    let (status, _body) = get(&app, "/v1/traces", None).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn trace_routes_absent_without_trace_store() {
    // Build a server without a trace store attached.
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "trace-no-store".into(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    );
    // ADR-0041 mounts optional module routes only when their concrete module
    // state exists; enabling the surface without a TraceStore still leaves the
    // trace router absent.
    state.admin.admin_api_config = AdminApiConfig {
        expose_trace_routes: true,
        ..AdminApiConfig::default()
    };
    // No trace store, no bearer token needed for this surface.
    let app = build_router(&state);

    let (status, _) = get(&app, "/v1/traces", None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn default_admin_api_config_omits_trace_routes() {
    // Regression for F20: a deployment that attaches a TraceStore but
    // leaves `AdminApiConfig::default()` in place must NOT expose the
    // trace API. Operators have to flip `expose_trace_routes = true`
    // explicitly.
    let dir = temp_trace_dir();
    let store = Arc::new(FileTraceStore::new(&dir).expect("FileTraceStore::new"));
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "trace-default-off".into(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    );
    state.trace = Some(TraceModuleState {
        trace_store: store as Arc<dyn TraceStore>,
    });
    let app = build_router(&state);
    let (status, _) = get(&app, "/v1/traces", None).await;
    assert_eq!(
        status, 404,
        "trace routes must be off by default even when a store is attached"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn expose_trace_routes_false_returns_404() {
    let dir = temp_trace_dir();
    let store = Arc::new(FileTraceStore::new(&dir).expect("FileTraceStore::new"));
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "trace-disabled".into(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    );
    state.trace = Some(TraceModuleState {
        trace_store: store as Arc<dyn TraceStore>,
    });
    state.admin.admin_api_config = AdminApiConfig {
        expose_trace_routes: false,
        ..AdminApiConfig::default()
    };

    let app = build_router(&state);
    let (status, _) = get(&app, "/v1/traces", None).await;
    assert_eq!(
        status, 404,
        "routes must be absent when expose_trace_routes=false"
    );
}
