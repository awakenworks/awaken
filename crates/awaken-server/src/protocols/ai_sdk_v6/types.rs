//! AI SDK v6 UI Message Stream protocol types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stream event types compatible with AI SDK v6.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UIStreamEvent {
    /// Message lifecycle start.
    #[serde(rename = "start")]
    MessageStart {
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(rename = "messageMetadata", skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },

    /// Text block start.
    TextStart {
        id: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Incremental text content.
    TextDelta {
        id: String,
        delta: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Text block end.
    TextEnd {
        id: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Reasoning block start.
    ReasoningStart {
        id: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Incremental reasoning content.
    ReasoningDelta {
        id: String,
        delta: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Reasoning block end.
    ReasoningEnd {
        id: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Tool input streaming start.
    ToolInputStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },

    /// Incremental tool input.
    ToolInputDelta {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "inputTextDelta")]
        input_text_delta: String,
    },

    /// Tool input complete.
    ToolInputAvailable {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },

    /// Tool output available.
    ToolOutputAvailable {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        output: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// When `true`, indicates this is a preliminary (streaming) output
        /// that will be superseded by a final output with `preliminary: false`.
        #[serde(skip_serializing_if = "Option::is_none")]
        preliminary: Option<bool>,
        /// When `true`, the tool was executed by the provider (server-side).
        /// AI SDK uses this to skip auto-resubmission for server-executed tools.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
    },

    /// Tool output error.
    ToolOutputError {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "errorText")]
        error_text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
        /// When `true`, the tool was executed by the provider (server-side).
        /// Prevents AI SDK auto-resubmission for server-side tool errors.
        #[serde(rename = "providerExecuted", skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
    },

    /// Tool output denied.
    ToolOutputDenied {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },

    /// Tool approval request.
    ToolApprovalRequest {
        #[serde(rename = "approvalId")]
        approval_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },

    /// Step start.
    StartStep,

    /// Step end.
    FinishStep,

    /// Message completion.
    Finish {
        #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(rename = "messageMetadata", skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },

    /// Stream error.
    Error {
        #[serde(rename = "errorText")]
        error_text: String,
    },

    /// Source URL reference (RAG).
    SourceUrl {
        #[serde(rename = "sourceId")]
        source_id: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Source document reference (RAG).
    SourceDocument {
        #[serde(rename = "sourceId")]
        source_id: String,
        #[serde(rename = "mediaType")]
        media_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// File content.
    File {
        url: String,
        #[serde(rename = "mediaType")]
        media_type: String,
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },

    /// Message-level metadata.
    MessageMetadata {
        #[serde(rename = "messageMetadata")]
        message_metadata: Value,
    },

    /// Tool input validation error.
    ToolInputError {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: Value,
        #[serde(rename = "errorText")]
        error_text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },

    /// Stream abort.
    Abort {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Custom data event.
    #[serde(untagged)]
    Data {
        #[serde(rename = "type")]
        data_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        data: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        transient: Option<bool>,
    },
}

impl UIStreamEvent {
    pub fn message_start(message_id: impl Into<String>) -> Self {
        Self::MessageStart {
            message_id: Some(message_id.into()),
            message_metadata: None,
        }
    }

    pub fn text_start(id: impl Into<String>) -> Self {
        Self::TextStart {
            id: id.into(),
            provider_metadata: None,
        }
    }

    pub fn text_delta(id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::TextDelta {
            id: id.into(),
            delta: delta.into(),
            provider_metadata: None,
        }
    }

    pub fn text_end(id: impl Into<String>) -> Self {
        Self::TextEnd {
            id: id.into(),
            provider_metadata: None,
        }
    }

    pub fn reasoning_start(id: impl Into<String>) -> Self {
        Self::ReasoningStart {
            id: id.into(),
            provider_metadata: None,
        }
    }

    pub fn reasoning_delta(id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::ReasoningDelta {
            id: id.into(),
            delta: delta.into(),
            provider_metadata: None,
        }
    }

    pub fn reasoning_end(id: impl Into<String>) -> Self {
        Self::ReasoningEnd {
            id: id.into(),
            provider_metadata: None,
        }
    }

    pub fn tool_input_start(tool_call_id: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self::ToolInputStart {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            dynamic: None,
        }
    }

    pub fn tool_input_delta(tool_call_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::ToolInputDelta {
            tool_call_id: tool_call_id.into(),
            input_text_delta: delta.into(),
        }
    }

    pub fn tool_input_available(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: Value,
    ) -> Self {
        Self::ToolInputAvailable {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            input,
            dynamic: None,
        }
    }

    /// Server-executed tool output (providerExecuted = true).
    pub fn tool_output_available(tool_call_id: impl Into<String>, output: Value) -> Self {
        Self::ToolOutputAvailable {
            tool_call_id: tool_call_id.into(),
            output,
            dynamic: None,
            preliminary: None,
            provider_executed: Some(true),
        }
    }

    pub fn tool_output_preliminary(tool_call_id: impl Into<String>, output: Value) -> Self {
        Self::ToolOutputAvailable {
            tool_call_id: tool_call_id.into(),
            output,
            dynamic: None,
            preliminary: Some(true),
            provider_executed: Some(true),
        }
    }

    /// Client-side (resumed) tool output — no providerExecuted flag.
    pub fn tool_output_resumed(tool_call_id: impl Into<String>, output: Value) -> Self {
        Self::ToolOutputAvailable {
            tool_call_id: tool_call_id.into(),
            output,
            dynamic: None,
            preliminary: None,
            provider_executed: None,
        }
    }

    pub fn tool_output_error(
        tool_call_id: impl Into<String>,
        error_text: impl Into<String>,
    ) -> Self {
        Self::ToolOutputError {
            tool_call_id: tool_call_id.into(),
            error_text: error_text.into(),
            dynamic: None,
            provider_executed: Some(true),
        }
    }

    pub fn tool_output_denied(tool_call_id: impl Into<String>) -> Self {
        Self::ToolOutputDenied {
            tool_call_id: tool_call_id.into(),
        }
    }

    pub fn tool_approval_request(
        approval_id: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self::ToolApprovalRequest {
            approval_id: approval_id.into(),
            tool_call_id: tool_call_id.into(),
        }
    }

    pub fn start_step() -> Self {
        Self::StartStep
    }

    pub fn finish_step() -> Self {
        Self::FinishStep
    }

    pub fn finish() -> Self {
        Self::Finish {
            finish_reason: None,
            message_metadata: None,
        }
    }

    pub fn finish_with_reason(reason: impl Into<String>) -> Self {
        Self::Finish {
            finish_reason: Some(reason.into()),
            message_metadata: None,
        }
    }

    pub fn error(error_text: impl Into<String>) -> Self {
        Self::Error {
            error_text: error_text.into(),
        }
    }

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

    pub fn source_document(
        source_id: impl Into<String>,
        media_type: impl Into<String>,
        title: Option<String>,
        filename: Option<String>,
    ) -> Self {
        Self::SourceDocument {
            source_id: source_id.into(),
            media_type: media_type.into(),
            title,
            filename,
            provider_metadata: None,
        }
    }

    pub fn file(url: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self::File {
            url: url.into(),
            media_type: media_type.into(),
            provider_metadata: None,
        }
    }

    pub fn message_metadata(metadata: Value) -> Self {
        Self::MessageMetadata {
            message_metadata: metadata,
        }
    }

    pub fn abort(reason: impl Into<String>) -> Self {
        Self::Abort {
            reason: Some(reason.into()),
        }
    }

    pub fn data(name: impl Into<String>, data: Value) -> Self {
        Self::data_with_options(name, data, None, None)
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_start_serde_roundtrip() {
        let event = UIStreamEvent::message_start("msg-1");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"start\""));
        assert!(json.contains("\"messageId\":\"msg-1\""));
    }

    #[test]
    fn text_delta_serde_roundtrip() {
        let event = UIStreamEvent::text_delta("txt_0", "hello");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text-delta\""));
        assert!(json.contains("\"delta\":\"hello\""));
    }

    #[test]
    fn tool_input_start_serde() {
        let event = UIStreamEvent::tool_input_start("c1", "search");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool-input-start\""));
        assert!(json.contains("\"toolCallId\":\"c1\""));
    }

    #[test]
    fn finish_omits_none_fields() {
        let event = UIStreamEvent::finish();
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("finishReason"));
    }

    #[test]
    fn data_event_prepends_data_prefix() {
        let event = UIStreamEvent::data("activity-snapshot", json!({"key": "val"}));
        match &event {
            UIStreamEvent::Data { data_type, .. } => {
                assert_eq!(data_type, "data-activity-snapshot");
            }
            _ => panic!("expected Data variant"),
        }
    }

    #[test]
    fn data_event_preserves_existing_prefix() {
        let event = UIStreamEvent::data("data-custom", json!(null));
        match &event {
            UIStreamEvent::Data { data_type, .. } => {
                assert_eq!(data_type, "data-custom");
            }
            _ => panic!("expected Data variant"),
        }
    }

    #[test]
    fn error_event_serde() {
        let event = UIStreamEvent::error("something failed");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"errorText\":\"something failed\""));
    }

    #[test]
    fn step_events_serde() {
        let start = UIStreamEvent::start_step();
        let end = UIStreamEvent::finish_step();
        let start_json = serde_json::to_string(&start).unwrap();
        let end_json = serde_json::to_string(&end).unwrap();
        assert!(start_json.contains("\"type\":\"start-step\""));
        assert!(end_json.contains("\"type\":\"finish-step\""));
    }

    #[test]
    fn reasoning_events_serde() {
        let start = UIStreamEvent::reasoning_start("r1");
        let delta = UIStreamEvent::reasoning_delta("r1", "thinking...");
        let end = UIStreamEvent::reasoning_end("r1");

        let s_json = serde_json::to_string(&start).unwrap();
        let d_json = serde_json::to_string(&delta).unwrap();
        let e_json = serde_json::to_string(&end).unwrap();

        assert!(s_json.contains("reasoning-start"));
        assert!(d_json.contains("thinking..."));
        assert!(e_json.contains("reasoning-end"));
    }

    #[test]
    fn tool_output_events_serde() {
        let available = UIStreamEvent::tool_output_available("c1", json!(42));
        let error = UIStreamEvent::tool_output_error("c1", "fail");
        let denied = UIStreamEvent::tool_output_denied("c1");

        let a_json = serde_json::to_string(&available).unwrap();
        let e_json = serde_json::to_string(&error).unwrap();
        let d_json = serde_json::to_string(&denied).unwrap();

        assert!(a_json.contains("tool-output-available"));
        assert!(e_json.contains("tool-output-error"));
        assert!(d_json.contains("tool-output-denied"));
    }

    #[test]
    fn source_url_serde() {
        let event =
            UIStreamEvent::source_url("src1", "https://example.com", Some("Example".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"source-url\""));
        assert!(json.contains("\"sourceId\":\"src1\""));
        assert!(json.contains("\"url\":\"https://example.com\""));
        assert!(json.contains("\"title\":\"Example\""));
        assert!(!json.contains("providerMetadata"));
    }

    #[test]
    fn source_document_serde() {
        let event = UIStreamEvent::source_document(
            "doc1",
            "application/pdf",
            None,
            Some("report.pdf".into()),
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"source-document\""));
        assert!(json.contains("\"sourceId\":\"doc1\""));
        assert!(json.contains("\"mediaType\":\"application/pdf\""));
        assert!(json.contains("\"filename\":\"report.pdf\""));
        assert!(!json.contains("title"));
    }

    #[test]
    fn file_event_serde() {
        let event = UIStreamEvent::file("https://example.com/img.png", "image/png");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"file\""));
        assert!(json.contains("\"url\":\"https://example.com/img.png\""));
        assert!(json.contains("\"mediaType\":\"image/png\""));
    }

    #[test]
    fn message_metadata_serde() {
        let event = UIStreamEvent::message_metadata(json!({"key": "val"}));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"message-metadata\""));
        assert!(json.contains("\"messageMetadata\""));
    }

    #[test]
    fn abort_event_serde() {
        let event = UIStreamEvent::abort("timeout");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"abort\""));
        assert!(json.contains("\"reason\":\"timeout\""));
    }

    #[test]
    fn abort_event_omits_none_reason() {
        let event = UIStreamEvent::Abort { reason: None };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"abort\""));
        assert!(!json.contains("reason"));
    }

    #[test]
    fn tool_output_preliminary_serde() {
        let event = UIStreamEvent::tool_output_preliminary("c1", json!("partial data"));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("tool-output-available"));
        assert!(json.contains("\"preliminary\":true"));
        assert!(json.contains("partial data"));

        // Non-preliminary should not contain preliminary field
        let final_event = UIStreamEvent::tool_output_available("c1", json!("final data"));
        let final_json = serde_json::to_string(&final_event).unwrap();
        assert!(!final_json.contains("preliminary"));
    }

    #[test]
    fn tool_input_error_serde() {
        let event = UIStreamEvent::ToolInputError {
            tool_call_id: "c1".into(),
            tool_name: "search".into(),
            input: json!({"q": "test"}),
            error_text: "invalid input".into(),
            dynamic: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool-input-error\""));
        assert!(json.contains("\"toolCallId\":\"c1\""));
        assert!(json.contains("\"errorText\":\"invalid input\""));
    }
}
