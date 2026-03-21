use super::AgentLoopError;
use crate::contracts::runtime::phase::StepContext;
use crate::contracts::runtime::state::AnyStateAction;
use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use crate::contracts::runtime::tool_call::tool_call_states_from_state;
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::SuspendedCall;
use crate::contracts::thread::{Message, Role};
use crate::contracts::RunAction;
use crate::contracts::RunContext;
use crate::runtime::control::{ToolCallResume, ToolCallState, ToolCallStatus};
use tirea_state::{Patch, TrackedPatch};

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const PENDING_APPROVAL_PLACEHOLDER_PREFIX: &str = "Tool '";
const PENDING_APPROVAL_PLACEHOLDER_SUFFIX: &str = "' is awaiting approval. Execution paused.";

pub(super) fn pending_approval_placeholder_message(tool_name: &str) -> String {
    format!("{PENDING_APPROVAL_PLACEHOLDER_PREFIX}{tool_name}{PENDING_APPROVAL_PLACEHOLDER_SUFFIX}")
}

fn is_pending_approval_placeholder_content(content: &str) -> bool {
    content
        .strip_prefix(PENDING_APPROVAL_PLACEHOLDER_PREFIX)
        .and_then(|rest| rest.strip_suffix(PENDING_APPROVAL_PLACEHOLDER_SUFFIX))
        .is_some_and(|tool_name| !tool_name.is_empty())
}

fn is_pending_approval_placeholder(msg: &Message) -> bool {
    msg.role == Role::Tool && is_pending_approval_placeholder_content(&msg.content)
}

pub(super) fn build_messages(step: &StepContext<'_>, system_prompt: &str) -> Vec<Message> {
    let mut messages = Vec::new();

    // Emit base system prompt as the first system message. Plugin-injected
    // context (via AddContextMessage) is inserted after this by the loop
    // runner, keeping each segment as a separate system message for
    // independent prompt cache boundaries.
    if !system_prompt.is_empty() {
        messages.push(Message::system(system_prompt.to_string()));
    }

    // Collect all tool_call IDs issued by the assistant so we can filter
    // orphaned tool results (e.g. from intercepted pseudo-tool invocations
    // like PermissionConfirm whose call IDs the LLM never issued).
    let known_tool_call_ids: HashSet<&str> = step
        .messages()
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();

    // When a frontend tool pending placeholder is followed by a real tool result
    // for the same call_id, keep only the real result in inference context.
    // This preserves append-only persisted history while avoiding stale
    // "awaiting approval" text from biasing subsequent model turns.
    let mut pending_placeholder_ids = HashSet::new();
    let mut resolved_result_ids = HashSet::new();
    for msg in step.messages() {
        let Some(tc_id) = msg.tool_call_id.as_deref() else {
            continue;
        };
        if !known_tool_call_ids.contains(tc_id) {
            continue;
        }
        if is_pending_approval_placeholder(msg) {
            pending_placeholder_ids.insert(tc_id.to_string());
        } else if msg.role == Role::Tool {
            resolved_result_ids.insert(tc_id.to_string());
        }
    }
    let superseded_pending_ids: HashSet<String> = pending_placeholder_ids
        .intersection(&resolved_result_ids)
        .cloned()
        .collect();

    for msg in step.messages() {
        if msg.role == Role::Tool {
            if let Some(ref tc_id) = msg.tool_call_id {
                if !known_tool_call_ids.contains(tc_id.as_str()) {
                    continue;
                }
                if superseded_pending_ids.contains(tc_id) && is_pending_approval_placeholder(msg) {
                    continue;
                }
            }
        }
        messages.push((**msg).clone());
    }

    // Patch dangling assistant tool_calls that have no matching tool result.
    // This happens when a run is interrupted after issuing tool_calls but before
    // tool results are saved. Providers like DeepSeek require every tool_call to
    // have a corresponding tool result message.
    patch_dangling_tool_calls(&mut messages);

    messages
}

