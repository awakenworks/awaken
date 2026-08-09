//! `awaken-coordinator` — the Coordinator application and protocol data plane.
//!
//! It composes one protocol-neutral [`SharedHost`] (from `awaken-runtime-host`,
//! the thread-keyed session substrate) and mounts public protocol adapters over
//! it. Each adapter is a thin port implementation that translates its own wire
//! vocabulary to the host's neutral operations; because every adapter keys by the
//! same thread id and drives the same coordinator, a turn started through one
//! protocol can be resumed or observed through another on the *same thread*.
//!
//! This crate is the Coordinator owner: its canonical
//! [`build_coordinator_component`] assembles Deployment/Session scheduling and
//! the session surface + protocol adapters
//! (`mount` / `mount_with_managed`, under `test-support`), exact published-model credential
//! materialization, the model publication resolver, the inert no-model placeholder,
//! workspace path addressing, and the Worker
//! role helper (the hand is now the separate `awaken-sandbox` execution-plane
//! binary). Its sibling **authoring / authz plane** lives in
//! `awaken-control`; neither depends on the other, and `awaken-cli` is the
//! composition root that weaves them into one management router. The service layer
//! (the host, the two port adapters, the per-plane resource routers) lives in
//! `awaken-runtime-host`.

pub mod admin;
pub mod application_access;
pub mod brokered_inference;
pub mod console;
pub mod control_service_boundary;
mod coordinator_component;
mod coordinator_persistence;
pub mod data_subject_boundary;
pub mod inference_materializer;
pub mod mcp_export;
pub mod model_directory;
pub mod model_discovery;
pub mod model_resolver;
mod oauth_refresh;
mod runtime_authority;
pub mod webhooks;
pub mod worker_observation_boundary;
pub mod worker_placement;
mod worker_registry;
pub mod workspace_path;

pub use awaken_protocol_managed::ModelDirectory;
pub use coordinator_component::{
    CoordinatorBuildError, CoordinatorComponent, CoordinatorDependencies,
    build_coordinator_component, restore_deployment_application,
};
#[cfg(any(test, feature = "test-support"))]
pub use coordinator_persistence::init_scenario_runtime;
pub use coordinator_persistence::{
    CoordinatorPersistence, migrate_postgres_schema as migrate_postgres_coordinator_schema,
    open as open_coordinator_persistence, open_existing as open_existing_coordinator_persistence,
};
pub use oauth_refresh::{VaultRefreshFactory, VaultRefresher};

use std::sync::Arc;

mod a2a_security;
mod dream;

use awaken_protocol_managed::{ManagedState, router};
use awaken_session_contract::RunApplication;
use axum::Router;

// This data-plane composition depends on each authoritative owner directly.
pub use awaken_acp_application::{
    LocalAcpPreparation, PreparedAcpCapabilities, ensure_workspace_bindings,
};
pub use awaken_config_service::{ConfigService, capabilities_router, config_router};
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed::{
    ResourcesRouterInput, default_models, models_router, resources_router,
};

/// Build the canonical MemoryStore identity/lifecycle application port for
/// composition roots that already own the Resources persistence ports.
pub fn memory_store_application(
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    purge: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
) -> Arc<dyn awaken_resource_contract::MemoryStoreApplicationService> {
    Arc::new(awaken_resource_application::MemoryStoreApplication::new(
        catalog, purge,
    ))
}
pub use awaken_run_ingress_http::durable_ops_router;
pub use awaken_runtime_host::{
    ExtMcpProbe, HostResume, ManagedHost, NoModelConfiguredExecutor, RunApplicationHost,
    SharedHost, ThreadEvent, ThreadEventHub, UNCONFIGURED_MODEL_REF, advertised_tools,
};
pub use awaken_sandbox_local::content_fingerprint;
pub use awaken_worker_registry::{WorkerDirectory, WorkerObservationSource};
pub use worker_registry::WorkerDirectoryHandle;
#[cfg(any(test, feature = "test-support"))]
pub use worker_registry::test_directory as test_worker_directory;

