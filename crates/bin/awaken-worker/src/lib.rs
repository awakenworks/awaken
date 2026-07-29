//! `awaken-worker` — the PRODUCTION database-less worker (Stage C).
//!
//! A peer of the control plane (`awaken-control`) and the data plane
//! (`awaken-server`). It owns no Run, Session, or authoring store: it drains runs
//! from a Control Node over the typed dispatch transport and sends claim-fenced
//! commit operations back through the same Worker boundary. A resource-capable
//! worker may open shared data-plane and Resource Catalog validation ports; those
//! remain platform truth, not worker-owned state.
//!
//! **Real per-run model resolution, no mocks.** A drained run arrives as a
//! `RunActivation` carrying its own `ExecutableAgentSnapshot`, whose
//! `resolved_spec.model_binding.model_ref` is the run's model identity. The host's
//! run loop resolves that ref through the injected [`InferenceExecutorMaterializer`]
//! ([`CredentialInferenceMaterializer`]),
//! which consumes the snapshot-pinned endpoint and credential reference and
//! injects the credential from the shared vault — see
//! [`awaken_control::open_inference_materialization_stores`]). The host's
//! [`NoModelConfiguredExecutor`]
//! is only an inert construction placeholder: because the materializer is installed,
//! an unavailable publication pin fails closed before that executor can run.

use std::sync::Arc;

mod admin;
mod application;
mod credential_liveness;
mod lifecycle;
mod manifest;
mod resource_plane;

use credential_liveness::WorkerObservationCache;
use lifecycle::{
    WorkerLifecycle, grace_window, new_incarnation_id, spawn_heartbeat, wait_for_in_flight,
    wall_clock_ms,
};
use manifest::{
    CredentialMaterializerSupport, ManifestSelection, ManifestSource, ResourceManifestSupport,
    StandardManifestInputs, derive_standard_manifest,
};
use resource_plane::shared_resource_wiring;

pub use application::{
    RegisteredApplicationFactory, RegisteredWorkerApplication, RegisteredWorkerContext,
};
pub use lifecycle::WorkerShutdown;
pub use manifest::StandardManifestConfig;
pub use resource_plane::WorkerResourcePlane;

use awaken_runtime_host::{WorkerControlClient, WorkerUpstream};
use awaken_server::inference_materializer::CredentialInferenceMaterializer;
use awaken_server::no_model::NoModelConfiguredExecutor;
use awaken_server::{InferenceExecutorMaterializer, SharedHost};
use awaken_worker_contract::{RegistryMutation, WorkerHeartbeat, WorkerManifest};

struct WorkerProcessConfig {
    deployment: awaken_runtime_host::DeploymentConfig,
    manifest: StandardManifestConfig,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
    repository_credentials: bool,
}

/// Product-command presentation and manifest values resolved at its one config
/// boundary.
pub struct WorkerRunOptions {
    pub worker_id: String,
    pub admin_listen: Option<String>,
    pub drain_grace: std::time::Duration,
    pub credential_probe_interval: std::time::Duration,
    pub credential_observation_ttl: std::time::Duration,
    pub manifest: StandardManifestConfig,
}

impl WorkerProcessConfig {
    fn embedded_defaults() -> Self {
        Self {
            deployment: awaken_runtime_host::DeploymentConfig::ephemeral(),
            manifest: StandardManifestConfig::default(),
            admin_listen: None,
            graceful_drain: grace_window(true, None),
            credential_probe_interval: std::time::Duration::from_secs(10),
            credential_observation_ttl: std::time::Duration::from_secs(30),
            repository_credentials: false,
        }
    }
}

/// Invalid explicit Worker composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerNodeBuildError(String);

impl std::fmt::Display for WorkerNodeBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WorkerNodeBuildError {}

