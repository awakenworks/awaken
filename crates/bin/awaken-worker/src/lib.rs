// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim
// @formal entrypoint worker.realization_scope_uses_only_the_kubernetes_namespace_or_persisted_local_workspace
// @formal entrypoint worker.sandbox_tool_recovery_is_a_hard_claim_axis
// @formal entrypoint worker_authority.independent_evidence_never_extends_expired_proof
// @formal entrypoint worker_credential.heartbeat_observations_are_one_atomic_batch
// @formal entrypoint worker_credential.probe_failure_does_not_drain_worker
// @formal entrypoint worker_credential.publication_budget_covers_probe_and_delivery
// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
// @formal evidence worker_drain.registry_transition_follows_local_fence
// awaken-allow(file-limit-ceiling): 1651
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

use futures_util::FutureExt as _;
use std::sync::Arc;

#[cfg(feature = "container-k8s")]
async fn request_registered_kubernetes_pod_replacement(worker_id: &str) -> Result<(), String> {
    use awaken_sandbox_kubernetes::{PodProcessObserver, PodProcessStopper};
    use awaken_worker_transport_security::kubernetes::KubernetesPodIdentity;

    let pod = KubernetesPodIdentity::from_worker_id(worker_id)?;
    let client = awaken_sandbox_kubernetes::connect_control_client()
        .await
        .map_err(|error| error.to_string())?;
    let observer =
        PodProcessObserver::new(client, pod.namespace()).map_err(|error| error.to_string())?;
    PodProcessStopper::new(observer)
        .request_registered_pod_replacement(pod.name(), pod.uid())
        .await
        .map_err(|error| error.to_string())
}

