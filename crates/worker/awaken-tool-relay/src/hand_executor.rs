// @formal surface obligation=remote_tool.stable_operation_identity
// @formal surface obligation=remote_tool.invoke_at_most_once
// @formal surface obligation=remote_tool.result_matches_operation
// @formal surface obligation=remote_tool.indeterminate_never_false_success
// @formal reason: Composition of the remote Hand/tool relay protocol.
//! The binding from a segregated Hand channel to the neutral tool port: the
//! Hand executor factory every composition that realizes Session environments
//! installs (the Worker, and the co-located execution of an AllInOne process).

use std::sync::Arc;

use awaken_agent_channel::AgentChannel as AgentChannelType;

struct RelayHandExecutorFactory;

struct RelayRequestRecovery {
    executor: Arc<crate::RemoteToolExecutor<Box<dyn AgentChannelType>>>,
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::HandRequestRecovery for RelayRequestRecovery {
    async fn recover(
        &self,
        channel: Box<dyn AgentChannelType>,
        cancel: bool,
    ) -> Result<awaken_session_environment_contract::HandRecoveryObservation, String> {
        use crate::{HandAction, HandResult};
        use awaken_session_environment_contract::HandRecoveryObservation;
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
        let executor =
            crate::RemoteToolExecutor::new(channel).with_operation_scope(operation_scope);
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
