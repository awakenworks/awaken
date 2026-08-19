//! Pure request/message conversion helpers for the execution loop.
//!
//! Extracted from `engine` (which crossed the 2000-line file limit): these are
//! leaf functions over contract types only — they build the model `ChatRequest`,
//! apply the context policy, and mint the transcript messages a step commits. No
//! `Runtime` or engine state; `drive`/`run_agent_loop`/resume are the consumers.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ToolCall};
use awaken_runtime_contract::resolved::{ContextPolicy, ResolvedSpec, ToolDescriptor};
use awaken_runtime_contract::tool::ToolOutput;

/// Build a model request from the resolved binding, transcript, and visible
/// tool descriptors. `dynamic` carries any plugin-contributed tools live for
/// this step (e.g. an MCP server's current tool set), merged after the pinned
/// config descriptors so the model sees both.
pub(crate) fn build_chat_request(
    spec: &ResolvedSpec,
    prelude: &[Message],
    transcript: &[Message],
    dynamic: &[ToolDescriptor],
    opened: &std::collections::BTreeSet<String>,
) -> ChatRequest {
    // The agent's instructions lead the request as a system message, ahead of the
    // transcript. Empty instructions contribute no system message.
    let mut messages = Vec::with_capacity(transcript.len() + prelude.len() + 1);
    if !spec.instructions.is_empty() {
        messages.push(ChatMessage {
            role: Role::System,
            content: vec![ContentBlock::text(spec.instructions.clone())],
        });
    }
    // Request-only context (recalled memories, retrieved docs): after the
    // instructions, before the conversation. Never committed — a view concern.
    messages.extend(prelude.iter().map(to_chat_message));
    messages.extend(transcript.iter().map(to_chat_message));
    let messages = apply_context_policy(&spec.context_policy, messages);
    // The model-facing tool face (ADR-0053): the static descriptors and the dynamic
    // (plugin/MCP) descriptors converge here — the ONE place both meet — so the agent's
    // `ToolPresentation` (alias + description override) is applied over the combined set,
    // covering static and MCP tools uniformly. An empty presentation returns the set
    // unchanged, so the tool face stays byte-identical for an agent with no overrides.
    // A plugin-provided DynamicTool owns both the live descriptor and executor.
    // Publication may still carry the same id as a selection/policy placeholder;
    // withhold that static copy at this single convergence seam so providers never
    // receive two names and the live plugin schema remains authoritative. Plugin
    // merge already rejects duplicate dynamic owners before this point.
    let dynamic_ids = dynamic
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let combined: Vec<ToolDescriptor> = spec
        .tool_descriptors
        .iter()
        .filter(|tool| !dynamic_ids.contains(tool.id.as_str()))
        .chain(dynamic.iter())
        .filter(
            |tool| match spec.plugin_config.agent.tool_policy(&tool.id) {
                Some(policy) => policy.enabled,
                // A normalized publication with any toolsets must explicitly bind
                // each MCP server toolset. Session plugins may be shared live wiring,
                // but they cannot broaden a child Agent whose publication omitted
                // that server. Empty toolsets retain legacy exact-id behavior.
                None if !spec.plugin_config.agent.toolsets.is_empty()
                    && tool.id.starts_with("mcp__") =>
                {
                    false
                }
                None => true,
            },
        )
        .cloned()
        .collect();
    // `model_tools` applies the alias/description overrides, withholds deferred tools the
    // model has not yet opened this run, and appends the `tool_open` meta-tool listing
    // whatever stays deferred (absent when nothing is deferred). Its descriptors go to
    // the request as-is — one tool type end to end, no lossy projection into a schema.
    let tools = spec.tool_presentation.model_tools(&combined, opened);
    ChatRequest {
        model_binding: spec.model_binding.binding.clone(),
        inference: spec.plugin_config.inference.clone(),
        messages,
        tools,
    }
}

/// Bound the model-visible message list per `policy`. Operates on the request
/// view only — the committed transcript is untouched (G13). For `KeepLast`, every
/// system message is kept regardless of position — the agent instructions and any
/// injected compaction summary must survive — and only the last `keep_last`
/// non-system (conversational) messages are kept; older ones are dropped.
pub(crate) fn apply_context_policy(
    policy: &ContextPolicy,
    messages: Vec<ChatMessage>,
) -> Vec<ChatMessage> {
    let bounded = match policy {
        ContextPolicy::KeepAll => messages,
        ContextPolicy::KeepLast { keep_last } => {
            let conversational = messages.iter().filter(|m| m.role != Role::System).count();
            if conversational <= *keep_last {
                messages
            } else {
                let mut to_drop = conversational - *keep_last;
                messages
                    .into_iter()
                    .filter(|message| {
                        if message.role == Role::System {
                            return true;
                        }
                        if to_drop > 0 {
                            to_drop -= 1;
                            return false;
                        }
                        true
                    })
                    .collect()
            }
        }
    };
    retain_complete_tool_rounds(bounded)
}