/// Canonical trusted-host ACP composition for outer product roots. This
/// service-layer boundary joins reusable discovery with the production wire.
pub async fn prepare_local_acp(
    input: awaken_acp_application::LocalAcpPreparation,
) -> Result<awaken_acp_application::PreparedAcpCapabilities, String> {
    awaken_acp_application::prepare_host_acp_with(
        input,
        std::sync::Arc::new(awaken_acp_application::AcpHostDiscovery::local(
            std::time::Duration::from_secs(3),
        )),
        std::sync::Arc::new(awaken_acp_application::NpmWrapperInstaller),
        std::sync::Arc::new(awaken_acp_application::HostAcpCapabilityNegotiator::new(
            std::time::Duration::from_secs(10),
            std::sync::Arc::new(awaken_protocol_acp::ProtocolAcpCapabilityHandshake),
        )),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformMemoryProjection {
    FusePreferred,
    CopyOnly,
}

fn platform_memory_projection(tier: awaken_runtime_host::SandboxTier) -> PlatformMemoryProjection {
    use awaken_runtime_host::SandboxTier;

    match tier {
        // Both run in a host-visible mount namespace. Namespace binds the host
        // projection into bwrap after realization, retaining live write-through.
        SandboxTier::Local | SandboxTier::Namespace => PlatformMemoryProjection::FusePreferred,
        // Docker and Podman cannot bind a host FUSE mount reliably. Kubernetes
        // owns native memory sidecars, but the same host adapter can still serve
        // housekeeping paths, so its portable fallback must remain bind-safe.
        SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => {
            PlatformMemoryProjection::CopyOnly
        }
    }
}

/// Assemble the governed MemoryRepository data plane with its worker-side mount adapter.
/// Authorization has already selected workspace/store/access before this adapter
/// sees an opaque store id; no IAM vocabulary crosses this seam.
pub fn install_platform_memory_data_plane(host: &SharedHost) {
    if !host.has_memory_mounter() {
        let repository = host.memory_repository();
        let mounter = match platform_memory_projection(host.sandbox_tier()) {
            PlatformMemoryProjection::FusePreferred => {
                awaken_sandbox_memoryd::MemoryStoreMounter::new(repository)
            }
            PlatformMemoryProjection::CopyOnly => {
                awaken_sandbox_memoryd::MemoryStoreMounter::copy_only(repository)
            }
        };
        host.install_memory_mounter(Arc::new(mounter));
    }
}

#[cfg(test)]
mod platform_memory_projection_tests {
    use super::{PlatformMemoryProjection, platform_memory_projection};
    use awaken_runtime_host::SandboxTier;

    /// Cause/effect graph: C1 host-visible projection namespace (Local/Namespace)
    /// produces E1 FUSE-preferred write-through; C2 OCI boundary (Docker/Podman)
    /// produces E2 copy-bind safety; C3 native-sidecar Kubernetes produces E3 a
    /// copy-safe host fallback while the runtime owns Session mounts.
    ///
    /// Decision table:
    /// | Rule | tier                 | host can expose FUSE | native sidecar | effect         |
    /// | P1   | Local, Namespace     | yes                  | no             | FusePreferred  |
    /// | P2   | Docker, Podman       | no                   | no             | CopyOnly       |
    /// | P3   | K8s                  | irrelevant           | yes            | CopyOnly host  |
    ///
    /// Enumerating every closed enum member also makes a newly added tier fail
    /// compilation until its projection contract is classified.
    #[test]
    fn sandbox_tier_selects_one_bind_compatible_memory_projection() {
        for tier in [SandboxTier::Local, SandboxTier::Namespace] {
            assert_eq!(
                platform_memory_projection(tier),
                PlatformMemoryProjection::FusePreferred
            );
        }
        for tier in [SandboxTier::Docker, SandboxTier::Podman, SandboxTier::K8s] {
            assert_eq!(
                platform_memory_projection(tier),
                PlatformMemoryProjection::CopyOnly
            );
        }
    }
}

/// Open one embedded resource persistence family for a durable local composition.
/// Keeping this factory at the data-plane composition edge prevents the runtime
/// substrate from depending on concrete resource stores and prevents independent
/// roots from drifting on filenames or backend selection.
pub fn embedded_resource_component(
    root: &std::path::Path,
) -> awaken_resource_contract::ResourceComponent {
    std::fs::create_dir_all(root).expect("create resource-plane directory");
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::open(root.join("resources.db"))
            .expect("open Resources sqlite"),
    );
    let memory = awaken_memory_store::SqliteMemoryRepository::open(
        root.join("memory_fs.db")
            .to_str()
            .expect("resource memory path is valid UTF-8"),
    )
    .expect("open resource memory sqlite");
    let files = Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(
            root.join("files.db")
                .to_str()
                .expect("resource file path is valid UTF-8"),
        )
        .expect("open resource file sqlite"),
    );
    awaken_resource_contract::build_resource_component(
        awaken_resource_contract::ResourceDependencies {
            resource_catalog: resources.clone(),
            file_store: files.clone(),
            file_catalog: files,
            memory_repository: Arc::new(memory),
            skill_store: embedded_skill_store(root),
            reclamation: resources,
        },
    )
}

/// Derive the one embedded Resources application used by HTTP and Runtime.
pub fn embedded_resources_application(
    root: &std::path::Path,
) -> awaken_resource_application::ResourcesApplication {
    awaken_resource_application::ResourcesApplication::new(embedded_resource_component(root))
}

