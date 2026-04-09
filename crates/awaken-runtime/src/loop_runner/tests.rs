use std::sync::Arc;

use super::*;
use crate::hooks::PhaseContext;
use crate::phase::ExecutionEnv;
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::registry::ResolvedAgent;
use crate::state::StateStore;
use crate::{PhaseHook, StateCommand};
use awaken_contract::StateError;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::VecEventSink;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::suspension::{
    PendingToolCall, SuspendTicket, Suspension, ToolCallOutcome, ToolCallResumeMode,
};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::contract::tool_intercept::{ToolInterceptAction, ToolInterceptPayload};
use awaken_contract::model::{
    PendingScheduledActions, Phase, ScheduledAction, ScheduledActionEnvelope,
    ScheduledActionQueueUpdate, ScheduledActionSpec,
};
use awaken_contract::state::{StateKey, StateKeyOptions};
use serde_json::{Value, json};

use super::actions::{
    LoopActionHandlersPlugin, apply_context_messages, apply_tool_filter_payloads,
    merge_override_payloads, resolve_intercept_payloads, take_context_messages,
};
use crate::agent::state::{
    AddContextMessage, InferenceOverrideState, InferenceOverrideStateAction,
    InferenceOverrideStateValue, RunLifecycle, RunLifecycleUpdate, ToolFilterState,
    ToolFilterStateAction, ToolFilterStateValue,
};
use crate::phase::PhaseRuntime;

/// Helper: create a PhaseRuntime + ExecutionEnv with action handlers registered.
fn test_runtime() -> (PhaseRuntime, ExecutionEnv) {
    let store = StateStore::new();
    store
        .install_plugin(LoopStatePlugin)
        .expect("install LoopStatePlugin");
    store
        .install_plugin(LoopActionHandlersPlugin)
        .expect("install LoopActionHandlersPlugin");

    // Initialize RunLifecycle so step counting works
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<RunLifecycle>(RunLifecycleUpdate::Start {
        run_id: "test".into(),
        updated_at: 0,
    });
    store.commit(patch).expect("init lifecycle");

    let runtime = PhaseRuntime::new(store).expect("create runtime");
    let env =
        ExecutionEnv::from_plugins(&[Arc::new(LoopActionHandlersPlugin)], &Default::default())
            .expect("build env");
    (runtime, env)
}

fn test_runtime_with_plugin<P>(plugin: P) -> (PhaseRuntime, ExecutionEnv)
where
    P: Plugin + Clone,
{
    let store = StateStore::new();
    store
        .install_plugin(LoopStatePlugin)
        .expect("install LoopStatePlugin");
    store
        .install_plugin(LoopActionHandlersPlugin)
        .expect("install LoopActionHandlersPlugin");
    store
        .install_plugin(plugin.clone())
        .expect("install plugin");

    let mut patch = crate::state::MutationBatch::new();
    patch.update::<RunLifecycle>(RunLifecycleUpdate::Start {
        run_id: "test".into(),
        updated_at: 0,
    });
    store.commit(patch).expect("init lifecycle");

    let runtime = PhaseRuntime::new(store).expect("create runtime");

    let env_plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(LoopActionHandlersPlugin), Arc::new(plugin)];
    let env = ExecutionEnv::from_plugins(&env_plugins, &Default::default())
        .expect("build env with plugin");
    (runtime, env)
}

/// Helper: push a context message action into the pending queue.
fn enqueue_context_message(store: &StateStore, id: u64, msg: ContextMessage) {
    let payload = AddContextMessage::encode_payload(&msg).expect("encode payload");
    let mut batch = crate::state::MutationBatch::new();
    batch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(
        ScheduledActionEnvelope {
            id,
            action: ScheduledAction::new(AddContextMessage::PHASE, AddContextMessage::KEY, payload),
        },
    ));
    store.commit(batch).expect("commit enqueue");
}

