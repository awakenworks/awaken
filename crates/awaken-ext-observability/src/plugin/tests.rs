use std::sync::Arc;

use awaken_runtime::extensions::background::{
    BackgroundTaskStateKey, BackgroundTaskStateSnapshot, PersistedTaskMeta, TaskParentContext,
    TaskStatus,
};
use awaken_runtime::{PhaseContext, PhaseHook, Plugin};
use awaken_runtime_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_runtime_contract::contract::inference::{LLMResponse, StreamResult, TokenUsage};
use awaken_runtime_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_runtime_contract::contract::tool::ToolResult;
use awaken_runtime_contract::model::Phase;
use awaken_runtime_contract::state::{Snapshot, StateMap};

use crate::metrics::{ContentCapture, TOOL_PAYLOAD_TRUNCATED_MARKER, ToolIoCapture};
use crate::sink::InMemorySink;

use super::ObservabilityPlugin;
use super::hooks::{
    AfterInferenceHook, AfterToolExecuteHook, BackgroundTaskObserveHook, BeforeInferenceHook,
    BeforeToolExecuteHook, RunEndHook, RunStartHook,
};
use super::shared::{extract_cache_tokens, extract_token_counts, lock_unpoison};

fn empty_snapshot() -> Snapshot {
    Snapshot::new(0, Arc::new(StateMap::default()))
}

fn snapshot_with_background_task(meta: PersistedTaskMeta) -> Snapshot {
    let mut state = StateMap::default();
    let mut tasks = std::collections::HashMap::new();
    tasks.insert(meta.task_id.clone(), meta);
    state.insert::<BackgroundTaskStateKey>(BackgroundTaskStateSnapshot { tasks });
    Snapshot::new(0, Arc::new(state))
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
    use awaken_runtime_contract::contract::content::ContentBlock;
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
        Phase::StepEnd => BackgroundTaskObserveHook(inner).run(ctx).await.unwrap(),
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
fn new_defaults_tool_io_capture_disabled() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert_eq!(plugin.inner.tool_io_capture, ToolIoCapture::Disabled);
}

#[tokio::test]
async fn background_task_state_records_lifecycle_once_per_status() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let running = PersistedTaskMeta {
        task_id: "bg-1".to_string(),
        owner_thread_id: "thread-bg".to_string(),
        task_type: "sub_agent".to_string(),
        name: Some("worker".to_string()),
        description: "background worker".to_string(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 10,
        completed_at_ms: None,
        parent_context: TaskParentContext {
            run_id: Some("run-parent".to_string()),
            call_id: Some("call-bg".to_string()),
            agent_id: Some("agent-parent".to_string()),
            ..Default::default()
        },
    };

    let ctx = PhaseContext::new(
        Phase::StepEnd,
        snapshot_with_background_task(running.clone()),
    );
    run_phase(&plugin, &ctx).await;
    run_phase(&plugin, &ctx).await;

    let mut completed = running;
    completed.status = TaskStatus::Completed;
    completed.completed_at_ms = Some(40);
    let ctx = PhaseContext::new(Phase::StepEnd, snapshot_with_background_task(completed));
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.background_tasks.len(), 2);
    assert_eq!(metrics.background_tasks[0].status, TaskStatus::Running);
    assert_eq!(metrics.background_tasks[1].status, TaskStatus::Completed);
    assert_eq!(
        metrics.background_tasks[1].context.run_id,
        "run-parent".to_string()
    );
    assert_eq!(
        metrics.background_tasks[1]
            .context
            .parent_tool_call_id
            .as_deref(),
        Some("call-bg")
    );
}

#[tokio::test]
async fn background_task_dedup_distinguishes_owner_thread() {
    // Two task managers on different owner threads can both mint `bg-0`. The
    // dedup map must treat them as distinct so the second event is recorded
    // instead of being silently absorbed.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let make = |thread: &str| PersistedTaskMeta {
        task_id: "bg-0".to_string(),
        owner_thread_id: thread.to_string(),
        task_type: "sub_agent".to_string(),
        name: Some("worker".to_string()),
        description: "shared id, different owners".to_string(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 10,
        completed_at_ms: None,
        parent_context: TaskParentContext::default(),
    };

    let ctx = PhaseContext::new(
        Phase::StepEnd,
        snapshot_with_background_task(make("thread-a")),
    );
    run_phase(&plugin, &ctx).await;
    let ctx = PhaseContext::new(
        Phase::StepEnd,
        snapshot_with_background_task(make("thread-b")),
    );
    run_phase(&plugin, &ctx).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.background_tasks.len(), 2);
    let owners: Vec<&str> = metrics
        .background_tasks
        .iter()
        .map(|s| s.context.thread_id.as_str())
        .collect();
    assert!(owners.contains(&"thread-a"));
    assert!(owners.contains(&"thread-b"));
}

