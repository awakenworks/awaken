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
use awaken_runtime_host::{AttemptExecutorDecorator, WorkerControlClient, WorkerUpstream};
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

/// Registration-time application factory. The returned decorator is applied to
/// every Session's authoritative Native/ACP/A2A attempt router.
pub type RegisteredDecoratorFactory =
    Arc<dyn Fn(&RegisteredWorkerContext) -> Result<AttemptExecutorDecorator, String> + Send + Sync>;

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

    fn with_repository_credentials(
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
    manifest: Option<WorkerManifest>,
    application_decorator_factory: Option<RegisteredDecoratorFactory>,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    acp_credentials: Option<awaken_runtime_host::PinnedCredentialMaterializer>,
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
}

impl WorkerNodeBuilder {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self {
            upstream,
            manifest: None,
            application_decorator_factory: None,
            materializer: None,
            acp_credentials: None,
            resources: None,
            admin_listen: Some("0.0.0.0:9090".to_string()),
        }
    }

    #[must_use]
    pub fn with_manifest(mut self, manifest: WorkerManifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Install the only application execution extension: a factory evaluated
    /// after registration whose decorator wraps the built-in Session router.
    #[must_use]
    pub fn with_application_decorator_factory(
        mut self,
        factory: RegisteredDecoratorFactory,
    ) -> Self {
        self.application_decorator_factory = Some(factory);
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

    fn with_acp_credentials(
        mut self,
        credentials: awaken_runtime_host::PinnedCredentialMaterializer,
    ) -> Self {
        self.acp_credentials = Some(credentials);
        self
    }

    /// Validate the immutable topology without registering or starting work.
    pub fn build(self) -> Result<WorkerNode, WorkerNodeBuildError> {
        if self.upstream.base_url().trim().is_empty() {
            return Err(WorkerNodeBuildError(
                "Worker upstream URL must not be empty".to_string(),
            ));
        }
        let manifest = self.manifest.ok_or_else(|| {
            WorkerNodeBuildError("Worker manifest must be supplied explicitly".to_string())
        })?;
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
        Ok(WorkerNode {
            upstream: self.upstream,
            manifest,
            application_decorator_factory: self.application_decorator_factory,
            materializer: self.materializer,
            acp_credentials: self.acp_credentials,
            resources: self.resources,
            admin_listen: self.admin_listen,
        })
    }
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
    application_decorator_factory: Option<RegisteredDecoratorFactory>,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    acp_credentials: Option<awaken_runtime_host::PinnedCredentialMaterializer>,
    resources: Option<WorkerResourcePlane>,
    admin_listen: Option<String>,
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
    run_with_standard_environment(WorkerUpstream::new(upstream), Default::default(), None).await
}

/// Run the standard database-less Worker with one registered application
/// decorator around its authoritative Session attempt router.
///
/// Credential, resource-plane, ACP, manifest, and lifecycle assembly remain
/// identical to [`run`]; the application contributes only the post-registration
/// wrapper.
pub async fn run_with_application_decorator(
    upstream: WorkerUpstream,
    application_capabilities: std::collections::BTreeSet<String>,
    factory: RegisteredDecoratorFactory,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_standard_environment(upstream, application_capabilities, Some(factory)).await
}

async fn run_with_standard_environment(
    upstream: WorkerUpstream,
    application_capabilities: std::collections::BTreeSet<String>,
    application: Option<RegisteredDecoratorFactory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stores = awaken_control::open_inference_materialization_stores_from_env().await;
    let resource_credentials =
        shared_credential_backend(std::env::var("AWAKEN_CREDENTIAL_DB").ok().as_deref())
            .then(|| stores.clone());
    let resources = shared_resource_wiring(resource_credentials).await?;
    let materializer =
        CredentialInferenceMaterializer::new(stores.credentials.clone(), stores.secrets.clone());
    let materializer: Arc<dyn InferenceExecutorMaterializer> = Arc::new(materializer);
    let manifest = worker_manifest(
        Some(materializer.as_ref()),
        resources.is_some(),
        resources
            .as_ref()
            .is_some_and(WorkerResourcePlane::supports_repository_credentials),
        application_capabilities,
    );
    run_configured_worker(
        upstream,
        manifest,
        materializer,
        Some(awaken_runtime_host::PinnedCredentialMaterializer::new(
            stores.credentials,
            stores.secrets,
        )),
        resources,
        application,
    )
    .await
}

/// Run a genuinely secretless worker with a deployment-provided materializer.
/// It receives each durable run's snapshot-pinned inference access and may
/// realize an executor through a remote broker without opening a credential
/// vault or persisting provider keys in this process.
pub async fn run_with_inference_materializer(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_upstream_and_inference_materializer(WorkerUpstream::new(upstream), materializer).await
}

/// Run a secretless Worker over one caller-configured control transport.
///
/// Managed compositions use this entrypoint so the same [`WorkerUpstream`]
/// (including its mTLS client and logical Worker id) is cloned through
/// registration, claim/renew/settle, claimed commit, and ordinary commit. This
/// function never reconstructs the transport from its URL.
pub async fn run_with_upstream_and_inference_materializer(
    upstream: WorkerUpstream,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_upstream_application_and_inference_materializer(
        upstream,
        materializer,
        Default::default(),
        None,
    )
    .await
}

/// Run a caller-materialized Worker with an optional registered application
/// decorator. This is the secretless counterpart of
/// [`run_with_application_decorator`].
pub async fn run_with_upstream_application_and_inference_materializer(
    upstream: WorkerUpstream,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    application_capabilities: std::collections::BTreeSet<String>,
    application: Option<RegisteredDecoratorFactory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resources = shared_resource_wiring(None).await?;
    let manifest = worker_manifest(
        Some(materializer.as_ref()),
        resources.is_some(),
        false,
        application_capabilities,
    );
    run_configured_worker(
        upstream,
        manifest,
        materializer,
        None,
        resources,
        application,
    )
    .await
}

async fn run_configured_worker(
    upstream: WorkerUpstream,
    manifest: WorkerManifest,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
    acp_credentials: Option<awaken_runtime_host::PinnedCredentialMaterializer>,
    resources: Option<WorkerResourcePlane>,
    application: Option<RegisteredDecoratorFactory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = WorkerNodeBuilder::new(upstream)
        .with_manifest(manifest)
        .with_inference_materializer(materializer)
        .with_optional_resource_plane(resources)
        .with_admin_listen(configured_admin_listen());
    if let Some(credentials) = acp_credentials {
        builder = builder.with_acp_credentials(credentials);
    }
    if let Some(factory) = application {
        builder = builder.with_application_decorator_factory(factory);
    }
    builder.build()?.run_until_shutdown().await
}

fn configured_admin_listen() -> String {
    std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "0.0.0.0:9090".to_string())
}