/// Helper: extract all text from a message's content blocks.
fn text_of(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// -----------------------------------------------------------------------
// apply_context_messages tests (message placement)
// -----------------------------------------------------------------------

#[test]
fn context_message_injected_at_system_target() {
    let mut messages = vec![
        Message::system("base system prompt"),
        Message::user("hello"),
    ];
    let ctx = vec![ContextMessage::system("reminder", "remember the rules")];
    apply_context_messages(&mut messages, ctx, true);

    // System-target message should be inserted after the base system prompt (index 1)
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(text_of(&messages[0]), "base system prompt");
    assert_eq!(messages[1].role, Role::System);
    assert_eq!(text_of(&messages[1]), "remember the rules");
    assert_eq!(messages[2].role, Role::User);
}

#[test]
fn context_message_injected_at_suffix_target() {
    let mut messages = vec![
        Message::system("system"),
        Message::user("hello"),
        Message::system(""), // simulate assistant-like; using system for simplicity
    ];
    let original_len = messages.len();
    let ctx = vec![ContextMessage::suffix_system(
        "suffix.key",
        "final reminder",
    )];
    apply_context_messages(&mut messages, ctx, true);

    // Suffix messages should be appended at the end
    assert_eq!(messages.len(), original_len + 1);
    let last = messages.last().unwrap();
    assert_eq!(last.role, Role::System);
    assert_eq!(text_of(last), "final reminder");
}

#[test]
fn multiple_context_messages_sorted_by_target() {
    let mut messages = vec![Message::system("system prompt"), Message::user("user msg")];

    let ctx = vec![
        ContextMessage::suffix_system("s1", "suffix text"),
        ContextMessage::system("sys1", "after-system text"),
        ContextMessage::conversation("conv1", Role::User, "conversation text"),
    ];
    apply_context_messages(&mut messages, ctx, true);

    // Expected order:
    // [0] system prompt (original)
    // [1] after-system text (System target, after base system prompt)
    // [2] conversation text (Conversation target, after system messages)
    // [3] user msg (original)
    // [4] suffix text (SuffixSystem target, at end)
    assert_eq!(messages.len(), 5);
    assert_eq!(text_of(&messages[0]), "system prompt");
    assert_eq!(text_of(&messages[1]), "after-system text");
    assert_eq!(text_of(&messages[2]), "conversation text");
    assert_eq!(text_of(&messages[3]), "user msg");
    assert_eq!(text_of(&messages[4]), "suffix text");
}

// -----------------------------------------------------------------------
// Handler-based context message tests (throttle logic via run_phase)
// -----------------------------------------------------------------------

#[tokio::test]
async fn throttle_zero_cooldown_always_injects() {
    let (runtime, env) = test_runtime();
    let store = runtime.store();

    for step in 0..5u32 {
        enqueue_context_message(
            store,
            step as u64,
            ContextMessage::system("always", "inject me").with_cooldown(0),
        );
        // Simulate step completion for throttle tracking
        if step > 0 {
            let mut patch = crate::state::MutationBatch::new();
            patch.update::<RunLifecycle>(RunLifecycleUpdate::StepCompleted { updated_at: 0 });
            store.commit(patch).expect("step completed");
        }
        let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
        runtime
            .run_phase_with_context(&env, ctx)
            .await
            .expect("run phase");
        let accepted = take_context_messages(store).expect("take");
        assert_eq!(
            accepted.len(),
            1,
            "cooldown=0 should inject at every step, failed at step {step}"
        );
    }
}

#[tokio::test]
async fn throttle_skips_within_cooldown() {
    let (runtime, env) = test_runtime();
    let store = runtime.store();

    // Step 1 (step_count=0 → current_step=1): first injection, should be accepted
    enqueue_context_message(
        store,
        1,
        ContextMessage::system("throttled", "content").with_cooldown(3),
    );
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
    runtime
        .run_phase_with_context(&env, ctx)
        .await
        .expect("step 1");
    let accepted = take_context_messages(store).expect("take step 1");
    assert_eq!(accepted.len(), 1, "first injection should pass");

    // Steps 2 and 3: within cooldown, should be skipped
    for step in 2..=3u32 {
        let mut patch = crate::state::MutationBatch::new();
        patch.update::<RunLifecycle>(RunLifecycleUpdate::StepCompleted { updated_at: 0 });
        store.commit(patch).expect("step completed");

        enqueue_context_message(
            store,
            10 + step as u64,
            ContextMessage::system("throttled", "content").with_cooldown(3),
        );
        let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
        runtime
            .run_phase_with_context(&env, ctx)
            .await
            .unwrap_or_else(|_| panic!("step {step}"));
        let accepted = take_context_messages(store).unwrap_or_else(|e| panic!("step {step}: {e}"));
        assert_eq!(
            accepted.len(),
            0,
            "should be throttled at step {step} (cooldown=3, last_step=1)"
        );
    }

    // Step 4 (step_count=3 → current_step=4): cooldown expired (4 - 1 >= 3)
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<RunLifecycle>(RunLifecycleUpdate::StepCompleted { updated_at: 0 });
    store.commit(patch).expect("step completed");

    enqueue_context_message(
        store,
        20,
        ContextMessage::system("throttled", "content").with_cooldown(3),
    );
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
    runtime
        .run_phase_with_context(&env, ctx)
        .await
        .expect("step 4");
    let accepted = take_context_messages(store).expect("take step 4");
    assert_eq!(
        accepted.len(),
        1,
        "cooldown expired at step 4, should inject"
    );
}

#[tokio::test]
async fn throttle_bypassed_on_content_change() {
    let (runtime, env) = test_runtime();
    let store = runtime.store();

    // Step 1: initial injection
    enqueue_context_message(
        store,
        1,
        ContextMessage::system("changing", "original content").with_cooldown(10),
    );
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
    runtime
        .run_phase_with_context(&env, ctx)
        .await
        .expect("step 1");
    let accepted = take_context_messages(store).expect("take step 1");
    assert_eq!(accepted.len(), 1);

    // Step 2: same content, within cooldown — should be throttled
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<RunLifecycle>(RunLifecycleUpdate::StepCompleted { updated_at: 0 });
    store.commit(patch).expect("step completed");

    enqueue_context_message(
        store,
        2,
        ContextMessage::system("changing", "original content").with_cooldown(10),
    );
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
    runtime
        .run_phase_with_context(&env, ctx)
        .await
        .expect("step 2");
    let accepted = take_context_messages(store).expect("take step 2");
    assert_eq!(
        accepted.len(),
        0,
        "same content within cooldown should be throttled"
    );

    // Step 3: different content, within cooldown — should bypass
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<RunLifecycle>(RunLifecycleUpdate::StepCompleted { updated_at: 0 });
    store.commit(patch).expect("step completed");

    enqueue_context_message(
        store,
        3,
        ContextMessage::system("changing", "updated content").with_cooldown(10),
    );
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
    runtime
        .run_phase_with_context(&env, ctx)
        .await
        .expect("step 3");
    let accepted = take_context_messages(store).expect("take step 3");
    assert_eq!(
        accepted.len(),
        1,
        "different content should bypass cooldown"
    );
    assert_eq!(text_of_ctx(&accepted[0]), "updated content");
}

/// Verify that tracing instrumentation does not panic when no subscriber is installed.
///
/// Exercises the context transform (which emits `truncation_applied`) and
/// direct tracing macro calls matching those added to the loop runner and engine.
#[test]
fn tracing_does_not_panic_without_subscriber() {
    use crate::context::ContextTransform;
    use awaken_contract::contract::inference::ContextWindowPolicy;
    use awaken_contract::contract::transform::InferenceRequestTransform;

    // Exercise ContextTransform truncation path (emits tracing::debug!)
    let policy = ContextWindowPolicy {
        max_context_tokens: 40,
        max_output_tokens: 0,
        min_recent_messages: 1,
        enable_prompt_cache: false,
        autocompact_threshold: None,
        compaction_mode: Default::default(),
        compaction_raw_suffix_messages: 2,
    };
    let transform = ContextTransform::new(policy);
    let mut msgs = vec![Message::system("sys")];
    for i in 0..10 {
        msgs.push(Message::user(format!("msg {i}")));
        msgs.push(Message::assistant(format!("reply {i}")));
    }
    // This triggers the truncation_applied tracing call — must not panic
    let _output = transform.transform(msgs, &[]);

    // Exercise loop-runner-style tracing macros directly — must not panic
    tracing::info!(step = 1u64, "step_start");
    tracing::info!(
        model = "test-model",
        input_tokens = 100u64,
        output_tokens = 50u64,
        duration_ms = 42u64,
        "inference_complete"
    );
    tracing::info!(
        tool_name = "calculator",
        call_id = "c1",
        outcome = "Succeeded",
        "tool_call_done"
    );
    tracing::warn!(reason = "NaturalEnd", "run_terminated");

    // Exercise engine-style tracing macros — must not panic
    tracing::debug!(phase = "StepStart", hooks = 3usize, "gather_start");
    tracing::debug!(phase = "StepStart", actions = 2usize, "execute_start");
    tracing::warn!(phase = "StepStart", "exclusive_conflict_fallback");

    // Exercise context compaction tracing — must not panic
    tracing::info!(
        pre_tokens = 2000usize,
        post_tokens = 500usize,
        boundary = 10usize,
        "compaction_complete"
    );
    tracing::debug!(dropped = 5usize, kept = 8usize, "truncation_applied");
}

/// Helper: extract text from a ContextMessage's content blocks.
fn text_of_ctx(msg: &ContextMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// -----------------------------------------------------------------------
// Tool filter tests (ToolFilterState + apply_tool_filter_payloads)
// -----------------------------------------------------------------------

/// Helper: create a simple tool descriptor with the given id.
fn tool(id: &str) -> ToolDescriptor {
    ToolDescriptor::new(id, id, format!("{id} tool"))
}

#[test]
fn tool_filter_state_accumulates_excludes() {
    let mut val = ToolFilterStateValue::default();
    ToolFilterState::apply(&mut val, ToolFilterStateAction::Exclude("search".into()));
    ToolFilterState::apply(&mut val, ToolFilterStateAction::Exclude("browser".into()));
    assert_eq!(val.excluded, vec!["search", "browser"]);
}

#[test]
fn tool_filter_state_accumulates_include_only() {
    let mut val = ToolFilterStateValue::default();
    ToolFilterState::apply(
        &mut val,
        ToolFilterStateAction::IncludeOnly(vec!["a".into(), "b".into()]),
    );
    ToolFilterState::apply(
        &mut val,
        ToolFilterStateAction::IncludeOnly(vec!["c".into()]),
    );
    assert_eq!(val.include_only.len(), 2);
}

#[test]
fn tool_filter_state_clear_resets() {
    let mut val = ToolFilterStateValue::default();
    ToolFilterState::apply(&mut val, ToolFilterStateAction::Exclude("x".into()));
    ToolFilterState::apply(
        &mut val,
        ToolFilterStateAction::IncludeOnly(vec!["y".into()]),
    );
    ToolFilterState::apply(&mut val, ToolFilterStateAction::Clear);
    assert!(val.excluded.is_empty());
    assert!(val.include_only.is_empty());
}

#[test]
fn exclude_tool_removes_from_request() {
    let mut tools = vec![tool("search"), tool("calculator"), tool("browser")];
    apply_tool_filter_payloads(&mut tools, vec!["search".into()], vec![]);

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert!(!ids.contains(&"search"), "search should be excluded");
    assert!(ids.contains(&"calculator"));
    assert!(ids.contains(&"browser"));
    assert_eq!(tools.len(), 2);
}

#[test]
fn include_only_tools_filters_to_subset() {
    let mut tools = vec![
        tool("search"),
        tool("calculator"),
        tool("browser"),
        tool("code_exec"),
    ];
    apply_tool_filter_payloads(
        &mut tools,
        vec![],
        vec![vec!["calculator".into(), "browser".into()]],
    );

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"calculator"));
    assert!(ids.contains(&"browser"));
}