/// Ensure every assistant tool_call has a matching tool result message.
///
/// Scans the message list for assistant messages with `tool_calls` and checks
/// that each call ID appears in a subsequent tool result. For any missing
/// results, a synthetic "interrupted" tool result is inserted immediately
/// after the last tool result for that assistant turn (or right after the
/// assistant message if no results exist for that turn at all).
fn patch_dangling_tool_calls(messages: &mut Vec<Message>) {
    // Collect all tool result call IDs for quick lookup.
    let result_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Find insertion points (index, synthetic messages to insert).
    // We walk forward: for each assistant message with tool_calls, find the
    // last contiguous tool result message belonging to this turn, then insert
    // synthetic results for any missing call IDs right after it.
    let mut insertions: Vec<(usize, Vec<Message>)> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        if msg.role == Role::Assistant {
            if let Some(ref calls) = msg.tool_calls {
                let call_ids: Vec<&str> = calls.iter().map(|tc| tc.id.as_str()).collect();
                let missing: Vec<&str> = call_ids
                    .iter()
                    .filter(|id| !result_ids.contains(**id))
                    .copied()
                    .collect();

                if !missing.is_empty() {
                    // Find the insertion point: skip past any tool result messages
                    // that immediately follow this assistant message.
                    let mut insert_at = i + 1;
                    while insert_at < messages.len() && messages[insert_at].role == Role::Tool {
                        insert_at += 1;
                    }
                    let synthetic: Vec<Message> = missing
                        .into_iter()
                        .map(|id| {
                            Message::tool(
                                id,
                                "[Tool execution was interrupted before producing a result.]",
                            )
                        })
                        .collect();
                    insertions.push((insert_at, synthetic));
                }
            }
        }
        i += 1;
    }

    // Apply insertions in reverse order to preserve indices.
    for (idx, msgs) in insertions.into_iter().rev() {
        let idx = idx.min(messages.len());
        for (offset, msg) in msgs.into_iter().enumerate() {
            messages.insert(idx + offset, msg);
        }
    }
}

pub(super) type InferenceInputs = (
    Vec<Message>,
    Vec<String>,
    RunAction,
    Vec<std::sync::Arc<dyn tirea_contract::runtime::inference::InferenceRequestTransform>>,
    Option<tirea_contract::runtime::inference::InferenceOverride>,
    Vec<tirea_contract::runtime::inference::ContextMessage>,
);

pub(super) fn inference_inputs_from_step(
    step: &mut StepContext<'_>,
    system_prompt: &str,
) -> InferenceInputs {
    let messages = build_messages(step, system_prompt);
    let tools = &step.inference.tools[..];
    let filtered_tools = tools.iter().map(|td| td.id.clone()).collect::<Vec<_>>();
    let run_action = step.run_action();
    let transforms = std::mem::take(&mut step.inference.request_transforms);
    let inference_override = step.inference.inference_override.take();
    let mut context_messages = std::mem::take(&mut step.inference.context_messages);
    context_messages.extend(
        std::mem::take(&mut step.inference.session_context)
            .into_iter()
            .map(tirea_contract::runtime::inference::ContextMessage::session_text),
    );
    (
        messages,
        filtered_tools,
        run_action,
        transforms,
        inference_override,
        context_messages,
    )
}

pub(super) fn build_request_for_filtered_tools(
    messages: &[Message],
    tools: &HashMap<String, Arc<dyn Tool>>,
    filtered_tools: &[String],
    transforms: &[std::sync::Arc<
        dyn tirea_contract::runtime::inference::InferenceRequestTransform,
    >],
) -> genai::chat::ChatRequest {
    let filtered: HashSet<&str> = filtered_tools.iter().map(String::as_str).collect();
    let filtered_tool_refs: Vec<&dyn Tool> = tools
        .values()
        .filter(|t| filtered.contains(t.descriptor().id.as_str()))
        .map(|t| t.as_ref())
        .collect();

    // Apply registered request transforms (if any).
    let (effective_messages, enable_prompt_cache) = if transforms.is_empty() {
        (messages.to_vec(), false)
    } else {
        let tool_descriptors: Vec<_> = filtered_tool_refs.iter().map(|t| t.descriptor()).collect();
        let output = tirea_contract::runtime::inference::apply_request_transforms(
            messages.to_vec(),
            &tool_descriptors,
            transforms,
        );
        (output.messages, output.enable_prompt_cache)
    };

    let mut request =
        crate::engine::convert::build_request(&effective_messages, &filtered_tool_refs);

    if enable_prompt_cache {
        crate::engine::convert::apply_prompt_cache_hints(&mut request);
    }

    request
}

