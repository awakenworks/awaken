use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Stream event types compatible with AI SDK v6.
///
/// These events map directly to the AI SDK UI Message Stream protocol.
/// See: <https://ai-sdk.dev/docs/ai-sdk-ui/stream-protocol>
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UIStreamEvent {
    // ========================================================================
    // Message Lifecycle
    // ========================================================================
    /// Indicates the beginning of a new message with metadata.
    ///
    /// AI SDK v6 expects this as `{"type":"start","messageId":"..."}`.
    #[serde(rename = "start")]
    MessageStart {
        /// Unique identifier for this message.
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// Optional message metadata.
        #[serde(rename = "messageMetadata", skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },

    // ========================================================================
    // Text Streaming (start/delta/end pattern)
    // ========================================================================
    /// Indicates the beginning of a text block.
    TextStart {
        /// Unique identifier for this text block.
        id: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Contains incremental text content for the text block.
    TextDelta {
        /// Identifier matching the text-start event.
        id: String,
        /// Incremental text content.
        delta: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Indicates the end of a text block.
    TextEnd {
        /// Identifier matching the text-start event.
        id: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    // ========================================================================
    // Reasoning Streaming
    // ========================================================================
    /// Indicates the beginning of a reasoning block.
    ReasoningStart {
        /// Unique identifier for this reasoning block.
        id: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Contains incremental reasoning content.
    ReasoningDelta {
        /// Identifier matching the reasoning-start event.
        id: String,
        /// Incremental reasoning content.
        delta: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Indicates the end of a reasoning block.
    ReasoningEnd {
        /// Identifier matching the reasoning-start event.
        id: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    // ========================================================================
    // Tool Input Streaming
    // ========================================================================
    /// Indicates the beginning of tool input streaming.
    ToolInputStart {
        /// Unique identifier for this tool call.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Name of the tool being called.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Whether the provider executed this tool directly.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        /// Whether this is a dynamic tool part.
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// Optional UI title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Contains incremental chunks of tool input as it's being generated.
    ToolInputDelta {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Incremental tool input text.
        #[serde(rename = "inputTextDelta")]
        input_text_delta: String,
    },

    /// Indicates that tool input is complete and ready for execution.
    ToolInputAvailable {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Name of the tool being called.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Complete tool input as JSON.
        input: Value,
        /// Whether the provider executed this tool directly.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
        /// Whether this is a dynamic tool part.
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// Optional UI title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Indicates tool input validation failed before execution.
    ///
    /// NOTE: Not currently emitted. Reserved for future client-side tool
    /// input validation. Backend tool input errors surface via `tool-output-error`.
    ToolInputError {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Name of the tool being called.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Tool input payload.
        input: Value,
        /// Whether the provider executed this tool directly.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
        /// Whether this is a dynamic tool part.
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// Error text.
        #[serde(rename = "errorText")]
        error_text: String,
        /// Optional UI title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Requests user approval for a tool call.
    ToolApprovalRequest {
        /// Approval request ID.
        #[serde(rename = "approvalId")]
        approval_id: String,
        /// Tool call ID this approval applies to.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },

    // ========================================================================
    // Tool Output
    // ========================================================================
    /// Contains the result of tool execution.
    ToolOutputAvailable {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Tool execution result as JSON.
        output: Value,
        /// Whether the provider executed this tool directly.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        /// Whether this is a dynamic tool part.
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// Marks provisional tool output.
        #[serde(skip_serializing_if = "Option::is_none")]
        preliminary: Option<bool>,
    },

    /// Indicates the tool output was denied by user approval.
    ToolOutputDenied {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },

    /// Indicates tool output failed with an execution error.
    ToolOutputError {
        /// Identifier matching the tool-input-start event.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Error text.
        #[serde(rename = "errorText")]
        error_text: String,
        /// Whether the provider executed this tool directly.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        /// Whether this is a dynamic tool part.
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },

    // ========================================================================
    // Step Boundaries (for multi-step agents)
    // ========================================================================
    /// Marks the beginning of a step.
    StartStep,

    /// Marks the completion of an LLM API call step.
    FinishStep,

    // ========================================================================
    // Source References (for RAG)
    // ========================================================================
    /// References an external URL.
    SourceUrl {
        /// Unique identifier for this source.
        #[serde(rename = "sourceId")]
        source_id: String,
        /// The URL being referenced.
        url: String,
        /// Optional title for the source.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// References a document or file.
    SourceDocument {
        /// Unique identifier for this source.
        #[serde(rename = "sourceId")]
        source_id: String,
        /// IANA media type of the document.
        #[serde(rename = "mediaType")]
        media_type: String,
        /// Title of the document.
        title: String,
        /// Optional filename.
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Contains a file reference.
    File {
        /// URL to the file.
        url: String,
        /// IANA media type.
        #[serde(rename = "mediaType")]
        media_type: String,
        /// Optional provider metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    // ========================================================================
    // Stream Lifecycle
    // ========================================================================
    /// Indicates message completion.
    Finish {
        /// Optional reason for finishing (stop, length, content-filter, tool-calls, error, other).
        #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        /// Optional message metadata.
        #[serde(rename = "messageMetadata", skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },

    /// Signals stream abortion with a reason.
    Abort {
        /// Optional reason for the abort.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Emits incremental metadata updates for the active message.
    MessageMetadata {
        /// Message metadata payload.
        #[serde(rename = "messageMetadata")]
        message_metadata: Value,
    },

    /// Appends error messages to stream.
    Error {
        /// Error text.
        #[serde(rename = "errorText")]
        error_text: String,
    },

    // ========================================================================
    // Custom Data (extensible)
    // ========================================================================
    /// Custom data event with a type prefix pattern (data-*).
    #[serde(untagged)]
    Data {
        /// Custom type name (must start with "data-").
        #[serde(rename = "type", deserialize_with = "deserialize_data_event_type")]
        data_type: String,
        /// Optional stable data part ID.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Custom data payload.
        data: Value,
        /// Whether the data should be treated as transient.
        #[serde(skip_serializing_if = "Option::is_none")]
        transient: Option<bool>,
    },
}

impl UIStreamEvent {
    // ========================================================================
    // Factory Methods
    // ========================================================================

    /// Create a start event (message start).
    pub fn message_start(message_id: impl Into<String>) -> Self {
        Self::MessageStart {
            message_id: Some(message_id.into()),
            message_metadata: None,
        }
    }

    /// Create a start event without message ID.
    pub fn start() -> Self {
        Self::MessageStart {
            message_id: None,
            message_metadata: None,
        }
    }

    /// Create a text-start event.
    pub fn text_start(id: impl Into<String>) -> Self {
        Self::TextStart {
            id: id.into(),
            provider_metadata: None,
        }
    }

    /// Create a text-delta event.
    pub fn text_delta(id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::TextDelta {
            id: id.into(),
            delta: delta.into(),
            provider_metadata: None,
        }
    }

    /// Create a text-end event.
    pub fn text_end(id: impl Into<String>) -> Self {
        Self::TextEnd {
            id: id.into(),
            provider_metadata: None,
        }
    }

    /// Create a reasoning-start event.
    pub fn reasoning_start(id: impl Into<String>) -> Self {
        Self::ReasoningStart {
            id: id.into(),
            provider_metadata: None,
        }
    }

    /// Create a reasoning-delta event.
    pub fn reasoning_delta(id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::ReasoningDelta {
            id: id.into(),
            delta: delta.into(),
            provider_metadata: None,
        }
    }

    /// Create a reasoning-end event.
    pub fn reasoning_end(id: impl Into<String>) -> Self {
        Self::ReasoningEnd {
            id: id.into(),
            provider_metadata: None,
        }
    }

    /// Create a tool-input-start event.
    pub fn tool_input_start(tool_call_id: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self::ToolInputStart {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            provider_executed: None,
            dynamic: None,
            title: None,
        }
    }

    /// Create a tool-input-delta event.
    pub fn tool_input_delta(tool_call_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::ToolInputDelta {
            tool_call_id: tool_call_id.into(),
            input_text_delta: delta.into(),
        }
    }

    /// Create a tool-input-available event.
    pub fn tool_input_available(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: Value,
    ) -> Self {
        Self::ToolInputAvailable {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            input,
            provider_executed: None,
            provider_metadata: None,
            dynamic: None,
            title: None,
        }
    }

    /// Create a tool-input-error event.
    pub fn tool_input_error(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: Value,
        error_text: impl Into<String>,
    ) -> Self {
        Self::ToolInputError {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            input,
            provider_executed: None,
            provider_metadata: None,
            dynamic: None,
            error_text: error_text.into(),
            title: None,
        }
    }

    /// Create a tool-approval-request event.
    pub fn tool_approval_request(
        approval_id: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self::ToolApprovalRequest {
            approval_id: approval_id.into(),
            tool_call_id: tool_call_id.into(),
        }
    }

    /// Create a tool-output-available event.
    pub fn tool_output_available(tool_call_id: impl Into<String>, output: Value) -> Self {
        Self::ToolOutputAvailable {
            tool_call_id: tool_call_id.into(),
            output,
            provider_executed: None,
            dynamic: None,
            preliminary: None,
        }
    }

    /// Create a tool-output-denied event.
    pub fn tool_output_denied(tool_call_id: impl Into<String>) -> Self {
        Self::ToolOutputDenied {
            tool_call_id: tool_call_id.into(),
        }
    }

    /// Create a tool-output-error event.
    pub fn tool_output_error(
        tool_call_id: impl Into<String>,
        error_text: impl Into<String>,
    ) -> Self {
        Self::ToolOutputError {
            tool_call_id: tool_call_id.into(),
            error_text: error_text.into(),
            provider_executed: None,
            dynamic: None,
        }
    }

    /// Create a start-step event.
    pub fn start_step() -> Self {
        Self::StartStep
    }

    /// Create a finish-step event.
    pub fn finish_step() -> Self {
        Self::FinishStep
    }

    /// Create a source-url event.
    pub fn source_url(
        source_id: impl Into<String>,
        url: impl Into<String>,
        title: Option<String>,
    ) -> Self {
        Self::SourceUrl {
            source_id: source_id.into(),
            url: url.into(),
            title,
            provider_metadata: None,
        }
    }

    /// Create a source-document event.
    pub fn source_document(
        source_id: impl Into<String>,
        media_type: impl Into<String>,
        title: impl Into<String>,
        filename: Option<String>,
    ) -> Self {
        Self::SourceDocument {
            source_id: source_id.into(),
            media_type: media_type.into(),
            title: title.into(),
            filename,
            provider_metadata: None,
        }
    }

    /// Create a file event.
    pub fn file(url: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self::File {
            url: url.into(),
            media_type: media_type.into(),
            provider_metadata: None,
        }
    }

    /// Create a finish event.
    pub fn finish() -> Self {
        Self::Finish {
            finish_reason: None,
            message_metadata: None,
        }
    }

    /// Create a finish event with a reason.
    pub fn finish_with_reason(reason: impl Into<String>) -> Self {
        Self::Finish {
            finish_reason: Some(reason.into()),
            message_metadata: None,
        }
    }

    /// Create an abort event.
    pub fn abort(reason: impl Into<String>) -> Self {
        Self::Abort {
            reason: Some(reason.into()),
        }
    }

    /// Create a message-metadata event.
    pub fn message_metadata(message_metadata: Value) -> Self {
        Self::MessageMetadata { message_metadata }
    }

    /// Create an error event.
    pub fn error(error_text: impl Into<String>) -> Self {
        Self::Error {
            error_text: error_text.into(),
        }
    }

    /// Create a custom data event.
    pub fn data(name: impl Into<String>, data: Value) -> Self {
        Self::data_with_options(name, data, None, None)
    }

    /// Create a custom data event with optional id/transient flags.
    pub fn data_with_options(
        name: impl Into<String>,
        data: Value,
        id: Option<String>,
        transient: Option<bool>,
    ) -> Self {
        let name = name.into();
        let data_type = if name.starts_with("data-") {
            name
        } else {
            format!("data-{name}")
        };
        Self::Data {
            data_type,
            id,
            data,
            transient,
        }
    }
}

fn deserialize_data_event_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.starts_with("data-") {
        Ok(value)
    } else {
        Err(D::Error::custom(format!(
            "invalid data event type '{value}', expected 'data-*'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn data_event_with_options_serializes_id_and_transient() {
        let event = UIStreamEvent::data_with_options(
            "reasoning-encrypted",
            json!({ "encryptedValue": "opaque" }),
            Some("reasoning_msg_1".to_string()),
            Some(true),
        );
        let value = serde_json::to_value(event).expect("serialize data event");

        assert_eq!(value["type"], "data-reasoning-encrypted");
        assert_eq!(value["id"], "reasoning_msg_1");
        assert_eq!(value["transient"], true);
    }

    #[test]
    fn data_event_rejects_non_prefixed_type() {
        let err = serde_json::from_value::<UIStreamEvent>(json!({
            "type": "reasoning-encrypted",
            "data": { "encryptedValue": "opaque" }
        }))
        .expect_err("invalid data type must be rejected");

        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn tool_input_error_roundtrip() {
        let event = UIStreamEvent::tool_input_error(
            "call_1",
            "search",
            json!({ "q": "rust" }),
            "invalid args",
        );
        let raw = serde_json::to_string(&event).expect("serialize tool-input-error");
        let restored: UIStreamEvent =
            serde_json::from_str(&raw).expect("deserialize tool-input-error");

        assert!(matches!(
            restored,
            UIStreamEvent::ToolInputError {
                tool_call_id,
                tool_name,
                error_text,
                ..
            } if tool_call_id == "call_1" && tool_name == "search" && error_text == "invalid args"
        ));
    }

    #[test]
    fn message_metadata_roundtrip() {
        let event = UIStreamEvent::message_metadata(json!({ "step": 2 }));
        let raw = serde_json::to_string(&event).expect("serialize message metadata");
        let restored: UIStreamEvent =
            serde_json::from_str(&raw).expect("deserialize message metadata");

        assert!(matches!(
            restored,
            UIStreamEvent::MessageMetadata { message_metadata } if message_metadata["step"] == 2
        ));
    }

    #[test]
    fn text_delta_roundtrip_preserves_provider_metadata() {
        let event = UIStreamEvent::TextDelta {
            id: "txt_1".to_string(),
            delta: "hello".to_string(),
            provider_metadata: Some(json!({ "model": "x" })),
        };
        let raw = serde_json::to_string(&event).expect("serialize text delta");
        let restored: UIStreamEvent = serde_json::from_str(&raw).expect("deserialize text delta");

        assert!(matches!(
            restored,
            UIStreamEvent::TextDelta {
                id,
                delta,
                provider_metadata: Some(provider_metadata),
            } if id == "txt_1" && delta == "hello" && provider_metadata["model"] == "x"
        ));
    }
}
