// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! Install physical execution only for the selected local Session placement.

use awaken_session_executor::HostDependencies;
use awaken_session_application::SessionExecutionPlacement;
use std::path::PathBuf;

pub(super) async fn prepare(
    host: HostDependencies,
    placement: SessionExecutionPlacement,
    namespace_hand_path: Option<PathBuf>,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> Result<HostDependencies, String> {
    let host = if placement == SessionExecutionPlacement::LocalWorker {
        let hand_factory = awaken_worker::relay_hand_executor_factory();
        let containers = awaken_execution_composition::ContainerTierPolicy::default();
        let host = match namespace_hand_path {
            Some(path) => {
                awaken_execution_composition::with_session_environment_from_deployment_at(
                    host,
                    Some(hand_factory),
                    path,
                    &containers,
                )
                .await?
            }
            None => {
                awaken_execution_composition::with_session_environment_from_deployment(
                    host,
                    Some(hand_factory),
                    &containers,
                )
                .await?
            }
        };
        awaken_execution_composition::with_acp_from_deployment(host, credentials, None)
    } else {
        // Coordinator-only placement still projects ACP Session tools. Give it
        // the existing catalog policy, never a second process/Hand executor.
        host.with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy())
    };
    Ok(host)
}
