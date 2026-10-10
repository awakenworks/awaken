// @formal surface obligation=remote_tool.stable_operation_identity
// @formal surface obligation=remote_tool.invoke_at_most_once
// @formal surface obligation=remote_tool.result_matches_operation
// @formal surface obligation=remote_tool.indeterminate_never_false_success
// @formal reason: Composition of the remote Hand/tool relay protocol.
//! Worker-owned binding from a segregated Hand channel to the neutral tool port.

use std::sync::Arc;

use awaken_run_executor_acp::AgentChannelType;

struct RelayHandExecutorFactory;

struct RelayRequestRecovery {
    executor: Arc<awaken_tool_relay::RemoteToolExecutor<Box<dyn AgentChannelType>>>,
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::HandRequestRecovery for RelayRequestRecovery {
    async fn recover(
        &self,
        channel: Box<dyn AgentChannelType>,
        cancel: bool,
    ) -> Result<awaken_session_environment_contract::HandRecoveryObservation, String> {
        use awaken_session_environment_contract::HandRecoveryObservation;
        use awaken_tool_relay::{HandAction, HandResult};
        match self
            .executor
            .recover_pending(
                channel,
                if cancel {
                    HandAction::Cancel
                } else {
                    HandAction::Observe
                },
            )
            .await
        {
            Ok(None) => Ok(HandRecoveryObservation::NoPendingRequest),
            Ok(Some(HandResult::Running { .. })) => Ok(HandRecoveryObservation::Running),
            Ok(Some(HandResult::Indeterminate)) => Ok(HandRecoveryObservation::Indeterminate),
            Ok(Some(HandResult::Observed { .. })) => Ok(HandRecoveryObservation::Returned),
            Ok(Some(HandResult::Err { error })) => Err(error.message),
            Ok(Some(HandResult::Ok { .. })) => {
                Err("Hand control returned an invocation response".into())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

impl awaken_session_environment_contract::HandExecutorFactory for RelayHandExecutorFactory {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
        recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> awaken_session_environment_contract::HandExecutorBinding {
        let executor = awaken_tool_relay::RemoteToolExecutor::new(channel)
            .with_operation_scope(operation_scope);
        let executor = Arc::new(
            if recovery == awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest {
                executor.with_durable_request_recovery()
            } else {
                executor
            },
        );
        let recovery = (recovery
            == awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest)
            .then(|| {
                Arc::new(RelayRequestRecovery {
                    executor: executor.clone(),
                })
                    as Arc<dyn awaken_session_environment_contract::HandRequestRecovery>
            });
        awaken_session_environment_contract::HandExecutorBinding { executor, recovery }
    }
}

/// Build the canonical execution-plane Hand adapter.
#[must_use]
pub fn relay_hand_executor_factory()
-> Arc<dyn awaken_session_environment_contract::HandExecutorFactory> {
    Arc::new(RelayHandExecutorFactory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_tool_contract::tool::{RawTool, ToolError, ToolOutput};

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn resident_request_recovery_cancels_real_bash_without_replaying_or_claiming_stop() {
        // DT-RB: C1 a real Bash child has produced its pid/entry files; C2 its
        // Worker-side future disappears after dispatch; C3 the same factory's
        // recovery role uses a fresh private channel and stable operation id.
        // RB1 C1+C2 => old stream remains uncertain. RB2 +C3 Cancel => one
        // cancellation intent, original invocation ends Indeterminate, actual
        // descendant exits. RB3 Observe => cached invocation evidence; neither
        // result is Sandbox/remote termination proof and neither re-invokes.
        use awaken_session_environment_contract::HandRecoveryObservation;
        use awaken_tool_contract::tool::{
            ToolOperationContext, ToolRecoveryCapability, with_tool_operation_context,
        };
        use awaken_tool_relay::HandOperationLedger;
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(
            awaken_tool_relay::FsOperationLedger::open(dir.path().join("ledger")).unwrap(),
        );
        let tools = awaken_ext_builtin_tools::all_hand_tools_in(
            awaken_ext_builtin_tools::HandToolContext::new(dir.path()),
        );
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        let peer = tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::new(tools, ledger.clone(), awaken_time::system_clock()),
        ));
        let bound = relay_hand_executor_factory().bind(
            Box::new(brain),
            "session-rb",
            ToolRecoveryCapability::DurableRequest,
        );
        let request = ToolCall {
            call_id: "bash".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({
                "command": "printf 'one\\n' >> calls; sh -c 'sleep 30' & printf '%s' $! > child; printf entered > started; wait"
            }),
        };
        let call = {
            let executor = bound.executor.clone();
            tokio::spawn(async move {
                with_tool_operation_context(
                    ToolOperationContext::for_run("run-rb", "op-rb"),
                    executor.invoke(&request),
                )
                .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("started").is_file() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("RB1 actual Bash entry fact");
        let pid = std::fs::read_to_string(dir.path().join("child"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        call.abort();
        let _ = call.await;
        let recovery = bound
            .recovery
            .clone()
            .expect("resident role selected at startup");
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        let control_peer = tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::new([], ledger.clone(), awaken_time::system_clock()),
        ));
        assert_eq!(
            recovery.recover(Box::new(brain), true).await.unwrap(),
            HandRecoveryObservation::Running
        );
        control_peer.await.unwrap().unwrap();
        let operation = awaken_tool_contract::tool::ToolOperationToken::from_context(
            &ToolOperationContext::for_run("run-rb", "op-rb"),
        )
        .unwrap()
        .ledger_id(Some("session-rb"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    ledger.observe(&operation, false).await.unwrap(),
                    awaken_tool_relay::HandResult::Observed { .. }
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("RB2 original ledger completion");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let status = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
                if status.as_ref().is_none_or(|s| {
                    s.rsplit_once(')')
                        .is_some_and(|(_, tail)| tail.trim_start().starts_with('Z'))
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("RB2 real descendant exit");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("calls")).unwrap(),
            "one\n",
            "RB3 no replay"
        );
        drop(recovery);
        drop(bound);
        peer.await.unwrap().unwrap();
    }

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
            awaken_tool_relay::HandSession::in_memory(
                [Arc::new(Echo) as Arc<dyn RawTool>],
                awaken_time::system_clock(),
            ),
        ));
        let bound = relay_hand_executor_factory().bind(
            Box::new(brain),
            "session-a",
            awaken_tool_contract::tool::ToolRecoveryCapability::NonRecoverable,
        );
        let executor = bound.executor;
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
        use awaken_tool_contract::tool::{
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
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        self.release.notified(),
                    )
                    .await
                    .expect("timed out waiting for self.release.notified()");
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
                awaken_tool_relay::HandSession::in_memory(
                    [tool.clone() as Arc<dyn RawTool>],
                    awaken_time::system_clock(),
                ),
            ));
            let factory = relay_hand_executor_factory();
            let executor = factory.bind(Box::new(brain), "session", recovery).executor;
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
            tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
                .await
                .expect("timed out waiting for entered.notified()");
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
            // R1 must retain the typed physical uncertainty and tool identity;
            // a generic execution error is not an equivalent recovery signal.
            assert!(
                matches!(old, ToolError::Indeterminate(ref tool) if tool == "echo"),
                "R1 must preserve indeterminate identity, got {old:?}"
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
                awaken_tool_relay::HandSession::in_memory(
                    [tool as Arc<dyn RawTool>],
                    awaken_time::system_clock(),
                ),
            ));
            let replacement = factory.bind(Box::new(brain), "session", recovery).executor;
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
