//! HTTP integration tests for `/v1/agents/:id/runtime-stats` and
//! `/v1/agents/runtime-stats`.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::ModelSpec;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_observability::{
    AgentRuntimeSnapshot, GenAISpan, MetricsEvent, MetricsSink, RuntimeStatsRegistry, SpanContext,
    ToolSpan,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, ServerConfig, ServerState};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "test-admin-token";

/// Stub executor — never invoked in these tests because no run is driven;
/// it only exists so `AgentRuntimeBuilder::build()` succeeds.
struct StubExecutor;

#[async_trait]
impl LlmExecutor for StubExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: Vec::new(),
            tool_calls: Vec::new(),
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }
    fn name(&self) -> &str {
        "stub"
    }
}

// ── Test fixtures ──────────────────────────────────────────────────

fn build_app(runtime_stats: Option<Arc<RuntimeStatsRegistry>>) -> axum::Router {
    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_in_memory_thread_run_store(store.clone())
            .with_provider("mock", Arc::new(StubExecutor))
            .with_model(ModelSpec::new("test-model", "mock", "mock"))
            .with_agent_spec(AgentSpec {
                id: "default".into(),
                model_id: "test-model".into(),
                system_prompt: "test".into(),
                max_rounds: 1,
                ..Default::default()
            })
            .build()
            .expect("runtime"),
    );
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".into(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    )
    .with_admin_api_config(AdminApiConfig {
        bearer_token: Some(ADMIN_TOKEN.into()),
        ..Default::default()
    });
    if let Some(reg) = runtime_stats {
        state = state.with_runtime_stats(reg);
    }
    build_router(&state)
}

fn ctx(agent: &str) -> SpanContext {
    SpanContext {
        run_id: "r".into(),
        thread_id: "t".into(),
        agent_id: agent.into(),
        parent_run_id: None,
        parent_tool_call_id: None,
        prompt_id: None,
        tool_desc_ids: vec![],
        skill_ids: vec![],
        release_tag: None,
        experiment_id: None,
        variant_name: None,
    }
}

fn inference(agent: &str, input: i32, output: i32, duration_ms: u64, err: bool) -> GenAISpan {
    GenAISpan {
        context: ctx(agent),
        step_index: None,
        model: "m".into(),
        provider: "p".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: Vec::new(),
        error_type: if err { Some("rate_limit".into()) } else { None },
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(input),
        output_tokens: Some(output),
        total_tokens: Some(input + output),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    }
}

fn tool(agent: &str, name: &str) -> ToolSpan {
    ToolSpan {
        context: ctx(agent),
        step_index: None,
        name: name.into(),
        operation: "execute_tool".into(),
        call_id: format!("call-{name}-{agent}"),
        tool_type: "function".into(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 5,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

async fn fetch(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 4096).await.expect("body");
    if bytes.is_empty() {
        return (status, Value::Null);
    }
    let value: Value = serde_json::from_slice(&bytes).expect("JSON body");
    (status, value)
}

// ── /v1/agents/:id/runtime-stats ───────────────────────────────────

#[tokio::test]
async fn returns_503_when_registry_not_configured() {
    let app = build_app(None);
    let (status, body) = fetch(app, "/v1/agents/anything/runtime-stats").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "runtime_stats registry not configured");
}

#[tokio::test]
async fn returns_404_for_unknown_agent_id() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    let app = build_app(Some(registry));
    let (status, body) = fetch(app, "/v1/agents/nobody/runtime-stats").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("nobody"));
}