#[test]
fn exclude_and_include_only_combined() {
    let mut tools = vec![tool("search"), tool("calculator"), tool("browser")];
    apply_tool_filter_payloads(
        &mut tools,
        vec!["search".into()],
        vec![vec!["search".into(), "calculator".into()]],
    );

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["calculator"]);
}

#[test]
fn multiple_exclude_tool_actions() {
    let mut tools = vec![tool("a"), tool("b"), tool("c"), tool("d")];
    apply_tool_filter_payloads(&mut tools, vec!["a".into(), "c".into()], vec![]);

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "d"]);
}

#[test]
fn no_filter_actions_leaves_tools_unchanged() {
    let mut tools = vec![tool("search"), tool("calculator")];
    apply_tool_filter_payloads(&mut tools, vec![], vec![]);
    assert_eq!(tools.len(), 2);
}

#[test]
fn multiple_include_only_actions_union() {
    let mut tools = vec![tool("a"), tool("b"), tool("c"), tool("d")];
    apply_tool_filter_payloads(&mut tools, vec![], vec![vec!["a".into()], vec!["c".into()]]);

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "c"]);
}

// -----------------------------------------------------------------------
// Permission × deferred-tools interaction tests
// -----------------------------------------------------------------------

#[test]
fn duplicate_exclude_from_permission_and_deferred_is_idempotent() {
    // Both permission plugin and deferred-tools plugin schedule ExcludeTool
    // for the same tool. The combined exclusion set should deduplicate.
    let mut tools = vec![
        tool("allowed"),
        tool("denied_and_deferred"),
        tool("ToolSearch"),
    ];

    // Simulate merged exclusions from both plugins.
    apply_tool_filter_payloads(
        &mut tools,
        vec!["denied_and_deferred".into(), "denied_and_deferred".into()],
        vec![],
    );

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["allowed", "ToolSearch"]);
}

