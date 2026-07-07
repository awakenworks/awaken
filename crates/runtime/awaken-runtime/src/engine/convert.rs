//! Pure request/message conversion helpers for the execution loop.
//!
//! Extracted from `engine` (which crossed the 2000-line file limit): these are
//! leaf functions over contract types only — they build the model `ChatRequest`,
//! apply the context policy, and mint the transcript messages a turn commits. No
//! `Runtime` or engine state; `drive`/`run_agent_loop`/resume are the consumers.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, ToolCall, ToolSchema};
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
) -> ChatRequest {
    // The agent's instructions lead the request as a system message, ahead of the
    // transcript. Empty instructions contribute no system message.
    let mut messages = Vec::with_capacity(transcript.len() + prelude.len() + 1);
    if !spec.instructions.is_empty() {
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: vec![ContentBlock::text(spec.instructions.clone())],
        });
    }
    // Request-only context (recalled memories, retrieved docs): after the
    // instructions, before the conversation. Never committed — a view concern.
    messages.extend(prelude.iter().map(to_chat_message));
    messages.extend(transcript.iter().map(to_chat_message));
    let messages = apply_context_policy(&spec.context_policy, messages);
    let tools = spec
        .tool_descriptors
        .iter()
        .chain(dynamic.iter())
        .map(to_tool_schema)
        .collect();
    ChatRequest {
        model_binding: spec.model_binding.clone(),
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
    let keep_last = match policy {
        ContextPolicy::KeepAll => return messages,
        ContextPolicy::KeepLast { keep_last } => *keep_last,
    };
    let conversational = messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .count();
    if conversational <= keep_last {
        return messages;
    }
    let mut to_drop = conversational - keep_last;
    messages
        .into_iter()
        .filter(|m| {
            if m.role == ChatRole::System {
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

pub(crate) fn to_chat_message(message: &Message) -> ChatMessage {
    ChatMessage {
        role: to_chat_role(&message.role),
        content: message.content.clone(),
    }
}

pub(crate) fn to_chat_role(role: &Role) -> ChatRole {
    match role {
        Role::System => ChatRole::System,
        Role::User => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
        Role::Tool => ChatRole::Tool,
    }
}

/// Project a pinned descriptor into the model-visible schema. The real
/// description and JSON Schema travel to the model, so it can call tools with
/// arguments; the descriptor's `content_hash` stays internal (G3/G8).
pub(crate) fn to_tool_schema(descriptor: &ToolDescriptor) -> ToolSchema {
    ToolSchema {
        id: descriptor.id.clone(),
        description: descriptor.description.clone(),
        parameters: descriptor.parameters.clone(),
    }
}

/// What a truncated turn is told so it resumes rather than restarts (mirrors
/// the goal runtime's continuation prompt verbatim).
const CONTINUATION_PROMPT: &str = "Your response was cut off because it exceeded the output \
     token limit. Please break your work into smaller pieces. Continue from where you left off.";

/// The committed partial text of a `MaxTokens`-truncated turn. Its id carries
/// the continuation round so it never collides with the step's final
/// assistant message.
pub(crate) fn truncated_assistant_message(
    run_id: &RunId,
    step: usize,
    nth: usize,
    blocks: Vec<ContentBlock>,
) -> Message {
    Message {
        id: MessageId(format!("{}-assistant-{step}-truncated-{nth}", run_id.0)),
        role: Role::Assistant,
        content: blocks,
    }
}

/// The user message that asks a truncated turn to continue where it left off.
pub(crate) fn continuation_message(run_id: &RunId, step: usize, nth: usize) -> Message {
    Message {
        id: MessageId(format!("{}-continuation-{step}-{nth}", run_id.0)),
        role: Role::User,
        content: vec![ContentBlock::text(CONTINUATION_PROMPT)],
    }
}

/// The committed assistant turn: its content blocks verbatim (text and tool-use
/// interleaved), so the transcript explains both what was said and what was
/// called.
pub(crate) fn assistant_message(run_id: &RunId, step: usize, blocks: Vec<ContentBlock>) -> Message {
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: blocks,
    }
}

pub(crate) fn tool_result_message(call: &ToolCall, output: &ToolOutput) -> Message {
    tool_result_message_from(&call.call_id, &output.content)
}

/// A tool-role message carrying a structured `ToolResult` block addressed to the
/// originating call, so the model sees a real tool result rather than loose text.
pub(crate) fn tool_result_message_from(call_id: &str, text: &str) -> Message {
    Message {
        id: MessageId(format!("tool-{call_id}")),
        role: Role::Tool,
        content: vec![ContentBlock::tool_result(
            call_id.to_string(),
            vec![ContentBlock::text(text.to_string())],
        )],
    }
}
