use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::InferenceExecutorMaterializer;

use crate::EchoModel;

/// Run this process as a database-less echo Worker of `upstream`. Its dispatch
/// pool claims and settles over the server transport and commits facts upstream;
/// it holds no store and serves no product HTTP surface. The production Worker
/// lifecycle remains authoritative; only the deterministic executor differs.
pub async fn run_echo_worker(
    upstream: &str,
    worker_id: &str,
    admin_listen: Option<&str>,
    request_authorizer: Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthorizer>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    struct EchoWorkerProvider;

    impl InferenceExecutorMaterializer for EchoWorkerProvider {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &[awaken_run_ingress::HOST_EXECUTOR_CAPABILITY]
        }

        fn materialize_pinned(
            &self,
            candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Option<Arc<dyn LlmExecutor>> {
            if !matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
            ) {
                return None;
            }
            Some(Arc::new(EchoModel))
        }
    }

    let mut worker_upstream =
        awaken_worker_transport_security::WorkerUpstream::new(upstream).with_worker_id(worker_id);
    if let Some(authorizer) = request_authorizer {
        worker_upstream = worker_upstream.with_request_authorizer(authorizer);
    }
    let mut builder = echo_worker_builder(worker_upstream, Arc::new(EchoWorkerProvider));
    builder = match admin_listen {
        Some(address) => builder.with_admin_listen(address),
        None => builder.without_admin_surface(),
    };
    builder.build()?.run_until_shutdown().await
}

fn echo_worker_builder(
    upstream: awaken_worker_transport_security::WorkerUpstream,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> awaken_worker::WorkerNodeBuilder {
    // The scenario image intentionally does not install bwrap: this test isolates
    // remote claim/commit recovery, while sandbox-tier tests own OS isolation.
    // Install Local explicitly through the canonical Worker builder rather than
    // relying on an unsafe fallback from the Namespace default.
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    awaken_worker::WorkerNodeBuilder::new(upstream)
        .with_deployment_config(deployment)
        .with_inference_materializer(materializer)
        // Reuse the production anonymous A2A adapter. The standard Worker
        // manifest derives `a2a-runtime` from this installed implementation, so
        // recovery scenarios cannot advertise an endpoint-specific capability
        // through a parallel environment-variable path without being able to
        // execute it.
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(None))
        // Managed Session placement requires the standard per-kind Resource clients.
        // This is the same registered HTTP Memory adapter factory used by production;
        // File, Skill, and Repository clients are installed by WorkerNode itself.
        .with_registered_memory_mounter_factory(awaken_cli::registered_memory_mounter_factory())
        .with_standard_manifest(Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_worker_advertises_only_its_installed_managed_session_boundary() {
        // Cause/effect graph: C1=registered Memory factory plus built-in per-kind
        // clients; C2=deterministic Host materializer; C3=production anonymous
        // A2A adapter. Effects are E1=session-resources/v1, E2=host-executor/v1,
        // and E3=a2a-runtime. No explicit fake manifest or environment override
        // may claim a capability absent from the canonical Worker builder.
        //
        // | Rule | C1 | C2 | C3 | E1 | E2 | E3 |
        // |---|---|---|---|---|---|---|
        // | W1 canonical scenario Worker | yes | yes | yes | yes | yes | yes |
        // | W2 A2A adapter absent (covered by awaken-worker) | any | any | no | as installed | as installed | no |
        struct HostMaterializer;
        impl InferenceExecutorMaterializer for HostMaterializer {
            fn supported_access_schemes(&self) -> &'static [&'static str] {
                &[awaken_run_ingress::HOST_EXECUTOR_CAPABILITY]
            }

            fn materialize_pinned(
                &self,
                _candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
                _context: &awaken_runtime_contract::RuntimeRunContext,
            ) -> Option<Arc<dyn LlmExecutor>> {
                None
            }
        }

        let worker = echo_worker_builder(
            awaken_worker_transport_security::WorkerUpstream::new("http://coordinator")
                .with_worker_id("worker-a"),
            Arc::new(HostMaterializer),
        )
        .build()
        .expect("canonical scenario Worker topology");
        assert!(
            worker
                .manifest()
                .capabilities
                .contains("session-resources/v1"),
            "W1"
        );
        assert!(
            worker.manifest().capabilities.contains("host-executor/v1"),
            "W2"
        );
        assert!(
            worker
                .manifest()
                .capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY),
            "W3"
        );
    }
}
