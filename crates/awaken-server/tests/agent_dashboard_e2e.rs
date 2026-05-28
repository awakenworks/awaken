//! End-to-end pipeline test for the per-agent dashboard.
//!
//! Wires a real `AgentRuntime` with a scripted `LlmExecutor` and an
//! `ObservabilityPlugin` whose sink list contains a shared
//! `RuntimeStatsRegistry`. The same registry is also installed on
//! `ServerState`. After driving real chat requests through the AI SDK route,
//! the test queries `/v1/agents/:id/runtime-stats` and asserts the
//! endpoint reflects what the runtime actually did.
//!
//! Pipeline under test:
//!
//! ```text
//!   POST /v1/ai-sdk/chat
//!        │
//!        ▼
//!   AgentRuntime  ──► ObservabilityPlugin (RunStart/Inference/Tool/RunEnd)
//!        │                   │
//!        │                   └─► RuntimeStatsRegistry (shared via Arc)
//!        │                              ▲
//!        ▼                              │
//!   SSE response                        │
//!                                       │
//!   GET /v1/agents/default/runtime-stats┘
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_observability::{
    AgentRuntimeSnapshot, MetricsEvent, MetricsSink, ObservabilityPlugin, RuntimeStatsRegistry,
    SinkError,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, ServerConfig, ServerState};
use awaken_server::routes::build_router;
use awaken_server_contract::ModelSpec;
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_server_contract::registry_spec::AgentSpec;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "test-admin-token";

// ---------------------------------------------------------------------------
// Scripted executor
// ---------------------------------------------------------------------------

struct ScriptedExecutor {
    response: String,
}