#[test]
fn permission_exclude_wins_over_deferred_promote() {
    // A tool was deferred, then promoted via ToolSearch (so deferred-tools
    // no longer schedules ExcludeTool for it). But permission still denies
    // it unconditionally, so the permission hook still schedules ExcludeTool.
    let mut tools = vec![
        tool("safe_tool"),
        tool("promoted_but_denied"),
        tool("ToolSearch"),
    ];

    // Only permission hook excludes (deferred-tools promoted it, so no exclusion from there).
    apply_tool_filter_payloads(&mut tools, vec!["promoted_but_denied".into()], vec![]);

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["safe_tool", "ToolSearch"]);
}

#[test]
fn deferred_excludes_and_permission_excludes_are_additive() {
    // Permission denies "rm", deferred-tools defers "calculator" and "browser".
    // Both exclusion sets should be applied.
    let mut tools = vec![
        tool("rm"),
        tool("calculator"),
        tool("browser"),
        tool("search"),
        tool("ToolSearch"),
    ];

    // Merged exclusions from both plugins.
    apply_tool_filter_payloads(
        &mut tools,
        vec!["rm".into(), "calculator".into(), "browser".into()],
        vec![],
    );

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["search", "ToolSearch"]);
}

#[test]
fn exclude_nonexistent_tool_is_harmless() {
    // Permission denies a tool that isn't in the tool list.
    let mut tools = vec![tool("a"), tool("b")];
    apply_tool_filter_payloads(
        &mut tools,
        vec!["nonexistent".into(), "also_missing".into()],
        vec![],
    );

    let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
}

// -----------------------------------------------------------------------
// Inference override extraction test
// -----------------------------------------------------------------------

#[test]
fn inference_override_state_merges_correctly() {
    let ovr1 = awaken_contract::contract::inference::InferenceOverride {
        model: Some("gpt-4".into()),
        temperature: Some(0.7),
        ..Default::default()
    };
    let ovr2 = awaken_contract::contract::inference::InferenceOverride {
        temperature: Some(0.9),
        max_tokens: Some(1000),
        ..Default::default()
    };

    let mut val = InferenceOverrideStateValue::default();
    InferenceOverrideState::apply(&mut val, InferenceOverrideStateAction::Merge(ovr1));
    InferenceOverrideState::apply(&mut val, InferenceOverrideStateAction::Merge(ovr2));

    let ovr = val.overrides.expect("should have overrides");
    assert_eq!(ovr.model.as_deref(), Some("gpt-4"));
    assert_eq!(ovr.temperature, Some(0.9)); // last-wins
    assert_eq!(ovr.max_tokens, Some(1000));
}

#[test]
fn inference_override_state_clear_resets() {
    let mut val = InferenceOverrideStateValue::default();
    InferenceOverrideState::apply(
        &mut val,
        InferenceOverrideStateAction::Merge(
            awaken_contract::contract::inference::InferenceOverride {
                model: Some("gpt-4".into()),
                ..Default::default()
            },
        ),
    );
    InferenceOverrideState::apply(&mut val, InferenceOverrideStateAction::Clear);
    assert!(val.overrides.is_none());
}