/// Hermetic in-memory Resources application for scenario composition.
#[cfg(feature = "test-support")]
pub fn ephemeral_resources_application() -> awaken_resource_application::ResourcesApplication {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resources store"),
    );
    awaken_resource_application::ResourcesApplication::new(
        awaken_resource_contract::build_resource_component(
            awaken_resource_contract::ResourceDependencies {
                resource_catalog: resources.clone(),
                file_store: files.clone(),
                file_catalog: files,
                memory_repository: Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
                skill_store: Arc::new(awaken_skill_store::InMemorySkillStore::new()),
                reclamation: resources,
            },
        ),
    )
}

pub fn resource_purge_scheduler(
    reclamation: Arc<dyn awaken_resource_contract::ResourceReclamationRepository>,
) -> Arc<dyn awaken_resource_contract::ResourcePurgeScheduler> {
    Arc::new(awaken_resource_application::RepositoryPurgeScheduler::new(
        reclamation,
    ))
}

/// Open only the Skill data adapter needed by the transitional Worker
/// composition. File and Memory content use claim-fenced network adapters and
/// therefore are not opened here.
pub fn embedded_skill_store(
    root: &std::path::Path,
) -> Arc<dyn awaken_resource_contract::SkillStore> {
    let skills = awaken_skill_store::FsSkillStore::open(root.join("skills"))
        .expect("open resource skill filesystem store");
    Arc::new(skills)
}

struct PinnedA2aTransportResolver {
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
}

#[async_trait::async_trait]
impl awaken_run_executor_a2a::TransportResolver for PinnedA2aTransportResolver {
    async fn resolve(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Arc<dyn awaken_run_executor_a2a::Transport>, String> {
        use awaken_runtime_contract::resolved::ModelProvisioning;

        let endpoint = candidate
            .binding
            .backend_ref
            .strip_prefix("a2a:")
            .filter(|endpoint| !endpoint.trim().is_empty())
            .ok_or_else(|| "A2A transport resolver received a non-remote candidate".to_string())?;
        let anonymous = awaken_run_executor_a2a::HttpTransport::new(endpoint);
        let ModelProvisioning::Remote {
            credential,
            security_fingerprint,
            ..
        } = &candidate.provisioning
        else {
            return match &candidate.provisioning {
                // Explicit scenario/test compositions may still install a
                // HostExecutor candidate. Persisted publication never does.
                ModelProvisioning::HostExecutor => Ok(Arc::new(anonymous)),
                _ => Err("A2A backend requires Remote provisioning".into()),
            };
        };
        if security_fingerprint.trim().is_empty() {
            return Err("A2A publication has no Agent Card security fingerprint".into());
        }
        let card = awaken_protocol_a2a::client::agent_card(&anonymous)
            .await
            .map_err(|error| format!("discover A2A Agent Card before launch: {error}"))?;
        let security = crate::a2a_security::project_agent_card_security(&card)?;
        if &security.fingerprint != security_fingerprint {
            return Err("A2A Agent Card security changed after publication".into());
        }
        let Some(access) = credential else {
            if !security.anonymous {
                return Err("A2A Agent Card requires authentication".into());
            }
            return Ok(Arc::new(anonymous));
        };
        if !security.accepted_headers.contains(&access.usage) {
            return Err("published A2A credential usage is not accepted by the Agent Card".into());
        }
        let materializer = self
            .credentials
            .as_ref()
            .ok_or_else(|| "authenticated A2A requires a credential materializer".to_string())?;
        let secret = materializer
            .materialize_claimed_remote(candidate, context)
            .await?
            .ok_or_else(|| {
                "authenticated A2A publication has no credential material".to_string()
            })?;
        let awaken_runtime_contract::CredentialUsage::HttpHeader { name, scheme } = &access.usage
        else {
            return Err("A2A credential usage is not an HTTP header".into());
        };
        let value = scheme.as_ref().map_or_else(
            || secret.expose_secret().to_string(),
            |scheme| format!("{scheme} {}", secret.expose_secret()),
        );
        Ok(Arc::new(anonymous.with_header(name, value)))
    }
}

/// Build the production A2A attempt adapter behind the runtime's neutral port.
/// The optional materializer is required only for an authenticated publication;
/// anonymous Agent Cards remain valid without credential infrastructure.
pub fn a2a_attempt_executor(
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> awaken_runtime_host::RemoteAttemptInstallation {
    let credential_realization = credentials.as_ref().map_or_else(
        awaken_runtime_contract::CredentialRealizationCapabilities::default,
        awaken_credential_materializer::PinnedCredentialMaterializer::worker_relay_capabilities,
    );
    awaken_runtime_host::RemoteAttemptInstallation {
        executor: Arc::new(awaken_run_executor_a2a::A2aRunExecutor::new(Arc::new(
            PinnedA2aTransportResolver { credentials },
        ))),
        credential_realization,
    }
}

/// An [`InferenceExecutorMaterializer`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
#[cfg(feature = "test-support")]
pub fn mount(host: Arc<SharedHost>) -> Router {
    // The webhook plane (ADR-0048) now lives in the management path
    // (all-in-one process assembly): subscriptions are a config resource in the admin
    // store and their secret is sealed in the vault, so a webhook needs the config
    // plane. The plain mount has neither, so it wires no sink — a bare host emits no
    // webhooks (identical to an unconfigured plane before).
    let catalog = ephemeral_resource_catalog();
    let state = local_managed_state(host.clone(), catalog.clone());
    mount_with_managed_and_resource_catalog_and_dreams(host, state, catalog).0
}

/// Assemble the local/single-process Managed adapter with one shared ephemeral
/// credential plane. Repository tokens are sealed immediately and runtime receives
/// only a credential reference. Production composition roots replace these
/// in-memory adapters with their durable equivalents; resource services remain
/// unaware of principals, API keys, roles, or authorization policy.
#[cfg(feature = "test-support")]
pub fn local_managed_state(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, None, None)
}

/// [`local_managed_state`] with one immutable Agent projection source. Embedded
/// scenario hosts use this to exercise publication-owned routing without mounting
/// a second authoring plane.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_agent_source(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, None, Some(agent_source))
}