/// Public assembly boundary for a recoverable database-less Worker.
pub struct WorkerNodeBuilder {
    upstream: WorkerUpstream,
    manifest: ManifestSelection,
    deployment: awaken_runtime_host::DeploymentConfig,
    standard_manifest_config: StandardManifestConfig,
    application_factory: Option<RegisteredApplicationFactory>,
    application_gate: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    credential_materializer: Option<awaken_runtime_host::PinnedCredentialMaterializer>,
    external_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::CredentialMaterialResolver>>,
    worker_local_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    credential_inference_derived: bool,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    mcp_attachment_realizer: Option<Arc<dyn awaken_runtime_host::McpAttachmentRealizer>>,
    web_search_providers: awaken_runtime_host::WebSearchProviderRegistry,
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

impl WorkerNodeBuilder {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self {
            upstream,
            manifest: ManifestSelection::Unset,
            deployment: awaken_runtime_host::DeploymentConfig::ephemeral(),
            standard_manifest_config: StandardManifestConfig::default(),
            application_factory: None,
            application_gate: None,
            materializer: None,
            credential_materializer: None,
            external_credential_resolver: None,
            worker_local_credential_resolver: None,
            acp_capability_observation_source: None,
            credential_inference_derived: false,
            session_container_provider: None,
            mcp_attachment_realizer: None,
            web_search_providers: awaken_runtime_host::WebSearchProviderRegistry::builtins(),
            resources: None,
            admin_listen: Some("0.0.0.0:9090".to_string()),
            graceful_drain: std::time::Duration::from_secs(20),
            credential_probe_interval: std::time::Duration::from_secs(10),
            credential_observation_ttl: std::time::Duration::from_secs(30),
        }
    }

    fn with_process_config(mut self, config: WorkerProcessConfig) -> Self {
        self.deployment = config.deployment;
        self.standard_manifest_config = config.manifest;
        self.admin_listen = config.admin_listen;
        self.graceful_drain = config.graceful_drain;
        self.credential_probe_interval = config.credential_probe_interval;
        self.credential_observation_ttl = config.credential_observation_ttl;
        self
    }

    #[must_use]
    pub fn with_manifest(mut self, manifest: WorkerManifest) -> Self {
        self.manifest
            .select(ManifestSource::Explicit(Box::new(manifest)));
        self
    }

    /// Derive the immutable Worker manifest from the dependencies installed on
    /// this builder. Application capabilities remain explicit because the
    /// registration-time factory cannot run until after this manifest has been
    /// accepted and assigned a Worker identity.
    #[must_use]
    pub fn with_standard_manifest(
        mut self,
        application_capabilities: std::collections::BTreeSet<String>,
    ) -> Self {
        self.manifest.select(ManifestSource::Standard {
            application_capabilities,
        });
        self
    }

    /// Install the same typed deployment value used to derive this Worker's
    /// manifest and to assemble its runtime Host.
    #[must_use]
    pub fn with_deployment_config(
        mut self,
        deployment: awaken_runtime_host::DeploymentConfig,
    ) -> Self {
        self.deployment = deployment;
        self
    }

    /// Install typed metadata for standard manifest derivation.
    #[must_use]
    pub fn with_standard_manifest_config(mut self, config: StandardManifestConfig) -> Self {
        self.standard_manifest_config = config;
        self
    }

    /// Install the only application extension factory, evaluated after Worker
    /// registration so both provisioning and execution use its assigned identity.
    #[must_use]
    pub fn with_application_factory(mut self, factory: RegisteredApplicationFactory) -> Self {
        self.application_factory = Some(factory);
        self
    }

    /// Install the sole application-owned permission gate around the canonical
    /// Session route.
    #[must_use]
    pub fn with_application_gate(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.application_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn InferenceExecutorMaterializer>,
    ) -> Self {
        self.materializer = Some(materializer);
        self.credential_inference_derived = false;
        self
    }

    /// Replace the deployment's WebSearch provider catalog. Management
    /// publication and Worker execution must receive registries assembled from
    /// the same provider definitions; the immutable snapshot remains the
    /// selection authority for each run.
    #[must_use]
    pub fn with_web_search_provider_registry(
        mut self,
        providers: awaken_runtime_host::WebSearchProviderRegistry,
    ) -> Self {
        self.web_search_providers = providers;
        self
    }