#[test]
fn inference_override_merge_helper_works() {
    let ovr1 = awaken_contract::contract::inference::InferenceOverride {
        model: Some("gpt-4".into()),
        temperature: Some(0.7),
        ..Default::default()
    };
    let ovr2 = awaken_contract::contract::inference::InferenceOverride {
        temperature: Some(0.9),
        max_tokens: Some(1000),
        ..Default::default()
    };

    let mut overrides = None;
    merge_override_payloads(&mut overrides, vec![ovr1, ovr2]);
    let ovr = overrides.expect("should have overrides");
    assert_eq!(ovr.model.as_deref(), Some("gpt-4"));
    assert_eq!(ovr.temperature, Some(0.9)); // last-wins
    assert_eq!(ovr.max_tokens, Some(1000));
}

// -----------------------------------------------------------------------
// Tool intercept resolution tests
// -----------------------------------------------------------------------

#[test]
fn intercept_block_wins_over_suspend() {
    use awaken_contract::contract::suspension::{
        PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
    };
    use awaken_contract::contract::tool_intercept::ToolInterceptPayload;

    let payloads = vec![
        ToolInterceptPayload::Suspend(SuspendTicket {
            suspension: Suspension::default(),
            pending: PendingToolCall::default(),
            resume_mode: ToolCallResumeMode::default(),
        }),
        ToolInterceptPayload::Block {
            reason: "blocked".into(),
        },
    ];
    let winner = resolve_intercept_payloads(payloads);
    assert!(matches!(winner, Some(ToolInterceptPayload::Block { .. })));
}

#[test]
fn intercept_same_priority_keeps_first() {
    use awaken_contract::contract::tool::ToolResult;
    use awaken_contract::contract::tool_intercept::ToolInterceptPayload;

    let payloads = vec![
        ToolInterceptPayload::SetResult(ToolResult::success("first", serde_json::json!({}))),
        ToolInterceptPayload::SetResult(ToolResult::success("second", serde_json::json!({}))),
    ];
    let winner = resolve_intercept_payloads(payloads);
    match winner {
        Some(ToolInterceptPayload::SetResult(r)) => assert_eq!(r.tool_name, "first"),
        other => panic!("expected SetResult, got {other:?}"),
    }
}

#[test]
fn intercept_empty_returns_none() {
    let winner = resolve_intercept_payloads(vec![]);
    assert!(winner.is_none());
}

#[derive(Default)]
struct DummyLlm;

#[async_trait::async_trait]
impl awaken_contract::contract::executor::LlmExecutor for DummyLlm {
    async fn execute(
        &self,
        _request: awaken_contract::contract::executor::InferenceRequest,
    ) -> Result<
        awaken_contract::contract::inference::StreamResult,
        awaken_contract::contract::executor::InferenceExecutionError,
    > {
        panic!("dummy llm should not execute in tool-only tests");
    }

    fn name(&self) -> &str {
        "dummy"
    }
}

#[derive(Default)]
struct RecordingExecutor {
    batches: std::sync::Mutex<Vec<Vec<String>>>,
}

impl RecordingExecutor {
    fn take(&self) -> Vec<Vec<String>> {
        std::mem::take(&mut *self.batches.lock().expect("lock poisoned"))
    }
}

