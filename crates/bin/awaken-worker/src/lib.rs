//! `awaken-worker` — the production authority-store-isolated executor.
//!
//! A peer of the control and data planes. It owns no Run, Session, Resource, or
//! authoring store: it drains runs from a Coordinator cell over the typed dispatch
//! transport and sends claim-fenced commit operations back through the same Worker
//! boundary. Exact Worker-private credentials enter only through an injected
//! material resolver such as [`WorkerCredentialFileResolver`]. Resource capability
//! is advertised only after per-kind remote adapters are installed.
//!
//! **Real per-run model resolution, no mocks.** A drained run arrives as a
//! `RunActivation` carrying its own `ExecutableAgentSnapshot`, whose
//! `resolved_spec.model_binding.model_ref` is the run's model identity. The host's
//! run loop resolves that ref through the injected [`InferenceExecutorMaterializer`]
//! which consumes the snapshot-pinned endpoint and credential reference through
//! the injected [`InferenceExecutorMaterializer`]. The host's inert executor
//! is only an inert construction placeholder: because the materializer is installed,
//! an unavailable publication pin fails closed before that executor can run.

use std::sync::Arc;

mod acp_capability;
mod admin;
mod application;
mod bootstrap;
mod credential_files;
mod credential_liveness;
mod lifecycle;
mod manifest;
mod relay_hand;

use credential_liveness::WorkerObservationCache;
use lifecycle::{
    WorkerSupervisor, grace_window, new_incarnation_id, spawn_environment_warmup_reconciliation,
    spawn_heartbeat, wait_for_in_flight, wall_clock_ms,
};
use manifest::{
    CredentialMaterializerSupport, ManifestSelection, ManifestSource, ResourceManifestSupport,
    StandardManifestInputs, derive_standard_manifest,
};

pub use application::{
    RegisteredApplicationFactory, RegisteredWorkerApplication, RegisteredWorkerContext,
};
pub use bootstrap::{WorkerBootstrap, WorkerBootstrapInput, WorkerDaemonConfig};
pub use credential_files::WorkerCredentialFileResolver;
pub use lifecycle::WorkerShutdown;
pub use manifest::StandardManifestConfig;
pub use relay_hand::relay_hand_executor_factory;

/// Registration-bound construction of the exact Memory projection adapter.
/// The assigned Worker identity is required to authenticate every claim-fenced
/// snapshot and CAS request.
pub type RegisteredMemoryMounterFactory = Arc<
    dyn Fn(
            &RegisteredWorkerContext,
        ) -> Result<Arc<dyn awaken_provisioning_contract::MemoryMounter>, String>
        + Send
        + Sync,
>;

use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_host::SharedHost;
use awaken_worker_contract::{RegistryMutation, WorkerHeartbeat, WorkerManifest};
use awaken_worker_runtime::WorkerControlClient;
use awaken_worker_transport_security::WorkerUpstream;

struct WorkerProcessConfig {
    deployment: awaken_runtime_host::DeploymentConfig,
    manifest: StandardManifestConfig,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
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
        }
    }
}

/// Invalid explicit Worker startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerNodeBuildError(String);

impl std::fmt::Display for WorkerNodeBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WorkerNodeBuildError {}

