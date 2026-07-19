//! Ordinary tool-call vocabulary for invoking an Agent-backed capability.
//!
//! Runtime does not define a Subagent service. An integration may register any
//! [`RawTool`] that accepts [`AgentRunArgs`]; callers invoke it exactly like every
//! other tool. The implementation may use a local/remote [`RunService`], but that
//! placement and lifecycle wiring stays outside the Runtime contract.

use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::Message;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput, invoke_raw_tool};
use serde::{Deserialize, Serialize};

/// Conventional payload understood by an Agent-backed ordinary tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRunArgs {
    pub agent_id: String,
    pub seed: Vec<Message>,
}

/// Invoke an Agent-backed tool through the generic tool capability. Cancellation
/// races the ordinary tool future; dropping that future is the same cooperative
/// cancellation mechanism used by other asynchronous tools.
pub async fn invoke_agent_tool(
    tool: &dyn RawTool,
    call_id: impl Into<String>,
    args: AgentRunArgs,
    cancellation: Option<&CancellationToken>,
) -> Result<ToolOutput, ToolError> {
    let call = ToolCall {
        call_id: call_id.into(),
        tool_id: tool.id().to_string(),
        arguments: serde_json::to_value(args)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?,
    };
    invoke_raw_tool(tool, call, cancellation).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    struct RecordingAgentTool {
        call: Mutex<Option<ToolCall>>,
    }

    #[async_trait]
    impl RawTool for RecordingAgentTool {
        fn id(&self) -> &str {
            "agent_capability"
        }

        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            *self.call.lock().unwrap() = Some(call.clone());
            Ok(ToolOutput::ok(call.call_id, "done"))
        }
    }

    #[tokio::test]
    async fn agent_capability_is_invoked_as_an_ordinary_tool_call() {
        let tool = RecordingAgentTool {
            call: Mutex::new(None),
        };
        let output = invoke_agent_tool(
            &tool,
            "call-1",
            AgentRunArgs {
                agent_id: "researcher".into(),
                seed: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();

        assert_eq!(output.call_id, "call-1");
        let call = tool.call.lock().unwrap().clone().unwrap();
        assert_eq!(call.tool_id, "agent_capability");
        assert_eq!(call.call_id, "call-1");
        assert_eq!(
            serde_json::from_value::<AgentRunArgs>(call.arguments).unwrap(),
            AgentRunArgs {
                agent_id: "researcher".into(),
                seed: Vec::new(),
            }
        );
    }

    struct PendingAgentTool;

    #[async_trait]
    impl RawTool for PendingAgentTool {
        fn id(&self) -> &str {
            "agent_capability"
        }

        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_uses_the_generic_tool_invocation_boundary() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = invoke_agent_tool(
            &PendingAgentTool,
            "call-2",
            AgentRunArgs {
                agent_id: "researcher".into(),
                seed: Vec::new(),
            },
            Some(&cancellation),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "tool execution failed: tool invocation cancelled"
        );
    }
}