pub(super) fn apply_context_messages_to_prompt(
    messages: &mut Vec<Message>,
    tracker: &mut ContextThrottleTracker,
    entries: Vec<tirea_contract::runtime::inference::ContextMessage>,
    current_step: usize,
    has_base_system_prompt: bool,
) -> Vec<String> {
    use tirea_contract::runtime::inference::ContextMessageTarget;

    let filtered = tracker.filter(normalize_context_messages(entries), current_step);
    if filtered.is_empty() {
        return Vec::new();
    }

    let mut prefix = Vec::new();
    let mut session = Vec::new();
    let mut suffix = Vec::new();
    let mut consumed_context_keys = Vec::new();
    for entry in filtered {
        if entry.consume_after_emit {
            consumed_context_keys.push(entry.key.clone());
        }
        let msg = Message::system(entry.content);
        match entry.target {
            ContextMessageTarget::System => prefix.push(msg),
            ContextMessageTarget::Session => session.push(msg),
            ContextMessageTarget::SuffixSystem => suffix.push(msg),
        }
    }

    let prefix_insert_pos = usize::from(has_base_system_prompt);
    for (offset, msg) in prefix.into_iter().enumerate() {
        messages.insert(prefix_insert_pos + offset, msg);
    }

    let session_insert_pos = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    for (offset, msg) in session.into_iter().enumerate() {
        messages.insert(session_insert_pos + offset, msg);
    }

    messages.extend(suffix);
    consumed_context_keys
}

fn normalize_context_messages(
    entries: Vec<tirea_contract::runtime::inference::ContextMessage>,
) -> Vec<tirea_contract::runtime::inference::ContextMessage> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(entries.len());
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.key.clone()) {
            deduped.push(entry);
        }
    }
    deduped.reverse();
    deduped
}

pub(super) fn consume_emitted_prompt_segments(
    run_ctx: &mut RunContext,
    context_keys: Vec<String>,
) -> Result<(), AgentLoopError> {
    if context_keys.is_empty() {
        return Ok(());
    }

    let actions = context_keys
        .into_iter()
        .map(tirea_contract::runtime::inference::remove_prompt_segment_context_key_action)
        .collect::<Vec<AnyStateAction>>();
    let serialized_actions = actions
        .iter()
        .map(AnyStateAction::to_serialized_state_action)
        .collect::<Vec<_>>();
    run_ctx.add_serialized_state_actions(serialized_actions);

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let patches = reduce_state_actions(
        actions,
        &base_state,
        "agent_loop:prompt_segments",
        &ScopeContext::run(),
    )
    .map_err(|e| {
        AgentLoopError::StateError(format!(
            "failed to reduce emitted prompt segment actions: {e}"
        ))
    })?;
    run_ctx.add_thread_patches(patches);
    Ok(())
}

#[allow(dead_code)]
pub(super) fn suspended_calls_from_ctx(run_ctx: &RunContext) -> HashMap<String, SuspendedCall> {
    run_ctx.suspended_calls()
}

// =========================================================================
// Context throttle tracker
// =========================================================================

/// Per-run tracker that filters [`ContextMessage`]s based on cooldown.
///
/// An attachment is injected when:
/// 1. It has never been seen before, OR
/// 2. Its `cooldown_turns` have elapsed since last injection, OR
/// 3. Its content has changed (regardless of cooldown)
pub(super) struct ContextThrottleTracker {
    /// key → (last injected step, content hash at that time)
    last_injected: HashMap<String, (usize, u64)>,
}