/// Public assembly boundary for the canonical Worker component.
///
/// `WorkerNodeBuilder` is intentionally the only Worker component builder;
/// AllInOne and split Worker process adapters install different transports and
/// application ports on this same path.
pub struct WorkerNodeBuilder {
    upstream: WorkerUpstream,
    manifest: ManifestSelection,
    deployment: awaken_runtime_host::DeploymentConfig,
    standard_manifest_config: StandardManifestConfig,
    application_factory: Option<RegisteredApplicationFactory>,
    application_gate: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    credential_materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    remote_attempt: Option<awaken_runtime_host::RemoteAttemptInstallation>,
    hand_executor_factory: Option<Arc<dyn awaken_runtime_host::HandExecutorFactory>>,
    worker_local_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    enclosing_sandbox_boundary: Option<InstalledSandboxBoundary>,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    mcp_attachment_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    memory_mounter_factory: Option<RegisteredMemoryMounterFactory>,
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
            remote_attempt: None,
            hand_executor_factory: None,
            worker_local_credential_resolver: None,
            acp_capability_observation_source: None,
            enclosing_sandbox_boundary: None,
            session_container_provider: None,
            mcp_attachment_realizer: None,
            web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            memory_mounter_factory: None,
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
        self
    }

    /// Install the already-configured remote-attempt adapter. A2A construction and
    /// transport credentials remain at the outer process startup.
    #[must_use]
    pub fn with_remote_attempt_executor(
        mut self,
        installation: awaken_runtime_host::RemoteAttemptInstallation,
    ) -> Self {
        self.remote_attempt = Some(installation);
        self
    }

    /// Install the existing hand-channel adapter used by container and ACP
    /// environments. The Worker owns lifecycle, not relay construction.
    #[must_use]
    pub fn with_hand_executor_factory(
        mut self,
        factory: Arc<dyn awaken_runtime_host::HandExecutorFactory>,
    ) -> Self {
        self.hand_executor_factory = Some(factory);
        self
    }

    /// Replace the deployment's WebSearch provider catalog. Management
    /// publication and Worker execution must receive registries assembled from
    /// the same provider definitions; the immutable snapshot remains the
    /// selection authority for each run.
    #[must_use]
    pub fn with_web_search_provider_registry(
        mut self,
        providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    ) -> Self {
        self.web_search_providers = providers;
        self
    }

    /// Install the Memory-specific adapter factory evaluated only after the
    /// Coordinator has assigned this Worker incarnation its transport identity.
    #[must_use]
    pub fn with_registered_memory_mounter_factory(
        mut self,
        factory: RegisteredMemoryMounterFactory,
    ) -> Self {
        self.memory_mounter_factory = Some(factory);
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
        credentials: awaken_credential_materializer::PinnedCredentialMaterializer,
    ) -> Self {
        self.credential_materializer = Some(credentials);
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
        self,
        backend: impl Into<String>,
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
    ) -> Self {
        self.with_session_container_provider_and_capacity(backend, provider, None)
    }

    /// Install a Session container provider together with the capacity lifecycle
    /// produced by the same downstream startup.
    #[must_use]
    pub fn with_session_container_provider_and_capacity(
        mut self,
        backend: impl Into<String>,
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
    ) -> Self {
        self.session_container_provider = Some(InstalledSessionContainerProvider {
            backend: backend.into(),
            provider,
            capacity,
        });
        self
    }

    /// Publish an isolation boundary enforced around this Worker process.
    ///
    /// Platform deployments use this when the Worker already runs inside a
    /// sandboxed Pod/VM and executes children locally within that boundary. It
    /// affects standard manifest evidence only; it does not install a second
    /// Session container provider or change runtime realization.
    #[must_use]
    pub fn with_enclosing_sandbox_boundary(
        mut self,
        backend: impl Into<String>,
        capabilities: awaken_provisioning_contract::SandboxCapabilities,
    ) -> Self {
        self.enclosing_sandbox_boundary = Some(InstalledSandboxBoundary {
            backend: backend.into(),
            capabilities,
        });
        self
    }

    /// Install the one exact-generation MCP realization adapter used by remote
    /// Session commands. Downstream platforms implement the public Session port;
    /// the Worker retains no parallel gateway or credential-selection contract.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_attachment_realizer = Some(realizer);
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
        if self
            .enclosing_sandbox_boundary
            .as_ref()
            .is_some_and(|installed| installed.backend.trim().is_empty())
        {
            return Err(WorkerNodeBuildError(
                "enclosing sandbox backend must not be empty".to_string(),
            ));
        }
        if self.session_container_provider.is_some() && self.enclosing_sandbox_boundary.is_some() {
            return Err(WorkerNodeBuildError(
                "Session container provider and enclosing sandbox boundary are mutually exclusive"
                    .to_string(),
            ));
        }
        if (self.session_container_provider.is_some()
            || self.deployment.sandbox_tier.is_container())
            && self.hand_executor_factory.is_none()
        {
            return Err(WorkerNodeBuildError(
                "container execution requires an installed hand executor factory".to_string(),
            ));
        }
        let credential_observation_resolver = self.worker_local_credential_resolver.clone();
        // `disable_local_pool` names the Serve process's co-located executor pool.
        // A WorkerNode instead owns one mandatory registered remote claim pool, so
        // normalize the host-level switch here. The CLI rejects run_local_pool for
        // Role::Worker rather than giving this field two public meanings.
        self.deployment.durable = true;
        self.deployment.disable_local_pool = false;
        let resource_support = ResourceManifestSupport::from((
            self.memory_mounter_factory.is_some(),
            self.credential_materializer.as_ref(),
        ));
        let sandbox_override = self
            .session_container_provider
            .as_ref()
            .map(|installed| {
                (
                    installed.provider.sandbox_capabilities(),
                    installed.backend.as_str(),
                )
            })
            .or_else(|| {
                self.enclosing_sandbox_boundary
                    .as_ref()
                    .map(|installed| (installed.capabilities.clone(), installed.backend.as_str()))
            });
        let manifest = match self.manifest {
            ManifestSelection::Selected(ManifestSource::Explicit(manifest)) => *manifest,
            ManifestSelection::Selected(ManifestSource::Standard {
                mut application_capabilities,
            }) => {
                if self.remote_attempt.is_some() {
                    application_capabilities
                        .insert(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY.to_string());
                }
                derive_standard_manifest(StandardManifestInputs {
                    deployment: &self.deployment,
                    materializer: self.materializer.as_deref(),
                    credential_materializer: self
                        .credential_materializer
                        .as_ref()
                        .map(CredentialMaterializerSupport::from),
                    worker_local_credential_resolver_installed: credential_observation_resolver
                        .is_some(),
                    remote_credential_realization: self
                        .remote_attempt
                        .as_ref()
                        .map(|remote| &remote.credential_realization),
                    sandbox_override,
                    resource_support,
                    application_capabilities,
                    config: &self.standard_manifest_config,
                })
            }
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
            remote_attempt: self.remote_attempt,
            hand_executor_factory: self.hand_executor_factory,
            credential_observation_resolver,
            acp_capability_observation_source: self.acp_capability_observation_source,
            session_container_provider: self.session_container_provider,
            mcp_attachment_realizer: self.mcp_attachment_realizer,
            web_search_providers: self.web_search_providers,
            memory_mounter_factory: self.memory_mounter_factory,
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
    credential_materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    remote_attempt: Option<awaken_runtime_host::RemoteAttemptInstallation>,
    hand_executor_factory: Option<Arc<dyn awaken_runtime_host::HandExecutorFactory>>,
    credential_observation_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    mcp_attachment_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    memory_mounter_factory: Option<RegisteredMemoryMounterFactory>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

struct InstalledSessionContainerProvider {
    backend: String,
    provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
    capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
}

struct InstalledSandboxBoundary {
    backend: String,
    capabilities: awaken_provisioning_contract::SandboxCapabilities,
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
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = WorkerNodeBuilder::new(upstream).with_process_config(process);
    if let Some(resolver) = material_resolver {
        builder = builder.with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(resolver),
        );
    }
    if let Some(resolver) = local_resolver {
        builder = builder.with_worker_local_credential_resolver(resolver);
    }
    builder = builder
        .with_inference_materializer(materializer)
        .with_standard_manifest(Default::default());
    Ok(builder.build()?)
}