    #[must_use]
    pub fn with_resource_plane(mut self, resources: WorkerResourcePlane) -> Self {
        if self.credential_materializer.is_none()
            && let Some(stores) = &resources.credentials
        {
            self.credential_materializer =
                Some(awaken_runtime_host::PinnedCredentialMaterializer::new(
                    stores.credentials.clone(),
                    stores.secrets.clone(),
                ));
        }
        self.resources = Some(resources);
        self
    }

    #[must_use]
    pub fn with_admin_listen(mut self, address: impl Into<String>) -> Self {
        self.admin_listen = Some(address.into());
        self
    }

    #[must_use]
    pub fn without_admin_surface(mut self) -> Self {
        self.admin_listen = None;
        self
    }

    #[must_use]
    pub fn with_graceful_drain(mut self, grace: std::time::Duration) -> Self {
        self.graceful_drain = grace;
        self
    }

    /// Configure the one credential-liveness loop used for heartbeat evidence
    /// and launch-time revalidation.
    #[must_use]
    pub fn with_credential_observation_window(
        mut self,
        probe_interval: std::time::Duration,
        observation_ttl: std::time::Duration,
    ) -> Self {
        self.credential_probe_interval = probe_interval;
        self.credential_observation_ttl = observation_ttl;
        self
    }