#[tokio::test]
async fn run_end_resets_per_run_metrics_but_keeps_background_task_dedup() {
    // Regression: persisted background-task snapshots can survive across
    // runs; the per-run metrics MUST reset, but the dedup map MUST NOT, or
    // already-emitted (status, task) pairs would be re-recorded each run.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

    let running = PersistedTaskMeta {
        task_id: "bg-reset".to_string(),
        owner_thread_id: "thread-bg".to_string(),
        task_type: "sub_agent".to_string(),
        name: Some("worker".to_string()),
        description: "background worker".to_string(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 10,
        completed_at_ms: None,
        parent_context: TaskParentContext::default(),
    };

    // Run 1: observe Running once.
    let ctx =
        PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;
    let ctx = PhaseContext::new(
        Phase::StepEnd,
        snapshot_with_background_task(running.clone()),
    );
    run_phase(&plugin, &ctx).await;
    assert_eq!(
        lock_unpoison(&plugin.inner.metrics).background_tasks.len(),
        1
    );
    assert_eq!(
        lock_unpoison(&plugin.inner.background_task_statuses).len(),
        1
    );

    // RunEnd resets the per-run metric Vec but preserves the dedup map.
    let ctx =
        PhaseContext::new(Phase::RunEnd, empty_snapshot()).with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;
    assert!(
        lock_unpoison(&plugin.inner.metrics)
            .background_tasks
            .is_empty(),
        "per-run metrics must reset",
    );
    assert_eq!(
        lock_unpoison(&plugin.inner.background_task_statuses).len(),
        1,
        "dedup map must persist across runs"
    );

    // Run 2: snapshot still contains the same Running task. Dedup MUST
    // suppress re-emission.
    let ctx =
        PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_identity(identity("agent-a"));
    run_phase(&plugin, &ctx).await;
    let ctx = PhaseContext::new(
        Phase::StepEnd,
        snapshot_with_background_task(running.clone()),
    );
    run_phase(&plugin, &ctx).await;
    assert!(
        lock_unpoison(&plugin.inner.metrics)
            .background_tasks
            .is_empty(),
        "Running was already emitted in run 1; run 2 must not duplicate it",
    );

    // Status transition Running -> Completed MUST still be recorded.
    let mut completed = running;
    completed.status = TaskStatus::Completed;
    completed.completed_at_ms = Some(40);
    let ctx = PhaseContext::new(Phase::StepEnd, snapshot_with_background_task(completed));
    run_phase(&plugin, &ctx).await;
    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.background_tasks.len(), 1);
    assert_eq!(metrics.background_tasks[0].status, TaskStatus::Completed);
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
        .with_stop_sequences(vec!["DONE".into()])
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults);

    assert_eq!(*lock_unpoison(&plugin.inner.model), "claude-3");
    assert_eq!(*lock_unpoison(&plugin.inner.provider), "anthropic");
    assert_eq!(*lock_unpoison(&plugin.inner.temperature), Some(0.5));
    assert_eq!(*lock_unpoison(&plugin.inner.top_p), Some(0.8));
    assert_eq!(*lock_unpoison(&plugin.inner.max_tokens), Some(2048));
    assert_eq!(*lock_unpoison(&plugin.inner.stop_sequences), vec!["DONE"]);
    assert_eq!(
        plugin.inner.tool_io_capture,
        ToolIoCapture::ArgumentsAndResults
    );
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
async fn after_inference_omits_content_when_capture_disabled() {
    // Default policy is Disabled — capture stays off without an explicit
    // opt-in. Existing trace storage stays metrics-only by default.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink).with_model("m");

    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(1, 1, 2))));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let span = &metrics.inferences[0];
    assert!(span.response_content.is_none());
    assert!(span.response_tool_calls.is_none());
}