/// [`local_managed_state`] with the Environment registry/work queue installed on
/// the same Managed aggregate. Composition roots that mount `/v1/environments`
/// must pass that exact state here so Session environment pins resolve through the
/// registry that authored them.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_environments(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    environments: Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, Some(environments), None)
}

/// Scenario/local composition variant that installs the same Agent projection
/// port used by production before the Managed aggregate starts its supervisors.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_environments_and_agent_source(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    environments: Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, Some(environments), Some(agent_source))
}

#[cfg(feature = "test-support")]
fn local_managed_state_over(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    environments: Option<
        Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
    >,
    agent_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
) -> Arc<ManagedState> {
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = Arc::new(awaken_protocol_managed::VaultState::new(
        secrets.clone(),
        credentials.clone(),
    ));
    // Keep the Session aggregate and its extraction intents in the same concrete
    // repository. The two application SPIs remain separate, while their local
    // durability boundary is shared at this composition root.
    let durable_session_repo = host.storage_dir().map(|root| {
        std::fs::create_dir_all(root).expect("create durable session repository directory");
        Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &root.join("sessions.db").to_string_lossy(),
            )
            .expect("open durable managed session repository"),
        )
    });
    if let Some(repo) = &durable_session_repo {
        host.install_memory_extraction_repository(repo.clone());
    }
    let session_repo: Arc<dyn awaken_session_contract::ManagedSessionRepository> =
        match durable_session_repo {
            Some(repo) => repo,
            None => Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .expect("open ephemeral managed Session repository"),
            ),
        };

    // Test/scenario composition must freeze the same topology fact as product
    // composition. Deriving placement from the canonical Host value eliminates
    // the former second path where ManagedState always selected LocalWorker even
    // when this process deliberately had no local dispatch pool.
    let execution_placement = if host.runs_local_dispatch_pool() {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    };
    let local_realization_owner = host.dispatch_owner().to_string();
    let environments = environments
        .unwrap_or_else(|| awaken_protocol_managed::test_support::environment_components().1);
    let runtime = Arc::new(
        ManagedHost::new(host)
            .with_resource_validator(catalog.clone())
            .with_repository_binding_verifier(Arc::new(
                awaken_resource_application::CatalogRepositoryBindingVerifier::new(catalog.clone()),
            ))
            .with_credentials(credentials, secrets),
    );
    let application = awaken_session_application::SessionApplication::new_with_configuration(
        runtime.clone(),
        runtime,
        session_repo,
        environments,
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement,
            local_realization_owner,
            ..Default::default()
        },
    );
    let managed = ManagedState::from_application(application);
    let managed = managed.with_vaults(vaults).with_resource_catalog(catalog);
    let managed = match agent_source {
        Some(source) => managed.with_config_source(source),
        None => managed,
    };
    let managed = Arc::new(managed);
    let _ = managed.session_application().spawn_lifecycle_supervisor();
    managed
}

