//! ACP protocol types — re-exports from `agent-client-protocol-schema`
//! with project-specific helpers.
//!
//! The canonical type definitions live in the official ACP schema crate.
//! This module re-exports the subset used by our encoder and stdio server.

// ── Re-exports from the official ACP schema crate ───────────────────

// Core enums
pub use agent_client_protocol_schema::ContentBlock;
pub use agent_client_protocol_schema::PermissionOptionKind;
pub use agent_client_protocol_schema::RequestPermissionOutcome;
pub use agent_client_protocol_schema::SessionUpdate;
pub use agent_client_protocol_schema::StopReason;
pub use agent_client_protocol_schema::ToolCallStatus;
pub use agent_client_protocol_schema::ToolKind;

// Initialization
pub use agent_client_protocol_schema::AgentCapabilities;
pub use agent_client_protocol_schema::Implementation;
pub use agent_client_protocol_schema::InitializeRequest;
pub use agent_client_protocol_schema::InitializeResponse;
pub use agent_client_protocol_schema::ProtocolVersion;

// Session lifecycle
pub use agent_client_protocol_schema::NewSessionRequest;
pub use agent_client_protocol_schema::NewSessionResponse;
pub use agent_client_protocol_schema::PromptRequest;
pub use agent_client_protocol_schema::PromptResponse;
pub use agent_client_protocol_schema::SessionNotification;

// Tool calls
pub use agent_client_protocol_schema::ToolCall;
pub use agent_client_protocol_schema::ToolCallLocation;
pub use agent_client_protocol_schema::ToolCallUpdate;
pub use agent_client_protocol_schema::ToolCallUpdateFields;

// Permission flow
pub use agent_client_protocol_schema::PermissionOption;
pub use agent_client_protocol_schema::RequestPermissionRequest;
pub use agent_client_protocol_schema::RequestPermissionResponse;
pub use agent_client_protocol_schema::SelectedPermissionOutcome;

// Content
pub use agent_client_protocol_schema::ContentChunk;
pub use agent_client_protocol_schema::TextContent;

/// Infer a tool call kind from the tool name using common heuristics.
pub fn infer_tool_kind(name: &str) -> ToolKind {
    let lower = name.to_ascii_lowercase();
    if lower.contains("read") || lower.contains("cat") || lower.contains("view") {
        ToolKind::Read
    } else if lower.contains("edit") || lower.contains("write") || lower.contains("patch") {
        ToolKind::Edit
    } else if lower.contains("delete") || lower.contains("remove") || lower.contains("rm") {
        ToolKind::Delete
    } else if lower.contains("move") || lower.contains("rename") || lower.contains("mv") {
        ToolKind::Move
    } else if lower.contains("search") || lower.contains("grep") || lower.contains("find") {
        ToolKind::Search
    } else if lower.contains("bash")
        || lower.contains("exec")
        || lower.contains("run")
        || lower.contains("shell")
    {
        ToolKind::Execute
    } else if lower.contains("think") || lower.contains("reason") || lower.contains("plan") {
        ToolKind::Think
    } else if lower.contains("fetch") || lower.contains("http") || lower.contains("curl") {
        ToolKind::Fetch
    } else {
        ToolKind::Other
    }
}

/// Build the default set of permission options with stable IDs.
pub fn default_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            "opt_allow_once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            "opt_allow_always",
            "Allow always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            "opt_reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            "opt_reject_always",
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn stop_reason_serde_roundtrip() {
        for reason in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
            StopReason::Cancelled,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[test]
    fn tool_call_status_serde_roundtrip() {
        for status in [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: ToolCallStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn tool_kind_serde_roundtrip() {
        for kind in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Execute,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::Other,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: ToolKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn permission_option_serde_roundtrip() {
        let opt = PermissionOption::new(
            "opt_allow_once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        );
        let json = serde_json::to_string(&opt).unwrap();
        let parsed: PermissionOption = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, opt);
    }

    #[test]
    fn content_block_text_serde() {
        let block = ContentBlock::from("hello");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
    }

    #[test]
    fn default_permission_options_count() {
        let opts = default_permission_options();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(opts[3].kind, PermissionOptionKind::RejectAlways);
    }

    #[test]
    fn infer_tool_kind_heuristics() {
        assert_eq!(infer_tool_kind("read_file"), ToolKind::Read);
        assert_eq!(infer_tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(infer_tool_kind("bash"), ToolKind::Execute);
        assert_eq!(infer_tool_kind("search"), ToolKind::Search);
        assert_eq!(infer_tool_kind("grep"), ToolKind::Search);
        assert_eq!(infer_tool_kind("http_fetch"), ToolKind::Fetch);
        assert_eq!(infer_tool_kind("think"), ToolKind::Think);
        assert_eq!(infer_tool_kind("unknown_tool"), ToolKind::Other);
    }

    #[test]
    fn initialize_response_builder() {
        let resp = InitializeResponse::new(ProtocolVersion::V1);
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("protocolVersion").is_some());
        assert!(json.get("agentCapabilities").is_some());
    }

    #[test]
    fn prompt_response_builder() {
        let resp = PromptResponse::new(StopReason::EndTurn);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["stopReason"], "end_turn");
    }

    #[test]
    fn tool_call_builder() {
        let tc = ToolCall::new("tc_1", "search")
            .kind(ToolKind::Search)
            .status(ToolCallStatus::Completed) // use non-default to verify serialization
            .raw_input(serde_json::json!({"q": "rust"}));
        let json = serde_json::to_value(&tc).unwrap();
        assert_eq!(json["toolCallId"], "tc_1");
        assert_eq!(json["title"], "search");
        assert_eq!(json["kind"], "search");
        assert_eq!(json["status"], "completed");
    }
}
