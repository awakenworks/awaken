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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    struct EchoWorkerProvider;

    impl InferenceExecutorMaterializer for EchoWorkerProvider {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &[awaken_runtime_host::HOST_EXECUTOR_CAPABILITY]
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

    // The scenario image intentionally does not install bwrap: this test isolates
    // remote claim/commit recovery, while sandbox-tier tests own OS isolation.
    // Install Local explicitly through the canonical Worker builder rather than
    // relying on an unsafe fallback from the Namespace default.
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    let mut builder = awaken_worker::WorkerNodeBuilder::new(
        awaken_runtime_host::WorkerUpstream::new(upstream).with_worker_id(worker_id),
    )
    .with_deployment_config(deployment)
    .with_inference_materializer(Arc::new(EchoWorkerProvider))
    .with_standard_manifest(Default::default());
    builder = match admin_listen {
        Some(address) => builder.with_admin_listen(address),
        None => builder.without_admin_surface(),
    };
    builder.build()?.run_until_shutdown().await
}