#[async_trait]
impl LlmExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        use awaken_server_contract::contract::content::ContentBlock;
        Ok(StreamResult {
            content: vec![ContentBlock::Text {
                text: self.response.clone(),
            }],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(42),
                completion_tokens: Some(7),
                total_tokens: Some(49),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                thinking_tokens: None,
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

/// Adapter that lets the plugin own a `MetricsSink` value while sharing
/// the underlying registry state with `ServerState` via the same backing
/// `Arc<Mutex<...>>`.
#[derive(Clone)]
struct SharedRegistrySink(RuntimeStatsRegistry);

impl MetricsSink for SharedRegistrySink {
    fn record(&self, event: &MetricsEvent) {
        self.0.record(event);
    }
    fn on_run_end(&self, metrics: &awaken_ext_observability::AgentMetrics) {
        self.0.on_run_end(metrics);
    }
    fn flush(&self) -> Result<(), SinkError> {
        Ok(())
    }
    fn shutdown(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// App fixture
// ---------------------------------------------------------------------------

fn build_app(response: &str) -> (axum::Router, Arc<RuntimeStatsRegistry>) {
    let registry = Arc::new(RuntimeStatsRegistry::new());

    // The plugin sink and the ServerState share the same registry instance —
    // RuntimeStatsRegistry's Clone keeps the inner Arc<Mutex<...>>, so
    // both views see the same buckets.
    let plugin_sink = SharedRegistrySink((*registry).clone());
    let plugin = ObservabilityPlugin::new(plugin_sink).with_provider("scripted");

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider(
                "scripted",
                Arc::new(ScriptedExecutor {
                    response: response.into(),
                }),
            )
            .with_model(ModelSpec::new("scripted-model", "scripted", "scripted"))
            .with_in_memory_thread_run_store(store.clone())
            .with_agent_spec(AgentSpec {
                id: "default".into(),
                model_id: "scripted-model".into(),
                system_prompt: "You are a test assistant.".into(),
                max_rounds: 2,
                plugin_ids: vec!["observability".into()],
                ..Default::default()
            })
            .with_plugin("observability", Arc::new(plugin))
            .build()
            .expect("runtime"),
    );

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "dashboard-e2e".into(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));

    let mut state = ServerState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    state.admin.admin_api_config = AdminApiConfig {
        bearer_token: Some(ADMIN_TOKEN.into()),
        ..Default::default()
    };
    state.run.runtime_stats = Some(Arc::clone(&registry));

    let app = build_router(&state);
    (app, registry)
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn drive_chat(app: axum::Router, thread_id: &str, prompt: &str) -> StatusCode {
    let payload = json!({
        "threadId": thread_id,
        "messages": [{
            "id": format!("u-{thread_id}"),
            "role": "user",
            "parts": [{"type": "text", "text": prompt}]
        }]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .expect("router responds");
    let status = resp.status();
    // Drain the body so the SSE stream actually drives the run to
    // completion — without consuming it, the run can be cancelled when
    // the receiver drops.
    let _ = to_bytes(resp.into_body(), 4 * 1024 * 1024).await;
    status
}

async fn fetch_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
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

// ---------------------------------------------------------------------------
// End-to-end tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn endpoint_returns_404_until_runtime_drives_a_run() {
    let (app, _registry) = build_app("ok");
    let (status, _) = fetch_json(app.clone(), "/v1/agents/default/runtime-stats").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "registry should be empty before any run"
    );
}

#[tokio::test]
async fn endpoint_reflects_a_real_run_end_to_end() {
    let (app, _registry) = build_app("the answer is 4");

    // 1) Drive a real chat through the AI SDK protocol.
    assert_eq!(
        drive_chat(app.clone(), "thread-e2e-1", "What is 2+2?").await,
        StatusCode::OK,
    );

    // 2) Query the per-agent dashboard endpoint.
    let (status, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);

    // 3) Body deserialises into the documented shape.
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body.clone()).expect("snapshot serde");
    assert_eq!(snap.agent_id, "default");
    assert!(
        snap.inference_count >= 1,
        "expected at least one inference, got {}",
        snap.inference_count
    );
    assert_eq!(
        snap.error_count, 0,
        "scripted EndTurn must not register as an error"
    );
    // Token usage from ScriptedExecutor's TokenUsage{42, 7, 49}.
    assert!(
        snap.input_tokens >= 42,
        "input_tokens = {}",
        snap.input_tokens
    );
    assert!(
        snap.output_tokens >= 7,
        "output_tokens = {}",
        snap.output_tokens
    );
    // Bucket window metadata propagates from the registry defaults.
    assert_eq!(snap.window_seconds, 24 * 60 * 60);
    assert_eq!(snap.bucket_count, 144);
}

#[tokio::test]
async fn endpoint_aggregates_across_multiple_runs() {
    let (app, _registry) = build_app("ok");

    for n in 0..3 {
        let thread = format!("thread-e2e-multi-{n}");
        assert_eq!(
            drive_chat(app.clone(), &thread, "say ok").await,
            StatusCode::OK,
        );
    }

    let (status, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body).expect("serde");
    assert!(
        snap.inference_count >= 3,
        "expected ≥3 inferences after 3 runs, got {}",
        snap.inference_count
    );
    // No tool spans were recorded by the scripted executor.
    assert!(snap.tool_calls_by_tool.is_empty());
}

#[tokio::test]
async fn list_endpoint_returns_only_agents_that_have_run() {
    let (app, _registry) = build_app("ok");

    // Empty list before any run.
    let (status, body) = fetch_json(app.clone(), "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let agents = body["agents"].as_array().expect("agents array");
    assert!(agents.is_empty(), "expected empty list before any run");

    // After a run, the default agent appears.
    drive_chat(app.clone(), "list-thread", "hi").await;
    let (status, body) = fetch_json(app, "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let agents = body["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["agent_id"], "default");
}

#[tokio::test]
async fn snapshot_avg_and_percentile_durations_are_finite() {
    let (app, _registry) = build_app("ok");
    drive_chat(app.clone(), "duration-thread", "hi").await;

    let (status, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body).expect("serde");

    assert!(
        snap.avg_inference_duration_ms.is_finite(),
        "avg_inference_duration_ms must be finite: {}",
        snap.avg_inference_duration_ms
    );
    // Percentiles are u64, finite by type. We just confirm p95 >= p50.
    assert!(snap.p95_inference_duration_ms >= snap.p50_inference_duration_ms);
}

#[tokio::test]
async fn snapshot_carries_histogram_and_extended_percentiles_after_a_run() {
    let (app, _registry) = build_app("ok");
    drive_chat(app.clone(), "histogram-thread", "hi").await;
    let (status, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body).expect("serde");

    // Inference percentiles + min/max are present (≥0; min ≤ max ≤ ~ms).
    assert!(snap.min_inference_duration_ms <= snap.max_inference_duration_ms);
    assert!(snap.p50_inference_duration_ms <= snap.p95_inference_duration_ms);
    assert!(snap.p95_inference_duration_ms <= snap.p99_inference_duration_ms);

    // Histogram is non-empty and counts sum to inference_count.
    assert!(!snap.inference_duration_histogram.is_empty());
    let total: u64 = snap
        .inference_duration_histogram
        .iter()
        .map(|b| b.count)
        .sum();
    assert_eq!(total, snap.inference_count);

    // Last entry of the histogram is the +infinity bucket.
    assert_eq!(
        snap.inference_duration_histogram
            .last()
            .map(|b| b.upper_bound_ms),
        Some(None),
    );
}

#[tokio::test]
async fn snapshot_field_names_match_extended_shape() {
    // Locks the JSON keys against accidental schema drift in the new
    // M12 fields — admin-console parser depends on the exact spelling.
    let (app, _registry) = build_app("ok");
    drive_chat(app.clone(), "shape-thread", "hi").await;
    let (_, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    let obj = body.as_object().expect("snapshot is JSON object");
    for key in [
        "min_inference_duration_ms",
        "max_inference_duration_ms",
        "p99_inference_duration_ms",
        "inference_duration_histogram",
    ] {
        assert!(obj.contains_key(key), "snapshot missing {key} key");
    }
    let bucket = body["inference_duration_histogram"][0]
        .as_object()
        .expect("histogram entry is object");
    for key in ["upper_bound_ms", "count"] {
        assert!(bucket.contains_key(key), "bucket missing {key} key");
    }
}

#[tokio::test]
async fn registry_unaffected_when_other_endpoint_called_first() {
    // Defends against an accidental shared-state coupling between the
    // /v1/agents/runtime-stats list endpoint and the per-agent endpoint.
    let (app, _registry) = build_app("ok");

    // Hit the list endpoint first (returns []).
    let (status, _) = fetch_json(app.clone(), "/v1/agents/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);

    // Drive a run.
    drive_chat(app.clone(), "ordering-thread", "hi").await;

    // Single-agent endpoint must now report the run.
    let (status, body) = fetch_json(app, "/v1/agents/default/runtime-stats").await;
    assert_eq!(status, StatusCode::OK);
    let snap: AgentRuntimeSnapshot = serde_json::from_value(body).expect("serde");
    assert!(snap.inference_count >= 1);
}