    /// Install the authoritative credential materializer used by ACP launch and
    /// Session secret delivery.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        credentials: awaken_runtime_host::PinnedCredentialMaterializer,
    ) -> Self {
        self.credential_materializer = Some(credentials);
        self
    }

    /// Install the one exact non-local credential material resolver used for
    /// Worker-private references and recipient-bound sealed envelopes. It is
    /// composed into the canonical materializer at `build()` and is never a
    /// fallback credential selector.
    #[must_use]
    pub fn with_external_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::CredentialMaterialResolver>,
    ) -> Self {
        self.external_credential_resolver = Some(resolver);
        self
    }

    /// Install the liveness-only adapter for host-owned credential identities.
    /// This port cannot materialize Provider secrets.
    #[must_use]
    pub fn with_worker_local_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker_local_credential_resolver = Some(resolver);
        self
    }

    /// Install dynamic, secret-free ACP capability evidence into the same
    /// Worker observation lifecycle as local credential liveness.
    #[must_use]
    pub fn with_acp_capability_observation_source(
        mut self,
        source: Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>,
    ) -> Self {
        self.acp_capability_observation_source = Some(source);
        self
    }

    /// Install the provider that realizes every Session-owned container on this
    /// Worker. `backend` is the stable backend identifier published in the
    /// derived Worker manifest (for example `awaken-cloud`).
    ///
    /// The provider's own capability evidence replaces deployment-derived
    /// capability inference. This is also the only downstream seam for
    /// substitution/no-bypass implementations; it does not add a credential port.
    #[must_use]
    pub fn with_session_container_provider(
        mut self,
        backend: impl Into<String>,
        provider: Arc<dyn awaken_runtime_host::ContainerEnvironmentProvider>,
    ) -> Self {
        self.session_container_provider = Some(InstalledSessionContainerProvider {
            backend: backend.into(),
            provider,
        });
        self
    }

    /// Install the one exact-generation MCP realization adapter used by remote
    /// Session commands. Downstream platforms implement the public Session port;
    /// the Worker retains no parallel gateway or credential-selection contract.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_runtime_host::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_attachment_realizer = Some(realizer);
        self
    }

    /// Derive both inference and Session-secret materializers from one pair of
    /// authoritative credential stores.
    #[must_use]
    pub fn with_credential_stores(
        mut self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        let materializer =
            awaken_runtime_host::PinnedCredentialMaterializer::new(credentials, secrets);
        self.materializer = Some(Arc::new(CredentialInferenceMaterializer::from_pinned(
            materializer.clone(),
        )));
        self.credential_materializer = Some(materializer);
        self.credential_inference_derived = true;
        self
    }

    /// Validate the immutable topology without registering or starting work.
    pub fn build(mut self) -> Result<WorkerNode, WorkerNodeBuildError> {
        if self.credential_probe_interval.is_zero()
            || self.credential_observation_ttl <= self.credential_probe_interval
        {
            return Err(WorkerNodeBuildError(
                "credential observation TTL must be greater than the non-zero probe interval"
                    .to_string(),
            ));
        }
        if self.upstream.base_url().trim().is_empty() {
            return Err(WorkerNodeBuildError(
                "Worker upstream URL must not be empty".to_string(),
            ));
        }
        if self
            .session_container_provider
            .as_ref()
            .is_some_and(|installed| installed.backend.trim().is_empty())
        {
            return Err(WorkerNodeBuildError(
                "Session container provider backend must not be empty".to_string(),
            ));
        }
        let credential_observation_resolver = self.worker_local_credential_resolver.clone();
        if let Some(resolver) = self.external_credential_resolver.take() {
            let Some(materializer) = self.credential_materializer.take() else {
                return Err(WorkerNodeBuildError(
                    "external credential resolver requires an installed credential materializer"
                        .to_string(),
                ));
            };
            let materializer = materializer.with_external_material_resolver(resolver);
            if self.credential_inference_derived {
                self.materializer = Some(Arc::new(CredentialInferenceMaterializer::from_pinned(
                    materializer.clone(),
                )));
            }
            self.credential_materializer = Some(materializer);
        }
        // A WorkerNode is, by definition, the database-less remote drain of the
        // Control Node's durable queue. Product deployment input may still carry
        // coordinator defaults; normalize those two process-role axes here so a
        // successfully registered Worker cannot report Ready without a claim pool.
        self.deployment.durable = true;
        self.deployment.disable_local_pool = false;
        let resource_support = ResourceManifestSupport::from(self.resources.as_ref());
        let manifest = match self.manifest {
            ManifestSelection::Selected(ManifestSource::Explicit(manifest)) => *manifest,
            ManifestSelection::Selected(ManifestSource::Standard {
                application_capabilities,
            }) => derive_standard_manifest(StandardManifestInputs {
                deployment: &self.deployment,
                materializer: self.materializer.as_deref(),
                credential_materializer: self
                    .credential_materializer
                    .as_ref()
                    .map(CredentialMaterializerSupport::from),
                worker_local_credentials: credential_observation_resolver.is_some(),
                sandbox_override: self.session_container_provider.as_ref().map(|installed| {
                    (
                        installed.provider.sandbox_capabilities(),
                        installed.backend.as_str(),
                    )
                }),
                resource_support,
                application_capabilities,
                config: &self.standard_manifest_config,
            }),
            ManifestSelection::Unset => {
                return Err(WorkerNodeBuildError(
                    "Worker manifest source must be selected".to_string(),
                ));
            }
            ManifestSelection::Conflict { first, second } => {
                return Err(WorkerNodeBuildError(format!(
                    "Worker manifest sources are mutually exclusive: selected {first} and {second}"
                )));
            }
        };
        validate_worker_manifest(&manifest)?;
        Ok(WorkerNode {
            upstream: self.upstream,
            manifest,
            deployment: self.deployment,
            application_factory: self.application_factory,
            application_gate: self.application_gate,
            materializer: self.materializer,
            credential_materializer: self.credential_materializer,
            credential_observation_resolver,
            acp_capability_observation_source: self.acp_capability_observation_source,
            session_container_provider: self.session_container_provider,
            mcp_attachment_realizer: self.mcp_attachment_realizer,
            web_search_providers: self.web_search_providers,
            resources: self.resources,
            admin_listen: self.admin_listen,
            graceful_drain: self.graceful_drain,
            credential_probe_interval: self.credential_probe_interval,
            credential_observation_ttl: self.credential_observation_ttl,
        })
    }
}