#[async_trait::async_trait]
impl crate::execution::ToolExecutor for RecordingExecutor {
    async fn execute(
        &self,
        _tools: &std::collections::HashMap<
            String,
            std::sync::Arc<dyn awaken_contract::contract::tool::Tool>,
        >,
        calls: &[awaken_contract::contract::message::ToolCall],
        _base_ctx: &awaken_contract::contract::tool::ToolCallContext,
    ) -> Result<Vec<crate::execution::ToolExecutionResult>, crate::execution::ToolExecutorError>
    {
        self.batches.lock().expect("lock poisoned").push(
            calls
                .iter()
                .map(|call| call.id.clone())
                .collect::<Vec<String>>(),
        );

        Ok(calls
            .iter()
            .map(|call| crate::execution::ToolExecutionResult {
                call: call.clone(),
                result: awaken_contract::contract::tool::ToolResult::success(
                    &call.name,
                    serde_json::json!({"id": call.id}),
                ),
                outcome: awaken_contract::contract::suspension::ToolCallOutcome::Succeeded,
                command: crate::state::StateCommand::new(),
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

#[tokio::test]
async fn resumed_calls_do_not_serialize_neighboring_fresh_batches() {
    let (runtime, _env) = test_runtime();
    let store = runtime.store();
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<ToolCallStates>(crate::agent::state::ToolCallStatesUpdate::Put(
        crate::agent::state::ToolCallState::new(
            "c3",
            "gamma",
            serde_json::json!({"resumed": true}),
            awaken_contract::contract::suspension::ToolCallStatus::Resuming,
            1,
        )
        .with_resume_mode(awaken_contract::contract::suspension::ToolCallResumeMode::ReplayToolCall)
        .with_suspension(
            Some("perm_c3".into()),
            Some("tool:PermissionConfirm".into()),
        )
        .with_resume_input(Some(
            awaken_contract::contract::suspension::ToolCallResume {
                decision_id: "decision-1".into(),
                action: awaken_contract::contract::suspension::ResumeDecisionAction::Resume,
                result: serde_json::json!({"approved": true}),
                reason: None,
                updated_at: 2,
            },
        )),
    ));
    store.commit(patch).expect("seed resume state");

    let executor = std::sync::Arc::new(RecordingExecutor::default());
    let mut agent = crate::registry::ResolvedAgent::new(
        "agent",
        "model",
        "system",
        std::sync::Arc::new(DummyLlm),
    )
    .with_tool_executor(executor.clone());

    let sink: std::sync::Arc<dyn awaken_contract::contract::event_sink::EventSink> =
        std::sync::Arc::new(awaken_contract::contract::event_sink::NullEventSink);
    let mut messages = vec![std::sync::Arc::new(Message::user("go"))];
    let run_identity = awaken_contract::contract::identity::RunIdentity::default();
    let run_overrides = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut truncation_state = crate::context::TruncationState::new();
    let mut ctx = super::step::StepContext {
        agent: &mut agent,
        messages: &mut messages,
        runtime: &runtime,
        sink,
        checkpoint_store: None,
        run_identity: &run_identity,
        cancellation_token: None,
        run_overrides: &run_overrides,
        total_input_tokens: &mut total_input_tokens,
        total_output_tokens: &mut total_output_tokens,
        truncation_state: &mut truncation_state,
        run_created_at: 0,
    };
    let calls = vec![
        awaken_contract::contract::message::ToolCall::new("c1", "alpha", serde_json::json!({})),
        awaken_contract::contract::message::ToolCall::new("c2", "beta", serde_json::json!({})),
        awaken_contract::contract::message::ToolCall::new("c3", "gamma", serde_json::json!({})),
        awaken_contract::contract::message::ToolCall::new("c4", "delta", serde_json::json!({})),
        awaken_contract::contract::message::ToolCall::new("c5", "epsilon", serde_json::json!({})),
    ];

    let (_blocked, suspended) = super::step::execute_tools_with_interception(&mut ctx, &calls)
        .await
        .expect("tool execution should succeed");
    assert!(!suspended, "recording executor never suspends");
    assert_eq!(
        executor.take(),
        vec![
            vec![String::from("c1"), String::from("c2")],
            vec![String::from("c3")],
            vec![String::from("c4"), String::from("c5")],
        ]
    );

    let states = store.read::<ToolCallStates>().expect("tool call states");
    let resumed = states.calls.get("c3").expect("resumed call state");
    assert_eq!(
        resumed.status,
        awaken_contract::contract::suspension::ToolCallStatus::Succeeded
    );
    assert!(
        resumed.resume_input.is_none(),
        "terminal tool state should clear consumed resume input"
    );
    assert!(
        resumed.suspension_id.is_none() && resumed.suspension_reason.is_none(),
        "terminal tool state should not retain an active suspension context"
    );
}

struct UnlockState;

impl StateKey for UnlockState {
    const KEY: &'static str = "test.loop_runner.unlock_state";
    type Value = bool;
    type Update = bool;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

struct BeforeToolCountState;

impl StateKey for BeforeToolCountState {
    const KEY: &'static str = "test.loop_runner.before_tool_count";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct UnlockTool;

#[async_trait::async_trait]
impl Tool for UnlockTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("unlock", "unlock", "unlock guarded tools")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let mut cmd = StateCommand::new();
        cmd.update::<UnlockState>(true);
        Ok(ToolOutput::with_command(
            ToolResult::success("unlock", json!({"unlocked": true})),
            cmd,
        ))
    }
}

struct GuardedTool;

#[async_trait::async_trait]
impl Tool for GuardedTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("guarded", "guarded", "guarded tool")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("guarded", json!({"ok": true})).into())
    }
}

struct SuspendingTool;

#[async_trait::async_trait]
impl Tool for SuspendingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("suspending", "suspending", "always suspends")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let ticket = SuspendTicket::new(
            Suspension {
                id: "suspend-1".into(),
                action: "tool:PermissionConfirm".into(),
                message: "Approve?".into(),
                parameters: json!({"tool": "suspending"}),
                response_schema: None,
            },
            PendingToolCall::new("c1", "suspending", json!({})),
            ToolCallResumeMode::ReplayToolCall,
        );
        Ok(ToolResult::suspended_with("suspending", "needs approval", ticket).into())
    }
}

#[derive(Clone)]
struct CountingGuardHook;

#[async_trait::async_trait]
impl PhaseHook for CountingGuardHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let Some(tool_name) = ctx.tool_name.as_deref() else {
            return Ok(StateCommand::new());
        };
        if tool_name != "guarded" {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.update::<BeforeToolCountState>(1);
        if !ctx.state::<UnlockState>().copied().unwrap_or(false) {
            cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Block {
                reason: "guarded tool requires unlock".into(),
            })?;
        }
        Ok(cmd)
    }
}

#[derive(Clone)]
struct AlwaysBlockGuardedHook;