#[tokio::test]
async fn after_inference_captures_chat_content_when_enabled() {
    // With ContentCapture::Enabled the assistant text is serialised onto
    // the span so ADR-0032 D5 (trace → fixture) has something to read.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink)
        .with_model("m")
        .with_content_capture(ContentCapture::Enabled);

    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(1, 1, 2))));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let span = &metrics.inferences[0];
    let content = span.response_content.as_ref().expect("content captured");
    // success_response builds vec![ContentBlock::text("hello")] — a single
    // text block. Round-tripping through serde_json preserves the type tag
    // + text payload so the eval converter can decode it back.
    let array = content.as_array().expect("content is an array");
    assert_eq!(array.len(), 1);
    assert!(array[0].to_string().contains("hello"));
    // No tool calls in this turn — that field stays empty rather than
    // being an empty array, so a ToolUse-only turn and a text-only turn
    // can be distinguished from the span alone.
    assert!(span.response_tool_calls.is_none());
}

#[tokio::test]
async fn after_inference_captures_tool_calls_when_enabled() {
    use awaken_runtime_contract::contract::inference::StopReason;
    use awaken_runtime_contract::contract::message::ToolCall;

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink)
        .with_model("m")
        .with_content_capture(ContentCapture::Enabled);

    let tool_use_response = LLMResponse::success(StreamResult {
        content: vec![],
        tool_calls: vec![ToolCall::new(
            "call-1",
            "weather.get",
            serde_json::json!({"city": "Paris"}),
        )],
        usage: Some(usage(5, 1, 6)),
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    });

    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(tool_use_response);
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let span = &metrics.inferences[0];
    assert!(span.response_content.is_none(), "text-only field absent");
    let tools = span
        .response_tool_calls
        .as_ref()
        .expect("tool calls captured");
    let array = tools.as_array().expect("tool calls is an array");
    assert_eq!(array.len(), 1);
    assert!(array[0].to_string().contains("weather.get"));
}

#[tokio::test]
async fn after_inference_captures_request_messages_on_first_inference_only() {
    use awaken_runtime_contract::contract::content::ContentBlock;
    use awaken_runtime_contract::contract::message::Message;

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink)
        .with_model("m")
        .with_content_capture(ContentCapture::Enabled);

    let _ = ContentBlock::text("ping"); // silence unused-import in tests below
    let messages: Arc<[Arc<Message>]> = Arc::from(vec![Arc::new(Message::user("ping"))]);

    // First inference: step counter is 0, request_messages must capture.
    let mut ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(1, 1, 2))));
    ctx.messages = messages.clone();
    run_phase(&plugin, &ctx).await;

    // Second inference on same run: step counter is 1, request_messages
    // must stay None to avoid O(turns²) duplicated history storage.
    let mut ctx2 = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(1, 1, 2))));
    ctx2.messages = messages.clone();
    run_phase(&plugin, &ctx2).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert!(metrics.inferences[0].request_messages.is_some());
    let captured = metrics.inferences[0]
        .request_messages
        .as_ref()
        .and_then(|v| v.as_array())
        .expect("first span captured a list");
    assert_eq!(captured.len(), 1);
    assert!(captured[0].to_string().contains("ping"));
    assert!(metrics.inferences[1].request_messages.is_none());
}

#[tokio::test]
async fn after_inference_omits_request_messages_when_capture_disabled() {
    use awaken_runtime_contract::contract::content::ContentBlock;
    use awaken_runtime_contract::contract::message::Message;

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink).with_model("m");

    let _ = ContentBlock::text("ignored");
    let messages: Arc<[Arc<Message>]> = Arc::from(vec![Arc::new(Message::user("ignored"))]);
    let mut ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(success_response(Some(usage(1, 1, 2))));
    ctx.messages = messages;
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert!(metrics.inferences[0].request_messages.is_none());
}

