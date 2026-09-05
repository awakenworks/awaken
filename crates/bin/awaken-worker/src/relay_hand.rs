//! Worker-owned binding from a segregated Hand channel to the neutral tool port.

use std::sync::Arc;

use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::ToolExecutor;

struct RelayHandExecutorFactory;

impl awaken_session_executor::HandExecutorFactory for RelayHandExecutorFactory {
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
pub fn relay_hand_executor_factory() -> Arc<dyn awaken_session_executor::HandExecutorFactory> {
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

    #[tokio::test]
    async fn cancelled_relay_preserves_operation_uncertainty_and_admits_only_a_new_request_after_rebind()
     {
        use awaken_runtime_contract::tool::{
            ToolOperationContext, ToolRecoveryCapability, with_tool_operation_context,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        // R1 attached/resident factory + cancellation after invocation begins ->
        // same Runtime operation stays indeterminate, despite a changed wire id.
        // R2 a distinct Runtime operation (even reusing the wire id) -> the
        // existing pre-dispatch error reaches the Session lifecycle unchanged.
        // R3 close the old channel and rebind through the same factory -> the new
        // call executes once; the old effect is never replayed. No Host-to-relay
        // dependency or alternate factory is introduced to test this composition.
        struct BlockingEcho {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl RawTool for BlockingEcho {
            fn id(&self) -> &str {
                "echo"
            }
            async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                Ok(ToolOutput::ok(call.call_id, "completed"))
            }
        }
        for recovery in [
            ToolRecoveryCapability::NonRecoverable,
            ToolRecoveryCapability::DurableRequest,
        ] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tool = Arc::new(BlockingEcho {
                entered: entered.clone(),
                release: release.clone(),
                calls: calls.clone(),
            });
            let (brain, hand) = tokio::io::duplex(64 * 1024);
            let peer = tokio::spawn(awaken_tool_relay::serve_hand(
                hand,
                awaken_tool_relay::HandSession::in_memory([tool.clone() as Arc<dyn RawTool>]),
            ));
            let factory = relay_hand_executor_factory();
            let executor = factory.bind(Box::new(brain), "session", recovery);
            let request = ToolCall {
                call_id: "call".into(),
                tool_id: "echo".into(),
                arguments: serde_json::json!({}),
            };
            let first = {
                let executor = executor.clone();
                let request = request.clone();
                tokio::spawn(async move {
                    with_tool_operation_context(
                        ToolOperationContext::for_run("run", "old"),
                        executor.invoke(&request),
                    )
                    .await
                })
            };
            entered.notified().await;
            first.abort();
            let _ = first.await;
            let renamed = ToolCall {
                call_id: "renamed".into(),
                ..request.clone()
            };
            let old = with_tool_operation_context(
                ToolOperationContext::for_run("run", "old"),
                executor.invoke(&renamed),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(old, ToolError::Execution(_)),
                "R1 must not authorize replacement/replay"
            );
            let new = with_tool_operation_context(
                ToolOperationContext::for_run("run", "new"),
                executor.invoke(&request),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(new, ToolError::UnavailableBeforeDispatch(_)),
                "R2 new request never sent"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release.notify_one();
            drop(executor);
            let _ = peer.await;
            let (brain, hand) = tokio::io::duplex(64 * 1024);
            let peer = tokio::spawn(awaken_tool_relay::serve_hand(
                hand,
                awaken_tool_relay::HandSession::in_memory([tool as Arc<dyn RawTool>]),
            ));
            let replacement = factory.bind(Box::new(brain), "session", recovery);
            let result = with_tool_operation_context(
                ToolOperationContext::for_run("run", "new"),
                replacement.invoke(&request),
            )
            .await
            .unwrap();
            assert_eq!(result.text(), "completed", "R3 actual rebound relay");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "R3 two distinct effects, never old replay"
            );
            drop(replacement);
            peer.await.unwrap().unwrap();
        }
    }
}