#[async_trait::async_trait]
impl PhaseHook for AlwaysBlockGuardedHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let Some(tool_name) = ctx.tool_name.as_deref() else {
            return Ok(StateCommand::new());
        };
        if tool_name != "guarded" {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Block {
            reason: "guarded tool blocked".into(),
        })?;
        Ok(cmd)
    }
}

#[derive(Clone)]
struct GuardPlugin<H> {
    name: &'static str,
    hook: H,
}

impl<H> Plugin for GuardPlugin<H>
where
    H: PhaseHook + Clone,
{
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.name }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<UnlockState>(StateKeyOptions::default())?;
        registrar.register_key::<BeforeToolCountState>(StateKeyOptions::default())?;
        registrar.register_phase_hook(self.name, Phase::BeforeToolExecute, self.hook.clone())?;
        Ok(())
    }
}

fn make_step_context<'a>(
    runtime: &'a PhaseRuntime,
    agent: &'a mut ResolvedAgent,
    messages: &'a mut Vec<Arc<Message>>,
    sink: Arc<dyn awaken_contract::contract::event_sink::EventSink>,
    run_identity: &'a awaken_contract::contract::identity::RunIdentity,
    run_overrides: &'a Option<awaken_contract::contract::inference::InferenceOverride>,
    total_input_tokens: &'a mut u64,
    total_output_tokens: &'a mut u64,
    truncation_state: &'a mut crate::context::TruncationState,
) -> super::step::StepContext<'a> {
    super::step::StepContext {
        agent,
        messages,
        runtime,
        sink,
        checkpoint_store: None,
        run_identity,
        cancellation_token: None,
        run_overrides,
        total_input_tokens,
        total_output_tokens,
        truncation_state,
        run_created_at: 0,
    }
}

#[tokio::test]
async fn recheck_after_flushing_allowed_calls_does_not_double_apply_before_tool_side_effects() {
    let plugin = GuardPlugin {
        name: "counting-guard",
        hook: CountingGuardHook,
    };
    let (runtime, env) = test_runtime_with_plugin(plugin);
    let store = runtime.store();

    let mut agent = ResolvedAgent::new("agent", "model", "system", Arc::new(DummyLlm))
        .with_tool(Arc::new(UnlockTool))
        .with_tool(Arc::new(GuardedTool));
    agent.env = env;

    let sink: Arc<dyn awaken_contract::contract::event_sink::EventSink> =
        Arc::new(awaken_contract::contract::event_sink::NullEventSink);
    let mut messages = vec![Arc::new(Message::user("go"))];
    let run_identity = awaken_contract::contract::identity::RunIdentity::default();
    let run_overrides = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut truncation_state = crate::context::TruncationState::new();
    let mut ctx = make_step_context(
        &runtime,
        &mut agent,
        &mut messages,
        sink,
        &run_identity,
        &run_overrides,
        &mut total_input_tokens,
        &mut total_output_tokens,
        &mut truncation_state,
    );
    let calls = vec![
        awaken_contract::contract::message::ToolCall::new("u1", "unlock", json!({})),
        awaken_contract::contract::message::ToolCall::new("g1", "guarded", json!({})),
    ];

    let (blocked, suspended) = super::step::execute_tools_with_interception(&mut ctx, &calls)
        .await
        .expect("tool execution should succeed");

    assert!(blocked.is_none(), "guarded call should be re-allowed");
    assert!(!suspended, "no tool should suspend");
    assert_eq!(
        store.read::<BeforeToolCountState>().unwrap_or_default(),
        1,
        "BeforeToolExecute side effects should only commit once for the guarded call",
    );
    assert_eq!(
        store.read::<UnlockState>(),
        Some(true),
        "unlock tool should commit state before guarded tool is rechecked",
    );
}

#[tokio::test]
async fn suspended_flush_backfills_interrupted_results_for_unprocessed_calls() {
    let plugin = GuardPlugin {
        name: "always-block-guarded",
        hook: AlwaysBlockGuardedHook,
    };
    let (runtime, env) = test_runtime_with_plugin(plugin);

    let mut agent = ResolvedAgent::new("agent", "model", "system", Arc::new(DummyLlm))
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(GuardedTool))
        .with_tool_executor(Arc::new(crate::execution::SequentialToolExecutor));
    agent.env = env;

    let sink = Arc::new(VecEventSink::new());
    let mut messages = vec![
        Arc::new(Message::user("go")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![
                awaken_contract::contract::message::ToolCall::new("c1", "suspending", json!({})),
                awaken_contract::contract::message::ToolCall::new("c2", "guarded", json!({})),
                awaken_contract::contract::message::ToolCall::new("c3", "tail", json!({})),
            ],
        )),
    ];
    let run_identity = awaken_contract::contract::identity::RunIdentity::default();
    let run_overrides = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut truncation_state = crate::context::TruncationState::new();
    let mut ctx = make_step_context(
        &runtime,
        &mut agent,
        &mut messages,
        sink.clone(),
        &run_identity,
        &run_overrides,
        &mut total_input_tokens,
        &mut total_output_tokens,
        &mut truncation_state,
    );
    let calls = vec![
        awaken_contract::contract::message::ToolCall::new("c1", "suspending", json!({})),
        awaken_contract::contract::message::ToolCall::new("c2", "guarded", json!({})),
        awaken_contract::contract::message::ToolCall::new("c3", "tail", json!({})),
    ];

    let (blocked, suspended) = super::step::execute_tools_with_interception(&mut ctx, &calls)
        .await
        .expect("tool execution should succeed");

    assert!(
        blocked.is_none(),
        "suspending prefix should not be reported as blocked"
    );
    assert!(suspended, "the run should suspend on the first tool");

    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| {
            (
                message.tool_call_id.clone().expect("tool message call id"),
                message.text(),
            )
        })
        .collect();
    assert_eq!(
        tool_messages.len(),
        3,
        "all tool calls need a terminal placeholder"
    );
    assert!(
        tool_messages[1].1.contains("interrupted"),
        "current guarded call should be backfilled with an interrupted result"
    );
    assert!(
        tool_messages[2].1.contains("interrupted"),
        "later calls should also be backfilled with interrupted results"
    );

    let done_events: Vec<_> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id, outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        done_events,
        vec![
            ("c1".into(), ToolCallOutcome::Suspended),
            ("c2".into(), ToolCallOutcome::Failed),
            ("c3".into(), ToolCallOutcome::Failed),
        ],
        "unprocessed calls should no longer be silently dropped after a suspended flush",
    );
}

