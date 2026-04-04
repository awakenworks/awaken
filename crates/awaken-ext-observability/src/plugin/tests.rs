use std::sync::Arc;

use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::inference::{LLMResponse, StreamResult, TokenUsage};
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::model::Phase;
use awaken_contract::state::{Snapshot, StateMap};
use awaken_runtime::{PhaseContext, PhaseHook, Plugin};

use crate::sink::InMemorySink;

use super::ObservabilityPlugin;
use super::hooks::{
    AfterInferenceHook, AfterToolExecuteHook, BeforeInferenceHook, BeforeToolExecuteHook,
    RunEndHook, RunStartHook,
};
use super::shared::{extract_cache_tokens, extract_token_counts, lock_unpoison};

fn empty_snapshot() -> Snapshot {
    Snapshot::new(0, Arc::new(StateMap::default()))
}

fn usage(prompt: i32, completion: i32, total: i32) -> TokenUsage {
    TokenUsage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        total_tokens: Some(total),
        cache_read_tokens: None,
        cache_creation_tokens: None,
        thinking_tokens: None,
    }
}

fn success_response(u: Option<TokenUsage>) -> LLMResponse {
    use awaken_contract::contract::content::ContentBlock;
    LLMResponse::success(StreamResult {
        content: vec![ContentBlock::text("hello")],
        tool_calls: vec![],
        usage: u,
        stop_reason: None,
        has_incomplete_tool_calls: false,
    })
}

/// Dispatch helper: invoke the appropriate phase hook sharing the plugin's inner state.
async fn run_phase(plugin: &ObservabilityPlugin, ctx: &PhaseContext) {
    let inner = Arc::clone(&plugin.inner);
    match ctx.phase {
        Phase::RunStart => RunStartHook(inner).run(ctx).await.unwrap(),
        Phase::BeforeInference => BeforeInferenceHook(inner).run(ctx).await.unwrap(),
        Phase::AfterInference => AfterInferenceHook(inner).run(ctx).await.unwrap(),
        Phase::BeforeToolExecute => BeforeToolExecuteHook(inner).run(ctx).await.unwrap(),
        Phase::AfterToolExecute => AfterToolExecuteHook(inner).run(ctx).await.unwrap(),
        Phase::RunEnd => RunEndHook(inner).run(ctx).await.unwrap(),
        _ => return,
    };
}

#[test]
fn new_defaults_model_empty() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    let model = lock_unpoison(&plugin.inner.model);
    assert!(model.is_empty());
}

#[test]
fn new_defaults_provider_empty() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    let provider = lock_unpoison(&plugin.inner.provider);
    assert!(provider.is_empty());
}

#[test]
fn new_defaults_temperature_none() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert!(lock_unpoison(&plugin.inner.temperature).is_none());
}

#[test]
fn new_defaults_top_p_none() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert!(lock_unpoison(&plugin.inner.top_p).is_none());
}

#[test]
fn new_defaults_max_tokens_none() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert!(lock_unpoison(&plugin.inner.max_tokens).is_none());
}

#[test]
fn new_defaults_operation_is_chat() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert_eq!(plugin.inner.operation, "chat");
}

#[test]
fn new_defaults_metrics_empty() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert!(metrics.inferences.is_empty());
    assert!(metrics.tools.is_empty());
    assert_eq!(metrics.session_duration_ms, 0);
}

#[test]
fn with_model_sets_model() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new()).with_model("gpt-4o");
    assert_eq!(*lock_unpoison(&plugin.inner.model), "gpt-4o");
}

#[test]
fn with_provider_sets_provider() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new()).with_provider("anthropic");
    assert_eq!(*lock_unpoison(&plugin.inner.provider), "anthropic");
}

#[test]
fn with_temperature_sets_temperature() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new()).with_temperature(0.7);
    assert_eq!(*lock_unpoison(&plugin.inner.temperature), Some(0.7));
}

#[test]
fn with_top_p_sets_top_p() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new()).with_top_p(0.9);
    assert_eq!(*lock_unpoison(&plugin.inner.top_p), Some(0.9));
}

#[test]
fn with_max_tokens_sets_max_tokens() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new()).with_max_tokens(4096);
    assert_eq!(*lock_unpoison(&plugin.inner.max_tokens), Some(4096));
}

#[test]
fn with_stop_sequences_sets_seqs() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new())
        .with_stop_sequences(vec!["STOP".into(), "END".into()]);
    let seqs = lock_unpoison(&plugin.inner.stop_sequences);
    assert_eq!(*seqs, vec!["STOP", "END"]);
}