// The **worker** lifecycle moved to the production `awaken-worker` crate. In the
// current shared-store composition it resolves each drained run's model from the
// DB-configured catalog + vault via `CredentialInferenceMaterializer`; its `NoModelConfiguredExecutor` is
// only an inert construction placeholder and is never a materialization fallback.
// The `awaken` binary's Worker role delegates to `awaken_worker::run`. The
// test-only echo-draining worker (for the worker-pool e2e) lives in
// `awaken-scenario-host::run_echo_worker`.

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
#[cfg(feature = "test-support")]
pub fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
    mount_with_managed_over(host, managed_state, ephemeral_resource_catalog(), None).0
}

#[cfg(feature = "test-support")]
fn ephemeral_resource_catalog() -> Arc<dyn awaken_resource_contract::ResourceCatalog> {
    Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Catalog"),
    )
}

#[cfg(feature = "test-support")]
fn ephemeral_dream_process_store() -> Arc<dyn awaken_session_contract::DreamProcessStore> {
    Arc::new(awaken_dream_application::InMemoryDreamProcessStore::default())
}

/// Assemble the data plane with the same secret-free Resource Catalog used by
/// the Managed Session ACL. Authorization remains an outer middleware concern;
/// this only shares resource identity/configuration/lifecycle truth.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_resource_catalog(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
) -> Router {
    mount_with_managed_and_resource_catalog_and_dreams(host, managed_state, resource_catalog).0
}

/// Scenario/embedder variant that returns the exact Dream aggregate mounted in
/// the data router. Production callers use the fuller application-access variant
/// below; deterministic hosts use this handle to mount the Awaken-only policy
/// projection without constructing a second scheduler or state authority.
///
/// Compile-matrix decision rule: when `test-support` is absent, neither this
/// scenario entry point nor its ephemeral dependencies may enter the product
/// build; when it is present, both must compile as one path. The corresponding
/// gates are exercised by no-feature product builds and test-support scenario
/// builds rather than by a second runtime implementation.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_resource_catalog_and_dreams(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let local_workspace = host.local_workspace().to_string();
    let (data, dreams) = mount_with_managed_over(host, managed_state, resource_catalog, None);
    // The bare scenario/test mount has no separate management edge. Keep the
    // Awaken policy authoring projection reachable here so deterministic SDK and
    // Console E2E can exercise it. Production composition mounts this exact
    // router on `CoordinatorComponent::management_router`, behind IAM + audit.
    let router = data.merge(awaken_protocol_awaken::dream_policy_router(dreams.clone()));
    (with_local_workspace_scope(router, local_workspace), dreams)
}

/// Test-support data plane with application credentials enforced on browser-facing
/// AI SDK and AG-UI routes. Product composition uses the full dependency constructor.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_application_access(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Arc<awaken_authz_enforce::ApplicationAccessStore>,
) -> Router {
    mount_with_managed_over(
        host,
        managed_state,
        resource_catalog,
        Some(application_access),
    )
    .0
}

/// Test-support data plane with a live executable model directory.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_application_access_and_models(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Arc<awaken_authz_enforce::ApplicationAccessStore>,
    model_directory: Arc<dyn awaken_protocol_managed::ModelDirectory>,
) -> Router {
    let resources = resource_management_router_from_host(&host, resource_catalog.clone());
    mount_with_managed_over_and_models(
        host,
        managed_state,
        resource_catalog,
        Some(application_access),
        Some(model_directory),
        ephemeral_dream_process_store(),
        ManagedRoutingExtensions {
            resource_management_router: resources,
            worker_authenticator: Arc::new(
                awaken_worker_transport_security::HeaderWorkerAuthenticator,
            ),
            worker_directory: test_worker_directory(),
        },
    )
    .expect("test-support Worker transport must assemble")
    .0
}

/// Router-owned services that must move together into the managed data plane.
pub struct ManagedRoutingExtensions {
    pub resource_management_router: Router,
    pub worker_authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    pub worker_directory: Arc<dyn awaken_worker_registry::WorkerDirectory>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerTransportBuildError {
    #[error("resolve durable Worker dispatch authority: {0}")]
    Dispatch(String),
    #[error("open durable Worker checkpoint authority at {path}: {error}")]
    CheckpointOpen {
        path: std::path::PathBuf,
        error: String,
    },
    #[error(
        "registered Worker transport requires durable stream checkpoints from Postgres or storage_dir"
    )]
    MissingCheckpointAuthority,
    #[error("registered Worker artifact transport requires the Resources File application")]
    MissingFileApplication,
}

fn worker_checkpoint_authority(
    dispatch: &awaken_run_ingress::AnyDispatchStore,
    store_dir: Option<&std::path::Path>,
) -> Result<
    Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
    WorkerTransportBuildError,