fn configured_container_acp_targets(
    deployment: &awaken_runtime_host::DeploymentConfig,
) -> Result<Vec<acp_capability::ConfiguredAcpCapabilityTarget>, String> {
    if !matches!(
        deployment.sandbox_tier,
        awaken_runtime_host::SandboxTier::Docker
            | awaken_runtime_host::SandboxTier::Podman
            | awaken_runtime_host::SandboxTier::K8s
    ) {
        return Ok(Vec::new());
    }
    let Some(profile) = deployment.acp.as_ref() else {
        return Ok(Vec::new());
    };
    let image = deployment
        .container_image
        .as_deref()
        .filter(|image| !image.trim().is_empty())
        .ok_or_else(|| {
            "configured container ACP capability requires an image identity".to_string()
        })?;
    profile
        .cli_ids()
        .map(|id| {
            let cli = awaken_run_executor_acp::acp_cli(id)
                .ok_or_else(|| format!("configured ACP capability has unknown adapter `{id}`"))?;
            acp_capability::ConfiguredAcpCapabilityTarget::new(
                id,
                format!("container-image:{image}"),
                cli.container_probe_argv
                    .unwrap_or(cli.container_argv)
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect(),
                cli.capability_probe_auth_method_id.map(str::to_string),
            )
        })
        .collect()
}

