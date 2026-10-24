// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! Install physical execution only for the selected local Session placement.

use awaken_deployment_config::DeploymentConfig;
use awaken_session_application::SessionExecutionPlacement;
use awaken_session_executor::SessionExecutorBuilder;
use std::path::PathBuf;

pub(super) async fn prepare(
    host: SessionExecutorBuilder,
    deployment: &DeploymentConfig,
    placement: SessionExecutionPlacement,
    namespace_hand_path: Option<PathBuf>,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> Result<SessionExecutorBuilder, String> {
    let host = if placement == SessionExecutionPlacement::LocalWorker {
        let hand_factory = awaken_tool_relay::relay_hand_executor_factory();
        let containers = awaken_execution_composition::ContainerTierPolicy::default();
        let host = match namespace_hand_path {
            Some(path) => {
                awaken_execution_composition::with_session_environment_from_deployment_at(
                    host,
                    deployment,
                    Some(hand_factory),
                    path,
                    &containers,
                )
                .await?
            }
            None => {
                awaken_execution_composition::with_session_environment_from_deployment(
                    host,
                    deployment,
                    Some(hand_factory),
                    &containers,
                )
                .await?
            }
        };
        awaken_execution_composition::with_acp_from_deployment(host, deployment, credentials, None)
    } else {
        // Registered-Worker placement installs no Sandbox role: every
        // execution, Hand and capacity role stays unavailable on this process,
        // and Worker retirement is decided from the retention Workers report.
        host.with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy())
    };
    Ok(host)
}

#[cfg(all(test, feature = "container-k8s"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct NoModel;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoModel {
        async fn infer(
            &self,
            _: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("a registered-Worker Coordinator cannot execute a model")
        }
    }

    #[tokio::test]
    async fn registered_worker_placement_installs_no_sandbox_role() {
        // Cause/effect table RCO: C1 registered-Worker placement; C2 a
        // Kubernetes container tier with warm capacity and a package builder.
        // R1 C1+C2 -> preparation succeeds without any Kubernetes client (no
        // kubeconfig is present), installs no Session environment provider,
        // and the uninstalled provider refuses creation: the Coordinator
        // neither executes nor observes Sandboxes, and Worker retirement is
        // decided from the retention Workers report.
        let mut deployment = DeploymentConfig::ephemeral();
        deployment.disable_local_pool = true;
        deployment.sandbox_tier = awaken_deployment_config::SandboxTier::K8s;
        deployment.container_image = Some(format!(
            "registry.invalid/sandbox@sha256:{}",
            "0".repeat(64)
        ));
        deployment.sandbox.k8s_namespace = "no-observation".into();
        deployment.sandbox.warm_pool_size = 2;
        deployment.sandbox.package_image_builder =
            Some(awaken_deployment_config::PackageImageBuilder::Kubernetes);
        let host = awaken_session_executor_testkit::unconfigured_dependencies_with_deployment(
            Arc::new(NoModel),
            "unused",
            deployment.clone(),
        )
        .await;
        let host = prepare(
            host,
            &deployment,
            SessionExecutionPlacement::RegisteredWorker,
            None,
            None,
        )
        .await
        .expect("RCO R1 registered-Worker preparation needs no Sandbox backend");
        assert!(!host.session_environment_provider_installed(), "RCO R1");
        let spec = awaken_provisioning_contract::SandboxSpec {
            process_tree_supervision: false,
            scope: "no-observation".into(),
            isolation: awaken_provisioning_contract::IsolationClass::Container,
            environment: None,
            command: vec!["unused".into()],
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: awaken_provisioning_contract::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Ephemeral,
            control_services: Default::default(),
            lease_ttl_secs: None,
        };
        assert!(
            host.installed_session_environment_provider()
                .realizer()
                .effective_spec(&spec)
                .is_err(),
            "RCO R1 no container metadata role is installed"
        );
    }
}