#[tokio::test]
async fn cancelled_resume_is_emitted_once_even_when_other_calls_replay() {
    let (runtime, _env) = test_runtime();
    let store = runtime.store();
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<ToolCallStates>(crate::agent::state::ToolCallStatesUpdate::Put(
        crate::agent::state::ToolCallState::new(
            "cancel_a",
            "alpha",
            serde_json::json!({"cancelled": true}),
            awaken_contract::contract::suspension::ToolCallStatus::Cancelled,
            1,
        )
        .with_resume_mode(awaken_contract::contract::suspension::ToolCallResumeMode::ReplayToolCall)
        .with_suspension(
            Some("perm_cancel_a".into()),
            Some("tool:PermissionConfirm".into()),
        )
        .with_resume_input(Some(
            awaken_contract::contract::suspension::ToolCallResume {
                decision_id: "decision-cancel".into(),
                action: awaken_contract::contract::suspension::ResumeDecisionAction::Cancel,
                result: serde_json::json!({"kind": "permission_decision", "approved": false}),
                reason: None,
                updated_at: 2,
            },
        )),
    ));
    patch.update::<ToolCallStates>(crate::agent::state::ToolCallStatesUpdate::Put(
        crate::agent::state::ToolCallState::new(
            "resume_b",
            "beta",
            serde_json::json!({"resumed": true}),
            awaken_contract::contract::suspension::ToolCallStatus::Resuming,
            1,
        )
        .with_resume_mode(awaken_contract::contract::suspension::ToolCallResumeMode::ReplayToolCall)
        .with_suspension(
            Some("perm_resume_b".into()),
            Some("tool:PermissionConfirm".into()),
        )
        .with_resume_input(Some(
            awaken_contract::contract::suspension::ToolCallResume {
                decision_id: "decision-resume".into(),
                action: awaken_contract::contract::suspension::ResumeDecisionAction::Resume,
                result: serde_json::json!({"kind": "permission_decision", "approved": true}),
                reason: None,
                updated_at: 2,
            },
        )),
    ));
    store
        .commit(patch)
        .expect("seed cancelled and resumed states");

    let executor = std::sync::Arc::new(RecordingExecutor::default());
    let agent = crate::registry::ResolvedAgent::new(
        "agent",
        "model",
        "system",
        std::sync::Arc::new(DummyLlm),
    )
    .with_tool_executor(executor);
    let run_identity = awaken_contract::contract::identity::RunIdentity::default();
    let sink = std::sync::Arc::new(VecEventSink::new());
    let mut messages = vec![std::sync::Arc::new(Message::user("go"))];

    super::resume::detect_and_replay_resume(
        &agent,
        &runtime,
        &run_identity,
        &mut messages,
        sink.clone(),
    )
    .await
    .expect("first replay should succeed");

    super::resume::detect_and_replay_resume(
        &agent,
        &runtime,
        &run_identity,
        &mut messages,
        sink.clone(),
    )
    .await
    .expect("second replay should not re-emit cancelled resume");

    let cancelled_events: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::ToolCallResumed { target_id, .. } if target_id == "cancel_a"
            )
        })
        .collect();
    assert_eq!(
        cancelled_events.len(),
        1,
        "cancelled resume should only emit one ToolCallResumed event"
    );

    let cancelled_messages: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("cancel_a")
        })
        .collect();
    assert_eq!(
        cancelled_messages.len(),
        1,
        "cancelled resume should only append one tool message"
    );

    let states = store.read::<ToolCallStates>().expect("tool call states");
    let cancelled = states.calls.get("cancel_a").expect("cancelled call state");
    assert_eq!(
        cancelled.status,
        awaken_contract::contract::suspension::ToolCallStatus::Cancelled
    );
    assert!(
        cancelled.resume_input.is_none(),
        "cancelled terminal state should clear consumed resume input"
    );
    assert!(
        cancelled.suspension_id.is_none() && cancelled.suspension_reason.is_none(),
        "cancelled terminal state should not retain an active suspension context"
    );
}
