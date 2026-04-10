//! Tool interception payloads resolved by `ToolGate` hooks.

use serde::{Deserialize, Serialize};

use crate::contract::suspension::SuspendTicket;
use crate::contract::tool::ToolResult;

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
}