#[tokio::test]
async fn after_inference_capture_skips_error_branch() {
    use awaken_runtime_contract::contract::inference::InferenceError;

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink)
        .with_model("m")
        .with_content_capture(ContentCapture::Enabled);

    // An errored inference has no StreamResult to serialise from. The hook
    // must leave the capture fields as None instead of inventing content.
    let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(
        LLMResponse::error(InferenceError {
            error_type: "rate_limit".into(),
            error_class: Some("rate_limit".into()),
            message: "429".into(),
        }),
    );
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let span = &metrics.inferences[0];
    assert!(span.response_content.is_none());
    assert!(span.response_tool_calls.is_none());
    assert!(
        span.error_type.is_some(),
        "error path still records error_type"
    );
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
    assert!(metrics.tools[0].call_arguments.is_none());
    assert!(metrics.tools[0].call_result.is_none());

    let sink_m = sink.metrics();
    assert_eq!(sink_m.tool_count(), 1);
}

#[tokio::test]
async fn tool_io_capture_records_opt_in_arguments_and_results() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults);
    let args = serde_json::json!({"query": "otel", "limit": 3});
    let result = serde_json::json!({"count": 2});

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(args.clone()),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(args.clone()))
        .with_tool_result(ToolResult::success("search", result.clone()));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(metrics.tools.len(), 1);
    assert_eq!(metrics.tools[0].call_arguments.as_ref(), Some(&args));
    assert_eq!(metrics.tools[0].call_result.as_ref(), Some(&result));

    let sink_m = sink.metrics();
    assert_eq!(sink_m.tools[0].call_arguments.as_ref(), Some(&args));
    assert_eq!(sink_m.tools[0].call_result.as_ref(), Some(&result));
}

#[tokio::test]
async fn tool_io_capture_redacts_sensitive_fields() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults);
    let args = serde_json::json!({
        "query": "otel",
        "api_key": "sample-api-key",
        "nested": {"token": "sample-token", "safe": "kept"}
    });
    let result = serde_json::json!({"password": "sample-password", "count": 1});

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(args.clone()),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(args))
        .with_tool_result(ToolResult::success("search", result));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let rendered = serde_json::to_string(&metrics.tools[0]).unwrap();
    assert!(!rendered.contains("sample-api-key"));
    assert!(!rendered.contains("sample-token"));
    assert!(!rendered.contains("sample-password"));
    assert!(rendered.contains("\"api_key\":\"***\""));
    assert!(rendered.contains("\"token\":\"***\""));
    assert!(rendered.contains("\"password\":\"***\""));
}

#[tokio::test]
async fn tool_io_capture_records_error_results_through_sanitizer() {
    // Error tool results were previously dropped from `call_result`; debugging
    // tool failures needs them. Verify error data is captured AND still goes
    // through the default redactor.
    use awaken_runtime_contract::contract::tool::{ToolResult, ToolStatus};
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults);

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "fetch",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let error_data = serde_json::json!({
        "error": "upstream 401",
        "details": { "Authorization": "Bearer leaked-bearer" }
    });
    let mut result = ToolResult::error("fetch", "auth failed");
    result.status = ToolStatus::Error;
    result.data = error_data;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("fetch", "c1", Some(serde_json::json!({})))
        .with_tool_result(result);
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let captured = metrics.tools[0]
        .call_result
        .as_ref()
        .expect("error result must still be captured");
    let rendered = serde_json::to_string(captured).unwrap();
    assert!(rendered.contains("\"error\":\"upstream 401\""));
    assert!(
        !rendered.contains("leaked-bearer"),
        "bearer must be redacted in error result: {rendered}"
    );
    assert_eq!(metrics.tools[0].error_type.as_deref(), Some("tool_error"));
}

#[tokio::test]
async fn tool_io_capture_redacts_extended_sensitive_keys_case_insensitive_nested() {
    // Pin the extended sensitive-key list across casing, nesting, arrays.
    // Each leaked-* literal MUST disappear; each *-redacted-here marker
    // shows where redaction landed.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults);
    let args = serde_json::json!({
        "Cookie": "leaked-cookie",
        "Set-Cookie": "leaked-set-cookie",
        "session_id": "leaked-session",
        "JWT": "leaked-jwt",
        "access_key": "leaked-access-key",
        "client_secret": "leaked-client-secret",
        "refresh_token": "leaked-refresh",
        "id_token": "leaked-id-token",
        "Auth": "leaked-auth-header",
        "headers": [
            { "Authorization": "Bearer leaked-bearer" },
            { "x-api-key": "leaked-x-api" }
        ],
        "deeply": { "nested": { "Password": "leaked-deep" } },
        "kept": "ok"
    });
    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(args.clone()),
    );
    run_phase(&plugin, &ctx).await;
    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(args))
        .with_tool_result(ToolResult::success(
            "search",
            serde_json::json!({ "ok": true }),
        ));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let rendered = serde_json::to_string(&metrics.tools[0]).unwrap();
    for leaked in [
        "leaked-cookie",
        "leaked-set-cookie",
        "leaked-session",
        "leaked-jwt",
        "leaked-access-key",
        "leaked-client-secret",
        "leaked-refresh",
        "leaked-id-token",
        "leaked-auth-header",
        "leaked-bearer",
        "leaked-x-api",
        "leaked-deep",
    ] {
        assert!(!rendered.contains(leaked), "found {leaked} in {rendered}");
    }
    assert!(rendered.contains("\"kept\":\"ok\""));
}

