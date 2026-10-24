// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! The Session execution a process co-locates with its Coordinator.

use awaken_execution_composition::{
    ContainerTierPolicy, local_auxiliary_sandboxes, with_acp_from_deployment,
    with_session_environment_from_deployment, with_session_environment_from_deployment_at,
};
use awaken_process_startup::{ColocatedExecution, ColocatedExecutionInputs};
use awaken_session_executor::SessionExecutorBuilder;

use crate::with_platform_memory_data_plane;

/// The `LocalWorker` placement's Session execution: the deployment's Session
/// environment with the relay Hand executor factory, its ACP routes, the
/// Memory mount data plane for its Sandbox tier and the auxiliary sandboxes.
/// Consumers: compositions that run Sessions on the process's own local claim
/// pool, such as the scenario host and the startup's AllInOne test builders.
/// The `awaken` launcher's AllInOne process co-locates a registered Worker
/// instead, which installs its own execution.
pub struct LocalExecution;

#[async_trait::async_trait]
impl ColocatedExecution for LocalExecution {
    async fn install(
        &self,
        host: SessionExecutorBuilder,
        inputs: ColocatedExecutionInputs,
    ) -> Result<SessionExecutorBuilder, String> {
        let ColocatedExecutionInputs {
            deployment,
            namespace_hand_path,
            credentials,
            memory_source,
            memory_invalidator,
        } = inputs;
        let hand_factory = awaken_tool_relay::relay_hand_executor_factory();
        let containers = ContainerTierPolicy::default();
        let host = match namespace_hand_path {
            Some(path) => {
                with_session_environment_from_deployment_at(
                    host,
                    &deployment,
                    Some(hand_factory),
                    path,
                    &containers,
                )
                .await?
            }
            None => {
                with_session_environment_from_deployment(
                    host,
                    &deployment,
                    Some(hand_factory),
                    &containers,
                )
                .await?
            }
        };
        let host = with_acp_from_deployment(host, &deployment, credentials, None)
            .with_auxiliary_sandboxes(local_auxiliary_sandboxes());
        Ok(with_platform_memory_data_plane(
            host,
            deployment.sandbox_tier,
            memory_source,
            memory_invalidator,
        ))
    }
}
