//! Worker-owned binding from a segregated Hand channel to the neutral tool port.

use std::sync::Arc;

use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::ToolExecutor;

struct RelayHandExecutorFactory;

impl awaken_runtime_host::HandExecutorFactory for RelayHandExecutorFactory {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
        recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        let executor = awaken_tool_relay::RemoteToolExecutor::new(channel)
            .with_operation_scope(operation_scope);
        Arc::new(
            if recovery == awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest {
                executor.with_durable_request_recovery()
            } else {
                executor
            },
        )
    }
}

/// Build the canonical execution-plane Hand adapter.
#[must_use]
pub fn relay_hand_executor_factory() -> Arc<dyn awaken_runtime_host::HandExecutorFactory> {
    Arc::new(RelayHandExecutorFactory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

    struct Echo;

    #[async_trait]
    impl RawTool for Echo {
        fn id(&self) -> &str {
            "echo"
        }

        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id, "relay-bound"))
        }
    }

    /// Cause/effect rule: a Hand channel plus operation scope produces exactly
    /// one relay-backed ToolExecutor; an invocation crosses that channel and
    /// returns the Hand result without a Coordinator-owned alternate adapter.
    #[tokio::test]
    async fn factory_binds_the_canonical_relay() {
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::in_memory([Arc::new(Echo) as Arc<dyn RawTool>]),
        ));
        let executor = relay_hand_executor_factory().bind(
            Box::new(brain),
            "session-a",
            awaken_runtime_contract::tool::ToolRecoveryCapability::NonRecoverable,
        );
        let output = executor
            .invoke(&ToolCall {
                call_id: "call-a".into(),
                tool_id: "echo".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(output.text(), "relay-bound");
    }
}
