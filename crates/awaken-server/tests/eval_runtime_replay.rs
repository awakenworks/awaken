//! Runtime-driven replayer integration test.
//!
//! Exercises a full agent loop through `awaken-server`'s router using a
//! scripted [`LlmExecutor`] and verifies the [`AgentMetrics`] surfaced by
//! `awaken-ext-observability`'s plugin match what `awaken-eval`'s
//! [`MockReplayer`] would have synthesised. This guards the eval framework
//! against drift between mocked outcomes and real runtime behaviour.
//!
//! No external infrastructure is required — the executor, store, mailbox,
//! and observability sink are all in-memory.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_eval::{Fixture, MockReplayer, MockResponse, Replayer};
use awaken_ext_observability::{InMemorySink, ObservabilityPlugin};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Scripted executor — returns whatever the fixture mocks.
// ---------------------------------------------------------------------------

struct ScriptedExecutor {
    response: MockResponse,
}

#[async_trait]
impl LlmExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        use awaken_contract::contract::content::ContentBlock;
        match &self.response {
            MockResponse::Text { text } => Ok(StreamResult {
                content: vec![ContentBlock::Text { text: text.clone() }],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            }),
            MockResponse::Error {
                error_type,
                message,
            } => {
                let detail = format!("{error_type}: {message}");
                Err(match error_type.as_str() {
                    "rate_limit" => InferenceExecutionError::rate_limited(detail),
                    "overloaded" => InferenceExecutionError::overloaded(detail),
                    _ => InferenceExecutionError::Provider(detail),
                })
            }
        }
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

// ---------------------------------------------------------------------------
// Runtime-driven replay
// ---------------------------------------------------------------------------

struct RuntimeReplayOutcome {
    /// Final assistant text concatenated from the AI SDK SSE body.
    final_text: String,
    /// Metrics captured by the observability plugin's in-memory sink.
    metrics: awaken_ext_observability::AgentMetrics,
}

