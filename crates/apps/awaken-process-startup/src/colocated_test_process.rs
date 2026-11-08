// @formal surface exclude=test_support_adapter
// @formal reason: Test-support composition of the shipped co-located Worker topology; the Worker transport,
// @formal reason: credential vault and drain order it wires are the production owners, proved where they live.
//! Test-support AllInOne whose Sessions run on its
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

/// Which stores the test process opens.
pub enum ColocatedTestStores<'a> {
    /// Everything in process memory.
    Volatile,
    /// Durable stores under `dir`, secrets sealed under `key`, no IAM guard.
    Durable {
        dir: &'a std::path::Path,
        key: &'a [u8; 32],
    },
    /// [`Self::Durable`] with the embedded IAM guard (self-managed identity).
    Secured {
        dir: &'a std::path::Path,
        key: &'a [u8; 32],
    },
}

/// A started AllInOne test process and the lifecycle that owns its
/// co-located Worker. Dropping the lifecycle stops the Worker.
pub struct ColocatedTestProcess {
    pub public_router: Router,
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    /// The embedded IAM of a [`ColocatedTestStores::Secured`] process.
    pub authz: Option<Arc<crate::ManagementAuthz>>,
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

/// The Worker-side inference of a process that publishes one deterministic
/// host executor; `None` when publications resolve from the authored catalog.
pub(crate) fn host_inference(
    supply: &crate::PublicationModelSupply,
) -> Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>> {
    match supply {
        crate::PublicationModelSupply::Host { executor, .. } => {
            Some(Arc::new(HostExecutorInference(executor.clone())))
        }
        _ => None,
    }
}

/// Start an AllInOne test process with its co-located registered Worker.
/// `model` publishes one deterministic host executor; `None` resolves
/// publications from the authored catalog.
pub async fn start_all_in_one_with_worker(
    stores: ColocatedTestStores<'_>,
    model: Option<(
        Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
        awaken_runtime_contract::resolved::ModelBinding,
    )>,
    worker: impl FnOnce(ColocatedTestWorkerInputs) -> Arc<dyn crate::ColocatedWorker>,
) -> Result<ColocatedTestProcess, String> {
    start_all_in_one_with_worker_and_web_providers(stores, model, None, worker).await
}

/// [`start_all_in_one_with_worker`] whose process selects its Web provider
/// catalog, as an Awaken Cloud identity extends it with its routed providers.
/// The catalog enters process startup before router assembly, so Control
/// validation and the co-located Worker's Runs read the same one.
pub async fn start_all_in_one_with_worker_and_web_providers(
    stores: ColocatedTestStores<'_>,
    model: Option<(
        Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
        awaken_runtime_contract::resolved::ModelBinding,
    )>,
    web_search_providers: Option<awaken_ext_builtin_tools::WebSearchProviderRegistry>,
    worker: impl FnOnce(ColocatedTestWorkerInputs) -> Arc<dyn crate::ColocatedWorker>,
) -> Result<ColocatedTestProcess, String> {
    let (stores, authz) = match stores {
        ColocatedTestStores::Volatile => (
            crate::local_process_stores::in_memory_process_stores().await,
            None,
        ),
        ColocatedTestStores::Durable { dir, key } => (
            crate::local_process_stores::open_local_process_stores(dir, key).await?,
            None,
        ),
        ColocatedTestStores::Secured { dir, key } => {
            let stores = crate::local_process_stores::open_local_process_stores(dir, key).await?;
            // IAM judges tokens on the same wall clock the stores stamp with.
            let iam = crate::embedded_iam(dir, stores.clock.clone())?;
            (stores, Some(iam))
        }
    };
    let mut options = crate::exact_host_model::registered_test_process_options(&stores);
    options.web_search_providers = web_search_providers;
    let model_supply = match model {
        Some((executor, binding)) => crate::PublicationModelSupply::Host { executor, binding },
        None => crate::PublicationModelSupply::PublishedProviders,
    };
    start_over(stores, authz, options, model_supply, worker).await
}

async fn start_over(
    stores: crate::ProcessStores,
    authz: Option<Arc<crate::ManagementAuthz>>,
    options: crate::ProcessStartup,
    model_supply: crate::PublicationModelSupply,
    worker: impl FnOnce(ColocatedTestWorkerInputs) -> Arc<dyn crate::ColocatedWorker>,
) -> Result<ColocatedTestProcess, String> {
    let control = stores
        .control
        .as_ref()
        .ok_or("an AllInOne test process owns Control stores")?;
    let credential_stores = awaken_control::InferenceMaterializationStores {
        credentials: control.credentials.clone(),
        secrets: control.secrets.clone(),
    };
    let clock = stores.clock.clone();
    let runtime = options
        .deployment
        .clone()
        .ok_or("the test process selects its deployment")?;
    let host_inference = host_inference(&model_supply);
    let routers = crate::runtime_process_router::prepare_runtime_routers(
        stores,
        authz.clone(),
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
        colocated_worker,
        ..
    } = routers;
    crate::service::spawn_colocated_worker(
        &service_lifecycle,
        listener,
        private_router,
        worker,
        upstream,
        deployment,
        colocated_worker,
        clock,
    )
    .await?;
    Ok(ColocatedTestProcess {
        public_router,
        service_lifecycle,
        authz,
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
