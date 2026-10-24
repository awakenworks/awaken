// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! Install physical execution only for the selected local Session placement.

use crate::{ColocatedExecution, ColocatedExecutionInputs};
use awaken_session_application::SessionExecutionPlacement;
use awaken_session_executor::SessionExecutorBuilder;
use std::sync::Arc;

/// Install the execution the placement selects.
///
/// Decision table:
/// | Rule | Placement | Co-located execution supplied | Effect |
/// |---|---|---|---|
/// | L1 | `LocalWorker` | yes | it installs the Session environment, Hand, ACP routes, Memory mount data plane and auxiliary sandboxes |
/// | L2 | `LocalWorker` | no | refused: a local claim pool needs the composition that co-locates execution |
/// | L3 | `RegisteredWorker` | any | no Sandbox, Hand, capacity or Memory mount role; only the ACP adapter projection policy. Sessions run on registered Workers and their retirement is decided from the retention they report |
pub(super) async fn prepare(
    host: SessionExecutorBuilder,
    placement: SessionExecutionPlacement,
    colocated: Option<Arc<dyn ColocatedExecution>>,
    inputs: ColocatedExecutionInputs,
) -> Result<SessionExecutorBuilder, String> {
    match placement {
        SessionExecutionPlacement::LocalWorker => match colocated {
            Some(colocated) => colocated.install(host, inputs).await,
            None => Err(
                "a local claim pool runs Sessions on this process and needs the composition \
                 that co-locates their execution; start the AllInOne service with the \
                 `awaken` launcher"
                    .to_owned(),
            ),
        },
        SessionExecutionPlacement::RegisteredWorker => {
            Ok(host.with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Counts installs and installs nothing.
    #[derive(Default)]
    struct CountingExecution(AtomicUsize);

    #[async_trait::async_trait]
    impl ColocatedExecution for CountingExecution {
        async fn install(
            &self,
            host: SessionExecutorBuilder,
            _: ColocatedExecutionInputs,
        ) -> Result<SessionExecutorBuilder, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(host)
        }
    }

    async fn inputs(
        deployment: &awaken_deployment_config::DeploymentConfig,
    ) -> ColocatedExecutionInputs {
        let invalidator = Arc::new(awaken_resource_contract::LocalInvalidator::default());
        let resources = awaken_resource_persistence::ephemeral(
            awaken_time::system_clock(),
            invalidator.clone(),
        )
        .await
        .expect("ephemeral Resources");
        ColocatedExecutionInputs {
            deployment: deployment.clone(),
            namespace_hand_path: None,
            credentials: None,
            memory_source: resources.authorities().memory_mount_source(),
            memory_invalidator: invalidator,
        }
    }

    #[tokio::test]
    async fn only_a_local_placement_installs_the_colocated_execution() {
        // Rows L1-L3 of `prepare`, over a Kubernetes container deployment with
        // warm capacity and a package builder: L3 a registered-Worker
        // placement installs nothing, even when an installer is supplied, so
        // the Coordinator neither executes nor observes Sandboxes and needs no
        // Kubernetes client; L2 a local placement without the co-located
        // execution is refused; L1 a local placement installs it exactly once.
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = awaken_deployment_config::SandboxTier::K8s;
        deployment.container_image = Some(format!(
            "registry.invalid/sandbox@sha256:{}",
            "0".repeat(64)
        ));
        deployment.sandbox.warm_pool_size = 2;
        deployment.sandbox.package_image_builder =
            Some(awaken_deployment_config::PackageImageBuilder::Kubernetes);
        let host = || async {
            awaken_session_executor_testkit::unconfigured_dependencies_with_deployment(
                Arc::new(NoModel),
                "unused",
                deployment.clone(),
            )
            .await
        };
        let counting = Arc::new(CountingExecution::default());
        let registered = prepare(
            host().await,
            SessionExecutionPlacement::RegisteredWorker,
            Some(counting.clone()),
            inputs(&deployment).await,
        )
        .await
        .expect("L3 registered-Worker preparation needs no Sandbox backend");
        assert_eq!(counting.0.load(Ordering::SeqCst), 0, "L3");
        assert!(!registered.session_environment_provider_installed(), "L3");
        assert!(
            !registered.has_memory_mounter(),
            "L3 no Memory mount data plane"
        );
        let refused = prepare(
            host().await,
            SessionExecutionPlacement::LocalWorker,
            None,
            inputs(&deployment).await,
        )
        .await
        .err()
        .expect("L2 a local claim pool needs its co-located execution");
        assert!(refused.contains("co-locates"), "L2: {refused}");
        prepare(
            host().await,
            SessionExecutionPlacement::LocalWorker,
            Some(counting.clone()),
            inputs(&deployment).await,
        )
        .await
        .expect("L1");
        assert_eq!(counting.0.load(Ordering::SeqCst), 1, "L1");
    }
}
