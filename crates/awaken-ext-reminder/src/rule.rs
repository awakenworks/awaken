//! Reminder rule definition.

use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::tool_pattern::ToolCallPattern;

use crate::output_matcher::OutputMatcher;

/// A reminder rule: when a tool call matches, inject a context message.
#[derive(Debug, Clone)]
pub struct ReminderRule {
    /// Human-readable name for this rule.
    pub name: String,
    /// Tool call pattern (matches tool name + input args).
    pub pattern: ToolCallPattern,
    /// Output matcher (matches tool result — JSON or plain text).
    pub output: OutputMatcher,
    /// The context message to inject when matched.
    pub message: ContextMessage,
}
