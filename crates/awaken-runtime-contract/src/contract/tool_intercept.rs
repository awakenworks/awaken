//! Tool interception payloads resolved by `ToolGate` hooks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::suspension::SuspendTicket;
use crate::contract::tool::ToolResult;

/// Execution mode for a tool policy decision.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Foreground,
    Scheduled,
    Resume,
    InternalWake,
}

/// Protocol or runtime adapter that surfaced the tool call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    #[default]
    Internal,
    Acp,
    AiSdk,
    AgUi,
    A2a,
    Mcp,
}

/// Coarse tool capability class used by policy hooks.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Execute,
    Read,
    Edit,
    Search,
    #[default]
    Other,
}

impl ToolKind {
    /// Infer a coarse policy kind from a tool name using shared framework
    /// heuristics.
    #[must_use]
    pub fn infer_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if contains_any(
            &lower,
            &[
                "edit",
                "write",
                "patch",
                "apply_patch",
                "replace",
                "create",
                "delete",
                "remove",
                "move",
                "rename",
            ],
        ) {
            Self::Edit
        } else if contains_any(&lower, &["search", "grep", "find", "rg"]) {
            Self::Search
        } else if contains_any(
            &lower,
            &[
                "read", "cat", "view", "open", "list", "ls", "glob", "fetch", "http", "curl",
            ],
        ) {
            Self::Read
        } else if contains_any(
            &lower,
            &[
                "bash", "shell", "exec", "execute", "run", "command", "terminal",
            ],
        ) {
            Self::Execute
        } else {
            Self::Other
        }
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

/// Typed context passed to framework-level tool policy hooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolPolicyContext {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    pub run_mode: RunMode,
    pub adapter: AdapterKind,
    pub tool_name: String,
    pub tool_kind: ToolKind,
    pub arguments: Value,
}

/// Typed policy decision. Converted into the existing ToolGate intercept result
/// before the tool executor is invoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolPolicyDecision {
    Allow,
    Deny { reason: String },
    Suspend(SuspendTicket),
    ReplaceResult(ToolResult),
    AuditOnly,
}

impl ToolPolicyDecision {
    #[must_use]
    pub fn into_intercept_payload(self) -> Option<ToolInterceptPayload> {
        match self {
            Self::Allow | Self::AuditOnly => None,
            Self::Deny { reason } => Some(ToolInterceptPayload::Block { reason }),
            Self::Suspend(ticket) => Some(ToolInterceptPayload::Suspend(ticket)),
            Self::ReplaceResult(result) => Some(ToolInterceptPayload::SetResult(result)),
        }
    }
}

/// Tool interception decision returned by `ToolGate` hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolInterceptPayload {
    /// Block tool execution and terminate the run.
    Block { reason: String },
    /// Suspend tool execution pending external decision (permission, frontend, etc.).
    Suspend(SuspendTicket),
    /// Skip execution and use this result directly (frontend tool resume, deny with message).
    SetResult(ToolResult),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::suspension::{
        PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
    };
    use crate::contract::tool::ToolResult;
    use serde_json::json;

    #[test]
    fn tool_intercept_payload_serde_roundtrip_block() {
        let payload = ToolInterceptPayload::Block {
            reason: "dangerous operation".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ToolInterceptPayload = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, ToolInterceptPayload::Block { reason } if reason == "dangerous operation")
        );
    }

    #[test]
    fn tool_intercept_payload_serde_roundtrip_suspend() {
        let ticket = SuspendTicket::new(
            Suspension {
                id: "s1".into(),
                action: "confirm".into(),
                message: "Approve?".into(),
                parameters: json!({"tool": "delete_file"}),
                response_schema: None,
            },
            PendingToolCall::new("c1", "delete_file", json!({"path": "/tmp/x"})),
            ToolCallResumeMode::ReplayToolCall,
        );
        let payload = ToolInterceptPayload::Suspend(ticket.clone());
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ToolInterceptPayload = serde_json::from_str(&json).unwrap();
        match parsed {
            ToolInterceptPayload::Suspend(t) => assert_eq!(t, ticket),
            other => panic!("expected Suspend, got {other:?}"),
        }
    }

    #[test]
    fn tool_intercept_payload_serde_roundtrip_set_result() {
        let result = ToolResult::success("my_tool", json!({"answer": 42}));
        let payload = ToolInterceptPayload::SetResult(result.clone());
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ToolInterceptPayload = serde_json::from_str(&json).unwrap();
        match parsed {
            ToolInterceptPayload::SetResult(r) => {
                assert_eq!(r.tool_name, result.tool_name);
                assert_eq!(r.data, result.data);
                assert_eq!(r.status, result.status);
            }
            other => panic!("expected SetResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_policy_decision_maps_to_tool_gate_payload() {
        let deny = ToolPolicyDecision::Deny {
            reason: "scheduled shell status update is not useful".into(),
        };
        assert!(matches!(
            deny.into_intercept_payload(),
            Some(ToolInterceptPayload::Block { reason }) if reason.contains("scheduled shell")
        ));

        let result = ToolResult::success("shell", json!({"blocked": true}));
        let replace = ToolPolicyDecision::ReplaceResult(result.clone());
        match replace.into_intercept_payload() {
            Some(ToolInterceptPayload::SetResult(mapped)) => {
                assert_eq!(mapped.tool_name, result.tool_name);
                assert_eq!(mapped.data, result.data);
            }
            other => panic!("expected SetResult, got {other:?}"),
        }

        assert!(ToolPolicyDecision::Allow.into_intercept_payload().is_none());
        assert!(
            ToolPolicyDecision::AuditOnly
                .into_intercept_payload()
                .is_none()
        );
    }

    #[test]
    fn tool_kind_inference_uses_shared_policy_heuristics() {
        assert_eq!(ToolKind::infer_name("bash"), ToolKind::Execute);
        assert_eq!(ToolKind::infer_name("shell_exec"), ToolKind::Execute);
        assert_eq!(ToolKind::infer_name("read_file"), ToolKind::Read);
        assert_eq!(ToolKind::infer_name("http_fetch"), ToolKind::Read);
        assert_eq!(ToolKind::infer_name("apply_patch"), ToolKind::Edit);
        assert_eq!(ToolKind::infer_name("delete_file"), ToolKind::Edit);
        assert_eq!(ToolKind::infer_name("grep_search"), ToolKind::Search);
        assert_eq!(ToolKind::infer_name("unknown_tool"), ToolKind::Other);
    }
}
