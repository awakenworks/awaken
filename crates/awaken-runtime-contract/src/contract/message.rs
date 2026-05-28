//! Core types for agent messages and tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::ContentBlock;

mod delivery;

pub use delivery::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, PendingMessageRecord,
    pending_queue_revision, select_pending_for_freeze, select_pending_for_freeze_for_run,
};

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message visibility — controls whether a message is exposed to external API consumers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Visible to both the user and the LLM.
    #[default]
    All,
    /// Only visible to the LLM, hidden from external API consumers.
    Internal,
}

impl Visibility {
    /// Returns `true` if this is the default visibility (`All`).
    pub fn is_default(&self) -> bool {
        *self == Visibility::All
    }
}

/// Optional metadata associating a message with a run and step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// The run that produced this message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Step (round) index within the run (0-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
}

/// A message persisted as part of a thread's append-only log.
///
/// `Message` remains the protocol payload. `MessageRecord` is the durable
/// thread-owned view that assigns a sequence number and exposes producer
/// relationships without making runs own message bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Stable message identifier.
    pub message_id: String,
    /// Thread that owns the message.
    pub thread_id: String,
    /// 1-based sequence number within the thread log.
    pub seq: u64,
    /// Message payload.
    pub message: Message,
    /// Run that produced this message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_by_run_id: Option<String>,
    /// Step that produced this message, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
    /// Tool call this message responds to, if this is a tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Unix timestamp (seconds) when the message was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
}

impl MessageRecord {
    /// Build a record from a thread-owned message payload.
    pub fn from_message(thread_id: impl Into<String>, seq: u64, message: Message) -> Self {
        let produced_by_run_id = message
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.run_id.clone());
        let step_index = message
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.step_index);
        let tool_call_id = message.tool_call_id.clone();
        Self {
            message_id: message.id.clone().unwrap_or_else(gen_message_id),
            thread_id: thread_id.into(),
            seq,
            message,
            produced_by_run_id,
            step_index,
            tool_call_id,
            created_at: None,
        }
    }
}

/// Build the read-time message view used for committed thread history.
///
/// A cancelled or superseded run can leave an assistant message with tool calls
/// that never received a `tool` result. Committed history is append-oriented, so
/// stores expose those removals as a view filter rather than rewriting message
/// bodies in place.
pub fn strip_unpaired_tool_calls_from_view(messages: &mut Vec<Message>) {
    use std::collections::HashSet;

    // A tool call is answered only by a real (`All`-visibility) tool result.
    // An `Internal` tool result is a retraction marker (ADR-0042 D6): the
    // runtime appends one when a suspended call is superseded/abandoned, so the
    // call and its pending tool result are removed from the view here at read time
    // rather than by rewriting committed history. No tool result is committed
    // `Internal` for any other reason.
    let mut answered: HashSet<String> = HashSet::new();
    let mut retracted: HashSet<String> = HashSet::new();
    for message in messages.iter() {
        if message.role != Role::Tool {
            continue;
        }
        if let Some(call_id) = message.tool_call_id.clone() {
            match message.visibility {
                Visibility::All => {
                    answered.insert(call_id);
                }
                Visibility::Internal => {
                    retracted.insert(call_id);
                }
            }
        }
    }

    for message in messages.iter_mut() {
        if message.role != Role::Assistant {
            continue;
        }
        if let Some(ref mut calls) = message.tool_calls {
            calls.retain(|call| answered.contains(&call.id) && !retracted.contains(&call.id));
            if calls.is_empty() {
                message.tool_calls = None;
            }
        }
    }

    // Keep only real tool results for calls that survived: drop `Internal`
    // retraction markers, retracted pending results, and orphan tool results.
    messages.retain(|message| {
        if message.role != Role::Tool {
            return true;
        }
        match message.tool_call_id.as_ref() {
            Some(call_id) => message.visibility == Visibility::All && !retracted.contains(call_id),
            None => true,
        }
    });
}

/// Return `messages` after applying the committed-history read view filter.
pub fn strip_unpaired_tool_calls_from_owned_view(mut messages: Vec<Message>) -> Vec<Message> {
    strip_unpaired_tool_calls_from_view(&mut messages);
    messages
}

/// Generate a time-ordered UUID v7 message identifier.
pub fn gen_message_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// A message in the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Stable message identifier (UUID v7, auto-generated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: Role,
    /// Multimodal content blocks.
    pub content: Vec<ContentBlock>,
    /// Tool calls made by the assistant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool call ID this message responds to (for tool role).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Message visibility. Defaults to `All` (visible everywhere).
    /// Internal messages (e.g. system reminders) are only sent to the LLM.
    #[serde(default, skip_serializing_if = "Visibility::is_default")]
    pub visibility: Visibility,
    /// Optional run/step association metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMetadata>,
}

impl Message {
    /// Create a system message.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_runtime_contract::contract::message::Message;
    ///
    /// let msg = Message::system("You are helpful");
    /// assert_eq!(msg.text(), "You are helpful");
    /// ```
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an internal system message (visible only to LLM, hidden from API consumers).
    pub fn internal_system(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::Internal,
            metadata: None,
        }
    }

    /// Create an internal user message (visible only to the LLM).
    pub fn internal_user(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::Internal,
            metadata: None,
        }
    }

    /// Create a user message with text.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_runtime_contract::contract::message::Message;
    ///
    /// let msg = Message::user("Hello");
    /// assert_eq!(msg.text(), "Hello");
    /// ```
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a user message with multimodal content blocks.
    pub fn user_with_content(content: Vec<ContentBlock>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content,
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message with tool calls.
    pub fn assistant_with_tool_calls(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a tool response message with text.
    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Tool,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a tool response message with multimodal content.
    pub fn tool_with_content(call_id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Extract concatenated text from content blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_runtime_contract::contract::message::Message;
    ///
    /// let msg = Message::user("Hello world");
    /// assert_eq!(msg.text(), "Hello world");
    /// ```
    pub fn text(&self) -> String {
        super::content::extract_text(&self.content)
    }

    pub fn is_internal_tool_result(&self) -> bool {
        self.role == Role::Tool && self.visibility == Visibility::Internal
    }

    /// Override the auto-generated message ID.
    #[must_use]
    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    /// Attach run/step metadata to this message.
    #[must_use]
    pub fn with_metadata(mut self, metadata: MessageMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Return the run that produced this message, if recorded.
    #[must_use]
    pub fn produced_by_run_id(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.run_id.as_deref())
    }

    /// Mark this message as produced by a run without overwriting existing
    /// producer metadata.
    pub fn mark_produced_by(&mut self, run_id: &str, step_index: Option<u32>) {
        let metadata = self.metadata.get_or_insert_with(MessageMetadata::default);
        if metadata.run_id.is_none() {
            metadata.run_id = Some(run_id.to_string());
        }
        if metadata.step_index.is_none() {
            metadata.step_index = step_index;
        }
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool to call.
    pub name: String,
    /// Arguments for the tool as JSON.
    pub arguments: Value,
}

impl ToolCall {
    /// Create a new tool call.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[cfg(test)]
#[path = "message/tests.rs"]
mod tests;