#[test]
fn builder_chaining() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new())
        .with_model("claude-3")
        .with_provider("anthropic")
        .with_temperature(0.5)
        .with_top_p(0.8)
        .with_max_tokens(2048)
        .with_stop_sequences(vec!["DONE".into()]);

    assert_eq!(*lock_unpoison(&plugin.inner.model), "claude-3");
    assert_eq!(*lock_unpoison(&plugin.inner.provider), "anthropic");
    assert_eq!(*lock_unpoison(&plugin.inner.temperature), Some(0.5));
    assert_eq!(*lock_unpoison(&plugin.inner.top_p), Some(0.8));
    assert_eq!(*lock_unpoison(&plugin.inner.max_tokens), Some(2048));
    assert_eq!(*lock_unpoison(&plugin.inner.stop_sequences), vec!["DONE"]);
}

#[test]
fn descriptor_returns_observability() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert_eq!(plugin.descriptor().name, "observability");
}

#[tokio::test]
async fn on_run_start_initializes_run_start_time() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink);

    assert!(lock_unpoison(&plugin.inner.run_start).is_none());

    let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot());
    run_phase(&plugin, &ctx).await;

    assert!(lock_unpoison(&plugin.inner.run_start).is_some());
}

#[tokio::test]
async fn on_before_inference_records_start_time() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink);

    assert!(lock_unpoison(&plugin.inner.inference_start).is_none());

    let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
    run_phase(&plugin, &ctx).await;

    assert!(lock_unpoison(&plugin.inner.inference_start).is_some());
}

#[tokio::test]
async fn on_after_inference_records_genai_span() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("gpt-4")
        .with_provider("openai");

    let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(100, 50, 150))));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.inferences.len(), 1);
    assert_eq!(metrics.inferences[0].model, "gpt-4");
    assert_eq!(metrics.inferences[0].provider, "openai");
    assert_eq!(metrics.inferences[0].input_tokens, Some(100));
    assert_eq!(metrics.inferences[0].output_tokens, Some(50));
    // Also recorded in sink
    let sink_m = sink.metrics();
    assert_eq!(sink_m.inference_count(), 1);
}

#[tokio::test]
async fn on_after_inference_without_before_uses_zero_duration() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

    // Skip BeforeInference — go straight to AfterInference
    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(10, 5, 15))));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.inferences.len(), 1);
    assert_eq!(metrics.inferences[0].duration_ms, 0);
}

#[tokio::test]
async fn on_before_tool_execute_records_tool_start() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink);

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "call_42",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let tool_starts = lock_unpoison(&plugin.inner.tool_start);
    assert!(tool_starts.contains_key("call_42"));
}

#[tokio::test]
async fn on_after_tool_execute_records_tool_span() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(serde_json::json!({})))
        .with_tool_result(ToolResult::success(
            "search",
            serde_json::json!({"found": true}),
        ));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.tools.len(), 1);
    assert_eq!(metrics.tools[0].name, "search");
    assert_eq!(metrics.tools[0].call_id, "c1");
    assert!(metrics.tools[0].is_success());

    let sink_m = sink.metrics();
    assert_eq!(sink_m.tool_count(), 1);
}

#[tokio::test]
async fn on_after_tool_execute_no_result_skips_recording() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    // AfterToolExecute without tool_result
    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert!(metrics.tools.is_empty());
}

#[tokio::test]
async fn on_after_tool_execute_error_records_error_type() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "write",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("write", "c1", Some(serde_json::json!({})))
        .with_tool_result(ToolResult::error("write", "permission denied"));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.tools.len(), 1);
    assert!(!metrics.tools[0].is_success());
    assert_eq!(metrics.tools[0].error_type.as_deref(), Some("tool_error"));
}

#[test]
fn extract_token_counts_with_some() {
    let u = TokenUsage {
        prompt_tokens: Some(10),
        completion_tokens: Some(20),
        total_tokens: Some(30),
        thinking_tokens: Some(5),
        cache_read_tokens: None,
        cache_creation_tokens: None,
    };
    let (i, o, t, th) = extract_token_counts(Some(&u));
    assert_eq!(i, Some(10));
    assert_eq!(o, Some(20));
    assert_eq!(t, Some(30));
    assert_eq!(th, Some(5));
}

#[test]
fn extract_token_counts_with_none() {
    let (i, o, t, th) = extract_token_counts(None);
    assert!(i.is_none());
    assert!(o.is_none());
    assert!(t.is_none());
    assert!(th.is_none());
}

#[test]
fn extract_cache_tokens_with_some() {
    let u = TokenUsage {
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        thinking_tokens: None,
        cache_read_tokens: Some(100),
        cache_creation_tokens: Some(50),
    };
    let (read, creation) = extract_cache_tokens(Some(&u));
    assert_eq!(read, Some(100));
    assert_eq!(creation, Some(50));
}

#[test]
fn extract_cache_tokens_with_none() {
    let (read, creation) = extract_cache_tokens(None);
    assert!(read.is_none());
    assert!(creation.is_none());
}