> {
    if let Some(checkpoint) = dispatch.stream_checkpoint_store() {
        return Ok(checkpoint);
    }
    if let Some(root) = store_dir {
        let path = root.join("worker-stream-checkpoints");
        return awaken_store_fs::FsStreamCheckpointStore::open(&path)
            .map(|store| {
                Arc::new(store)
                    as Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>
            })
            .map_err(|error| WorkerTransportBuildError::CheckpointOpen {
                path,
                error: error.to_string(),
            });
    }
    #[cfg(feature = "test-support")]
    {
        Ok(Arc::new(
            awaken_store_inmem::MemoryStreamCheckpointStore::new(),
        ))
    }
    #[cfg(not(feature = "test-support"))]
    {
        Err(WorkerTransportBuildError::MissingCheckpointAuthority)
    }
}

#[cfg(test)]
mod worker_checkpoint_authority_tests {
    use super::*;
    use awaken_run_ingress::{AnyDispatchStore, Dispatch, MemoryDispatchStore};

    #[test]
    fn checkpoint_authority_selection_is_fail_closed() {
        // Cause/effect graph: C1 the dispatch store owns a paired checkpoint ->
        // E1 reuse it; otherwise C2 storage_dir exists and opens -> E2 durable FS;
        // C2 exists but cannot open -> E3 startup error; with neither, C3 explicit
        // test-support build -> E4 volatile test authority, otherwise E5 startup
        // refusal. Product composition cannot reach E4.
        //
        // | Rule | C1 paired | C2 dir/open | C3 test build | Effect |
        // | R1   | yes       | -           | -             | E1     |
        // | R2   | no        | yes/yes     | -             | E2     |
        // | R3   | no        | yes/no      | -             | E3     |
        // | R4   | no        | no          | yes           | E4     |
        // | R5   | no        | no          | no            | E5     |
        // R1 is covered by the PostgreSQL active/active acceptance test.
        let dispatch = AnyDispatchStore::from_dispatch(
            Arc::new(MemoryDispatchStore::new()) as Arc<dyn Dispatch>
        );
        let root = tempfile::tempdir().expect("R2 root");
        assert!(
            worker_checkpoint_authority(&dispatch, Some(root.path())).is_ok(),
            "R2"
        );

        let invalid = tempfile::NamedTempFile::new().expect("R3 invalid root");
        assert!(
            matches!(
                worker_checkpoint_authority(&dispatch, Some(invalid.path())),
                Err(WorkerTransportBuildError::CheckpointOpen { .. })
            ),
            "R3"
        );

        #[cfg(feature = "test-support")]
        assert!(worker_checkpoint_authority(&dispatch, None).is_ok(), "R4");
        #[cfg(not(feature = "test-support"))]
        assert!(
            matches!(
                worker_checkpoint_authority(&dispatch, None),
                Err(WorkerTransportBuildError::MissingCheckpointAuthority)
            ),
            "R5"
        );
    }
}

/// Production data plane with the live model directory and the exact Dream state
/// mounted in that same Router. The composition root uses the returned state for
/// periodic policies instead of constructing a second scheduler.
pub fn mount_with_managed_application_access_models_and_dreams(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Arc<awaken_authz_enforce::ApplicationAccessStore>,
    model_directory: Arc<dyn awaken_protocol_managed::ModelDirectory>,
    dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
    routing: ManagedRoutingExtensions,
) -> Result<(Router, Arc<awaken_dream_application::DreamApplication>), WorkerTransportBuildError> {
    mount_with_managed_over_and_models(
        host,
        managed_state,
        resource_catalog,
        Some(application_access),
        Some(model_directory),
        dream_process_store,
        routing,
    )
}

#[cfg(feature = "test-support")]
fn mount_with_managed_over(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Option<Arc<awaken_authz_enforce::ApplicationAccessStore>>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let resources = resource_management_router_from_host(&host, resource_catalog.clone());
    mount_with_managed_over_and_models(
        host,
        managed_state,
        resource_catalog,
        application_access,
        None,
        ephemeral_dream_process_store(),
        ManagedRoutingExtensions {
            resource_management_router: resources,
            worker_authenticator: Arc::new(
                awaken_worker_transport_security::HeaderWorkerAuthenticator,
            ),
            worker_directory: test_worker_directory(),
        },
    )
    .expect("test-support Worker transport must assemble")
}