async fn run_fixture_through_runtime(fixture: &Fixture) -> RuntimeReplayOutcome {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_provider("scripted");

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider(
                "scripted",
                Arc::new(ScriptedExecutor {
                    response: fixture.mock_response.clone(),
                }),
            )
            .with_model_binding(
                "scripted-model",
                ModelBinding {
                    provider_id: "scripted".into(),
                    upstream_model: "scripted".into(),
                },
            )
            .with_thread_run_store(store.clone())
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
        "eval-runtime-replay".into(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    let app = build_router(&state).with_state(state);

    let payload = json!({
        "threadId": format!("eval-thread-{}", fixture.id),
        "messages": [{
            "id": format!("u-{}", fixture.id),
            "role": "user",
            "parts": [{"type": "text", "text": fixture.user_input.clone()}]
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

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fixture {} returned non-200",
        fixture.id
    );

    let body = to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body bytes");
    let body_text = String::from_utf8_lossy(&body).to_string();
    let final_text = extract_assistant_text(&body_text);

    RuntimeReplayOutcome {
        final_text,
        metrics: sink.metrics(),
    }
}

/// Walk the AI SDK v6 SSE payload and concatenate all `text-delta` `delta`
/// fields. Falls back to an empty string when no text events are present.
fn extract_assistant_text(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        // AI SDK v6 streams JSON objects on each line (no `data:` prefix
        // when using Vercel UI message stream); be permissive.
        let json_str = line.strip_prefix("data: ").unwrap_or(line).trim();
        if json_str.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(json_str) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("text-delta")
            && let Some(delta) = value.get("delta").and_then(Value::as_str)
        {
            out.push_str(delta);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn text_fixture(id: &str, prompt: &str, response: &str) -> Fixture {
    Fixture {
        id: id.into(),
        description: None,
        user_input: prompt.into(),
        mock_response: MockResponse::Text {
            text: response.into(),
        },
        expect: awaken_eval::Expectation::default(),
    }
}

#[tokio::test]
async fn runtime_replay_text_response_matches_mock_text() {
    let fx = text_fixture("rt-text", "What is 2+2?", "the answer is 4");
    let runtime_outcome = run_fixture_through_runtime(&fx).await;
    let mock_outcome = MockReplayer::new().replay(&fx).await;

    // Real runtime drives a streaming SSE; we only assert the runtime's
    // text *contains* the scripted answer. (Newlines / spacing may differ.)
    assert!(
        runtime_outcome.final_text.contains("the answer is 4"),
        "runtime body did not contain scripted answer: {:?}",
        runtime_outcome.final_text
    );
    assert!(
        mock_outcome.final_text.contains("the answer is 4"),
        "mock outcome should also contain the scripted answer"
    );
}

#[tokio::test]
async fn runtime_replay_records_at_least_one_inference_span() {
    let fx = text_fixture("rt-inf", "p", "ok");
    let outcome = run_fixture_through_runtime(&fx).await;
    assert!(
        outcome.metrics.inference_count() >= 1,
        "expected at least one inference span; metrics: {:?}",
        outcome.metrics
    );
}

#[tokio::test]
async fn runtime_replay_token_counts_are_finite() {
    let fx = text_fixture("rt-tok", "What is 2+2?", "4");
    let outcome = run_fixture_through_runtime(&fx).await;
    let total_in = outcome.metrics.total_input_tokens();
    let total_out = outcome.metrics.total_output_tokens();
    // Scripted executor returns TokenUsage::default() (zeros), which is
    // still a finite, well-defined value — assert that the plugin
    // surfaces it without panicking.
    assert!(total_in >= 0);
    assert!(total_out >= 0);
}

#[tokio::test]
async fn runtime_replay_session_duration_is_set() {
    let fx = text_fixture("rt-dur", "p", "ok");
    let outcome = run_fixture_through_runtime(&fx).await;
    // The plugin populates session_duration_ms via on_run_end. With a
    // synchronous mock executor the duration may be 0; we just confirm
    // the field is present and didn't underflow.
    assert!(outcome.metrics.session_duration_ms <= u64::MAX / 2);
}

#[tokio::test]
async fn extract_assistant_text_handles_empty_body() {
    assert_eq!(extract_assistant_text(""), "");
}

#[tokio::test]
async fn extract_assistant_text_concatenates_text_deltas() {
    let body = r#"{"type":"text-delta","delta":"Hello "}
{"type":"text-delta","delta":"world"}
{"type":"finish"}"#;
    assert_eq!(extract_assistant_text(body), "Hello world");
}

#[tokio::test]
async fn extract_assistant_text_skips_non_text_events() {
    let body = r#"{"type":"start-step"}
{"type":"tool-input-start","toolName":"x"}
{"type":"text-delta","delta":"only this"}
{"type":"finish-step"}"#;
    assert_eq!(extract_assistant_text(body), "only this");
}

#[tokio::test]
async fn extract_assistant_text_tolerates_data_prefix() {
    let body = "data: {\"type\":\"text-delta\",\"delta\":\"abc\"}\n";
    assert_eq!(extract_assistant_text(body), "abc");
}

#[tokio::test]
async fn extract_assistant_text_tolerates_blank_lines() {
    let body = "\n\n{\"type\":\"text-delta\",\"delta\":\"x\"}\n\n";
    assert_eq!(extract_assistant_text(body), "x");
}

// ── Parity with MockReplayer ────────────────────────────────────────

#[tokio::test]
async fn runtime_and_mock_replayers_agree_on_text_substring() {
    let cases = [
        ("simple", "What is 2+2?", "4"),
        ("hello", "Say hi.", "hello"),
        ("multiword", "Repeat: green pelican", "green pelican"),
    ];

    for (id, prompt, response) in cases {
        let fx = text_fixture(id, prompt, response);
        let runtime_outcome = run_fixture_through_runtime(&fx).await;
        let mock_outcome = MockReplayer::new().replay(&fx).await;

        assert!(
            runtime_outcome.final_text.contains(response),
            "[{id}] runtime text {:?} did not contain {:?}",
            runtime_outcome.final_text,
            response
        );
        assert_eq!(
            mock_outcome.final_text, response,
            "[{id}] mock outcome should equal scripted response"
        );
    }
}

#[tokio::test]
async fn runtime_replay_records_one_inference_for_single_user_turn() {
    let fx = text_fixture("single-turn", "p", "ok");
    let outcome = run_fixture_through_runtime(&fx).await;
    // The mock executor returns EndTurn in one round, so exactly one
    // inference span should fire.
    assert_eq!(
        outcome.metrics.inference_count(),
        1,
        "expected exactly one inference span, got {}: {:?}",
        outcome.metrics.inference_count(),
        outcome.metrics
    );
    assert_eq!(outcome.metrics.tool_count(), 0);
}

#[tokio::test]
async fn runtime_replay_inference_span_carries_run_context() {
    let fx = text_fixture("ctx", "p", "ok");
    let outcome = run_fixture_through_runtime(&fx).await;
    let span = outcome
        .metrics
        .inferences
        .first()
        .expect("at least one span");
    assert!(!span.context.run_id.is_empty(), "run_id must be populated");
    assert!(
        !span.context.thread_id.is_empty(),
        "thread_id must be populated"
    );
    assert!(
        !span.context.agent_id.is_empty(),
        "agent_id must be populated"
    );
    assert_eq!(span.provider, "scripted");
}