// ---------------------------------------------------------------------------
// Handoff detection
// ---------------------------------------------------------------------------

fn identity(agent: &str) -> RunIdentity {
    RunIdentity::new(
        "t1".into(),
        None,
        "r1".into(),
        None,
        agent.into(),
        RunOrigin::User,
    )
}

#[tokio::test]
async fn handoff_detected_on_agent_change() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

    // RunStart with agent-A seeds the span context.
    let ctx =
        PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;

    // BeforeInference with agent-B should detect handoff.
    let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot())
        .with_run_identity(identity("agent-b"));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.handoffs.len(), 1);
    assert_eq!(metrics.handoffs[0].from_agent_id, "agent-a");
    assert_eq!(metrics.handoffs[0].to_agent_id, "agent-b");
    assert!(metrics.handoffs[0].timestamp_ms > 0);
}

#[tokio::test]
async fn no_handoff_on_same_agent() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

    let ctx =
        PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot())
        .with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert!(metrics.handoffs.is_empty());
}

#[tokio::test]
async fn no_handoff_on_first_inference() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

    // No RunStart -- span_context.agent_id is empty.
    let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot())
        .with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert!(metrics.handoffs.is_empty());
}

// ---------------------------------------------------------------------------
// Suspension detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspension_detected_on_pending_tool() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("approve", "c1", None);
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("approve", "c1", None)
        .with_tool_result(ToolResult::suspended("approve", "awaiting approval"));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    // Should have both a ToolSpan and a SuspensionSpan.
    assert_eq!(metrics.tools.len(), 1);
    assert_eq!(metrics.suspensions.len(), 1);
    assert_eq!(metrics.suspensions[0].action, "suspended");
    assert_eq!(metrics.suspensions[0].tool_call_id, "c1");
    assert_eq!(metrics.suspensions[0].tool_name, "approve");
    assert!(metrics.suspensions[0].timestamp_ms > 0);
}

#[tokio::test]
async fn no_suspension_on_success_tool() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", None);
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", None)
        .with_tool_result(ToolResult::success("search", serde_json::json!({})));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert!(metrics.suspensions.is_empty());
}

#[tokio::test]
async fn resume_detected_on_before_tool_with_resume_input() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let resume = ToolCallResume {
        decision_id: "d1".into(),
        action: ResumeDecisionAction::Resume,
        result: serde_json::Value::Null,
        reason: None,
        updated_at: 0,
    };

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("approve", "c1", None)
        .with_resume_input(resume);
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.suspensions.len(), 1);
    assert_eq!(metrics.suspensions[0].action, "resumed");
    assert_eq!(
        metrics.suspensions[0].resume_mode.as_deref(),
        Some("resume")
    );
    assert_eq!(metrics.suspensions[0].tool_call_id, "c1");
}

// ---------------------------------------------------------------------------
// Delegation detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delegation_detected_on_agent_tool() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    // Seed identity so delegation span has a parent_run_id.
    let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot())
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"))
        .with_tool_result(ToolResult::success(
            "agent_run_worker",
            serde_json::json!({"agent_id": "worker", "status": "completed"}),
        ));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.delegations.len(), 1);
    assert_eq!(metrics.delegations[0].target_agent_id, "worker");
    assert_eq!(metrics.delegations[0].parent_run_id, "r1");
    assert!(metrics.delegations[0].success);
    assert!(metrics.delegations[0].error_message.is_none());
    assert!(metrics.delegations[0].child_run_id.is_none());
}

#[tokio::test]
async fn delegation_extracts_child_run_id_from_metadata() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot())
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let tool_result = ToolResult::success(
        "agent_run_worker",
        serde_json::json!({"agent_id": "worker", "status": "completed"}),
    )
    .with_metadata(
        "child_run_id",
        serde_json::Value::String("child-456".into()),
    );

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"))
        .with_tool_result(tool_result);
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.delegations.len(), 1);
    assert_eq!(
        metrics.delegations[0].child_run_id.as_deref(),
        Some("child-456")
    );
    assert!(metrics.delegations[0].success);
}

#[tokio::test]
async fn delegation_not_detected_on_regular_tool() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", None);
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", None)
        .with_tool_result(ToolResult::success("search", serde_json::json!({})));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert!(metrics.delegations.is_empty());
}

#[tokio::test]
async fn delegation_records_error_on_failure() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot())
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"));
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("agent_run_worker", "c1", None)
        .with_run_identity(identity("orchestrator"))
        .with_tool_result(ToolResult::error("agent_run_worker", "sub-agent failed"));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.delegations.len(), 1);
    assert!(!metrics.delegations[0].success);
    assert_eq!(
        metrics.delegations[0].error_message.as_deref(),
        Some("sub-agent failed")
    );
}