impl WorkerNodeBuilder {
    fn with_optional_resource_plane(mut self, resources: Option<WorkerResourcePlane>) -> Self {
        self.resources = resources;
        self
    }
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
        let application_decorator = match self.application_decorator_factory {
            Some(factory) => match factory(&RegisteredWorkerContext::new(
                registration.clone(),
                upstream.clone(),
            )) {
                Ok(decorator) => Some(decorator),
                Err(error) => {
                    let _ = control.deregister(&registration.snapshot.identity).await;
                    return Err(std::io::Error::other(format!(
                        "application decorator factory failed: {error}"
                    ))
                    .into());
                }
            },
            None => None,
        };
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
            .with_worker_upstream(upstream)
            .with_dispatch_store(dispatch_store)
            .with_remote_attempt_executor(awaken_server::a2a_attempt_executor());
        if let Some(materializer) = &self.materializer {
            host = host.with_inference_materializer(materializer.clone());
        }
        if let Some(decorator) = application_decorator {
            host = host.with_application_attempt_decorator(decorator);
        }
        awaken_server::install_platform_memory_data_plane(&host);

        // Serve only the ACP CLI capability this worker advertises. The run's snapshot
        // selects the matching backend and supplies its published provider access.
        host = host
            .with_acp_from_deployment(
                awaken_server::relay_hand_executor_factory(),
                self.acp_credentials,
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
        let grace = drain_grace(graceful);
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

fn worker_manifest(
    materializer: Option<&dyn InferenceExecutorMaterializer>,
    resource_support: bool,
    repository_credential_support: bool,
    application_capabilities: std::collections::BTreeSet<String>,
) -> WorkerManifest {
    use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};
    let tier = std::env::var("AWAKEN_SANDBOX_TIER").unwrap_or_else(|_| "namespace".to_string());
    let (sandbox, backend) = match tier.as_str() {
        "local" => (WorkerManifest::default().sandbox, "local"),
        "docker" | "podman" | "k8s" => (
            SandboxCapabilities {
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                secret_egress_substitution: false,
                resource_limits: true,
                custom_rootfs: true,
            },
            tier.as_str(),
        ),
        _ => (
            SandboxCapabilities {
                isolation: IsolationClass::Namespace,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
            },
            "namespace",
        ),
    };
    let mut capabilities = std::collections::BTreeSet::from([
        NATIVE_RUNTIME_CAPABILITY.to_string(),
        awaken_runtime_contract::A2A_RUNTIME_CAPABILITY.to_string(),
    ]);
    capabilities.extend(
        materializer
            .into_iter()
            .flat_map(InferenceExecutorMaterializer::supported_access_schemes)
            .map(|capability| (*capability).to_string()),
    );
    if resource_support {
        capabilities.insert(SESSION_RESOURCES_CAPABILITY.to_string());
        if repository_credential_support {
            capabilities.insert(REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    capabilities.extend(application_capabilities);
    if let Some(cli) = std::env::var("AWAKEN_ACP_CLI")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        capabilities.insert(format!("acp:{cli}"));
    }
    capabilities.extend(
        std::env::var("AWAKEN_WORKER_CAPABILITIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
    WorkerManifest {
        build_digest: std::env::var("AWAKEN_WORKER_BUILD_DIGEST")
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
        capabilities,
        zone: std::env::var("AWAKEN_WORKER_ZONE").ok(),
        sandbox,
        sandbox_backends: std::collections::BTreeSet::from([backend.to_string()]),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        checkpoint_formats: std::collections::BTreeSet::from(["stream-v1".to_string()]),
        capacity: WorkerCapacity {
            max_concurrent: std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(1),
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

/// The graceful-drain window: how long to let in-flight runs finish after we stop
/// claiming. Reads `AWAKEN_WORKER_DRAIN_GRACE_SECS` and delegates to the pure
/// [`grace_window`] so the policy is unit-testable without touching the environment.
fn drain_grace(graceful: bool) -> std::time::Duration {
    let configured = std::env::var("AWAKEN_WORKER_DRAIN_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    grace_window(graceful, configured)
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
        InferenceExecutorMaterializer, credential_observations, grace_window,
        shared_credential_backend, worker_manifest,
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

        fn materialize_pinned(
            &self,
            _candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        ) -> Option<Arc<dyn LlmExecutor>> {
            None
        }
    }

    #[test]
    fn worker_manifest_derives_materialization_capabilities_from_the_adapter() {
        let materializer = SchemeMaterializer;
        let manifest = worker_manifest(Some(&materializer), false, false, Default::default());

        assert!(manifest.capabilities.contains("native-runtime"));
        assert!(
            manifest
                .capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
        );
        assert!(manifest.capabilities.contains("test-access/v1"));
        assert_eq!(
            credential_observations(Some(&materializer)),
            std::collections::BTreeSet::from([awaken_worker_contract::WorkerCredentialRevision {
                source_id: "cred:worker".into(),
                revision: 4,
            }])
        );
    }

    #[test]
    fn worker_manifest_advertises_only_installed_resource_seams() {
        let without = worker_manifest(None, false, false, Default::default());
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

        let secretless = worker_manifest(None, true, false, Default::default());
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

        let credentialed = worker_manifest(None, true, true, Default::default());
        assert!(
            credentialed
                .capabilities
                .contains(super::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
    }

    #[test]
    fn worker_manifest_includes_explicit_application_capabilities() {
        let manifest = worker_manifest(
            None,
            false,
            false,
            std::collections::BTreeSet::from([
                "application:flow-envelope/v1".to_string(),
                "application:flow-tools/v1".to_string(),
            ]),
        );

        assert!(
            manifest
                .capabilities
                .contains("application:flow-envelope/v1")
        );
        assert!(manifest.capabilities.contains("application:flow-tools/v1"));
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