/// Keep the model request structurally valid after truncation. Pairing is local
/// to one assistant occurrence and its immediately-following Tool messages, so a
/// reused provider call id in a later round cannot borrow an earlier result.
/// Incomplete calls/results are removed from the request view only; committed
/// transcript truth remains untouched (G13).
fn retain_complete_tool_rounds(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    use std::collections::BTreeSet;

    let mut output = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == Role::Tool {
            // An orphan result (including one exposed by KeepLast) is never sent
            // to a provider.
            index += 1;
            continue;
        }
        if message.role != Role::Assistant {
            output.push(message.clone());
            index += 1;
            continue;
        }

        let call_ids = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        if call_ids.is_empty() {
            output.push(message.clone());
            index += 1;
            continue;
        }

        let mut end = index + 1;
        let mut result_ids = BTreeSet::new();
        while end < messages.len() && messages[end].role == Role::Tool {
            for block in &messages[end].content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block
                    && call_ids.contains(tool_use_id)
                {
                    result_ids.insert(tool_use_id.clone());
                }
            }
            end += 1;
        }

        let mut assistant = message.clone();
        assistant.content.retain(
            |block| !matches!(block, ContentBlock::ToolUse { id, .. } if !result_ids.contains(id)),
        );
        if !assistant.content.is_empty() {
            output.push(assistant);
        }

        let mut emitted = BTreeSet::new();
        for result_message in &messages[index + 1..end] {
            let mut result_message = result_message.clone();
            result_message.content.retain(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    call_ids.contains(tool_use_id)
                        && result_ids.contains(tool_use_id)
                        && emitted.insert(tool_use_id.clone())
                }
                _ => false,
            });
            if !result_message.content.is_empty() {
                output.push(result_message);
            }
        }
        index = end;
    }
    output
}

pub(crate) fn to_chat_message(message: &Message) -> ChatMessage {
    ChatMessage {
        role: message.role,
        content: message.content.clone(),
    }
}

/// What a truncated Step is told so it resumes rather than restarts.
const CONTINUATION_PROMPT: &str = "Your response was cut off because it exceeded the output \
     token limit. Please break your work into smaller pieces. Continue from where you left off.";

/// The committed partial text of a `MaxTokens`-truncated step. Its id carries
/// the continuation round so it never collides with the step's final
/// assistant message.
pub(crate) fn truncated_assistant_message(
    run_id: &RunId,
    step: usize,
    nth: usize,
    blocks: Vec<ContentBlock>,
) -> Message {
    Message {
        id: MessageId::assistant_truncated(run_id, step, nth),
        role: Role::Assistant,
        content: blocks,
    }
}

/// The user message that asks a truncated step to continue where it left off.
pub(crate) fn continuation_message(run_id: &RunId, step: usize, nth: usize) -> Message {
    Message {
        id: MessageId::continuation(run_id, step, nth),
        role: Role::User,
        content: vec![ContentBlock::text(CONTINUATION_PROMPT)],
    }
}

/// The committed assistant step: its content blocks verbatim (text and tool-use
/// interleaved), so the transcript explains both what was said and what was
/// called.
pub(crate) fn assistant_message(run_id: &RunId, step: usize, blocks: Vec<ContentBlock>) -> Message {
    Message {
        id: MessageId::assistant(run_id, step),
        role: Role::Assistant,
        content: blocks,
    }
}

pub(crate) fn tool_result_message(call: &ToolCall, output: &ToolOutput) -> Message {
    Message {
        id: MessageId::tool_result(&call.call_id),
        role: Role::Tool,
        content: vec![ContentBlock::tool_result_with_error(
            call.call_id.clone(),
            output.content.clone(),
            output.is_error,
        )],
    }
}

/// A tool-role message carrying a structured `ToolResult` block addressed to the
/// originating call, preserving success/error semantics for resumed results.
pub(crate) fn tool_result_message_from(
    call_id: &str,
    content: &[ContentBlock],
    is_error: bool,
) -> Message {
    Message {
        id: MessageId::tool_result(call_id),
        role: Role::Tool,
        content: vec![ContentBlock::tool_result_with_error(
            call_id.to_string(),
            content.to_vec(),
            is_error,
        )],
    }
}