fn mount_with_managed_over_and_models(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Option<Arc<awaken_authz_enforce::ApplicationAccessStore>>,
    model_directory: Option<Arc<dyn awaken_protocol_managed::ModelDirectory>>,
    dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
    routing: ManagedRoutingExtensions,
) -> Result<(Router, Arc<awaken_dream_application::DreamApplication>), WorkerTransportBuildError> {
    let ManagedRoutingExtensions {
        resource_management_router,
        worker_authenticator,
        worker_directory,
    } = routing;
    // This is the sole Coordinator-owned installation point. It runs after the
    // Session provider is selected, and `install_memory_mounter` updates every
    // provider atomically while preserving an explicitly injected Worker adapter.
    install_platform_memory_data_plane(&host);
    // Spawn the process-level dispatch pool once when durable ingress is enabled
    // (O2): it is the sole claimer of the shared queue and drives every session's
    // runs. This is the single seam that owns an `Arc<SharedHost>`, which the pool's
    // session resolver needs.
    //
    // A coordinator-only cell server (`DeploymentConfig::disable_local_pool=1`) skips its
    // co-located pool so registered remote workers are the sole drainers, claiming
    // and settling over the dispatch transport.
    if host.runs_local_dispatch_pool() {
        host.ensure_dispatch_pool();
    }
    let dream_memory_stores = Arc::new(awaken_resource_application::MemoryStoreApplication::new(
        resource_catalog.clone(),
        resource_purge_scheduler(
            host.resource_reclamation()
                .expect("Dream requires resource lifecycle persistence"),
        ),
    ));
    let dream_worker = Arc::new(dream::BuiltInDreamAgent::new(
        managed_state.clone(),
        host.clone(),
        host.memory_repository(),
        resource_catalog.clone(),
        dream_memory_stores,
    ));
    let dream_application = Arc::new(
        awaken_dream_application::DreamApplication::with_store(dream_worker, dream_process_store)
            .expect("load durable Dream jobs"),
    );
    dream_application.bind_session_source(managed_state.clone());
    struct DreamModelDirectory(Arc<dyn awaken_protocol_managed::ModelDirectory>);
    #[async_trait::async_trait]
    impl awaken_dream_application::DreamModelReadiness for DreamModelDirectory {
        async fn is_ready(&self, workspace_id: &str, model_id: &str) -> Result<bool, String> {
            self.0.list(workspace_id).await.map(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.id == model_id || entry.display_name == model_id)
            })
        }
    }
    if let Some(directory) = model_directory.clone() {
        dream_application.bind_model_readiness(Arc::new(DreamModelDirectory(directory)));
    }
    dream_application.resume_incomplete();
    let dreams = awaken_protocol_managed::dreams_router(dream_application.clone());
    let resource_manifests = awaken_protocol_awaken::session_resource_manifest_router(
        awaken_protocol_managed::replace_resource_manifest,
    )
    .with_state(managed_state.clone());
    let managed = router(managed_state.clone())
        .merge(dreams)
        .merge(awaken_protocol_awaken::live_inbox_router(
            managed_state.session_application(),
        ))
        .merge(resource_manifests);
    // One neutral port impl behind the three wire adapters (each `router` takes
    // `Arc<dyn RunApplication>`), so they share the host with no per-protocol twin.
    let raw_port: Arc<dyn RunApplication> = Arc::new(RunApplicationHost::new(host.clone()));
    let workspace_host = host.clone();
    let agent_host = host.clone();
    let port: Arc<dyn RunApplication> =
        Arc::new(awaken_session_application::AdmittedRunApplication::new(
            raw_port,
            managed_state.session_application(),
            move |thread| workspace_host.thread_workspace(thread),
            move |thread| agent_host.thread_agent_projection(thread),
        ));
    let mut ai_sdk = awaken_protocol_ai_sdk::router(port.clone());
    let mut ag_ui = awaken_protocol_ag_ui::router(port.clone());
    if let Some(application_access) = application_access {
        ai_sdk = ai_sdk.layer(axum::middleware::from_fn_with_state(
            application_access.clone(),
            awaken_authz_enforce::application_guard,
        ));
        ag_ui = ag_ui.layer(axum::middleware::from_fn_with_state(
            application_access,
            awaken_authz_enforce::application_guard,
        ));
    }
    let a2a = awaken_protocol_a2a::router_with_storage_root(port.clone(), host.storage_dir());
    // A coordinator-only Host owns both dispatch and committed Thread truth but
    // deliberately has no execution pool. Start its one environment-free repair
    // loop before exposing the Worker transport; local-pool embeddings already
    // perform the same maintenance inside that pool.
    host.ensure_terminal_dispatch_reconciliation();
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / reap / dead-letter GC) over the same shared host.
    let durable_ops = durable_ops_router(host.clone());
    // The Worker-facing cross-node seam: a dispatch-store-isolated Worker claims/settles runs
    // over the dispatch transport and pushes committed facts to the commit ingest.
    let dispatch = host
        .dispatch_store()
        .map_err(|error| WorkerTransportBuildError::Dispatch(error.to_string()))?;
    let checkpoint = worker_checkpoint_authority(dispatch.as_ref(), host.storage_dir())?;
    let dispatch_router = awaken_run_ingress_http::registered_dispatch_router(
        awaken_run_ingress_http::RegisteredDispatchDependencies {
            dispatch: dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
            checkpoint,
            directory: worker_directory.clone(),
            policy: worker_placement::shared_worker_placement_policy(),
            sessions: managed_state.session_application(),
            authenticator: worker_authenticator.clone(),
            recovery: host.worker_recovery_source(),
            completion: host.worker_completion_sink(),
            stream_sink: host.worker_stream_sink(),
        },
    );
    let file_content = awaken_resource_worker_http::worker_file_content_router(Arc::new(
        awaken_resource_worker_http::WorkerFileContentService::new(
            host.worker_file_content_source(),
            dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
            worker_authenticator.clone(),
        )
        .with_worker_directory(worker_directory.clone())
        .with_application_sessions(
            managed_state
                .session_application()
                .session_repository_handle(),
        ),
    ));
    let file_application = host
        .file_application()
        .ok_or(WorkerTransportBuildError::MissingFileApplication)?;
    let artifact_publication =
        awaken_resource_worker_http::worker_artifact_publication_router(Arc::new(
            awaken_resource_worker_http::WorkerArtifactPublicationService::new(
                file_application,
                dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
                worker_authenticator.clone(),
                worker_directory.clone(),
            ),
        ));
    let memory = awaken_resource_worker_http::worker_memory_router(Arc::new(
        awaken_resource_worker_http::WorkerMemoryService::new(
            host.memory_repository(),
            resource_catalog.clone(),
            dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
            worker_authenticator.clone(),
            worker_directory.clone(),
        ),
    ));
    let repositories = awaken_resource_worker_http::worker_repository_binding_router(Arc::new(
        awaken_resource_worker_http::WorkerRepositoryBindingService::new(
            resource_catalog,
            dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
            worker_authenticator.clone(),
        )
        .with_worker_directory(worker_directory.clone()),
    ));
    let mut resource_worker = file_content
        .merge(artifact_publication)
        .merge(memory)
        .merge(repositories);
    if let Some(store) = host.skill_store() {
        resource_worker = resource_worker.merge(
            awaken_resource_worker_http::worker_skill_bundle_router(Arc::new(
                awaken_resource_worker_http::WorkerSkillBundleService::new(
                    Arc::new(awaken_resource_application::StoreSkillBundleSource::new(
                        store,
                    )),
                    dispatch.clone() as Arc<dyn awaken_run_ingress::DispatchQueue>,
                    worker_authenticator.clone(),
                )
                .with_worker_directory(worker_directory.clone()),
            )),
        );
    }
    let commit = Arc::new(awaken_run_ingress_http::ClaimedCommitHttpService::new(
        Arc::new(awaken_runtime_host::claimed_commit_service(
            dispatch as Arc<dyn awaken_run_ingress::DispatchQueue>,
            host.clone(),
        )),
        worker_directory,
        worker_authenticator,
    ));
    let worker_transport =
        awaken_run_ingress_http::registered_worker_transport_router_with_services(
            dispatch_router,
            resource_worker,
            commit,
        );
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = model_directory.map_or_else(
        || models_router(std::sync::Arc::new(default_models())),
        awaken_protocol_managed::models_router_with_directory,
    );
    let local_workspace = host.local_workspace().to_string();
    let router = managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
        .merge(worker_transport)
        .merge(resource_management_router)
        .merge(models);
    let router = with_local_workspace_scope(router, local_workspace);
    Ok((router, dream_application))
}