fn validate_worker_manifest(manifest: &WorkerManifest) -> Result<(), WorkerNodeBuildError> {
    if manifest.build_digest.trim().is_empty() {
        return Err(WorkerNodeBuildError(
            "Worker manifest build_digest must not be empty".to_string(),
        ));
    }
    if manifest.capacity.max_concurrent == 0 {
        return Err(WorkerNodeBuildError(
            "Worker manifest max_concurrent must be greater than zero".to_string(),
        ));
    }
    if !manifest.dispatch_contract.contains(1) || !manifest.runtime_protocol.contains(1) {
        return Err(WorkerNodeBuildError(
            "Worker manifest must support dispatch and runtime protocol version 1".to_string(),
        ));
    }
    manifest
        .fingerprint()
        .map_err(|error| WorkerNodeBuildError(error.to_string()))?;
    Ok(())
}

/// One assembled remote Worker lifecycle.
pub struct WorkerNode {
    upstream: WorkerUpstream,
    manifest: WorkerManifest,
    deployment: awaken_runtime_host::DeploymentConfig,
    application_factory: Option<RegisteredApplicationFactory>,
    application_gate: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    credential_materializer: Option<awaken_runtime_host::PinnedCredentialMaterializer>,
    credential_observation_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    mcp_attachment_realizer: Option<Arc<dyn awaken_runtime_host::McpAttachmentRealizer>>,
    web_search_providers: awaken_runtime_host::WebSearchProviderRegistry,
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

struct InstalledSessionContainerProvider {
    backend: String,
    provider: Arc<dyn awaken_runtime_host::ContainerEnvironmentProvider>,
}

/// Run a Worker from the product command's already-resolved deployment and
/// store configuration. This path performs no deployment rediscovery.
pub async fn run_with_config(
    upstream: &str,
    deployment: awaken_runtime_host::DeploymentConfig,
    control: awaken_control::ControlStoreConfig,
    resource_url: Option<String>,
    seal_key: [u8; 32],
    options: WorkerRunOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let repository_credentials = matches!(
        &control.credential,
        awaken_control::StoreBackend::Postgres(_)
    );
    let process = WorkerProcessConfig {
        deployment,
        manifest: options.manifest,
        admin_listen: options.admin_listen,
        graceful_drain: options.drain_grace,
        credential_probe_interval: options.credential_probe_interval,
        credential_observation_ttl: options.credential_observation_ttl,
        repository_credentials,
    };
    let stores = awaken_control::open_inference_materialization_stores(&control, &seal_key).await;
    let resource_credentials = process.repository_credentials.then(|| stores.clone());
    let resources = shared_resource_wiring(
        resource_credentials,
        resource_url.as_deref(),
        Some(&control.admin),
    )
    .await?;
    let mut builder =
        WorkerNodeBuilder::new(WorkerUpstream::new(upstream).with_worker_id(options.worker_id))
            .with_process_config(process)
            .with_credential_stores(stores.credentials, stores.secrets)
            .with_standard_manifest(Default::default());
    if let Some(resources) = resources {
        builder = builder.with_resource_plane(resources);
    }
    builder.build()?.run_until_shutdown().await
}

/// Run a genuinely secretless worker with a deployment-provided materializer.
/// It receives each durable run's snapshot-pinned inference access and may
/// realize an executor through a remote broker without opening a credential
/// vault or persisting provider keys in this process.
pub async fn run_with_inference_materializer(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(),
        materializer,
        None,
        None,
        None,
    )
    .await?
    .run_until_shutdown()
    .await
}

/// Run a secretless Worker whose one authoritative adapter supplies both exact
/// executor materialization and typed Worker-local credential observations.
pub async fn run_with_inference_and_credential_resolver<R>(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    resolver: Arc<R>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: awaken_runtime_contract::CredentialMaterialResolver
        + awaken_runtime_contract::WorkerLocalCredentialResolver
        + 'static,
{
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(),
        materializer,
        Some(resolver.clone()),
        Some(resolver),
        None,
    )
    .await?
    .run_until_shutdown()
    .await
}

