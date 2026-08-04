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
//! ([`mount`] / [`mount_with_managed`]), exact published-model credential
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
mod relay_hand;
pub mod webhooks;
pub mod worker_observation_boundary;
pub mod worker_placement;
mod worker_registry;
pub mod workspace_path;

pub use awaken_protocol_managed_resources::ModelDirectory;
pub use coordinator_component::{
    CoordinatorBuildError, CoordinatorComponent, CoordinatorDependencies,
    build_coordinator_component, restore_deployment_state,
};
#[cfg(any(test, feature = "test-support"))]
pub use coordinator_persistence::init_scenario_runtime;
pub use coordinator_persistence::{
    migrate_postgres_schema as migrate_postgres_coordinator_schema,
    open as open_coordinator_persistence, open_existing as open_existing_coordinator_persistence,
};

use std::sync::Arc;

mod a2a_security;
mod dream;

use awaken_protocol_managed::{ManagedState, router};
use awaken_provider_genai::{GenaiExecutor, OpenAiResponsesExecutor};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_session_contract::RunApplication;
use axum::Router;

// This data-plane composition depends on each authoritative owner directly.
pub use awaken_acp_application::{
    LocalAcpPreparation, PreparedAcpCapabilities, ensure_workspace_bindings,
};
pub use awaken_config_service::{ConfigService, capabilities_router, config_router};
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed_resources::{
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
pub use awaken_coordinator_runtime::durable_ops_router;
pub use awaken_runtime_host::{
    ExtMcpProbe, HostResume, InferenceExecutorMaterializer, ManagedHost, NoModelConfiguredExecutor,
    RunApplicationHost, SharedHost, ThreadEvent, ThreadEventHub, UNCONFIGURED_MODEL_REF,
    VaultRefresher, advertised_tools,
};
pub use awaken_sandbox_local::content_fingerprint;
pub use awaken_worker_registry::{WorkerDirectory, WorkerObservationSource};
pub use relay_hand::relay_hand_executor_factory;
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
            lifecycle: resources,
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
                lifecycle: resources,
            },
        ),
    )
}

pub fn resource_purge_scheduler(
    lifecycle: Arc<dyn awaken_resource_contract::ResourceLifecycleRepository>,
) -> Arc<dyn awaken_resource_contract::ResourcePurgeScheduler> {
    Arc::new(awaken_resource_application::RepositoryPurgeScheduler::new(
        lifecycle,
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
    environments: Arc<awaken_protocol_managed::EnvironmentExecutionState>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, Some(environments), None)
}

/// Scenario/local composition variant that installs the same Agent projection
/// port used by production before the Managed aggregate starts its supervisors.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_environments_and_agent_source(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    environments: Arc<awaken_protocol_managed::EnvironmentExecutionState>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> Arc<ManagedState> {
    local_managed_state_over(host, catalog, Some(environments), Some(agent_source))
}

#[cfg(feature = "test-support")]
fn local_managed_state_over(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    environments: Option<Arc<awaken_protocol_managed::EnvironmentExecutionState>>,
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
    let session_repo = host.storage_dir().map(|root| {
        std::fs::create_dir_all(root).expect("create durable session repository directory");
        Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &root.join("sessions.db").to_string_lossy(),
            )
            .expect("open durable managed session repository"),
        )
    });
    if let Some(repo) = &session_repo {
        host.install_memory_extraction_repository(repo.clone());
    }
    let managed = ManagedState::new_with_mcp(
        ManagedHost::new(host)
            .with_resource_validator(catalog.clone())
            .with_credentials(credentials, secrets),
    );
    let managed = match session_repo {
        Some(repo) => managed.with_session_repo(repo),
        None => managed,
    };
    let managed = managed.with_vaults(vaults).with_resource_catalog(catalog);
    let managed = match environments {
        Some(environments) => managed.with_environments(environments),
        None => managed,
    };
    let managed = match agent_source {
        Some(source) => managed.with_config_source(source),
        None => managed,
    };
    let managed = Arc::new(managed);
    let _ = managed.spawn_realization_lease_supervisor();
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
    model_directory: Arc<dyn awaken_protocol_managed_resources::ModelDirectory>,
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

/// Production data plane with the live model directory and the exact Dream state
/// mounted in that same Router. The composition root uses the returned state for
/// periodic policies instead of constructing a second scheduler.
pub fn mount_with_managed_application_access_models_and_dreams(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    application_access: Arc<awaken_authz_enforce::ApplicationAccessStore>,
    model_directory: Arc<dyn awaken_protocol_managed_resources::ModelDirectory>,
    dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
    routing: ManagedRoutingExtensions,
) -> Result<
    (Router, Arc<awaken_dream_application::DreamApplication>),
    awaken_runtime_host::RegisteredWorkerTransportBuildError,
> {
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
    model_directory: Option<Arc<dyn awaken_protocol_managed_resources::ModelDirectory>>,
    dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
    routing: ManagedRoutingExtensions,
) -> Result<
    (Router, Arc<awaken_dream_application::DreamApplication>),
    awaken_runtime_host::RegisteredWorkerTransportBuildError,