#[tokio::test]
async fn returns_200_with_snapshot_for_known_agent() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    registry.record(&MetricsEvent::Inference(inference(
        "alpha", 100, 50, 200, false,
    )));
    registry.record(&MetricsEvent::Inference(inference(
        "alpha", 50, 25, 100, true,
    )));
    registry.record(&MetricsEvent::Tool(tool("alpha", "search")));
    registry.record(&MetricsEvent::Tool(tool("alpha", "search")));
    registry.record(&MetricsEvent::Tool(tool("alpha", "write")));

    let app = build_app(Some(registry));
    let (status, body) = fetch(app, "/v1/agents/alpha/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);

    // Verify the body deserialises into the public snapshot type.
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body.clone()).expect("snapshot serde");
    assert_eq!(snap.agent_id, "alpha");
    assert_eq!(snap.inference_count, 2);
    assert_eq!(snap.error_count, 1);
    assert_eq!(snap.input_tokens, 150);
    assert_eq!(snap.output_tokens, 75);
    let search = snap
        .tool_calls_by_tool
        .iter()
        .find(|s| s.tool == "search")
        .unwrap();
    assert_eq!(search.call_count, 2);
    let write = snap
        .tool_calls_by_tool
        .iter()
        .find(|s| s.tool == "write")
        .unwrap();
    assert_eq!(write.call_count, 1);
}

#[tokio::test]
async fn returns_404_for_known_other_agent_when_path_id_unknown() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    registry.record(&MetricsEvent::Inference(inference("alpha", 1, 1, 1, false)));
    let app = build_app(Some(registry));
    let (status, _) = fetch(app, "/v1/agents/beta/runtime-stats").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_field_names_match_documented_shape() {
    // Locking the JSON keys guards against accidental schema drift —
    // the admin console parser depends on exact field names.
    let registry = Arc::new(RuntimeStatsRegistry::new());
    registry.record(&MetricsEvent::Inference(inference("a", 1, 1, 10, false)));
    let app = build_app(Some(registry));
    let (_status, body) = fetch(app, "/v1/agents/a/runtime-stats").await;
    let obj = body.as_object().expect("snapshot is JSON object");
    for key in [
        "agent_id",
        "window_seconds",
        "bucket_window_seconds",
        "bucket_count",
        "inference_count",
        "error_count",
        "input_tokens",
        "output_tokens",
        "avg_inference_duration_ms",
        "p50_inference_duration_ms",
        "p95_inference_duration_ms",
        "suspensions",
        "handoffs",
        "delegations",
        "tool_calls_by_tool",
    ] {
        assert!(obj.contains_key(key), "snapshot missing {key} key");
    }
    let tool_arr = body["tool_calls_by_tool"].as_array().expect("tool array");
    if let Some(first) = tool_arr.first() {
        for key in [
            "tool",
            "call_count",
            "failure_count",
            "total_duration_ms",
            "avg_duration_ms",
        ] {
            assert!(first.get(key).is_some(), "tool row missing {key}");
        }
    }
}

#[tokio::test]
async fn agent_id_with_special_characters_is_url_decoded() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    registry.record(&MetricsEvent::Inference(inference(
        "alpha/beta",
        1,
        1,
        1,
        false,
    )));
    let app = build_app(Some(registry));
    // /alpha%2Fbeta/runtime-stats — URL-encoded slash.
    let (status, body) = fetch(app, "/v1/agents/alpha%2Fbeta/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body).expect("serde");
    assert_eq!(snap.agent_id, "alpha/beta");
}

// ── /v1/agents/runtime-stats (list) ─────────────────────────────────

#[tokio::test]
async fn list_returns_503_when_registry_not_configured() {
    let app = build_app(None);
    let (status, _) = fetch(app, "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn list_returns_empty_array_when_registry_has_no_agents() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    let app = build_app(Some(registry));
    let (status, body) = fetch(app, "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body["agents"].as_array().expect("agents array");
    assert!(arr.is_empty());
}

#[tokio::test]
async fn list_returns_one_snapshot_per_known_agent_sorted() {
    let registry = Arc::new(RuntimeStatsRegistry::new());
    registry.record(&MetricsEvent::Inference(inference(
        "worker", 1, 1, 1, false,
    )));
    registry.record(&MetricsEvent::Inference(inference(
        "planner", 1, 1, 1, false,
    )));
    registry.record(&MetricsEvent::Inference(inference(
        "reviewer", 1, 1, 1, false,
    )));
    let app = build_app(Some(registry));
    let (status, body) = fetch(app, "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body["agents"].as_array().expect("agents array");
    assert_eq!(arr.len(), 3);
    let ids: Vec<&str> = arr
        .iter()
        .map(|s| s["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["planner", "reviewer", "worker"]);
}
