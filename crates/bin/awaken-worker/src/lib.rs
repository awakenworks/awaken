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
//! [`awaken_control::open_inference_materialization_stores_from_env`]). The host's
//! [`NoModelConfiguredExecutor`]
//! is only an inert construction placeholder: because the materializer is installed,
//! an unavailable publication pin fails closed before that executor can run.

use std::sync::Arc;

mod admin;

use awaken_runtime_contract::execution::NATIVE_RUNTIME_CAPABILITY;
use awaken_runtime_host::{
    ApplicationSessionProvisioner, AttemptExecutorDecorator, WorkerControlClient, WorkerUpstream,
};
use awaken_server::inference_materializer::CredentialInferenceMaterializer;
use awaken_server::no_model::NoModelConfiguredExecutor;
use awaken_server::{InferenceExecutorMaterializer, SharedHost};
use awaken_worker_contract::{
    REPOSITORY_CREDENTIALS_CAPABILITY, RegisteredWorker, RegistryMutation,
    SESSION_RESOURCES_CAPABILITY, VersionRange, WorkerCapacity, WorkerHeartbeat, WorkerIdentity,
    WorkerManifest, WorkerSnapshot,
};

/// Immutable application assembly context created only after the Control Node
/// allocates this process's Worker identity.
#[derive(Clone)]
pub struct RegisteredWorkerContext {
    registration: RegisteredWorker,
    upstream: WorkerUpstream,
}

impl RegisteredWorkerContext {
    fn new(registration: RegisteredWorker, upstream: WorkerUpstream) -> Self {
        Self {
            registration,
            upstream,
        }
    }

    #[must_use]
    pub fn registration(&self) -> &RegisteredWorker {
        &self.registration
    }

    #[must_use]
    pub fn snapshot(&self) -> &WorkerSnapshot {
        &self.registration.snapshot
    }

    #[must_use]
    pub fn identity(&self) -> &WorkerIdentity {
        &self.registration.snapshot.identity
    }

    /// The identity-bound transport shared by dispatch, claimed commit, and
    /// application-owned control requests.
    #[must_use]
    pub fn upstream(&self) -> &WorkerUpstream {
        &self.upstream
    }
}

/// The complete application assembly created from one registered Worker identity.
///
/// The provisioner adds claim-bound material to the Host's authoritative Session
/// environment; the decorator wraps the authoritative Native/ACP/A2A router.
pub struct RegisteredWorkerApplication {
    decorator: AttemptExecutorDecorator,
    session_provisioner: Option<Arc<dyn ApplicationSessionProvisioner>>,
}

impl RegisteredWorkerApplication {
    #[must_use]
    pub fn new(decorator: AttemptExecutorDecorator) -> Self {
        Self {
            decorator,
            session_provisioner: None,
        }
    }

    #[must_use]
    pub fn with_session_provisioner(
        mut self,
        provisioner: Arc<dyn ApplicationSessionProvisioner>,
    ) -> Self {
        self.session_provisioner = Some(provisioner);
        self
    }

    fn into_parts(
        self,
    ) -> (
        AttemptExecutorDecorator,
        Option<Arc<dyn ApplicationSessionProvisioner>>,
    ) {
        (self.decorator, self.session_provisioner)
    }
}

/// Registration-time application factory.
pub type RegisteredApplicationFactory = Arc<
    dyn Fn(&RegisteredWorkerContext) -> Result<RegisteredWorkerApplication, String> + Send + Sync,
>;

/// Explicit resource-plane wiring for a database-less Worker.
///
/// The ports remain authoritative data-plane dependencies; the Worker only
/// materializes their already-authorized bindings for an attempt.
pub struct WorkerResourcePlane {
    ports: awaken_runtime_host::ResourcePlanePorts,
    validator: awaken_server::ResourceBindingValidatorPort,
    credentials: Option<awaken_control::InferenceMaterializationStores>,
}