/// Embedded secretless Worker with an explicitly resolved deployment and shared
/// ResourcePlane. This extends the same authoritative builder used above; it
/// performs no environment/config rediscovery and installs no second resolver.
pub async fn run_with_inference_and_credential_resolver_and_resources<R>(
    upstream: &str,
    deployment: awaken_runtime_host::DeploymentConfig,
    resource_url: &str,
    admin_backend: &awaken_control::StoreBackend,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    resolver: Arc<R>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: awaken_runtime_contract::CredentialMaterialResolver
        + awaken_runtime_contract::WorkerLocalCredentialResolver
        + 'static,
{
    let resources = shared_resource_wiring(None, Some(resource_url), Some(admin_backend)).await?;
    let mut process = WorkerProcessConfig::embedded_defaults();
    process.deployment = deployment;
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        process,
        materializer,
        Some(resolver.clone()),
        Some(resolver),
        resources,
    )
    .await?
    .run_until_shutdown()
    .await
}

/// Run a secretless worker with a caller-provided shutdown source.
///
/// Embedders and deterministic process tests use this seam when platform process
/// signals cannot express a graceful stop (notably Node child processes on
/// Windows). Production binaries should normally use
/// [`run_with_inference_materializer`].
pub async fn run_with_inference_materializer_until<F>(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    shutdown: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: std::future::Future<
            Output = Result<WorkerShutdown, Box<dyn std::error::Error + Send + Sync>>,
        >,
{
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(),
        materializer,
        None,
        None,
        None,
    )
    .await?
    .run_until(shutdown)
    .await
}

async fn build_secretless_worker(
    upstream: WorkerUpstream,
    process: WorkerProcessConfig,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    material_resolver: Option<Arc<dyn awaken_runtime_contract::CredentialMaterialResolver>>,
    local_resolver: Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    resources: Option<WorkerResourcePlane>,
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = WorkerNodeBuilder::new(upstream).with_process_config(process);
    if let Some(resolver) = material_resolver {
        builder = builder
            .with_credential_stores(
                Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
                Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            )
            .with_external_credential_resolver(resolver);
    }
    if let Some(resolver) = local_resolver {
        builder = builder.with_worker_local_credential_resolver(resolver);
    }
    builder = builder
        .with_inference_materializer(materializer)
        .with_standard_manifest(Default::default());
    if let Some(resources) = resources {
        builder = builder.with_resource_plane(resources);
    }
    Ok(builder.build()?)
}

impl WorkerNode {
    #[must_use]
    pub fn manifest(&self) -> &WorkerManifest {
        &self.manifest
    }