> {
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
            host.resource_lifecycle()
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
    struct DreamModelDirectory(Arc<dyn awaken_protocol_managed_resources::ModelDirectory>);
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
    let managed = router(managed_state.clone()).merge(dreams).merge(
        awaken_protocol_awaken::live_inbox_router(managed_state.clone()),
    );
    // One neutral port impl behind the three wire adapters (each `router` takes
    // `Arc<dyn RunApplication>`), so they share the host with no per-protocol twin.
    struct ManagedSessionDefaults(Arc<ManagedState>);

    #[async_trait::async_trait]
    impl awaken_runtime_host::SessionDefaultsPreparer for ManagedSessionDefaults {
        async fn prepare(
            &self,
            workspace_id: &str,
            thread_id: &str,
            agent_id: &str,
        ) -> Result<(), awaken_runtime_host::SessionDefaultsPreparationError> {
            self.0
                .prepare_protocol_session(workspace_id, thread_id, agent_id)
                .await
                .map_err(|error| {
                    awaken_runtime_host::SessionDefaultsPreparationError(error.to_string())
                })
        }
    }

    let port: Arc<dyn RunApplication> = Arc::new(
        RunApplicationHost::new(host.clone())
            .with_session_defaults(Arc::new(ManagedSessionDefaults(managed_state.clone()))),
    );
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
    let worker_transport = awaken_runtime_host::registered_worker_transport_router(
        host.clone(),
        worker_directory,
        worker_placement::shared_worker_placement_policy(),
        managed_state,
        resource_catalog.clone(),
        worker_authenticator,
    )?;
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = model_directory.map_or_else(
        || models_router(std::sync::Arc::new(default_models())),
        awaken_protocol_managed_resources::models_router_with_directory,
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
                    .get::<awaken_protocol_managed::WorkspaceScope>()
                    .is_none()
                {
                    request
                        .extensions_mut()
                        .insert(awaken_protocol_managed::WorkspaceScope(local_workspace));
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
        host.resource_lifecycle()
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

/// The worker composition seam refuses incomplete or unsupported materialized
/// provider access (ADR-0043, fail-closed).
#[derive(Debug, thiserror::Error)]
pub enum ResolvedExecutorError {
    #[error("resolved inference has no base_url for adapter `{0}`")]
    MissingBaseUrl(String),
    #[error("resolved inference carries no credential (unauthenticated run refused)")]
    MissingCredential,
    #[error("no provider executor in this build serves adapter `{0}`")]
    UnsupportedAdapter(String),
    #[error("no provider executor in this build serves API dialect `{0}`")]
    UnsupportedDialect(String),
    #[error("API dialect `{dialect}` is incompatible with adapter `{adapter}`")]
    DialectAdapterMismatch { dialect: String, adapter: String },
    #[error("provider executor could not be constructed: {0}")]
    ExecutorBuild(String),
}

/// Construct from the exact protocol frozen in a modern publication. An empty
/// dialect is accepted only for legacy snapshots and follows the adapter-family
/// path; declared modern dialects are checked before any credential-backed
/// executor is constructed.
pub fn executor_from_materialized_endpoint(
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    let expected_adapter = match api_dialect {
        "" => None,
        "anthropic_messages" => Some("anthropic"),
        "open_ai_chat" => Some("openai"),
        "gemini" => Some("gemini"),
        "vertex_gemini" => Some("vertex"),
        "open_ai_responses" => Some("openai"),
        other => return Err(ResolvedExecutorError::UnsupportedDialect(other.to_string())),
    };
    if expected_adapter.is_some_and(|expected| expected != adapter_kind) {
        return Err(ResolvedExecutorError::DialectAdapterMismatch {
            dialect: api_dialect.to_string(),
            adapter: adapter_kind.to_string(),
        });
    }
    if api_dialect == "open_ai_responses" {
        let base_url = base_url
            .ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
        let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
        return OpenAiResponsesExecutor::new(base_url, credential.expose_secret())
            .map(|executor| Arc::new(executor) as Arc<dyn LlmExecutor>)
            .map_err(|error| ResolvedExecutorError::ExecutorBuild(error.to_string()));
    }
    executor_from_materialized_access(adapter_kind, base_url, credential)
}

/// Construct a provider executor from publication-pinned endpoint facts and
/// worker-materialized credential material. It does not consult a catalog, select
/// a route, select a credential, or accept a management-plane preview result.
pub fn executor_from_materialized_access(
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    // One path for every API-key provider: map the catalog's adapter-kind to a genai
    // adapter and hand it the resolved credential + (optional) gateway base URL. The
    // key comes from the resolved credential, never inlined by the Managed wire. A new
    // provider is one line in `genai_adapter` + catalog config — no new branch here.
    let adapter = genai_adapter(adapter_kind)
        .ok_or_else(|| ResolvedExecutorError::UnsupportedAdapter(adapter_kind.to_string()))?;
    // Fail closed on an incomplete resolution: the management plane always resolves the
    // execution triple's endpoint, so a `None` base URL means the inference never bound
    // an endpoint — refuse rather than silently fall back to the genai default endpoint.
    let base_url =
        base_url.ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
    let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
    Ok(Arc::new(GenaiExecutor::from_resolved(
        adapter,
        Some(base_url.to_string()),
        credential.expose_secret(),
    )))
}

/// Map our catalog's wire dialect (`ApiDialect::adapter_kind`) to a genai adapter.
/// The one place a supported provider wire is named; genai's default endpoint is used
/// unless the catalog endpoint supplies a gateway base URL.
fn genai_adapter(adapter_kind: &str) -> Option<awaken_provider_genai::AdapterKind> {
    use awaken_provider_genai::AdapterKind;
    Some(match adapter_kind {
        "anthropic" => AdapterKind::Anthropic,
        "gemini" => AdapterKind::Gemini,
        "vertex" => AdapterKind::Vertex,
        "openai" => AdapterKind::OpenAI,
        _ => return None,
    })
}

#[cfg(test)]
mod executor_seam_tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_provider_genai::AdapterKind;

    /// The one place a supported provider wire is named maps exactly the three the
    /// build serves, and returns `None` (→ `UnsupportedAdapter`) for everything else,
    /// case-sensitively.
    #[test]
    fn genai_adapter_maps_the_supported_wires_and_rejects_the_rest() {
        assert_eq!(genai_adapter("anthropic"), Some(AdapterKind::Anthropic));
        assert_eq!(genai_adapter("gemini"), Some(AdapterKind::Gemini));
        assert_eq!(genai_adapter("vertex"), Some(AdapterKind::Vertex));
        assert_eq!(genai_adapter("openai"), Some(AdapterKind::OpenAI));
        // Fail-closed: an unserved wire, the empty string, and a case variant all miss.
        assert_eq!(genai_adapter("cohere"), None);
        assert_eq!(genai_adapter(""), None);
        assert_eq!(genai_adapter("Anthropic"), None);
    }

    /// `UnsupportedAdapter` is the reachable fail-closed arm: a resolved inference
    /// whose `adapter_kind` no provider in this build serves is refused, naming the
    /// adapter — never silently built into some default executor.
    #[test]
    fn materialized_access_fails_closed_on_an_unsupported_adapter() {
        let credential = RedactedString::new("sk-secret");
        // `Ok` carries an `Arc<dyn LlmExecutor>` (not `Debug`), so map to the error first.
        match executor_from_materialized_access("cohere", Some("https://gw/"), Some(&credential))
            .err()
        {
            Some(ResolvedExecutorError::UnsupportedAdapter(a)) => assert_eq!(a, "cohere"),
            other => panic!("expected UnsupportedAdapter, got {other:?}"),
        }
    }

    #[test]
    fn exact_dialect_is_checked_and_responses_uses_a_dedicated_executor() {
        let credential = RedactedString::new("sk-secret");
        assert!(
            executor_from_materialized_endpoint(
                "open_ai_responses",
                "openai",
                Some("https://api.openai.com/v1"),
                Some(&credential),
            )
            .is_ok()
        );
        assert!(matches!(
            executor_from_materialized_endpoint(
                "anthropic_messages",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::DialectAdapterMismatch { .. })
        ));
        assert!(
            executor_from_materialized_endpoint(
                "open_ai_chat",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            )
            .is_ok()
        );
    }

    /// `MissingBaseUrl` is a reachable fail-closed arm: a resolved inference whose
    /// `base_url` is `None` never bound an endpoint, so the seam refuses to build an
    /// executor (rather than silently falling back to the genai default endpoint),
    /// naming the adapter.
    #[test]
    fn missing_base_url_is_refused_by_the_seam() {
        // A supported adapter + a credential but NO base URL fails closed.
        let credential = RedactedString::new("sk-secret");
        match executor_from_materialized_access("anthropic", None, Some(&credential)).err() {
            Some(ResolvedExecutorError::MissingBaseUrl(a)) => assert_eq!(a, "anthropic"),
            other => panic!("expected MissingBaseUrl, got {other:?}"),
        }
        // The variant renders its intended message.
        let err = ResolvedExecutorError::MissingBaseUrl("anthropic".into());
        assert_eq!(
            err.to_string(),
            "resolved inference has no base_url for adapter `anthropic`"
        );
    }

    /// A supported adapter with both a base URL and a credential builds an executor
    /// (the happy path the three fail-closed arms bracket).
    #[test]
    fn materialized_access_builds_supported_adapters_with_a_credential() {
        let credential = RedactedString::new("k");
        assert!(
            executor_from_materialized_access("openai", Some("https://gw/"), Some(&credential))
                .is_ok()
        );
        assert!(
            executor_from_materialized_access("gemini", Some("https://gw/"), Some(&credential))
                .is_ok()
        );
        assert!(
            executor_from_materialized_access("vertex", Some("https://gw/"), Some(&credential))
                .is_ok()
        );
    }
}