#[tokio::test]
async fn tool_io_capture_allows_fields_and_truncates_oversized_payloads() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::Arguments)
        .with_tool_io_allowed_fields(["query"])
        .with_tool_io_max_payload_bytes(48);
    let args = serde_json::json!({
        "query": "x".repeat(200),
        "api_key": "sample-api-key",
        "dropped": "value"
    });

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(args.clone()),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(args))
        .with_tool_result(ToolResult::success(
            "search",
            serde_json::json!({"ok": true}),
        ));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let captured = metrics.tools[0]
        .call_arguments
        .as_ref()
        .expect("captured arguments");
    assert_eq!(
        captured
            .get(TOOL_PAYLOAD_TRUNCATED_MARKER)
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    let rendered = serde_json::to_string(captured).unwrap();
    assert!(!rendered.contains("sample-api-key"));
    assert!(!rendered.contains("dropped"));
    assert!(metrics.tools[0].has_truncated_payload());
}

#[tokio::test]
async fn default_redactor_runs_after_custom_redactor() {
    let sink = InMemorySink::new();
    // A pathological custom redactor that re-introduces a sensitive field.
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_tool_io_capture(ToolIoCapture::ArgumentsAndResults)
        .with_tool_io_redactor(|value| match value {
            serde_json::Value::Object(mut map) => {
                map.insert(
                    "api_key".to_string(),
                    serde_json::Value::String("leaked-by-custom".into()),
                );
                serde_json::Value::Object(map)
            }
            other => other,
        });
    let args = serde_json::json!({ "query": "search" });

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(args.clone()),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info("search", "c1", Some(args))
        .with_tool_result(ToolResult::success(
            "search",
            serde_json::json!({ "ok": true }),
        ));
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    let rendered =
        serde_json::to_string(metrics.tools[0].call_arguments.as_ref().unwrap()).unwrap();
    assert!(
        !rendered.contains("leaked-by-custom"),
        "default redactor must mask after custom: {rendered}"
    );
    assert!(rendered.contains("\"api_key\":\"***\""));
}

#[tokio::test]
async fn on_after_tool_execute_no_result_records_synthetic_failure_span() {
    // Missing `tool_result` is a real failure mode (executor crash, dropped
    // result channel, ...). The hook must still emit a terminal ToolSpan so
    // the sink, Prometheus counters, and any pending OTel context all see
    // one event for this call.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone());

    let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot()).with_tool_info(
        "search",
        "c1",
        Some(serde_json::json!({})),
    );
    run_phase(&plugin, &ctx).await;

    let metrics = lock_unpoison(&plugin.inner.metrics);
    assert_eq!(
        metrics.tools.len(),
        1,
        "synthetic failure span must be recorded"
    );
    assert_eq!(metrics.tools[0].name, "search");
    assert_eq!(metrics.tools[0].call_id, "c1");
    assert_eq!(
        metrics.tools[0].error_type.as_deref(),
        Some("missing_tool_result")
    );
    assert!(metrics.tools[0].call_result.is_none());
    drop(metrics);

    let sink_metrics = sink.metrics();
    assert_eq!(
        sink_metrics.tool_count(),
        1,
        "sink must observe the failure too"
    );
    assert_eq!(sink_metrics.tool_failures(), 1);

    assert!(
        plugin.inner.tool_tracing_span.lock().await.is_empty(),
        "tracing span must be released even when tool_result is missing",
    );
    assert!(
        plugin.inner.tool_start.lock().await.is_empty(),
        "tool_start must be released even when tool_result is missing",
    );
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
