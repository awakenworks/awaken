//! Outer-layer binding from a segregated hand channel to the neutral tool executor.

use std::sync::Arc;

use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::ToolExecutor;

struct RelayHandExecutorFactory;

impl awaken_runtime_host::HandExecutorFactory for RelayHandExecutorFactory {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
    ) -> Arc<dyn ToolExecutor> {
        Arc::new(
            awaken_tool_relay::RemoteToolExecutor::new(channel)
                .with_operation_scope(operation_scope),
        )
    }
}

/// Build the production hand-channel adapter injected into the protocol-neutral host.
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

    #[tokio::test]
    async fn factory_binds_the_existing_relay_without_reimplementing_its_wire() {
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::new([Arc::new(Echo) as Arc<dyn RawTool>]),
        ));
        let executor = relay_hand_executor_factory().bind(Box::new(brain), "session-a");
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
