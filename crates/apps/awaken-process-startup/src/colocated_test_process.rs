// @formal surface exclude=test_support_adapter
// @formal reason: Test-support composition of the shipped co-located Worker topology; the Worker transport,
// @formal reason: credential vault and drain order it wires are the production owners, proved where they live.
//! Test-support AllInOne over volatile stores whose Sessions run on its
//! co-located registered Worker, reached over a loopback private listener.
//!
//! This is the topology the `awaken` launcher ships: the Coordinator admits
//! and the Worker claims over the same Worker transport and authority checks
//! a remote Worker uses. Process startup links no Worker, so the composing
//! test supplies it from the inputs below; the lifecycle supervises it the way
//! `service.rs` does, stopping it before the private surface it drains through.

use std::sync::Arc;

use axum::Router;

/// What the composing test needs to build the co-located Worker.
pub struct ColocatedTestWorkerInputs {
    /// The process's own Control credential stores; the Worker materializes
    /// credentials from the same vault the public API authors.
    pub stores: awaken_control::InferenceMaterializationStores,
    /// When the process publishes one deterministic host executor, the Worker
    /// materializes exactly that executor for host-executor candidates.
    pub host_inference: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
}

/// A started volatile AllInOne process and the lifecycle that owns its
/// co-located Worker.
pub struct ColocatedTestProcess {
    pub public_router: Router,
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
}

/// The one host executor a deterministic test process publishes.
struct HostExecutorInference(Arc<dyn awaken_runtime_contract::llm::LlmExecutor>);

impl awaken_inference_endpoint::InferenceExecutorMaterializer for HostExecutorInference {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[awaken_dispatch::HOST_EXECUTOR_CAPABILITY]
    }

    fn materialize_pinned(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>> {
        matches!(
            candidate.provisioning(),
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        )
        .then(|| self.0.clone())
    }
}

/// Start a volatile AllInOne process with its co-located registered Worker.
/// `model` publishes one deterministic host executor; `None` resolves
/// publications from the authored catalog.
pub async fn start_ephemeral_all_in_one_with_worker(
    model: Option<(
        Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
        awaken_runtime_contract::resolved::ModelBinding,
    )>,
    worker: impl FnOnce(ColocatedTestWorkerInputs) -> Arc<dyn crate::ColocatedWorker>,
) -> Result<ColocatedTestProcess, String> {
    let (stores, credential_stores) =
        crate::local_process_stores::in_memory_process_stores_with_credentials().await;
    let clock = stores.clock.clone();
    let options = crate::exact_host_model::registered_test_process_options(&stores);
    let runtime = options
        .deployment
        .clone()
        .ok_or("the test process selects its deployment")?;
    let (model_supply, host_inference) = match model {
        Some((executor, binding)) => (
            crate::PublicationModelSupply::Host {
                executor: executor.clone(),
                binding,
            },
            Some(Arc::new(HostExecutorInference(executor))
                as Arc<
                    dyn awaken_inference_endpoint::InferenceExecutorMaterializer,
                >),
        ),
        None => (crate::PublicationModelSupply::PublishedProviders, None),
    };
    let routers = crate::runtime_process_router::prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        model_supply,
        options,
    )
    .await?;
    let worker = worker(ColocatedTestWorkerInputs {
        stores: credential_stores,
        host_inference,
    });
    let deployment = worker_deployment(runtime, clock.clone())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("bind the loopback Worker surface: {error}"))?;
    let upstream = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|error| format!("loopback Worker surface address: {error}"))?
    );
    let crate::prepared_process::ProcessRouters {
        public_router,
        private_router,
        service_lifecycle,
        admin_tools,
        ..
    } = routers;
    crate::service::spawn_colocated_worker(
        &service_lifecycle,
        listener,
        private_router,
        worker,
        upstream,
        deployment,
        admin_tools,
        clock,
    )
    .await?;
    Ok(ColocatedTestProcess {
        public_router,
        service_lifecycle,
    })
}

/// The Worker's view of this process's deployment: the test `runtime` over a
/// private, empty configuration so no host file or home directory leaks in.
fn worker_deployment(
    runtime: awaken_deployment_config::DeploymentConfig,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<crate::config::ResolvedDeployment, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "awaken-colocated-worker-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create {}: {error}", root.display()))?;
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "")
        .map_err(|error| format!("write {}: {error}", config_path.display()))?;
    let mut deployment = crate::config::ResolvedDeployment::load(
        crate::config::ConfigOverrides {
            config_path: Some(config_path),
            role: Some(crate::config::Role::AllInOne),
            data_dir: Some(root),
            no_browser: Some(true),
            ..Default::default()
        },
        clock,
    )?;
    deployment.runtime = runtime;
    Ok(deployment)
}