    /// Register, enter Ready, run until SIGINT/SIGTERM, then drain, quiesce, and
    /// deregister. Losing registry authority closes the local claim gate.
    pub async fn run_until_shutdown(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if std::env::var("AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF").as_deref() == Ok("1") {
            return self
                .run_until(async {
                    use tokio::io::{AsyncReadExt, stdin};

                    let mut byte = [0_u8; 1];
                    let _ = stdin().read(&mut byte).await;
                    Ok(WorkerShutdown::Graceful)
                })
                .await;
        }
        self.run_until(async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate())?;
                let mode = tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        result?;
                        WorkerShutdown::Prompt
                    },
                    _ = term.recv() => WorkerShutdown::Graceful,
                };
                Ok(mode)
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await?;
                Ok(WorkerShutdown::Prompt)
            }
        })
        .await
    }

    /// Run the same lifecycle with an injected shutdown source. This keeps
    /// embedding tests and supervisors independent of process signals.
    pub async fn run_until<F>(
        self,
        shutdown: F,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        F: std::future::Future<
                Output = Result<WorkerShutdown, Box<dyn std::error::Error + Send + Sync>>,
            >,
    {
        let upstream_url = self.upstream.base_url().to_string();
        let upstream = self.upstream;
        let bootstrap_control = WorkerControlClient::new(upstream.clone());
        let registration = bootstrap_control
            .register(new_incarnation_id()?, self.manifest)
            .await
            .map_err(std::io::Error::other)?;
        let upstream = upstream.with_worker_identity(registration.snapshot.identity.clone());
        let control = WorkerControlClient::new(upstream.clone());
        let application = match self.application_factory {
            Some(factory) => match factory(&RegisteredWorkerContext::new(
                registration.clone(),
                upstream.clone(),
            )) {
                Ok(application) => Some(application),
                Err(error) => {
                    let _ = control.deregister(&registration.snapshot.identity).await;
                    return Err(std::io::Error::other(format!(
                        "registered application factory failed: {error}"
                    ))
                    .into());
                }
            },
            None => None,
        };
        let (application_decorator, application_provisioner) = application
            .map(RegisteredWorkerApplication::into_parts)
            .map_or((None, None), |(decorator, provisioner)| {
                (Some(decorator), provisioner)
            });
        // Route the dispatch pool's claim/settle over HTTP to the cell server.
        let dispatch_store = awaken_runtime_host::worker_dispatch_store_with_upstream(
            &upstream,
            registration.snapshot.identity.clone(),
        );

        let resource_validator = self
            .resources
            .as_ref()
            .map(|resources| resources.validator.clone());
        let managed_credential_materializer = self.credential_materializer.clone();
        let host = match self.resources {
            Some(resources) => SharedHost::new_with_resource_plane_and_deployment(
                Arc::new(NoModelConfiguredExecutor),
                "worker",
                resources.ports,
                self.deployment,
            ),
            None => SharedHost::new_with_deployment(
                Arc::new(NoModelConfiguredExecutor),
                "worker",
                self.deployment,
            ),
        };
        let mut host = host
            .with_worker_upstream(upstream)
            .with_dispatch_store(dispatch_store)
            .with_web_search_provider_registry(self.web_search_providers)
            .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(
                managed_credential_materializer.clone(),
            ));
        if let Some(materializer) = &self.materializer {
            host = host.with_inference_materializer(materializer.clone());
        }
        if let Some(resolver) = &self.credential_observation_resolver {
            host = host.with_worker_credential_resolver(resolver.clone());
        }
        if let Some(decorator) = application_decorator {
            host = host.with_application_attempt_decorator(decorator);
        }
        if let Some(provisioner) = application_provisioner {
            host = host
                .with_application_session_provisioner(provisioner)
                .with_application_session_control(Arc::new(
                    awaken_runtime_host::WorkerControlApplicationSessionClient::new(
                        control.clone(),
                        registration.snapshot.identity.clone(),
                    ),
                ));
        }
        if let Some(gate) = self.application_gate {
            host = host.with_gate_override(gate);
        }
        awaken_server::install_platform_memory_data_plane(&host);

        if let Some(installed) = self.session_container_provider {
            host = host.with_session_container_provider(
                installed.provider,
                awaken_server::relay_hand_executor_factory(),
            );
        }
        // Serve only the ACP CLI capability this worker advertises. The run's snapshot
        // selects the matching backend and supplies its published provider access.
        host = host
            .with_acp_from_deployment(
                awaken_server::relay_hand_executor_factory(),
                self.credential_materializer,
            )
            .await;

        let host = Arc::new(host);
        // Install one dispatch-facing Session adapter even when no Resource
        // validator is present: MCP hot attachment commands have an independent
        // lifecycle and must not be enabled accidentally by Resource wiring.
        let mut managed = awaken_server::ManagedHost::new(host.clone());
        if let Some(validator) = resource_validator {
            managed = managed.with_resource_validator(validator);
        }
        if let Some(credentials) = managed_credential_materializer {
            managed = managed.with_credential_materializer(credentials);
        }
        if let Some(realizer) = self.mcp_attachment_realizer {
            managed = managed.with_mcp_attachment_realizer(realizer);
        }
        drop(managed);
        let observations = Arc::new(WorkerObservationCache::default());
        let lifecycle = Arc::new(WorkerLifecycle {
            host: host.clone(),
            control: control.clone(),
            identity: registration.snapshot.identity,
            credential_observation_resolver: self.credential_observation_resolver,
            acp_capability_observation_source: self.acp_capability_observation_source,
            observations,
            observation_ttl: self.credential_observation_ttl,
        });
        // Publish Ready before starting the pull loop. Starting the pool while the
        // directory still says Starting creates a tight claim/reject race; publishing
        // first is safe because any assignment remains queued until this process starts
        // polling immediately below.
        if let Err(error) = lifecycle
            .observations
            .refresh(
                lifecycle.credential_observation_resolver.as_deref(),
                lifecycle.acp_capability_observation_source.as_deref(),
                wall_clock_ms(),
                self.credential_observation_ttl,
            )
            .await
        {
            eprintln!("worker_observation_probe_failed: {error}; publishing no dynamic evidence");
        }
        let initial = control
            .heartbeat(
                &lifecycle.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    credential_observations: lifecycle.observations.credential_snapshot(),
                    acp_capability_observations: lifecycle.observations.acp_capability_snapshot(),
                },
            )
            .await;
        let initial = match initial {
            Ok(initial) => initial,
            Err(error) => {
                let _ = control.deregister(&lifecycle.identity).await;
                return Err(std::io::Error::other(error).into());
            }
        };
        if initial != RegistryMutation::Applied {
            let _ = control.deregister(&lifecycle.identity).await;
            return Err(std::io::Error::other(format!(
                "initial worker heartbeat rejected: {initial:?}"
            ))
            .into());
        }
        host.ensure_dispatch_pool();
        let credential_probe = credential_liveness::spawn_probe(
            lifecycle.observations.clone(),
            lifecycle.credential_observation_resolver.clone(),
            lifecycle.acp_capability_observation_source.clone(),
            self.credential_probe_interval,
            self.credential_observation_ttl,
        );
        let heartbeat = spawn_heartbeat(lifecycle.clone(), 2);
        eprintln!("awaken-worker draining from {upstream_url}");

        // The cloud-native admin surface on a SEPARATE port from any data path: an
        // orchestrator gates routing on `/readyz` and calls `POST /admin/drain` in a
        // `preStop` hook before SIGTERM. Best-effort — a bind failure is logged but does
        // not stop the worker draining runs (the core job).
        let mut admin_task = None;
        if let Some(admin_addr) = self.admin_listen {
            match tokio::net::TcpListener::bind(&admin_addr).await {
                Ok(listener) => {
                    let router = admin::worker_admin_router_with_lifecycle(lifecycle.clone());
                    eprintln!(
                        "awaken-worker admin surface on {admin_addr} (/readyz /metrics /admin/drain)"
                    );
                    admin_task = Some(tokio::spawn(async move {
                        if let Err(err) = axum::serve(listener, router).await {
                            eprintln!("awaken-worker admin server exited: {err}");
                        }
                    }));
                }
                Err(err) => {
                    eprintln!("awaken-worker admin surface disabled (bind {admin_addr}: {err})")
                }
            }
        }

        let shutdown = shutdown.await;
        let graceful = shutdown
            .as_ref()
            .is_ok_and(|mode| *mode == WorkerShutdown::Graceful);

        // Stop claiming immediately so no NEW run is taken; the in-flight ones finish
        // within the grace window before the process exits.
        let grace = if graceful {
            self.graceful_drain
        } else {
            std::time::Duration::ZERO
        };
        let deadline_ms = wall_clock_ms().saturating_add(grace.as_millis() as u64);
        if let Err(error) = lifecycle.begin_drain(Some(deadline_ms)).await {
            eprintln!("awaken-worker drain registration failed closed: {error}");
        }
        if !grace.is_zero() {
            eprintln!(
                "awaken-worker draining: finishing in-flight runs (≤{}s)",
                grace.as_secs()
            );
            wait_for_in_flight(&host, grace).await;
        }
        heartbeat.abort();
        credential_probe.abort();
        if let Some(admin_task) = admin_task {
            admin_task.abort();
        }
        if host.pool_in_flight() == 0 {
            let _ = control.mark_quiesced(&lifecycle.identity).await;
        }
        let _ = control.deregister(&lifecycle.identity).await;
        shutdown?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
