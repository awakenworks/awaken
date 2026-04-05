//! ACP protocol types aligned with the ACP specification.
//!
//! Reference: <https://agentclientprotocol.com/protocol/schema>

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// ACP protocol version (uint16).
pub const PROTOCOL_VERSION: u16 = 1;

// ── Tool call types ─────────────────────────────────────────────────

/// ACP tool call status (spec: pending → in_progress → completed | failed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Tool call kind — describes the nature of the tool operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

// ── Stop reasons ────────────────────────────────────────────────────

/// ACP stop reason for session completion (spec-defined values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ── Permission types ────────────────────────────────────────────────

/// Permission option kind (spec: allow_once, allow_always, reject_once, reject_always).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// A structured permission option with id, name, and kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// Response to a `session/request_permission` RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResponse {
    pub outcome: PermissionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

/// Permission outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected,
    Cancelled,
}

// ── Content blocks ──────────────────────────────────────────────────

/// ACP content block (baseline: text and resource_link).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

// ── Initialization types ────────────────────────────────────────────

/// Client capabilities sent during `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
}

/// Client info sent during `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Agent capabilities returned by `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<McpCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(default)]
    pub http: bool,
    #[serde(default)]
    pub sse: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(default)]
    pub list: bool,
}

/// Agent info returned by `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Auth method advertised during initialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ── Session types ───────────────────────────────────────────────────

/// Parameters for `session/new`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<Value>,
}

/// Result of `session/new`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResult {
    pub session_id: String,
}

/// Parameters for `session/prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// File location for follow-along.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLocation {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// File diff for edit review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tool_call_kind_serde_roundtrip() {
        for kind in [
            ToolCallKind::Read,
            ToolCallKind::Edit,
            ToolCallKind::Delete,
            ToolCallKind::Move,
            ToolCallKind::Search,
            ToolCallKind::Execute,
            ToolCallKind::Think,
            ToolCallKind::Fetch,
            ToolCallKind::Other,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: ToolCallKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

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
    fn permission_option_serde_roundtrip() {
        let opt = PermissionOption {
            option_id: "opt_allow_once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        };
        let json = serde_json::to_string(&opt).unwrap();
        let parsed: PermissionOption = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, opt);
    }

    #[test]
    fn permission_option_kind_serde_roundtrip() {
        for kind in [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: PermissionOptionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn content_block_text_serde() {
        let block = ContentBlock::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn content_block_resource_link_serde() {
        let block = ContentBlock::ResourceLink {
            uri: "file:///foo.rs".into(),
            name: "foo.rs".into(),
            description: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "resource_link");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn agent_capabilities_default() {
        let caps = AgentCapabilities::default();
        assert!(!caps.load_session);
        assert!(caps.prompt_capabilities.is_none());
    }

    #[test]
    fn protocol_version_is_u16() {
        assert!(PROTOCOL_VERSION <= u16::MAX);
    }

    #[test]
    fn permission_response_selected() {
        let resp = PermissionResponse {
            outcome: PermissionOutcome::Selected,
            option_id: Some("opt_allow_once".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["outcome"], "selected");
        assert_eq!(json["optionId"], "opt_allow_once");
    }

    #[test]
    fn permission_response_cancelled() {
        let resp = PermissionResponse {
            outcome: PermissionOutcome::Cancelled,
            option_id: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["outcome"], "cancelled");
    }

    #[test]
    fn session_new_params_serde() {
        let params = SessionNewParams {
            cwd: "/home/user/project".into(),
            mcp_servers: vec![],
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["cwd"], "/home/user/project");
        assert!(json.get("mcpServers").is_none()); // empty vec skipped
    }

    #[test]
    fn session_prompt_params_serde() {
        let params = SessionPromptParams {
            session_id: "sess_abc123".into(),
            prompt: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["sessionId"], "sess_abc123");
        assert_eq!(json["prompt"][0]["type"], "text");
    }

    #[test]
    fn file_location_serde() {
        let loc = FileLocation {
            path: "/src/main.rs".into(),
            line: Some(42),
        };
        let json = serde_json::to_value(&loc).unwrap();
        assert_eq!(json["path"], "/src/main.rs");
        assert_eq!(json["line"], 42);
    }

    #[test]
    fn file_diff_serde() {
        let diff = FileDiff {
            path: "/src/main.rs".into(),
            old_text: Some("let x = 1;".into()),
            new_text: "let x = 2;".into(),
        };
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["path"], "/src/main.rs");
        assert_eq!(json["oldText"], "let x = 1;");
        assert_eq!(json["newText"], "let x = 2;");
    }
}