impl ContextThrottleTracker {
    pub fn new() -> Self {
        Self {
            last_injected: HashMap::new(),
        }
    }

    /// Filter context entries by cooldown, returning those that pass throttle.
    /// Updates internal tracking state for injected entries.
    pub fn filter(
        &mut self,
        entries: Vec<tirea_contract::runtime::inference::ContextMessage>,
        current_step: usize,
    ) -> Vec<tirea_contract::runtime::inference::ContextMessage> {
        let mut result = Vec::new();
        for entry in entries {
            let content_hash = Self::hash_content(&entry.content);
            let should_inject = match self.last_injected.get(&entry.key) {
                None => true,
                Some((last_step, last_hash)) => {
                    // Content changed → inject immediately
                    if *last_hash != content_hash {
                        true
                    } else if entry.cooldown_turns == 0 {
                        // No cooldown → inject every turn
                        true
                    } else {
                        // Cooldown elapsed?
                        current_step.saturating_sub(*last_step) > entry.cooldown_turns as usize
                    }
                }
            };
            if should_inject {
                self.last_injected
                    .insert(entry.key.clone(), (current_step, content_hash));
                result.push(entry);
            }
        }
        result
    }

    fn hash_content(content: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }
}

pub(super) fn tool_call_states_from_ctx(run_ctx: &RunContext) -> HashMap<String, ToolCallState> {
    run_ctx
        .snapshot()
        .map(|s| tool_call_states_from_state(&s))
        .unwrap_or_default()
}

pub(super) struct ToolCallStateSeed<'a> {
    pub(super) call_id: &'a str,
    pub(super) tool_name: &'a str,
    pub(super) arguments: &'a Value,
    pub(super) status: ToolCallStatus,
    pub(super) resume_token: Option<String>,
}

pub(super) struct ToolCallStateTransition {
    pub(super) status: ToolCallStatus,
    pub(super) resume_token: Option<String>,
    pub(super) resume: Option<ToolCallResume>,
    pub(super) updated_at: u64,
}

pub(super) fn transition_tool_call_state(
    current: Option<ToolCallState>,
    seed: ToolCallStateSeed<'_>,
    transition: ToolCallStateTransition,
) -> Option<ToolCallState> {
    let mut tool_state = current.unwrap_or_else(|| ToolCallState {
        call_id: seed.call_id.to_string(),
        tool_name: seed.tool_name.to_string(),
        arguments: seed.arguments.clone(),
        status: seed.status,
        resume_token: seed.resume_token.clone(),
        resume: None,
        scratch: Value::Null,
        updated_at: transition.updated_at,
    });
    if !tool_state.status.can_transition_to(transition.status) {
        return None;
    }

    tool_state.call_id = seed.call_id.to_string();
    tool_state.tool_name = seed.tool_name.to_string();
    tool_state.arguments = seed.arguments.clone();
    tool_state.status = transition.status;
    tool_state.resume_token = transition.resume_token;
    tool_state.resume = transition.resume;
    tool_state.updated_at = transition.updated_at;

    Some(tool_state)
}