#[cfg(feature = "container-k8s")]
async fn await_registered_kubernetes_pod_replacement(worker_id: &str) {
    let mut attempts = 0_u64;
    loop {
        match request_registered_kubernetes_pod_replacement(worker_id).await {
            Ok(()) => break,
            Err(error) => {
                if attempts.is_multiple_of(10) {
                    eprintln!("worker Pod replacement intent pending: {error}");
                }
                attempts = attempts.saturating_add(1);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

mod acp_capability;
mod admin;
mod attempt_decorator;
mod bootstrap;
mod credential_files;
mod credential_liveness;
#[cfg(feature = "container-k8s")]
mod k8s_create_client;
mod lifecycle;
mod manifest;
mod relay_hand;

use credential_liveness::WorkerObservationCache;
use lifecycle::{
    WorkerSupervisor, grace_window, spawn_environment_warmup_reconciliation, spawn_heartbeat,
    spawn_session_preparation_reconciliation, spawn_session_realization_reconciliation,
};
use manifest::{
    CredentialMaterializerSupport, ManifestSelection, ManifestSource, ResourceManifestSupport,
    StandardManifestInputs, derive_standard_manifest,
};

use attempt_decorator::WorkerApplicationSource;
pub use attempt_decorator::{
    RegisteredAttemptDecoratorFactory, RegisteredWorkerApplicationFactory, RegisteredWorkerContext,
    WorkerApplicationCapabilities, WorkerApplicationDependencies,
};
pub use bootstrap::{
    WorkerBootstrap, WorkerBootstrapInput, WorkerDaemonConfig, WorkerUpstreamSecurity,
};
pub use credential_files::WorkerCredentialFileResolver;
#[cfg(feature = "container-k8s")]
pub use k8s_create_client::WorkerK8sCreateClient;
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

use awaken_credential_materializer::InferenceExecutorMaterializer;
use awaken_session_executor::ExecutorComponents;
use awaken_worker_contract::{RegistryMutation, WorkerManifest};
use awaken_coordinator_client::WorkerControlClient;
use awaken_worker_transport_security::WorkerUpstream;

/// Completion of the one Worker preparation lane. A timeout or task failure
/// never asserts quiescence; the caller must abort admission and run the
/// existing exact-owner revocation before closing the Worker transport.
async fn preparation_drained_before(
    task: &mut tokio::task::JoinHandle<()>,
    budget: std::time::Duration,
) -> bool {
    matches!(tokio::time::timeout(budget, task).await, Ok(Ok(())))
}

struct WorkerProcessConfig {
    /// The embedded Worker process's one wall clock.
    clock: Arc<dyn awaken_time::WallClock>,
    deployment: awaken_session_executor::DeploymentConfig,
    manifest: StandardManifestConfig,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

impl WorkerProcessConfig {
    /// The `run_with_*` embedder entry points take the embedding process's
    /// clock from its `main`, like every other Worker composition.
    fn embedded_defaults(clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self {
            clock,
            deployment: awaken_session_executor::DeploymentConfig::ephemeral(),
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
    clock: Arc<dyn awaken_time::WallClock>,
    preselected_incarnation_id: Option<String>,
    manifest: ManifestSelection,
    deployment: awaken_session_executor::DeploymentConfig,
    standard_manifest_config: StandardManifestConfig,
    application: WorkerApplicationSource,
    tool_gate: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    credential_materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    mcp_relay_outbound_proxy: Option<Arc<dyn awaken_session_mcp::McpRelayOutboundProxy>>,
    brokered_acp_model_access:
        Option<Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>>,
    remote_attempt: Option<awaken_session_executor::RemoteAttemptInstallation>,
    hand_executor_factory:
        Option<Arc<dyn awaken_session_environment_contract::HandExecutorFactory>>,
    worker_local_credential_resolver:
        Option<Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    enclosing_sandbox_boundary: Option<InstalledSandboxBoundary>,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    #[cfg(feature = "container-k8s")]
    k8s_create_client: Option<Arc<WorkerK8sCreateClient>>,
    prepared_local_sandbox: Option<manifest::PreparedLocalSandbox>,
    mcp_attachment_realizer:
        Option<Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
    memory_mounter_factory: Option<RegisteredMemoryMounterFactory>,
    admin_tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

#[cfg(feature = "container-k8s")]
fn policy_with_k8s_create_client(
    mut policy: awaken_execution_composition::ContainerTierPolicy,
    client: Option<&Arc<WorkerK8sCreateClient>>,
) -> awaken_execution_composition::ContainerTierPolicy {
    if let Some(client) = client {
        policy.k8s_create_transport = Some(client.clone());
    }
    policy
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerRealizationScopeSource {
    KubernetesNamespace,
    PersistedLocalWorkspace,
}

/// Select the sole stable installation coordinate consumed by container
/// realization. Keeping this decision closed and pure prevents deployment
/// manifests from inventing a second Workspace marker for Kubernetes Workers.
const fn worker_realization_scope_source(
    sandbox_tier: awaken_session_executor::SandboxTier,
) -> WorkerRealizationScopeSource {
    match sandbox_tier {
        awaken_session_executor::SandboxTier::K8s => {
            WorkerRealizationScopeSource::KubernetesNamespace
        }
        awaken_session_executor::SandboxTier::Local
        | awaken_session_executor::SandboxTier::Namespace
        | awaken_session_executor::SandboxTier::Docker
        | awaken_session_executor::SandboxTier::Podman => {
            WorkerRealizationScopeSource::PersistedLocalWorkspace
        }
    }
}

#[cfg(kani)]
/// @obligation worker.realization_scope_uses_only_the_kubernetes_namespace_or_persisted_local_workspace
#[kani::proof]
fn worker_realization_scope_source_is_total_and_exact() {
    // Cause/effect decision table. C1 is one of the five closed SandboxTier
    // variants. E1 K8s selects only its Kubernetes namespace; E2 every host-local
    // tier selects only the persisted local Workspace. R1 C1=K8s=>E1;
    // R2 C1 in {Local,Namespace,Docker,Podman}=>E2. The symbolic index covers
    // every variant and proves no local marker can become a Kubernetes startup
    // precondition.
    let index: u8 = kani::any();
    kani::assume(index < 5);
    let tier = match index {
        0 => awaken_session_executor::SandboxTier::Local,
        1 => awaken_session_executor::SandboxTier::Namespace,
        2 => awaken_session_executor::SandboxTier::Docker,
        3 => awaken_session_executor::SandboxTier::Podman,
        4 => awaken_session_executor::SandboxTier::K8s,
        _ => unreachable!(),
    };
    let source = worker_realization_scope_source(tier);
    assert_eq!(
        source == WorkerRealizationScopeSource::KubernetesNamespace,
        tier == awaken_session_executor::SandboxTier::K8s
    );
}

/// Project the physical installation scope shared by every interchangeable
/// Worker that can realize the same durable Session substrate. Kubernetes owns
/// a namespace-wide substrate, whereas host-local runtimes retain the existing
/// persisted Workspace marker. Worker and process incarnation remain
/// lease/effect-fence facts; including either here would make an exact object
/// impossible to adopt after reassignment.
/// Downstream process roots reuse this same installation projection when they
/// decorate the canonical container provider; it grants no execution authority.
pub fn worker_realization_namespace(
    deployment: &awaken_session_executor::DeploymentConfig,
) -> Result<awaken_sandbox_container::ContainerRealizationNamespace, WorkerNodeBuildError> {
    let installation_scope = match worker_realization_scope_source(deployment.sandbox_tier) {
        WorkerRealizationScopeSource::KubernetesNamespace => {
            std::borrow::Cow::Borrowed(deployment.sandbox.k8s_namespace.as_str())
        }
        WorkerRealizationScopeSource::PersistedLocalWorkspace => {
            let installation_root = deployment.storage_dir.as_deref().ok_or_else(|| {
                WorkerNodeBuildError(
                    "container Worker requires DeploymentConfig::storage_dir for stable realization identity"
                        .into(),
                )
            })?;
            std::borrow::Cow::Owned(awaken_installation_workspace::provision_at(
                installation_root,
                awaken_session_executor::ExecutorComponents::mint_local_workspace,
            ))
        }
    };
    awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
        "worker-installation",
        installation_scope.as_ref(),
    ])
    .map_err(|error| WorkerNodeBuildError(error.to_string()))
}

impl WorkerNodeBuilder {
    /// `clock` is the Worker process's one wall clock: the Host this builder
    /// assembles drives its dispatch pool, recovery and child schedulers on it.
    #[must_use]
    pub fn new(upstream: WorkerUpstream, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self {
            upstream,
            clock,
            preselected_incarnation_id: None,
            manifest: ManifestSelection::Unset,
            deployment: awaken_session_executor::DeploymentConfig::ephemeral(),
            standard_manifest_config: StandardManifestConfig::default(),
            application: WorkerApplicationSource::default(),
            tool_gate: None,
            credential_materializer: None,
            mcp_relay_outbound_proxy: None,
            brokered_acp_model_access: None,
            remote_attempt: None,
            hand_executor_factory: None,
            worker_local_credential_resolver: None,
            acp_capability_observation_source: None,
            enclosing_sandbox_boundary: None,
            session_container_provider: None,
            #[cfg(feature = "container-k8s")]
            k8s_create_client: None,
            prepared_local_sandbox: None,
            mcp_attachment_realizer: None,
            memory_mounter_factory: None,
            admin_tools: Vec::new(),
            admin_listen: Some("0.0.0.0:9090".to_string()),
            graceful_drain: std::time::Duration::from_secs(20),
            credential_probe_interval: std::time::Duration::from_secs(10),
            credential_observation_ttl: std::time::Duration::from_secs(30),
        }
    }

    /// Bind the Worker boot to the exact identity already selected by a trusted
    /// independent process launcher. Ordinary Workers still generate their own
    /// fresh identity when this coordinate is absent.
    pub fn with_preselected_incarnation_id(
        mut self,
        incarnation_id: impl Into<String>,
    ) -> Result<Self, WorkerNodeBuildError> {
        let incarnation_id = incarnation_id.into();
        if !awaken_worker_contract::is_local_process_incarnation(&incarnation_id) {
            return Err(WorkerNodeBuildError(
                "preselected Worker incarnation must be exactly 32 lowercase hex digits".into(),
            ));
        }
        self.preselected_incarnation_id = Some(incarnation_id);
        Ok(self)
    }

    fn with_process_config(mut self, config: WorkerProcessConfig) -> Self {
        self.deployment = config.deployment;
        self.prepared_local_sandbox = None;
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

    /// Install process-local administration tools for a co-located all-in-one
    /// Worker. Distributed Workers receive none: these executors close over
    /// Control-owned stores and must never cross a process boundary.
    #[must_use]
    pub fn with_admin_tools(
        mut self,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Self {
        self.admin_tools = tools;
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
        deployment: awaken_session_executor::DeploymentConfig,
    ) -> Self {
        self.deployment = deployment;
        self.prepared_local_sandbox = None;
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
    pub fn with_attempt_decorator_factory(
        mut self,
        factory: RegisteredAttemptDecoratorFactory,
    ) -> Self {
        match &mut self.application {
            WorkerApplicationSource::Static {
                decorator,
                selected,
                ..
            } => {
                *selected = true;
                *decorator = Some(factory);
            }
            _ => self.application = WorkerApplicationSource::Conflict,
        }
        self
    }

    /// Build one complete application dependency value using the exact
    /// Coordinator-returned identity. Static dependency setters cannot coexist.
    #[must_use]
    pub fn with_registered_application_factory(
        mut self,
        capabilities: WorkerApplicationCapabilities,
        factory: RegisteredWorkerApplicationFactory,
    ) -> Self {
        self.application = match &self.application {
            WorkerApplicationSource::Static {
                selected: false, ..
            } => WorkerApplicationSource::Registered {
                capabilities,
                factory,
            },
            _ => WorkerApplicationSource::Conflict,
        };
        self
    }

    /// Install the sole application-owned permission gate around the canonical
    /// Session route.
    #[must_use]
    pub fn with_tool_gate(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.tool_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn InferenceExecutorMaterializer>,
    ) -> Self {
        if let Some(dependencies) = self.application.static_dependencies() {
            dependencies.inference = Some(materializer);
        }
        self
    }

    /// Install the already-configured remote-attempt adapter. A2A construction and
    /// transport credentials remain at the outer process startup.
    #[must_use]
    pub fn with_remote_attempt_executor(
        mut self,
        installation: awaken_session_executor::RemoteAttemptInstallation,
    ) -> Self {
        self.remote_attempt = Some(installation);
        self
    }

    /// Install the existing hand-channel adapter used by container and ACP
    /// environments. The Worker owns lifecycle, not relay construction.
    #[must_use]
    pub fn with_hand_executor_factory(
        mut self,
        factory: Arc<dyn awaken_session_environment_contract::HandExecutorFactory>,
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
        if let Some(dependencies) = self.application.static_dependencies() {
            dependencies.web_providers = providers;
        }
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

    /// Install the canonical identity-bound remote Session Resource projection.
    ///
    /// Standalone and platform-composed Workers must use this same constructor:
    /// the registered upstream is the sole File/Memory/Skill/Repository authority,
    /// and the resulting Memory mounter is the evidence used to advertise
    /// `session-resources/v1` in the immutable Worker manifest.
    #[must_use]
    pub fn with_remote_session_resource_support(mut self) -> Self {
        self.memory_mounter_factory = Some(Arc::new(|context| {
            let repository = Arc::new(awaken_resource_worker_http::HttpMemoryRepository::new(
                context.upstream().clone(),
            ));
            Ok(Arc::new(
                awaken_sandbox_memoryd::MemoryStoreMounter::copy_only(repository),
            ))
        }));
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

    /// Install the deployment's existing authenticated HTTPS CONNECT issuer
    /// for a Worker MCP relay exposed to isolated containers.
    #[must_use]
    pub fn with_mcp_relay_outbound_proxy(
        mut self,
        issuer: Arc<dyn awaken_session_mcp::McpRelayOutboundProxy>,
    ) -> Self {
        self.mcp_relay_outbound_proxy = Some(issuer);
        self
    }

    /// Install the hosted attempt-time model-access exchanger. It produces
    /// Gateway lease references only and is mutually exclusive with local Vault
    /// materialization.
    #[must_use]
    pub fn with_brokered_acp_model_access(
        mut self,
        materializer: Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>,
    ) -> Self {
        self.brokered_acp_model_access = Some(materializer);
        self
    }

    /// Install the liveness-only adapter for host-owned credential identities.
    /// This port cannot materialize Provider secrets.
    #[must_use]
    pub fn with_worker_local_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>,
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
            extra_mounts: Vec::new(),
            cache_volume_initializer: None,
        });
        self.prepared_local_sandbox = None;
        self
    }

    /// Install a decorated provider while preserving the capacity, creation
    /// mounts, and cache owner returned by the canonical deployment builder.
    #[must_use]
    pub fn with_session_container_environment_components(
        mut self,
        backend: impl Into<String>,
        components: awaken_execution_composition::ContainerEnvironmentComponents,
    ) -> Self {
        self.session_container_provider = Some(InstalledSessionContainerProvider {
            backend: backend.into(),
            provider: components.provider,
            capacity: components.capacity,
            extra_mounts: components.extra_mounts,
            cache_volume_initializer: components.cache_volume_initializer,
        });
        self.prepared_local_sandbox = None;
        self
    }

    /// Construct the deployment-selected provider before deriving the immutable
    /// Worker manifest. The resulting instance is shared by capability evidence,
    /// readiness probes, capacity lifecycle, and Runtime Host realization.
    pub async fn prepare_session_environment_from_deployment(
        self,
    ) -> Result<Self, WorkerNodeBuildError> {
        self.prepare_session_environment_from_deployment_with_policy(
            awaken_execution_composition::ContainerTierPolicy::default(),
            |components| components,
        )
        .await
    }

    /// Apply deployment-owned container security and a provider decorator on
    /// the same preparation path. The Worker still selects and binds the sole
    /// Kubernetes Create client; the decorator retains the canonical components.
    pub async fn prepare_session_environment_from_deployment_with_policy<F>(
        mut self,
        policy: awaken_execution_composition::ContainerTierPolicy,
        decorate: F,
    ) -> Result<Self, WorkerNodeBuildError>
    where
        F: FnOnce(
            awaken_execution_composition::ContainerEnvironmentComponents,
        ) -> awaken_execution_composition::ContainerEnvironmentComponents,
    {
        #[cfg(feature = "container-k8s")]
        if policy.k8s_create_transport.is_some() {
            return Err(WorkerNodeBuildError(
                "Worker builder owns its Kubernetes Create transport".into(),
            ));
        }
        self.manifest
            .require_capacity(&self.standard_manifest_config, &self.deployment)?;
        if !self.deployment.sandbox_tier.is_container()
            && self.session_container_provider.is_none()
            && self.enclosing_sandbox_boundary.is_none()
        {
            // Every local tier proves its backend before the manifest is
            // derived and the Worker registers (ADR-0092 decision 5).
            self.prepared_local_sandbox =
                Some(manifest::PreparedLocalSandbox::probe(&self.deployment).await?);
        }
        if !self.deployment.sandbox_tier.is_container() || self.session_container_provider.is_some()
        {
            return Ok(self);
        }
        #[cfg(feature = "container-k8s")]
        if self.deployment.sandbox_tier == awaken_session_executor::SandboxTier::K8s
            && self.k8s_create_client.is_none()
        {
            self.k8s_create_client =
                Some(Arc::new(WorkerK8sCreateClient::new(self.upstream.clone())));
        }
        let backend = self.deployment.sandbox_support().1;
        let realization_namespace = worker_realization_namespace(&self.deployment)?;
        #[cfg(feature = "container-k8s")]
        let policy = policy_with_k8s_create_client(policy, self.k8s_create_client.as_ref());
        let components = awaken_execution_composition::build_container_environment_for_realization(
            realization_namespace,
            self.deployment.sandbox_tier,
            self.deployment.container_image.as_deref(),
            &self.deployment.sandbox,
            &policy,
            self.clock.clone(),
        )
        .await
        .map_err(WorkerNodeBuildError)?;
        let components = decorate(components);
        Ok(self.with_session_container_environment_components(backend, components))
    }

    /// Install the one checkpoint-byte custody adapter used by the selected
    /// Session environment provider. Lifecycle truth remains in the Session;
    /// this dependency stores bytes only.
    #[must_use]
    pub fn with_environment_checkpoint_store(
        mut self,
        store: Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>,
    ) -> Self {
        if let Some(dependencies) = self.application.static_dependencies() {
            dependencies.checkpoint_store = Some(store);
        }
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
        self.prepared_local_sandbox = None;
        self
    }

    /// Install the one exact-generation MCP realization adapter used by remote
    /// Session commands. Downstream platforms implement the public Session port;
    /// the Worker retains no parallel gateway or credential-selection contract.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_attachment_realizer = Some(realizer);
        self
    }

    fn validate_build_topology(&self) -> Result<(), WorkerNodeBuildError> {
        self.deployment
            .validate_local_execution()
            .map_err(WorkerNodeBuildError)?;
        awaken_credential_materializer::reject_ambient_api_key_environment()
            .map_err(|error| WorkerNodeBuildError(error.to_string()))?;
        if self.credential_materializer.is_some() && self.brokered_acp_model_access.is_some() {
            return Err(WorkerNodeBuildError(
                "local and brokered ACP credential materializers are mutually exclusive"
                    .to_string(),
            ));
        }
        credential_liveness::validate_observation_timing(
            self.credential_probe_interval,
            self.credential_observation_ttl,
            None,
        )
        .map_err(WorkerNodeBuildError)?;
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
        if self.deployment.sandbox_tier == awaken_session_executor::SandboxTier::Namespace
            && self.deployment.sandbox.allow_local_fallback
            && self.session_container_provider.is_none()
            && self.enclosing_sandbox_boundary.is_none()
            && self.prepared_local_sandbox.is_none()
        {
            return Err(WorkerNodeBuildError(
                "Namespace local fallback requires preparing the effective Session provider before Worker registration".into(),
            ));
        }
        if self.deployment.sandbox_tier.is_container() && self.session_container_provider.is_none()
        {
            return Err(WorkerNodeBuildError(
                "container deployment must prepare its canonical Session provider before Worker build"
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
        self.manifest
            .require_capacity(&self.standard_manifest_config, &self.deployment)?;
        Ok(())
    }

    /// Validate the immutable topology without registering or starting work.
    pub fn build(mut self) -> Result<WorkerNode, WorkerNodeBuildError> {
        self.validate_build_topology()?;
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
        let provider_checkpoint_formats = self
            .session_container_provider
            .as_ref()
            .map(|installed| installed.provider.checkpoint_formats())
            .unwrap_or_default()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let dependency_capabilities = self
            .application
            .capabilities()
            .map_err(WorkerNodeBuildError)?;
        if !provider_checkpoint_formats.is_empty() && !dependency_capabilities.checkpoint_store {
            return Err(WorkerNodeBuildError(
                "checkpoint-capable Session provider requires a checkpoint store".to_string(),
            ));
        }
        if dependency_capabilities.checkpoint_store && provider_checkpoint_formats.is_empty() {
            return Err(WorkerNodeBuildError(
                "checkpoint store requires a checkpoint-capable Session provider".to_string(),
            ));
        }
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
            })
            .or_else(|| {
                self.prepared_local_sandbox
                    .as_ref()
                    .map(|prepared| (prepared.capabilities.clone(), prepared.backend))
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
                    inference_schemes: &dependency_capabilities.inference_schemes,
                    inference_credentials: &dependency_capabilities.inference_credentials,
                    credential_materializer: self
                        .credential_materializer
                        .as_ref()
                        .map(CredentialMaterializerSupport::from),
                    brokered_acp_model_access: self.brokered_acp_model_access.as_deref(),
                    worker_local_credential_resolver_installed: credential_observation_resolver
                        .is_some(),
                    remote_credential_realization: self
                        .remote_attempt
                        .as_ref()
                        .map(|remote| &remote.credential_realization),
                    sandbox_override,
                    checkpoint_formats: provider_checkpoint_formats.clone(),
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
        if let Some(prepared) = &self.prepared_local_sandbox {
            let (capabilities, backend) = (&prepared.capabilities, prepared.backend);
            let mut expected_backends = std::collections::BTreeSet::from([backend.to_string()]);
            if self.deployment.trusted_host_enabled() {
                expected_backends
                    .insert(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND.to_string());
            }
            if manifest.sandbox != *capabilities || manifest.sandbox_backends != expected_backends {
                return Err(WorkerNodeBuildError(
                    "Worker manifest must match the prepared effective local Session provider"
                        .into(),
                ));
            }
        }
        validate_worker_manifest(&manifest, &self.deployment)?;
        if !provider_checkpoint_formats.is_subset(&manifest.checkpoint_formats) {
            return Err(WorkerNodeBuildError(
                "Worker manifest omits an installed provider checkpoint format".to_string(),
            ));
        }
        Ok(WorkerNode {
            upstream: self.upstream,
            clock: self.clock,
            preselected_incarnation_id: self.preselected_incarnation_id,
            manifest,
            deployment: self.deployment,
            application: self.application,
            tool_gate: self.tool_gate,
            credential_materializer: self.credential_materializer,
            mcp_relay_outbound_proxy: self.mcp_relay_outbound_proxy,
            brokered_acp_model_access: self.brokered_acp_model_access,
            remote_attempt: self.remote_attempt,
            hand_executor_factory: self.hand_executor_factory,
            credential_observation_resolver,
            acp_capability_observation_source: self.acp_capability_observation_source,
            session_container_provider: self.session_container_provider,
            #[cfg(feature = "container-k8s")]
            k8s_create_client: self.k8s_create_client,
            prepared_local_sandbox: self.prepared_local_sandbox,
            mcp_attachment_realizer: self.mcp_attachment_realizer,
            memory_mounter_factory: self.memory_mounter_factory,
            admin_tools: self.admin_tools,
            admin_listen: self.admin_listen,
            graceful_drain: self.graceful_drain,
            credential_probe_interval: self.credential_probe_interval,
            credential_observation_ttl: self.credential_observation_ttl,
        })
    }
}

fn validate_worker_manifest(
    manifest: &WorkerManifest,
    deployment: &awaken_session_executor::DeploymentConfig,
) -> Result<(), WorkerNodeBuildError> {
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
    if !manifest
        .dispatch_contract
        .contains(awaken_worker_contract::CURRENT_CONTRACT_VERSION)
        || !manifest.supports_current_runtime_protocol()
    {
        return Err(WorkerNodeBuildError(
            "Worker manifest must support the current dispatch contract and explicitly support the current runtime protocol".to_string(),
        ));
    }
    let installed_recovery = deployment
        .sandbox
        .container_hand_residency
        .recovery_capability();
    // A kernel probe or a bootstrap primitive is not an installed Session
    // lifecycle. Until both Native dispatch and fenced recovery use that
    // boundary, neither standard nor explicit manifests may claim it.
    if manifest.sandbox.process_tree_supervision
        || manifest
            .trusted_host_sandbox
            .as_ref()
            .is_some_and(|caps| caps.process_tree_supervision)
    {
        return Err(WorkerNodeBuildError(
            "process-tree supervision is not installed in the complete Session lifecycle".into(),
        ));
    }
    if let Some(host) = &manifest.trusted_host_sandbox
        && (Some(host) != deployment.trusted_host_support().as_ref()
            || !manifest
                .sandbox_backends
                .contains(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND))
    {
        return Err(WorkerNodeBuildError(
            "TrustedHost capability profile requires the installed exact Workdir backend".into(),
        ));
    }
    if !awaken_worker_contract::manifest_recovery_matches_installed(
        manifest.sandbox_tool_recovery,
        installed_recovery,
    ) {
        return Err(WorkerNodeBuildError(format!(
            "Worker manifest sandbox tool recovery {:?} does not match installed SessionEnvironment {:?}",
            manifest.sandbox_tool_recovery, installed_recovery
        )));
    }
    manifest
        .fingerprint()
        .map_err(|error| WorkerNodeBuildError(error.to_string()))?;
    Ok(())
}

/// One assembled remote Worker lifecycle.
pub struct WorkerNode {
    upstream: WorkerUpstream,
    /// The Worker process's one wall clock, from its builder.
    clock: Arc<dyn awaken_time::WallClock>,
    preselected_incarnation_id: Option<String>,
    manifest: WorkerManifest,
    deployment: awaken_session_executor::DeploymentConfig,
    application: WorkerApplicationSource,
    tool_gate: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    credential_materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    mcp_relay_outbound_proxy: Option<Arc<dyn awaken_session_mcp::McpRelayOutboundProxy>>,
    brokered_acp_model_access:
        Option<Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>>,
    remote_attempt: Option<awaken_session_executor::RemoteAttemptInstallation>,
    hand_executor_factory:
        Option<Arc<dyn awaken_session_environment_contract::HandExecutorFactory>>,
    credential_observation_resolver:
        Option<Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>>,
    acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    session_container_provider: Option<InstalledSessionContainerProvider>,
    #[cfg(feature = "container-k8s")]
    k8s_create_client: Option<Arc<WorkerK8sCreateClient>>,
    prepared_local_sandbox: Option<manifest::PreparedLocalSandbox>,
    mcp_attachment_realizer:
        Option<Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
    memory_mounter_factory: Option<RegisteredMemoryMounterFactory>,
    admin_tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    credential_probe_interval: std::time::Duration,
    credential_observation_ttl: std::time::Duration,
}

struct InstalledSessionContainerProvider {
    backend: String,
    provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
    capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
    extra_mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    cache_volume_initializer: Option<Arc<dyn awaken_session_executor::CacheVolumeInitializer>>,
}

struct InstalledSandboxBoundary {
    backend: String,
    capabilities: awaken_provisioning_contract::SandboxCapabilities,
}

/// Run a genuinely secretless worker with a deployment-provided materializer.
/// It receives each durable run's snapshot-pinned inference access and may
/// realize an executor through a remote broker without opening a credential
/// vault or persisting provider keys in this process.
///
/// `clock` is the embedding process's one wall clock, chosen in its `main`.
pub async fn run_with_inference_materializer(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(clock),
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
/// `clock` is the embedding process's one wall clock, chosen in its `main`.
pub async fn run_with_inference_and_credential_resolver<R>(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    resolver: Arc<R>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: awaken_credential_contract::CredentialMaterialResolver
        + awaken_credential_contract::WorkerLocalCredentialResolver
        + 'static,
{
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(clock),
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
/// [`run_with_inference_materializer`]. `clock` is the embedding process's one
/// wall clock, chosen in its `main`.
pub async fn run_with_inference_materializer_until<F>(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    shutdown: F,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: std::future::Future<
            Output = Result<WorkerShutdown, Box<dyn std::error::Error + Send + Sync>>,
        >,
{
    build_secretless_worker(
        WorkerUpstream::new(upstream),
        WorkerProcessConfig::embedded_defaults(clock),
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
    material_resolver: Option<Arc<dyn awaken_credential_contract::CredentialMaterialResolver>>,
    local_resolver: Option<Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>>,
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let clock = process.clock.clone();
    let mut builder = WorkerNodeBuilder::new(upstream, clock.clone()).with_process_config(process);
    if let Some(resolver) = material_resolver {
        builder = builder.with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                resolver, clock,
            ),
        );
    }
    if let Some(resolver) = local_resolver {
        builder = builder.with_worker_local_credential_resolver(resolver);
    }
    builder = builder
        .with_inference_materializer(materializer)
        .with_standard_manifest(Default::default());
    Ok(builder
        .prepare_session_environment_from_deployment()
        .await?
        .build()?)
}

/// Construct the two registered content clients from the same authenticated
/// upstream, deriving every Memory execution role once from one HTTP client.
fn remote_content_adapters(
    upstream: &WorkerUpstream,
) -> awaken_session_executor::WorkerContentAdapters {
    let memory = Arc::new(awaken_resource_worker_http::HttpMemoryRepository::new(
        upstream.clone(),
    ));
    let files = Arc::new(awaken_resource_worker_http::HttpFileContentSource::new(
        upstream.clone(),
    ));
    awaken_session_executor::WorkerContentAdapters {
        file_content_source: files,
        memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
    }
}

fn configured_container_acp_targets(
    deployment: &awaken_session_executor::DeploymentConfig,
) -> Result<Vec<acp_capability::ConfiguredAcpCapabilityTarget>, String> {
    if !matches!(
        deployment.sandbox_tier,
        awaken_session_executor::SandboxTier::Docker
            | awaken_session_executor::SandboxTier::Podman
            | awaken_session_executor::SandboxTier::K8s
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
    deployment: &awaken_session_executor::DeploymentConfig,
    host: Arc<ExecutorComponents>,
) -> Result<Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>, String> {
    let targets = configured_container_acp_targets(deployment)?;
    if targets.is_empty() {
        return Ok(None);
    }
    let clock = host.clock();
    let negotiator = Arc::new(
        awaken_session_executor::SessionAcpCapabilityNegotiator::new(
            host.run_assembly().clone(),
            deployment.dispatch_owner.clone(),
            // Some native adapters perform image-local plugin discovery before
            // opening their first prompt-free Session. Keep the probe bounded while
            // allowing that deterministic cold start to complete.
            std::time::Duration::from_secs(30),
            Arc::new(awaken_protocol_acp::ProtocolAcpCapabilityHandshake),
        ),
    );
    Ok(Some(Arc::new(
        acp_capability::ConfiguredAcpCapabilityObservationSource::new(
            targets,
            negotiator,
            std::path::PathBuf::from(awaken_provisioning_contract::WorkspaceLayout::ROOT),
            clock,
        ),
    )))
}

async fn start_admin_surface(admin_listen: Option<String>) -> Option<admin::WorkerAdminSurface> {
    let admin_addr = admin_listen?;
    match tokio::net::TcpListener::bind(&admin_addr).await {
        Ok(listener) => {
            eprintln!(
                "awaken-worker admin surface on {admin_addr} (/readyz /metrics /admin/drain)"
            );
            Some(admin::WorkerAdminSurface::start(listener))
        }
        Err(error) => {
            eprintln!("awaken-worker admin surface disabled (bind {admin_addr}: {error})");
            None
        }
    }
}

impl WorkerNode {
    #[must_use]
    pub fn manifest(&self) -> &WorkerManifest {
        &self.manifest
    }

    fn selected_container_policy(
        &self,
    ) -> Result<awaken_execution_composition::ContainerTierPolicy, std::io::Error> {
        let policy = awaken_execution_composition::ContainerTierPolicy::default();
        #[cfg(feature = "container-k8s")]
        {
            if self.deployment.sandbox_tier == awaken_session_executor::SandboxTier::K8s
                && self.k8s_create_client.is_none()
            {
                return Err(std::io::Error::other(
                    "Kubernetes Worker requires its brokered Create provider before registration",
                ));
            }
            Ok(policy_with_k8s_create_client(
                policy,
                self.k8s_create_client.as_ref(),
            ))
        }
        #[cfg(not(feature = "container-k8s"))]
        {
            Ok(policy)
        }
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
        let mut deployment = self.deployment.clone();
        let container_policy = self.selected_container_policy()?;
        #[cfg(feature = "container-k8s")]
        let k8s_create_client = self.k8s_create_client.clone();
        let upstream_url = self.upstream.base_url().to_string();
        let upstream = self.upstream;
        let bootstrap_control = WorkerControlClient::new(upstream.clone());
        let incarnation_id = lifecycle::select_incarnation_id(self.preselected_incarnation_id)?;
        // Registration and dependency construction can wait for another service.
        // Serve process liveness now; the same router remains unready until the
        // existing Supervisor and pool are installed below. Its guard also
        // closes the listener on every early return or unwind.
        let admin_surface = start_admin_surface(self.admin_listen).await;
        let mut shutdown = std::pin::pin!(shutdown);
        let mut registration_wait_attempts = 0_u64;
        let registration_receipt = loop {
            let attempt = bootstrap_control
                .register_classified(incarnation_id.clone(), self.manifest.clone())
                .await;
            match attempt {
                Ok(registration) => break registration,
                Err(
                    error @ (awaken_coordinator_client::WorkerRegistrationError::SlotOccupied(_)
                    | awaken_coordinator_client::WorkerRegistrationError::Unavailable(_)),
                ) => {
                    if registration_wait_attempts.is_multiple_of(10) {
                        eprintln!("worker registration waiting for Coordinator: {error}");
                    }
                    registration_wait_attempts = registration_wait_attempts.saturating_add(1);
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
        let startup_timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
            registration_receipt.lease_ttl_ms,
        );
        let registration = registration_receipt.worker;
        let upstream = upstream.with_worker_identity(registration.snapshot.identity.clone());
        // The registry-assigned incarnation is the only execution identity. The
        // pre-registration deployment owner is configuration input, not a second
        // lease namespace; bind every Host-local scheduler to the exact identity
        // used by the remote dispatch and Session Work protocols.
        deployment.dispatch_owner = registration.snapshot.identity.lease_owner();
        let control = WorkerControlClient::new(upstream.clone());
        let identity = registration.snapshot.identity.clone();
        #[cfg(feature = "container-k8s")]
        if let Some(client) = &k8s_create_client {
            client
                .bind_identity(identity.clone())
                .map_err(std::io::Error::other)?;
        }
        // Registration has one exit owner. All startup errors, cooperative
        // cancellation and ordinary shutdown reach the same bounded withdrawal;
        // no early-return branch maintains a second deregistration policy.
        let result = async {
        if registration_receipt.lease_ttl_ms == 0 {
            return Err(std::io::Error::other(
                "worker registration returned a zero lease TTL",
            ).into());
        }
        if registration_receipt.runtime_protocol_version
            != Some(awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION)
        {
            return Err(std::io::Error::other(
                "Control did not select the current Worker runtime protocol; upgrade Control and Worker together",
            ).into());
        }
        if !matches!(registration.snapshot.state,
            awaken_worker_contract::WorkerState::Starting | awaken_worker_contract::WorkerState::Ready)
        {
            return Err(std::io::Error::other("registered Worker admission is already closed").into());
        }
        let (roles, lifecycle, heartbeat_timing, environment_proof_started) = {
        let startup = std::panic::AssertUnwindSafe(async {
        let registered_context =
            RegisteredWorkerContext::new(registration.clone(), upstream.clone());
        let application = self.application.realize(&registered_context).map_err(|error| std::io::Error::other(format!("registered Worker application factory failed: {error}")))?;
        let memory_mounter = match self.memory_mounter_factory {
            Some(factory) => match factory(&registered_context) {
                Ok(mounter) => Some(mounter),
                Err(error) => {
                    return Err(std::io::Error::other(format!(
                        "registered Memory mounter factory failed: {error}"
                    ))
                    .into());
                }
            },
            None => None,
        };
        // Route the dispatch pool's claim/settle over HTTP to the cell server.
        let (dispatch, stream_publisher) = awaken_coordinator_client::worker_transports_with_upstream(
            &upstream,
            registration.snapshot.identity.clone(),
        );
        // The capacity this incarnation just registered. It is read once, it has
        // no `Clone`, and the only thing that can be done with it is to move it
        // into the pool installer below — so the registry's number and the
        // installed slot count cannot drift apart unnoticed.
        let registered_capacity =
            awaken_worker_contract::RegisteredCapacity::of(&self.manifest).map_err(|error| {
                std::io::Error::other(format!("registered Worker capacity: {error}"))
            })?;

        let managed_credential_materializer = self.credential_materializer.clone();
        let remote_content = remote_content_adapters(&upstream);
        let remote_artifacts = Arc::new(awaken_resource_worker_http::HttpArtifactPublisher::new(
            upstream.clone(),
        ));
        let remote_skills = Arc::new(awaken_resource_worker_http::HttpSkillBundleSource::new(
            upstream.clone(),
        ));
        let remote_repositories = Arc::new(
            awaken_resource_worker_http::HttpRepositoryBindingVerifier::new(upstream.clone()),
        );
        // A durable Worker installation keeps one platform Workspace across
        // restarts (the same marker its container realization namespace reads);
        // a storage-less Worker mints a volatile one.
        let local_workspace = match deployment.storage_dir.as_deref() {
            Some(root) => awaken_installation_workspace::provision_at(
                root,
                ExecutorComponents::mint_local_workspace,
            ),
            None => ExecutorComponents::mint_local_workspace(),
        };
        // Claimed Runs commit back to the Coordinator under this registered
        // identity; the Host holds only the neutral claim-fenced port.
        let claimed_commit = awaken_coordinator_client::remote_claimed_commit(&upstream)?;
        // Every registered Worker realizes the already-frozen Session projection
        // through Control; the same client projects the one durable Environment
        // root authorization before provider I/O and hands out cold terminal
        // cleanup. WorkQueue remains the sole Session ownership path.
        let session_control = Arc::new(awaken_coordinator_client::WorkerControlSessionClient::new(
            control.clone(),
            registration.snapshot.identity.clone(),
        ));
        // Committed truth lives in the Coordinator: the Host reaches it only
        // through these claim-fenced transports.
        let authority = awaken_session_executor::UpstreamAuthority::default()
            .with_dispatch_binding(awaken_dispatch_erasure::DispatchBinding::remote(dispatch, claimed_commit))
            .with_memory_reference_encoder(Arc::new(
                awaken_resource_worker_http::HttpMemoryMaterializationReferenceEncoder,
            ))
            .with_stream_publisher(stream_publisher)
            .with_terminal_cleanup_control(session_control.clone());
        let mut deps = awaken_session_executor::SessionExecutorBuilder::for_worker(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            "worker",
            remote_content,
            deployment
                .session_execution_process(local_workspace, self.clock.clone())
                .map_err(std::io::Error::other)?,
            authority,
            registered_capacity,
        )
        .map_err(std::io::Error::other)?
        .with_auxiliary_sandboxes(awaken_execution_composition::local_auxiliary_sandboxes())
        .with_artifact_publisher(remote_artifacts)
        // Every execution topology installs the same Session MCP adapter.
        // Host permission grants and Environment-bound executors remain owners.
        .with_acp_tool_exporter(Arc::new(awaken_protocol_mcp::SessionToolExporter))
        .with_skill_bundle_source(remote_skills)
        .with_web_search_provider_registry(application.web_providers)
        .with_admin_tools(self.admin_tools);
        if let Some(remote_attempt) = self.remote_attempt {
            deps = deps.with_remote_attempt_executor(remote_attempt);
        }
        if let Some(materializer) = &application.inference {
            deps = deps.with_inference_materializer(materializer.clone());
        }
        if let Some(resolver) = &self.credential_observation_resolver {
            deps = deps.with_worker_credential_resolver(resolver.clone());
        }
        if let Some(decorator) = application.attempt_decorator {
            deps = deps.with_attempt_decorator(decorator);
        }
        deps = deps
            .with_session_control(session_control.clone())
            .with_environment_binding_sink(session_control);
        if let Some(gate) = self.tool_gate {
            deps = deps.with_gate_override(gate);
        }
        if let Some(memory_mounter) = memory_mounter {
            deps = deps.with_memory_mounter(memory_mounter);
        }

        let session_environment_provider = self
            .session_container_provider
            .as_ref()
            .map(|installed| installed.provider.clone());
        if let Some(installed) = self.session_container_provider {
            let hand_factory = self
                .hand_executor_factory
                .clone()
                .expect("container provider was validated with a hand factory");
            deps = awaken_execution_composition::install_container_environment_components(
                deps,
                &deployment,
                awaken_execution_composition::ContainerEnvironmentComponents {
                    provider: installed.provider,
                    capacity: installed.capacity,
                    extra_mounts: installed.extra_mounts,
                    cache_volume_initializer: installed.cache_volume_initializer,
                },
                hand_factory,
            );
        }
        if let Some(store) = application.checkpoint_store {
            deps = deps.with_environment_checkpoint_store(store);
        }
        if let Some(credentials) = managed_credential_materializer.clone() {
            deps = deps.with_credential_materializer(credentials);
        }
        if let Some(issuer) = self.mcp_relay_outbound_proxy {
            deps = deps.with_mcp_relay_outbound_proxy(issuer);
        }
        // Serve only the ACP CLI capability this worker advertises. The run's snapshot
        // selects the matching backend and supplies its published provider access.
        deps = awaken_execution_composition::with_session_environment_from_deployment(
            deps,
            &deployment,
            self.hand_executor_factory,
            &container_policy,
        )
        .await
        .map_err(std::io::Error::other)?;
        deps = awaken_execution_composition::with_acp_from_deployment(
            deps,
            &deployment,
            self.credential_materializer,
            self.brokered_acp_model_access,
        );
        manifest::ensure_registered_local_provider(&self.prepared_local_sandbox, &self.manifest, &deps)?;

        let mut execution = awaken_session_executor::SessionExecutionDependencies::default()
            .with_repository_binding_verifier(remote_repositories);
        if let Some(realizer) = self.mcp_attachment_realizer {
            execution = execution.with_mcp_attachment_realizer(realizer);
        }
        deps = deps.with_session_execution(execution);
        let authority = Arc::new(lifecycle::WorkerAuthorityProof::default());
        let host = (deps.with_execution_authority(authority.clone())).build().map_err(std::io::Error::other)?;
        host.run_assembly().prepare_container_mcp_relay().await.map_err(|error| {
            std::io::Error::other(format!(
                "container MCP relay transport failed before Ready: {error}"
            ))
        })?;
        let acp_capability_observation_source = match self.acp_capability_observation_source {
            Some(source) => Some(source),
            None => configured_container_acp_capability_source(&deployment, host.clone())
                .map_err(std::io::Error::other)?,
        };
        let observations = Arc::new(WorkerObservationCache::default());
        let lifecycle = Arc::new(WorkerSupervisor {
            authority,
            roles: awaken_session_executor::WorkerRoles::of(&host),
            control: control.clone(),
            identity: registration.snapshot.identity,
            credential_observation_resolver: self.credential_observation_resolver,
            acp_capability_observation_source,
            session_environment_provider,
            local_readiness: self.prepared_local_sandbox.map(|prepared| prepared.readiness),
            observations,
            observation_ttl: self.credential_observation_ttl,
            probe_interval: self.credential_probe_interval,
            warm_environments: Default::default(),
        });
        // Publish Ready before starting the pull loop. Dynamic capability probes
        // and capacity warmups are eligibility evidence, not process liveness:
        // they start immediately below and remain absent until proven. Keeping
        // them off this boundary prevents an unavailable Sandbox backend from
        // making the Worker itself permanently Starting while claim admission
        // still fails closed for any Run that needs their evidence.
        let environment_proof_started = awaken_time::mono::now();
        lifecycle.prove_environment_before_ready(startup_timing.request_timeout()).await?;
        // A lease receipt proves no more time than remained when the request
        // started. Anchoring locally before I/O prevents response latency from
        // extending the Coordinator-owned expiry in the Worker process.
        let initial_heartbeat_started = lifecycle::authority_proof_started()?;
        let initial_heartbeat = lifecycle.observations.snapshot().into_heartbeat(
            1,
            awaken_worker_contract::process_ready_after_startup(
                awaken_worker_contract::DynamicEvidenceProbeState::Pending,
            ),
            0,
            lifecycle.warm_environment_shapes(),
        );
        let initial = tokio::time::timeout(
            startup_timing.request_timeout(),
            control.heartbeat(&lifecycle.identity, initial_heartbeat),
        )
        .await
        .map_err(|_| "initial worker heartbeat timed out".to_owned())
        .and_then(std::convert::identity);
        let initial = match initial {
            Ok(initial) => initial,
            Err(error) => {
                return Err(std::io::Error::other(error).into());
            }
        };
        if initial.mutation != RegistryMutation::Applied {
            return Err(std::io::Error::other(format!(
                "initial worker heartbeat rejected: {:?}",
                initial.mutation
            ))
            .into());
        }
        let heartbeat_timing =
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
                initial
                    .lease_ttl_ms
                    .expect("WorkerControlClient validates applied heartbeat TTL"),
            );
        credential_liveness::validate_observation_timing(
            self.credential_probe_interval,
            self.credential_observation_ttl,
            Some(heartbeat_timing),
        )
        .map_err(std::io::Error::other)?;
        if lifecycle.session_environment_provider.is_some()
            && !lifecycle::environment_proof_is_fresh(
                environment_proof_started.elapsed(),
                heartbeat_timing.proof_window(),
            )
        {
            return Err(
                std::io::Error::other("sandbox evidence expired before pool admission").into(),
            );
        }
        if let Err(error) = lifecycle
            .authority
            .apply(initial_heartbeat_started, heartbeat_timing)
        {
            return Err(error.into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((lifecycle.roles.clone(), lifecycle, heartbeat_timing, environment_proof_started))
        }).catch_unwind();
        tokio::pin!(startup);
        tokio::select! {
            result = &mut startup => result.map_err(|_| std::io::Error::other(
                "Worker startup dependency panicked; registration will be withdrawn"
            ))??,
            request = &mut shutdown => {
                request?;
                return Ok(());
            }
        }
        };
        roles.start_pool()?;
        let credential_probe = credential_liveness::spawn_probe(
            lifecycle.observations.clone(),
            lifecycle.credential_observation_resolver.clone(),
            lifecycle.acp_capability_observation_source.clone(),
            self.credential_probe_interval,
            self.credential_observation_ttl,
            self.clock.clone(),
        );
        let environment_warmups = spawn_environment_warmup_reconciliation(lifecycle.clone());
        let session_realizations =
            spawn_session_realization_reconciliation(lifecycle.clone(), heartbeat_timing);
        let mut session_preparations =
            spawn_session_preparation_reconciliation(lifecycle.clone(), heartbeat_timing);
        let mut heartbeat = spawn_heartbeat(
            lifecycle.clone(),
            2,
            heartbeat_timing,
            environment_proof_started,
        );
        eprintln!("awaken-worker registered with {upstream_url}");

        if let Some(surface) = &admin_surface {
            surface.bind_lifecycle(lifecycle.clone())?;
        }

        // Authority loss is an irreversible boundary for this incarnation. The
        // heartbeat task has already fenced local claim admission; terminate the
        // process after the ordinary drain cleanup so Kubernetes/systemd can start
        // a newly registered incarnation. Reusing this process would make a stale
        // epoch capable of silently becoming authoritative again.
        let (shutdown, authority_lost, heartbeat_cleanup) = tokio::select! {
            shutdown = &mut shutdown => (shutdown, false, None),
            heartbeat = &mut heartbeat => {
                match heartbeat {
                    Ok(outcome) => (Ok(WorkerShutdown::Graceful), true, Some(outcome)),
                    Err(error) => {
                        eprintln!("worker heartbeat task failed: {error}; terminating incarnation");
                        (Ok(WorkerShutdown::Graceful), true, None)
                    }
                }
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
        let deadline_ms = awaken_time::Expiry::after(self.clock.now(), grace)
            .deadline().as_epoch_ms();
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            lifecycle.begin_drain(Some(deadline_ms)),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("awaken-worker drain registration failed closed: {error}"),
            Err(_) => eprintln!(
                "awaken-worker drain registration exceeded 5s; local admission remains closed"
            ),
        }
        let (mut cleanup, mut cleanup_deadline) = match heartbeat_cleanup {
            Some(report) => (report.outcome, Some(report.deadline)),
            None if authority_lost => (
                lifecycle::WorkerRevocationOutcome::Unconfirmed {
                    stage: lifecycle::WorkerRevocationStage::LocalFenceAndInterrupt,
                    interrupted: None,
                    detail: Some("heartbeat cleanup task did not return an outcome".to_string()),
                },
                None,
            ),
            None => (
                lifecycle::WorkerRevocationOutcome::Completed { interrupted: 0 },
                None,
            ),
        };
        if graceful && !authority_lost {
            eprintln!(
                "awaken-worker draining: finishing in-flight runs (≤{}s)",
                grace.as_secs()
            );
            let (runs_drained, preparation_drained) = tokio::join!(
                roles.drain(grace),
                preparation_drained_before(&mut session_preparations, grace),
            );
            if !runs_drained || !preparation_drained {
                // A drained admission gate does not stop an in-flight inference
                // or Session preparation effect. Stop the preparation future
                // before revoking its exact local Session projection; a timer
                // does not certify physical quiescence or create an ACK.
                session_preparations.abort();
                let deadline =
                    awaken_time::mono::now() + lifecycle::WORKER_REVOCATION_BUDGET;
                cleanup = Box::pin(lifecycle::revoke_worker_session_authority_before(
                    &lifecycle,
                    deadline,
                ))
                .await;
                cleanup_deadline = Some(deadline);
            }
        } else if !authority_lost {
            // Prompt stop has no Run grace, but still gives an idle preparation
            // loop one tick to observe the absorbing drain. An occupied or
            // unresponsive preparation is revoked through the existing bounded
            // Session owner cleanup, never abandoned as a successful effect.
            if !preparation_drained_before(
                &mut session_preparations,
                std::time::Duration::from_millis(100),
            )
            .await
            {
                session_preparations.abort();
                let deadline =
                    awaken_time::mono::now() + lifecycle::WORKER_REVOCATION_BUDGET;
                cleanup = Box::pin(lifecycle::revoke_worker_session_authority_before(
                    &lifecycle,
                    deadline,
                ))
                .await;
                cleanup_deadline = Some(deadline);
            }
        }
        lifecycle.authority.close();
        heartbeat.abort();
        credential_probe.abort();
        environment_warmups.abort();
        session_realizations.abort();
        session_preparations.abort();
        // No producer can refill unused capacity after these aborts. When
        // revocation ran, its original absolute deadline remains the one owner
        // for this final effect; an exhausted deadline becomes Unconfirmed.
        let deadline = cleanup_deadline
            .unwrap_or_else(|| awaken_time::mono::now() + lifecycle::WORKER_REVOCATION_BUDGET);
        cleanup = lifecycle::shutdown_worker_capacity_before(&lifecycle, deadline, cleanup).await;
        // A timeout carries no authority to publish quiescence, even when the
        // instantaneous process-local counter happens to read zero.
        if cleanup.permits_quiescence_publication(roles.dispatch().in_flight()) {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                control.mark_quiesced(&lifecycle.identity),
            )
            .await;
        }
        #[cfg(feature = "container-k8s")]
        if k8s_create_client.is_some() && identity.worker_id.starts_with("k8s:") {
            // One Pod UID is one Registry incarnation and one immutable Create
            // ledger. Keep /livez serving while this drained process waits for
            // a UID-fenced Kubernetes deletion intent; exiting sooner would
            // let StatefulSet restart a second process under the same UID.
            // This never emits a stop receipt or releases retained evidence.
            await_registered_kubernetes_pod_replacement(&identity.worker_id).await;
        }
        shutdown?;
        if authority_lost {
            return Err(std::io::Error::other(
                "worker lost execution authority; supervisor restart required",
            )
            .into());
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }.await;
        let withdrawal = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            control.deregister(&identity),
        )
        .await
        {
            // Absence or a newer incarnation also proves that this exact
            // registration cannot retain authority; never touch its replacement.
            Ok(Ok(
                RegistryMutation::Applied
                | RegistryMutation::NotFound
                | RegistryMutation::StaleIncarnation,
            )) => Ok(()),
            Ok(Ok(other)) => Err(format!("worker deregistration rejected: {other:?}")),
            Ok(Err(error)) => Err(format!("worker deregistration unconfirmed: {error}")),
            Err(_) => Err("worker deregistration unconfirmed after 5s".to_string()),
        };
        if let Err(error) = &withdrawal {
            eprintln!("awaken-worker {error}");
        }
        // Preserve the primary startup/shutdown failure. A successful local
        // stop cannot be reported as complete when remote withdrawal is unknown.
        result?;
        withdrawal.map_err(|error| std::io::Error::other(error).into())
    }
}

#[cfg(test)]
mod tests;