impl WorkerResourcePlane {
    #[must_use]
    pub fn new(
        ports: awaken_runtime_host::ResourcePlanePorts,
        validator: awaken_server::ResourceBindingValidatorPort,
    ) -> Self {
        Self {
            ports,
            validator,
            credentials: None,
        }
    }

    #[must_use]
    pub fn with_repository_credentials(
        mut self,
        credentials: awaken_control::InferenceMaterializationStores,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    fn supports_repository_credentials(&self) -> bool {
        self.credentials.is_some()
    }
}

async fn shared_resource_wiring(
    credentials: Option<awaken_control::InferenceMaterializationStores>,
) -> Result<Option<WorkerResourcePlane>, Box<dyn std::error::Error + Send + Sync>> {
    let ports = awaken_server::shared_worker_resource_plane_from_env().await?;
    let validator = awaken_control::open_shared_resource_validator_from_env().await?;
    match (ports, validator) {
        (None, None) => Ok(None),
        (Some(ports), Some(validator)) => {
            let resources = WorkerResourcePlane::new(ports, validator);
            Ok(Some(match credentials {
                Some(credentials) => resources.with_repository_credentials(credentials),
                None => resources,
            }))
        }
        (Some(_), None) => Err(std::io::Error::other(
            "AWAKEN_RESOURCE_DATABASE_URL requires shared AWAKEN_ADMIN_DB on a remote worker",
        )
        .into()),
        (None, Some(_)) => Err(std::io::Error::other(
            "shared AWAKEN_ADMIN_DB requires AWAKEN_RESOURCE_DATABASE_URL on a resource worker",
        )
        .into()),
    }
}

fn shared_credential_backend(value: Option<&str>) -> bool {
    value
        .is_some_and(|value| value.starts_with("postgres://") || value.starts_with("postgresql://"))
}

struct WorkerProcessConfig {
    deployment: awaken_runtime_host::DeploymentConfig,
    manifest: StandardManifestConfig,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
    repository_credentials: bool,
}

impl WorkerProcessConfig {
    fn from_env() -> Self {
        let configured_grace = std::env::var("AWAKEN_WORKER_DRAIN_GRACE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        Self {
            deployment: awaken_runtime_host::DeploymentConfig::from_env(),
            manifest: StandardManifestConfig::from_env(),
            admin_listen: Some(
                std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "0.0.0.0:9090".to_string()),
            ),
            graceful_drain: grace_window(true, configured_grace),
            repository_credentials: shared_credential_backend(
                std::env::var("AWAKEN_CREDENTIAL_DB").ok().as_deref(),
            ),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Explicit,
    Standard,
}

impl std::fmt::Display for ManifestKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Explicit => "explicit",
            Self::Standard => "standard",
        })
    }
}

enum ManifestSource {
    Explicit(Box<WorkerManifest>),
    Standard {
        application_capabilities: std::collections::BTreeSet<String>,
    },
}

impl ManifestSource {
    fn kind(&self) -> ManifestKind {
        match self {
            Self::Explicit(_) => ManifestKind::Explicit,
            Self::Standard { .. } => ManifestKind::Standard,
        }
    }
}

enum ManifestSelection {
    Unset,
    Selected(ManifestSource),
    Conflict {
        first: ManifestKind,
        second: ManifestKind,
    },
}

impl ManifestSelection {
    fn select(&mut self, next: ManifestSource) {
        let current = std::mem::replace(self, Self::Unset);
        *self = match current {
            Self::Unset => Self::Selected(next),
            Self::Selected(previous) if previous.kind() == next.kind() => Self::Selected(next),
            Self::Selected(previous) => Self::Conflict {
                first: previous.kind(),
                second: next.kind(),
            },
            conflict @ Self::Conflict { .. } => conflict,
        };
    }
}