fn with_local_workspace_scope(router: Router, local_workspace: String) -> Router {
    router.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let local_workspace = local_workspace.clone();
            async move {
                // A scope-less request is the local/single-tenant mode. Resolve
                // that mode once at the composition edge so every adapter sees
                // the same platform-provisioned Workspace. Authenticated/cloud
                // edges already stamped a scope, which must remain authoritative.
                if request
                    .extensions()
                    .get::<awaken_tenancy::WorkspaceScope>()
                    .is_none()
                {
                    request
                        .extensions_mut()
                        .insert(awaken_tenancy::WorkspaceScope(local_workspace));
                }
                next.run(request).await
            }
        },
    ))
}

#[cfg(feature = "test-support")]
fn resource_management_router_from_host(
    host: &Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
) -> Router {
    let purge = resource_purge_scheduler(
        host.resource_reclamation()
            .expect("resource management requires lifecycle persistence"),
    );
    let memory_stores = memory_store_application(catalog, purge.clone());
    resources_router(ResourcesRouterInput {
        files: host
            .file_application()
            .expect("resource management requires the File application"),
        memories: host.memory_repository(),
        memory_stores,
        skills: host.skill_store(),
        purge,
    })
}

pub use awaken_credential_materializer::{
    ResolvedExecutorError, executor_from_materialized_endpoint,
};