pub(super) fn upsert_tool_call_state(
    base_state: &Value,
    call_id: &str,
    tool_state: ToolCallState,
) -> Result<TrackedPatch, AgentLoopError> {
    if call_id.trim().is_empty() {
        return Err(AgentLoopError::StateError(
            "failed to upsert tool call state: call_id must not be empty".to_string(),
        ));
    }

    let mut reduced = reduce_state_actions(
        vec![tool_state.into_state_action()],
        base_state,
        "agent_loop",
        &ScopeContext::run(),
    )
    .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    match reduced.pop() {
        Some(patch) => Ok(patch),
        None => Ok(TrackedPatch::new(Patch::new()).with_source("agent_loop")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::control::{ResumeDecisionAction, ToolCallStatus};
    use serde_json::json;
    use tirea_state::apply_patch;

    fn sample_state(call_id: &str, status: ToolCallStatus) -> ToolCallState {
        ToolCallState {
            call_id: call_id.to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"msg": call_id}),
            status,
            resume_token: None,
            resume: None,
            scratch: Value::Null,
            updated_at: 1,
        }
    }

    #[test]
    fn upsert_tool_call_state_generates_single_call_scoped_patch() {
        let state = json!({
            "__tool_call_scope": {
                "call_a": {
                    "tool_call_state": sample_state("call_a", ToolCallStatus::Suspended)
                },
                "call_b": {
                    "tool_call_state": sample_state("call_b", ToolCallStatus::Suspended)
                }
            }
        });

        let updated = sample_state("call_a", ToolCallStatus::Resuming);
        let patch = upsert_tool_call_state(&state, "call_a", updated).expect("patch should build");
        let ops = patch.patch().ops();
        assert_eq!(ops.len(), 1);
        assert!(
            ops[0]
                .path()
                .to_string()
                .starts_with("$.__tool_call_scope.call_a.tool_call_state"),
            "unexpected patch path: {}",
            ops[0].path()
        );

        let merged = apply_patch(&state, patch.patch()).expect("patch should apply");
        assert_eq!(
            merged["__tool_call_scope"]["call_a"]["tool_call_state"]["status"],
            json!("resuming")
        );
        assert_eq!(
            merged["__tool_call_scope"]["call_b"]["tool_call_state"]["status"],
            json!("suspended")
        );
    }

    #[test]
    fn upsert_tool_call_state_rejects_empty_call_id() {
        let error = upsert_tool_call_state(&json!({}), " ", sample_state("x", ToolCallStatus::New))
            .expect_err("empty call_id must fail");
        let AgentLoopError::StateError(message) = error else {
            panic!("unexpected error type");
        };
        assert!(message.contains("call_id must not be empty"));
    }

    #[test]
    fn transition_tool_call_state_applies_seed_and_runtime_fields() {
        let transitioned = transition_tool_call_state(
            None,
            ToolCallStateSeed {
                call_id: "call_1",
                tool_name: "echo",
                arguments: &json!({"message":"hi"}),
                status: ToolCallStatus::Suspended,
                resume_token: Some("resume_token_1".to_string()),
            },
            ToolCallStateTransition {
                status: ToolCallStatus::Resuming,
                resume_token: Some("resume_token_1".to_string()),
                resume: Some(ToolCallResume {
                    decision_id: "decision_1".to_string(),
                    action: ResumeDecisionAction::Resume,
                    result: json!(true),
                    reason: None,
                    updated_at: 42,
                }),
                updated_at: 42,
            },
        )
        .expect("transition should be allowed");

        assert_eq!(transitioned.call_id, "call_1");
        assert_eq!(transitioned.tool_name, "echo");
        assert_eq!(transitioned.arguments, json!({"message":"hi"}));
        assert_eq!(transitioned.status, ToolCallStatus::Resuming);
        assert_eq!(transitioned.resume_token.as_deref(), Some("resume_token_1"));
        assert_eq!(
            transitioned
                .resume
                .as_ref()
                .map(|resume| &resume.decision_id),
            Some(&"decision_1".to_string())
        );
        assert_eq!(transitioned.updated_at, 42);
    }

    #[test]
    fn transition_tool_call_state_rejects_invalid_lifecycle_transition() {
        let current = ToolCallState {
            call_id: "call_1".to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"message":"done"}),
            status: ToolCallStatus::Succeeded,
            resume_token: None,
            resume: None,
            scratch: Value::Null,
            updated_at: 1,
        };

        let transitioned = transition_tool_call_state(
            Some(current),
            ToolCallStateSeed {
                call_id: "call_1",
                tool_name: "echo",
                arguments: &json!({"message":"done"}),
                status: ToolCallStatus::New,
                resume_token: None,
            },
            ToolCallStateTransition {
                status: ToolCallStatus::Running,
                resume_token: None,
                resume: None,
                updated_at: 2,
            },
        );
        assert!(transitioned.is_none(), "terminal state should not reopen");
    }

    #[test]
    fn patch_dangling_tool_calls_adds_synthetic_results() {
        use crate::contracts::thread::ToolCall;

        let mut messages = vec![
            Message::user("hi"),
            Message::assistant_with_tool_calls(
                "let me check",
                vec![
                    ToolCall::new("c1", "search", json!({"q": "x"})),
                    ToolCall::new("c2", "fetch", json!({"url": "y"})),
                ],
            ),
            // Only c1 has a result; c2 is dangling
            Message::tool("c1", "found it"),
        ];

        patch_dangling_tool_calls(&mut messages);

        // c2 should have a synthetic result inserted after c1's result
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c2"));
        assert!(messages[3].content.contains("interrupted"));
    }

    #[test]
    fn patch_dangling_tool_calls_handles_end_of_history() {
        use crate::contracts::thread::ToolCall;

        let mut messages = vec![
            Message::user("do something"),
            Message::assistant_with_tool_calls(
                "calling tool",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            // No tool results at all — run was interrupted
        ];

        patch_dangling_tool_calls(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn patch_dangling_tool_calls_in_middle_of_history() {
        use crate::contracts::thread::ToolCall;

        let mut messages = vec![
            Message::user("first"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "t", json!({}))],
            ),
            // c1 is dangling, then user starts new conversation
            Message::user("second"),
            Message::assistant("ok"),
        ];

        patch_dangling_tool_calls(&mut messages);

        // Synthetic result for c1 should be inserted at index 2 (before "second")
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[3].role, Role::User);
        assert_eq!(messages[4].role, Role::Assistant);
    }

    #[test]
    fn patch_dangling_tool_calls_no_op_when_complete() {
        use crate::contracts::thread::ToolCall;

        let mut messages = vec![
            Message::user("hi"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "t", json!({}))],
            ),
            Message::tool("c1", "result"),
            Message::assistant("done"),
        ];

        let len_before = messages.len();
        patch_dangling_tool_calls(&mut messages);
        assert_eq!(messages.len(), len_before, "no synthetic results needed");
    }

    #[test]
    fn pending_approval_placeholder_detection_requires_exact_template() {
        let exact = pending_approval_placeholder_message("echo");
        assert!(is_pending_approval_placeholder_content(&exact));
        assert!(!is_pending_approval_placeholder_content(
            "prefix Tool 'echo' is awaiting approval. Execution paused. suffix"
        ));
        assert!(!is_pending_approval_placeholder_content(
            "Tool '' is awaiting approval. Execution paused."
        ));
    }

    // ===================================================================
    // ContextThrottleTracker tests
    // ===================================================================

    fn entry(
        key: &str,
        content: &str,
        cooldown: u32,
    ) -> tirea_contract::runtime::inference::ContextMessage {
        tirea_contract::runtime::inference::ContextMessage {
            key: key.into(),
            content: content.into(),
            cooldown_turns: cooldown,
            target: tirea_contract::runtime::inference::ContextMessageTarget::System,
            consume_after_emit: false,
        }
    }

    #[test]
    fn throttle_first_injection_always_passes() {
        let mut tracker = ContextThrottleTracker::new();
        let result = tracker.filter(vec![entry("a", "hello", 5)], 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello");
    }

    #[test]
    fn throttle_skips_during_cooldown() {
        let mut tracker = ContextThrottleTracker::new();
        // Step 0: inject
        let r = tracker.filter(vec![entry("a", "hello", 3)], 0);
        assert_eq!(r.len(), 1);
        // Steps 1-3: skip (cooldown = 3 → skip turns 1, 2, 3)
        for step in 1..=3 {
            let r = tracker.filter(vec![entry("a", "hello", 3)], step);
            assert!(r.is_empty(), "should be throttled at step {step}");
        }
        // Step 4: inject again (cooldown elapsed)
        let r = tracker.filter(vec![entry("a", "hello", 3)], 4);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn throttle_content_change_bypasses_cooldown() {
        let mut tracker = ContextThrottleTracker::new();
        // Step 0: inject "v1"
        let r = tracker.filter(vec![entry("a", "v1", 10)], 0);
        assert_eq!(r.len(), 1);
        // Step 1: same content → throttled
        let r = tracker.filter(vec![entry("a", "v1", 10)], 1);
        assert!(r.is_empty());
        // Step 2: content changed → inject immediately
        let r = tracker.filter(vec![entry("a", "v2", 10)], 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].content, "v2");
    }

    #[test]
    fn throttle_zero_cooldown_injects_every_turn() {
        let mut tracker = ContextThrottleTracker::new();
        for step in 0..5 {
            let r = tracker.filter(vec![entry("a", "always", 0)], step);
            assert_eq!(r.len(), 1, "should inject at step {step}");
        }
    }

    #[test]
    fn throttle_multiple_keys_independent() {
        let mut tracker = ContextThrottleTracker::new();
        // Step 0: inject both
        let r = tracker.filter(vec![entry("a", "alpha", 2), entry("b", "beta", 4)], 0);
        assert_eq!(r.len(), 2);
        // Step 3: a's cooldown elapsed (2 < 3), b's not (4 >= 3)
        let r = tracker.filter(vec![entry("a", "alpha", 2), entry("b", "beta", 4)], 3);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "a");
    }

    #[test]
    fn apply_context_messages_positions_prefix_session_and_suffix() {
        use tirea_contract::runtime::inference::{ContextMessage, ContextMessageTarget};

        let mut tracker = ContextThrottleTracker::new();
        let mut messages = vec![
            Message::system("base"),
            Message::user("hello"),
            Message::assistant("world"),
        ];

        let _ = apply_context_messages_to_prompt(
            &mut messages,
            &mut tracker,
            vec![
                ContextMessage {
                    key: "prefix".into(),
                    content: "prefix".into(),
                    cooldown_turns: 0,
                    target: ContextMessageTarget::System,
                    consume_after_emit: false,
                },
                ContextMessage {
                    key: "session".into(),
                    content: "session".into(),
                    cooldown_turns: 0,
                    target: ContextMessageTarget::Session,
                    consume_after_emit: false,
                },
                ContextMessage {
                    key: "suffix".into(),
                    content: "suffix".into(),
                    cooldown_turns: 0,
                    target: ContextMessageTarget::SuffixSystem,
                    consume_after_emit: false,
                },
            ],
            0,
            true,
        );

        let contents: Vec<_> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["base", "prefix", "session", "hello", "world", "suffix"]
        );
    }

    #[test]
    fn apply_context_messages_without_base_system_starts_with_prefix() {
        use tirea_contract::runtime::inference::{ContextMessage, ContextMessageTarget};

        let mut tracker = ContextThrottleTracker::new();
        let mut messages = vec![Message::user("hello")];

        let _ = apply_context_messages_to_prompt(
            &mut messages,
            &mut tracker,
            vec![
                ContextMessage {
                    key: "prefix".into(),
                    content: "prefix".into(),
                    cooldown_turns: 0,
                    target: ContextMessageTarget::System,
                    consume_after_emit: false,
                },
                ContextMessage {
                    key: "session".into(),
                    content: "session".into(),
                    cooldown_turns: 0,
                    target: ContextMessageTarget::Session,
                    consume_after_emit: false,
                },
            ],
            0,
            false,
        );

        let contents: Vec<_> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["prefix", "session", "hello"]);
    }

    #[test]
    fn normalize_context_messages_last_key_wins() {
        use tirea_contract::runtime::inference::ContextMessage;

        let entries = normalize_context_messages(vec![
            ContextMessage::system("dup", "first"),
            ContextMessage::session("keep", "session"),
            ContextMessage::suffix_system("dup", "second"),
        ]);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "keep");
        assert_eq!(entries[1].key, "dup");
        assert_eq!(entries[1].content, "second");
    }
}