fn configured_container_acp_capability_source(
    deployment: &awaken_runtime_host::DeploymentConfig,
    host: Arc<SharedHost>,
) -> Result<Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>, String> {
    let targets = configured_container_acp_targets(deployment)?;
    if targets.is_empty() {
        return Ok(None);
    }
    let negotiator = Arc::new(awaken_runtime_host::SessionAcpCapabilityNegotiator::new(
        host,
        // Some native adapters perform image-local plugin discovery before
        // opening their first prompt-free Session. Keep the probe bounded while
        // allowing that deterministic cold start to complete.
        std::time::Duration::from_secs(30),
        Arc::new(awaken_protocol_acp::ProtocolAcpCapabilityHandshake),
    ));
    Ok(Some(Arc::new(
        acp_capability::ConfiguredAcpCapabilityObservationSource::new(
            targets,
            negotiator,
            std::path::PathBuf::from("/workspace"),
        ),
    )))
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
        let deployment = self.deployment.clone();
        let upstream_url = self.upstream.base_url().to_string();
        let upstream = self.upstream;
        let bootstrap_control = WorkerControlClient::new(upstream.clone());
        let incarnation_id = new_incarnation_id().map_err(|error| {
            std::io::Error::other(format!("generate Worker incarnation: {error}"))
        })?;
        let mut shutdown = std::pin::pin!(shutdown);
        let mut occupied_attempts = 0_u64;
        let registration = loop {
            let attempt = bootstrap_control
                .register_classified(incarnation_id.clone(), self.manifest.clone())
                .await;
            match attempt {
                Ok(registration) => break registration,
                Err(awaken_worker_runtime::WorkerRegistrationError::SlotOccupied(error)) => {
                    if occupied_attempts.is_multiple_of(10) {
                        eprintln!("worker registration waiting for the prior lease: {error}");
                    }
                    occupied_attempts = occupied_attempts.saturating_add(1);
                    tokio::select! {
                        () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        shutdown = &mut shutdown => {
                            shutdown?;
                            return Ok(());
                        }
                    }
                }
                Err(error) => return Err(std::io::Error::other(error.to_string()).into()),
            }
        };
        let upstream = upstream.with_worker_identity(registration.snapshot.identity.clone());
        let control = WorkerControlClient::new(upstream.clone());
        let registered_context =
            RegisteredWorkerContext::new(registration.clone(), upstream.clone());
        let application = match self.application_factory {
            Some(factory) => match factory(&registered_context) {
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
        let memory_mounter = match self.memory_mounter_factory {
            Some(factory) => match factory(&registered_context) {
                Ok(mounter) => Some(mounter),
                Err(error) => {
                    let _ = control.deregister(&registration.snapshot.identity).await;
                    return Err(std::io::Error::other(format!(
                        "registered Memory mounter factory failed: {error}"
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
        let (dispatch, stream_publisher) = awaken_worker_runtime::worker_transports_with_upstream(
            &upstream,
            registration.snapshot.identity.clone(),
        );

        let managed_credential_materializer = self.credential_materializer.clone();
        let remote_memory = Arc::new(awaken_resource_worker_http::HttpMemoryRepository::new(
            upstream.clone(),
        ));
        let remote_files = Arc::new(awaken_resource_worker_http::HttpFileContentSource::new(
            upstream.clone(),
        ));
        let remote_artifacts = Arc::new(awaken_resource_worker_http::HttpArtifactPublisher::new(
            upstream.clone(),
        ));
        let remote_skills = Arc::new(awaken_resource_worker_http::HttpSkillBundleSource::new(
            upstream.clone(),
        ));
        let remote_repositories = Arc::new(
            awaken_resource_worker_http::HttpRepositoryBindingVerifier::new(upstream.clone()),
        );
        let mut host = SharedHost::new_worker_with_deployment(
            Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            "worker",
            remote_files,
            remote_memory,
            self.deployment,
        )
        .with_artifact_publisher(remote_artifacts)
        .with_worker_upstream(upstream)
        .with_memory_reference_encoder(Arc::new(
            awaken_resource_worker_http::HttpMemoryMaterializationReferenceEncoder,
        ))
        .with_worker_stream_publisher(stream_publisher)
        .with_worker_dispatch(dispatch)
        .with_skill_bundle_source(remote_skills)
        .with_web_search_provider_registry(self.web_search_providers);
        if let Some(remote_attempt) = self.remote_attempt {
            host = host.with_remote_attempt_executor(remote_attempt);
        }
        if let Some(materializer) = &self.materializer {
            host = host.with_inference_materializer(materializer.clone());
        }
        if let Some(resolver) = &self.credential_observation_resolver {
            host = host.with_worker_credential_resolver(resolver.clone());
        }
        if let Some(decorator) = application_decorator {
            host = host.with_application_attempt_decorator(decorator);
        }
        // Every registered Worker realizes the frozen Session projection through
        // Control. Application contribution is optional; realization ownership
        // is not. Wiring the client only with a provisioner created a second,
        // unfenced ordinary-Worker path.
        host = host.with_application_session_control(Arc::new(
            awaken_worker_runtime::WorkerControlApplicationSessionClient::new(
                control.clone(),
                registration.snapshot.identity.clone(),
            ),
        ));
        if let Some(provisioner) = application_provisioner {
            host = host.with_application_session_provisioner(provisioner);
        }
        if let Some(gate) = self.application_gate {
            host = host.with_gate_override(gate);
        }
        if let Some(memory_mounter) = memory_mounter {
            host.install_memory_mounter(memory_mounter);
        }

        if let Some(installed) = self.session_container_provider {
            let hand_factory = self
                .hand_executor_factory
                .clone()
                .expect("container provider was validated with a hand factory");
            host = host.with_session_container_provider_and_capacity(
                installed.provider,
                installed.capacity,
                hand_factory,
            );
        }
        // Serve only the ACP CLI capability this worker advertises. The run's snapshot
        // selects the matching backend and supplies its published provider access.
        host = host
            .with_session_environment_from_deployment(self.hand_executor_factory)
            .await
            .with_acp_from_deployment(self.credential_materializer)
            .await;

        let host = Arc::new(host);
        let acp_capability_observation_source = match self.acp_capability_observation_source {
            Some(source) => Some(source),
            None => configured_container_acp_capability_source(&deployment, host.clone())
                .map_err(std::io::Error::other)?,
        };
        // Install one dispatch-facing Session adapter even when no Resource
        // validator is present: MCP hot attachment commands have an independent
        // lifecycle and must not be enabled accidentally by Resource wiring.
        let mut managed = awaken_runtime_host::ManagedHost::new(host.clone())
            .with_repository_binding_verifier(remote_repositories);
        if let Some(credentials) = managed_credential_materializer {
            managed = managed.with_credential_materializer(credentials);
        }
        if let Some(realizer) = self.mcp_attachment_realizer {
            managed = managed.with_mcp_attachment_realizer(realizer);
        }
        managed = managed.install_dispatch_session_runtime();
        drop(managed);
        let observations = Arc::new(WorkerObservationCache::default());
        let lifecycle = Arc::new(WorkerSupervisor {
            host: host.clone(),
            control: control.clone(),
            identity: registration.snapshot.identity,
            credential_observation_resolver: self.credential_observation_resolver,
            acp_capability_observation_source,
            observations,
            observation_ttl: self.credential_observation_ttl,
            warm_environments: Default::default(),
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
        match lifecycle.reconcile_environment_warmups().await {
            Ok(ready) if ready > 0 => {
                eprintln!("awaken-worker reconciled {ready} current Environment shapes")
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "initial Environment warmup reconciliation failed; cold path retained: {error}"
            ),
        }
        let initial = control
            .heartbeat(
                &lifecycle.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: lifecycle.warm_environment_shapes(),
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
        let environment_warmups = spawn_environment_warmup_reconciliation(lifecycle.clone());
        let mut heartbeat = spawn_heartbeat(lifecycle.clone(), 2);
        eprintln!("awaken-worker registered with {upstream_url}");

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

        // Authority loss is an irreversible boundary for this incarnation. The
        // heartbeat task has already fenced local claim admission; terminate the
        // process after the ordinary drain cleanup so Kubernetes/systemd can start
        // a newly registered incarnation. Reusing this process would make a stale
        // epoch capable of silently becoming authoritative again.
        let (shutdown, authority_lost) = tokio::select! {
            shutdown = &mut shutdown => (shutdown, false),
            heartbeat = &mut heartbeat => {
                if let Err(error) = heartbeat {
                    eprintln!("worker heartbeat task failed: {error}; terminating incarnation");
                }
                (Ok(WorkerShutdown::Graceful), true)
            }
        };
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
        environment_warmups.abort();
        if let Some(admin_task) = admin_task {
            admin_task.abort();
        }
        host.shutdown_environment_capacity().await;
        if host.pool_in_flight() == 0 {
            let _ = control.mark_quiesced(&lifecycle.identity).await;
        }
        let _ = control.deregister(&lifecycle.identity).await;
        shutdown?;
        if authority_lost {
            return Err(std::io::Error::other(
                "worker lost registry authority; supervisor restart required",
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