/// Typed metadata used only by the canonical standard manifest derivation.
///
/// Embedding code gets deterministic defaults. The process adapters explicitly
/// call [`StandardManifestConfig::from_env`] once when environment-driven Worker
/// deployment is desired; the Builder itself never rereads process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandardManifestConfig {
    build_digest: String,
    zone: Option<String>,
    extra_capabilities: std::collections::BTreeSet<String>,
    max_concurrent: u32,
}

impl Default for StandardManifestConfig {
    fn default() -> Self {
        Self {
            build_digest: env!("CARGO_PKG_VERSION").to_string(),
            zone: None,
            extra_capabilities: Default::default(),
            max_concurrent: std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(1),
        }
    }
}

impl StandardManifestConfig {
    #[must_use]
    pub fn new(build_digest: impl Into<String>) -> Self {
        Self {
            build_digest: build_digest.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
    }

    #[must_use]
    pub fn with_extra_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = String>,
    ) -> Self {
        self.extra_capabilities = capabilities.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_max_concurrent(mut self, max_concurrent: u32) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }

    /// Parse the legacy `AWAKEN_WORKER_*` manifest metadata once at a process
    /// composition edge.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.build_digest = std::env::var("AWAKEN_WORKER_BUILD_DIGEST")
            .unwrap_or_else(|_| config.build_digest.clone());
        config.zone = std::env::var("AWAKEN_WORKER_ZONE").ok();
        config.extra_capabilities = std::env::var("AWAKEN_WORKER_CAPABILITIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        config
    }
}

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
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
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
            resources: None,
            admin_listen: Some("0.0.0.0:9090".to_string()),
            graceful_drain: std::time::Duration::from_secs(20),
        }
    }

    fn with_process_config(mut self, config: WorkerProcessConfig) -> Self {
        self.deployment = config.deployment;
        self.standard_manifest_config = config.manifest;
        self.admin_listen = config.admin_listen;
        self.graceful_drain = config.graceful_drain;
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

    #[must_use]
    pub fn with_resource_plane(mut self, resources: WorkerResourcePlane) -> Self {
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

    /// Derive both inference and Session-secret materializers from one pair of
    /// authoritative credential stores.
    #[must_use]
    pub fn with_credential_stores(
        mut self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.materializer = Some(Arc::new(CredentialInferenceMaterializer::new(
            credentials.clone(),
            secrets.clone(),
        )));
        self.credential_materializer = Some(
            awaken_runtime_host::PinnedCredentialMaterializer::new(credentials, secrets),
        );
        self
    }

    /// Validate the immutable topology without registering or starting work.
    pub fn build(self) -> Result<WorkerNode, WorkerNodeBuildError> {
        if self.upstream.base_url().trim().is_empty() {
            return Err(WorkerNodeBuildError(
                "Worker upstream URL must not be empty".to_string(),
            ));
        }
        let resource_support = ResourceManifestSupport::from(self.resources.as_ref());
        let manifest = match self.manifest {
            ManifestSelection::Selected(ManifestSource::Explicit(manifest)) => *manifest,
            ManifestSelection::Selected(ManifestSource::Standard {
                application_capabilities,
            }) => derive_standard_manifest(StandardManifestInputs {
                deployment: &self.deployment,
                materializer: self.materializer.as_deref(),
                credential_materializer: self.credential_materializer.is_some(),
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
            resources: self.resources,
            admin_listen: self.admin_listen,
            graceful_drain: self.graceful_drain,
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

/// Why the Worker lifecycle is stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerShutdown {
    /// Developer/foreground stop: close admission and exit without an in-flight
    /// grace window.
    Prompt,
    /// Orchestrator scale-in: close admission and honor the configured grace.
    Graceful,
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
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
    graceful_drain: std::time::Duration,
}

#[derive(Clone)]
struct WorkerLifecycle {
    host: Arc<SharedHost>,
    control: WorkerControlClient,
    identity: WorkerIdentity,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
}

impl WorkerLifecycle {
    async fn begin_drain(&self, deadline_ms: Option<u64>) -> Result<(), String> {
        let remote = self.control.begin_drain(&self.identity, deadline_ms).await;
        self.host.begin_pool_drain().await;
        match remote? {
            RegistryMutation::Applied => Ok(()),
            other => Err(format!("worker drain rejected: {other:?}")),
        }
    }
}

/// Run this process as a database-less **worker** of the cell server at `upstream`.
///
/// 1. Inject the registered identity-bearing HTTP dispatch transport, so the
///    Worker drains the Control queue instead of a local one.
/// 2. Open the shared credential vault + secret store
///    the same way the Serve composition does — durable under `AWAKEN_MGMT_DIR`
///    (Option A shared-DB) or in-memory.
/// 3. Build a [`CredentialInferenceMaterializer`] over those stores, so each drained
///    run consumes only its snapshot-pinned inference access.
/// 4. When shared resource backends are explicitly configured, inject the same
///    File/Memory/Skill/lifecycle ports and Resource Catalog validator used by the
///    server. Otherwise advertise no resource capability.
/// 5. Assemble a [`SharedHost`] with an inert default executor and the mandatory
///    per-run materializer, pushing committed facts to `upstream`. A failed pin
///    never falls back to the inert executor.
/// 6. Start the dispatch pool and drain in the background until SIGINT / SIGTERM.
///
/// The injected remote dispatch store is the durable-ingress authority and enables
/// the pool directly; embedding does not require `AWAKEN_INGRESS=durable`.
pub async fn run(upstream: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    build_standard_worker(WorkerUpstream::new(upstream))
        .await?
        .run_until_shutdown()
        .await
}

async fn build_standard_worker(
    upstream: WorkerUpstream,
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let process = WorkerProcessConfig::from_env();
    let stores = awaken_control::open_inference_materialization_stores_from_env().await;
    build_worker_with_materialization_stores(upstream, process, stores).await
}

async fn build_worker_with_materialization_stores(
    upstream: WorkerUpstream,
    process: WorkerProcessConfig,
    stores: awaken_control::InferenceMaterializationStores,
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let resource_credentials = process.repository_credentials.then(|| stores.clone());
    let resources = shared_resource_wiring(resource_credentials).await?;
    let mut builder = WorkerNodeBuilder::new(upstream)
        .with_process_config(process)
        .with_credential_stores(stores.credentials, stores.secrets)
        .with_standard_manifest(Default::default());
    if let Some(resources) = resources {
        builder = builder.with_resource_plane(resources);
    }
    Ok(builder.build()?)
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
        WorkerProcessConfig::from_env(),
        materializer,
    )
    .await?
    .run_until_shutdown()
    .await
}

async fn build_secretless_worker(
    upstream: WorkerUpstream,
    process: WorkerProcessConfig,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> Result<WorkerNode, Box<dyn std::error::Error + Send + Sync>> {
    let resources = shared_resource_wiring(None).await?;
    let mut builder = WorkerNodeBuilder::new(upstream)
        .with_process_config(process)
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
        let resource_credentials = self
            .resources
            .as_ref()
            .and_then(|resources| resources.credentials.clone());
        let host = match self.resources {
            Some(resources) => SharedHost::new_with_resource_plane(
                Arc::new(NoModelConfiguredExecutor),
                "worker",
                resources.ports,
            ),
            None => SharedHost::new(Arc::new(NoModelConfiguredExecutor), "worker"),
        };
        let mut host = host
            .with_deployment_config(self.deployment)
            .with_worker_upstream(upstream)
            .with_dispatch_store(dispatch_store)
            .with_remote_attempt_executor(awaken_server::a2a_attempt_executor());
        if let Some(materializer) = &self.materializer {
            host = host.with_inference_materializer(materializer.clone());
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

        // Serve only the ACP CLI capability this worker advertises. The run's snapshot
        // selects the matching backend and supplies its published provider access.
        host = host
            .with_acp_from_deployment(
                awaken_server::relay_hand_executor_factory(),
                self.credential_materializer,
            )
            .await;

        let host = Arc::new(host);
        if let Some(validator) = resource_validator {
            let managed =
                awaken_server::ManagedHost::new(host.clone()).with_resource_validator(validator);
            if let Some(credentials) = resource_credentials {
                let _managed =
                    managed.with_credentials(credentials.credentials, credentials.secrets);
            } else {
                let _managed = managed;
            }
        }
        let lifecycle = Arc::new(WorkerLifecycle {
            host: host.clone(),
            control: control.clone(),
            identity: registration.snapshot.identity,
            materializer: self.materializer,
        });
        // Publish Ready before starting the pull loop. Starting the pool while the
        // directory still says Starting creates a tight claim/reject race; publishing
        // first is safe because any assignment remains queued until this process starts
        // polling immediately below.
        let initial = control
            .heartbeat(
                &lifecycle.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    available_credentials: credential_observations(
                        lifecycle.materializer.as_deref(),
                    ),
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

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_incarnation_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceManifestSupport {
    None,
    Session,
    SessionWithRepositoryCredentials,
}

impl From<Option<&WorkerResourcePlane>> for ResourceManifestSupport {
    fn from(resources: Option<&WorkerResourcePlane>) -> Self {
        match resources {
            Some(resources) if resources.supports_repository_credentials() => {
                Self::SessionWithRepositoryCredentials
            }
            Some(_) => Self::Session,
            None => Self::None,
        }
    }
}

struct StandardManifestInputs<'a> {
    deployment: &'a awaken_runtime_host::DeploymentConfig,
    materializer: Option<&'a dyn InferenceExecutorMaterializer>,
    credential_materializer: bool,
    resource_support: ResourceManifestSupport,
    application_capabilities: std::collections::BTreeSet<String>,
    config: &'a StandardManifestConfig,
}

fn derive_standard_manifest(inputs: StandardManifestInputs<'_>) -> WorkerManifest {
    let (sandbox, backend) = inputs.deployment.sandbox_support();
    let mut capabilities = std::collections::BTreeSet::from([
        NATIVE_RUNTIME_CAPABILITY.to_string(),
        awaken_runtime_contract::A2A_RUNTIME_CAPABILITY.to_string(),
    ]);
    capabilities.extend(
        inputs
            .materializer
            .into_iter()
            .flat_map(InferenceExecutorMaterializer::supported_access_schemes)
            .map(|capability| (*capability).to_string()),
    );
    match inputs.resource_support {
        ResourceManifestSupport::None => {}
        ResourceManifestSupport::Session => {
            capabilities.insert(SESSION_RESOURCES_CAPABILITY.to_string());
        }
        ResourceManifestSupport::SessionWithRepositoryCredentials => {
            capabilities.insert(SESSION_RESOURCES_CAPABILITY.to_string());
            capabilities.insert(REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    capabilities.extend(inputs.application_capabilities);
    if let Some(profile) = &inputs.deployment.acp {
        capabilities.extend(profile.cli_ids().map(|cli| format!("acp:{cli}")));
    }
    capabilities.extend(inputs.config.extra_capabilities.iter().cloned());
    let mut credential_realization = inputs
        .materializer
        .map(InferenceExecutorMaterializer::credential_realization_capabilities)
        .unwrap_or_default();
    if inputs.credential_materializer && inputs.deployment.acp.is_some() {
        credential_realization
            .holders
            .insert(awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            ));
        credential_realization
            .material_sources
            .insert(awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference);
        credential_realization
            .realization_kinds
            .insert(awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment);
    }
    if let Some(capability) = credential_realization
        .manifest_capability()
        .expect("credential realization capability serializes")
    {
        capabilities.insert(capability);
    }
    WorkerManifest {
        build_digest: inputs.config.build_digest.clone(),
        capabilities,
        zone: inputs.config.zone.clone(),
        sandbox,
        sandbox_backends: std::collections::BTreeSet::from([backend.to_string()]),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        checkpoint_formats: std::collections::BTreeSet::from(["stream-v1".to_string()]),
        capacity: WorkerCapacity {
            max_concurrent: inputs.config.max_concurrent,
            ..WorkerCapacity::default()
        },
        ..WorkerManifest::default()
    }
}

fn spawn_heartbeat(
    lifecycle: Arc<WorkerLifecycle>,
    mut sequence: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.tick().await;
        loop {
            interval.tick().await;
            let mutation = lifecycle
                .control
                .heartbeat(
                    &lifecycle.identity,
                    WorkerHeartbeat {
                        sequence,
                        ready: lifecycle.host.pool_accepting_work(),
                        in_flight: lifecycle.host.pool_in_flight(),
                        available_credentials: credential_observations(
                            lifecycle.materializer.as_deref(),
                        ),
                    },
                )
                .await;
            sequence = sequence.saturating_add(1);
            match mutation {
                Ok(RegistryMutation::Applied) => {}
                Ok(other) => {
                    eprintln!("worker heartbeat lost authority: {other:?}; draining locally");
                    lifecycle.host.begin_pool_drain().await;
                    break;
                }
                Err(error) => {
                    eprintln!("worker heartbeat failed: {error}");
                }
            }
        }
    })
}

fn credential_observations(
    materializer: Option<&dyn InferenceExecutorMaterializer>,
) -> std::collections::BTreeSet<awaken_worker_contract::WorkerCredentialRevision> {
    materializer
        .map(InferenceExecutorMaterializer::available_credential_refs)
        .unwrap_or_default()
        .into_iter()
        .map(
            |credential| awaken_worker_contract::WorkerCredentialRevision {
                source_id: credential.id,
                revision: credential.revision,
            },
        )
        .collect()
}

async fn wait_for_in_flight(host: &SharedHost, grace: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    while host.pool_in_flight() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The grace policy (pure): a SIGTERM (orchestrator scale-in) waits `configured` or
/// the 20s default so in-flight runs finish; a SIGINT (developer ctrl-c) exits
/// promptly (zero) so the foreground stop is snappy.
fn grace_window(graceful: bool, configured_secs: Option<u64>) -> std::time::Duration {
    if !graceful {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs(configured_secs.unwrap_or(20))
}

#[cfg(test)]
mod grace_tests {
    use std::sync::Arc;

    use awaken_runtime_contract::llm::LlmExecutor;

    use super::{
        InferenceExecutorMaterializer, ResourceManifestSupport, StandardManifestConfig,
        StandardManifestInputs, WorkerNodeBuilder, credential_observations,
        derive_standard_manifest, grace_window, shared_credential_backend,
    };

    struct SchemeMaterializer;

    impl InferenceExecutorMaterializer for SchemeMaterializer {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &["test-access/v1"]
        }

        fn available_credential_refs(
            &self,
        ) -> std::collections::BTreeSet<awaken_runtime_contract::CredentialRef> {
            std::collections::BTreeSet::from([awaken_runtime_contract::CredentialRef {
                id: "cred:worker".into(),
                revision: 4,
            }])
        }

        fn credential_realization_capabilities(
            &self,
        ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
            awaken_runtime_contract::CredentialRealizationCapabilities {
                holders: [awaken_runtime_contract::PlaintextHolder::new(
                    awaken_runtime_contract::PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                )]
                .into_iter()
                .collect(),
                material_sources: [
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                ]
                .into_iter()
                .collect(),
                realization_kinds: [
                    awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
                ]
                .into_iter()
                .collect(),
            }
        }

        fn materialize_pinned(
            &self,
            _candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Option<Arc<dyn LlmExecutor>> {
            None
        }
    }

    fn deployment() -> awaken_runtime_host::DeploymentConfig {
        awaken_runtime_host::DeploymentConfig::ephemeral()
    }

    #[test]
    fn worker_manifest_derives_materialization_capabilities_from_the_adapter() {
        let materializer = SchemeMaterializer;
        let deployment = deployment();
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: Some(&materializer),
            credential_materializer: false,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });

        assert!(manifest.capabilities.contains("native-runtime"));
        assert!(
            manifest
                .capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
        );
        assert!(manifest.capabilities.contains("test-access/v1"));
        let realization =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &manifest.capabilities,
            )
            .expect("credential realization capability decodes");
        assert!(
            realization
                .holders
                .contains(&awaken_runtime_contract::PlaintextHolder::new(
                    awaken_runtime_contract::PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                ))
        );
        assert!(
            realization.realization_kinds.contains(
                &awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter
            )
        );
        assert_eq!(
            credential_observations(Some(&materializer)),
            std::collections::BTreeSet::from([awaken_worker_contract::WorkerCredentialRevision {
                source_id: "cred:worker".into(),
                revision: 4,
            }])
        );
    }

    #[test]
    fn standard_builder_derives_from_its_installed_materializer() {
        let worker =
            WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new("http://control"))
                .with_inference_materializer(Arc::new(SchemeMaterializer))
                .with_standard_manifest(Default::default())
                .build()
                .expect("installed standard topology is valid");

        assert!(worker.manifest().capabilities.contains("test-access/v1"));
    }

    /// Credential capability derivation cause graph:
    ///
    /// inference materializer -> Worker provider adapter; exact credential
    /// materializer + installed ACP profile -> Workload process-secret delivery.
    /// Missing either ACP cause advertises neither half of that workload tuple.
    ///
    /// | Rule | materializer | credential store | ACP | Result |
    /// |---|---|---|---|---|
    /// | C1 | credential-aware | - | - | Worker/provider-adapter |
    /// | C2 | - | T | T | Workload/process-secret |
    /// | C3 | - | T | F | no Workload/process-secret |
    #[test]
    fn standard_manifest_advertises_only_installed_credential_mechanisms() {
        let mut acp = deployment();
        acp.acp = Some(
            awaken_runtime_host::AcpWorkerProfile::new(["claude".to_string()], None)
                .expect("one ACP profile"),
        );
        let workload = derive_standard_manifest(StandardManifestInputs {
            deployment: &acp,
            materializer: None,
            credential_materializer: true,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        let workload_realization =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &workload.capabilities,
            )
            .expect("ACP credential realization capability decodes");
        assert!(workload_realization.holders.contains(
            &awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )
        ));
        assert!(workload_realization.realization_kinds.contains(
            &awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment
        ));

        let without_acp = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment(),
            materializer: None,
            credential_materializer: true,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &without_acp.capabilities,
            )
            .expect("empty credential realization capability decodes")
            .is_empty()
        );
    }

    #[test]
    /// Cause graph: C1 Resource plane installed -> session capability; C2 exact
    /// Repository credential backend installed -> Repository capability; C3
    /// inference materializer installed -> its exact realization evidence.
    /// C1/C2 never synthesize C3: a Repository transport cannot authorize an
    /// inference or MCP Worker relay merely because both hold material in Worker.
    ///
    /// | Rule | C1 | C2 | C3 | Session | Repository | Credential evidence |
    /// |---|---|---|---|---|---|---|
    /// | W1 | F | - | F | F | F | empty |
    /// | W2 | T | F | F | T | F | empty |
    /// | W3 | T | T | F | T | T | empty |
    /// | W4 | F | - | T | F | F | materializer evidence only |
    fn worker_manifest_advertises_only_installed_resource_seams() {
        let deployment = deployment();
        let without = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: None,
            credential_materializer: false,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(
            !without
                .capabilities
                .contains(super::SESSION_RESOURCES_CAPABILITY)
        );
        assert!(
            !without
                .capabilities
                .contains(super::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        assert!(
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &without.capabilities,
            )
            .expect("W1 evidence decodes")
            .is_empty()
        );

        let secretless = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: None,
            credential_materializer: false,
            resource_support: ResourceManifestSupport::Session,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(
            secretless
                .capabilities
                .contains(super::SESSION_RESOURCES_CAPABILITY)
        );
        assert!(
            !secretless
                .capabilities
                .contains(super::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        assert!(
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &secretless.capabilities,
            )
            .expect("W2 evidence decodes")
            .is_empty()
        );

        let credentialed = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: None,
            credential_materializer: false,
            resource_support: ResourceManifestSupport::SessionWithRepositoryCredentials,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(
            credentialed
                .capabilities
                .contains(super::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        let evidence =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &credentialed.capabilities,
            )
            .expect("W3 evidence decodes");
        assert!(evidence.is_empty(), "W3");

        let materializer = SchemeMaterializer;
        let inference = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: Some(&materializer),
            credential_materializer: false,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(
            !inference
                .capabilities
                .contains(super::SESSION_RESOURCES_CAPABILITY)
        );
        assert!(
            !inference
                .capabilities
                .contains(super::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        let evidence =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &inference.capabilities,
            )
            .expect("W4 evidence decodes");
        assert_eq!(
            evidence.holders,
            std::collections::BTreeSet::from([awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),])
        );
        assert_eq!(
            evidence.material_sources,
            std::collections::BTreeSet::from([
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            ])
        );
        assert_eq!(
            evidence.realization_kinds,
            std::collections::BTreeSet::from([
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ])
        );
    }

    #[test]
    fn worker_manifest_includes_explicit_application_capabilities() {
        let deployment = deployment();
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: None,
            credential_materializer: false,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: std::collections::BTreeSet::from([
                "application:flow-envelope/v1".to_string(),
                "application:flow-tools/v1".to_string(),
            ]),
            config: &StandardManifestConfig::default(),
        });

        assert!(
            manifest
                .capabilities
                .contains("application:flow-envelope/v1")
        );
        assert!(manifest.capabilities.contains("application:flow-tools/v1"));
    }

    #[test]
    fn standard_manifest_uses_one_typed_metadata_source() {
        let deployment = deployment();
        let config = StandardManifestConfig::new("build:test")
            .with_zone("zone:test")
            .with_extra_capabilities(["operator:test/v1".to_string()])
            .with_max_concurrent(7);
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            materializer: None,
            credential_materializer: false,
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &config,
        });

        assert_eq!(manifest.build_digest, "build:test");
        assert_eq!(manifest.zone.as_deref(), Some("zone:test"));
        assert!(manifest.capabilities.contains("operator:test/v1"));
        assert_eq!(manifest.capacity.max_concurrent, 7);
    }

    #[test]
    fn repository_credentials_require_an_explicit_shared_backend() {
        assert!(!shared_credential_backend(None));
        assert!(!shared_credential_backend(Some(
            "/var/lib/awaken/credential.db"
        )));
        assert!(shared_credential_backend(Some("postgres://db/credentials")));
        assert!(shared_credential_backend(Some(
            "postgresql://db/credentials"
        )));
    }

    #[test]
    fn sigint_exits_promptly_sigterm_waits() {
        assert!(
            grace_window(false, None).is_zero(),
            "ctrl-c drains then exits at once"
        );
        assert!(
            grace_window(false, Some(99)).is_zero(),
            "a configured grace never delays a foreground ctrl-c"
        );
        assert_eq!(
            grace_window(true, None).as_secs(),
            20,
            "SIGTERM default grace is 20s"
        );
        assert_eq!(
            grace_window(true, Some(5)).as_secs(),
            5,
            "the grace window is configurable"
        );
    }

    // Boundary: an explicitly configured zero grace collapses SIGTERM to the
    // prompt-exit behavior — the orchestrator asked for no in-flight wait, so a
    // graceful stop must not silently substitute the 20s default.
    #[test]
    fn a_configured_zero_grace_exits_immediately_even_on_sigterm() {
        assert!(
            grace_window(true, Some(0)).is_zero(),
            "grace of 0 means no wait, not the default"
        );
    }
}
