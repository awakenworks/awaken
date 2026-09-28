// @formal entrypoint authz.application_protocol_is_scoped_by_application_guard_not_service_guard
// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
//! `awaken-coordinator` — the Coordinator application and protocol data plane.
//!
//! It owns one protocol-neutral [`SharedHost`] (from `awaken-session-executor`,
//! the thread-keyed session substrate) and mounts public protocol adapters over
//! it. Each adapter is a thin translator from its wire
//! vocabulary to the host's neutral operations; because every adapter keys by the
//! same Thread id and drives the same coordinator, a Run started through one
//! protocol can be resumed or observed through another on the *same Thread*.
//!
//! This crate is the Coordinator owner: its canonical
//! [`build_coordinator_component`] assembles Deployment/Session scheduling and
//! the session surface + protocol adapters
//! (`mount` / `mount_with_managed`, under `test-support`), injected exact
//! published-model materialization and executable-directory ports, the inert
//! no-model placeholder, workspace path addressing, and the Worker
//! role helper (the hand is now the separate `awaken-sandbox` execution-plane
//! binary). Its sibling **authoring / authz plane** lives in
//! `awaken-control`; it owns model-catalog publication resolution and neither
//! crate depends on the other. `awaken-cli` is the
//! process entry point that joins them into one management router. The service layer
//! (the Host, the two Run translators, and the per-kind Resource routers) lives in
//! `awaken-session-executor`.

pub mod admin;
pub mod application_access;
pub mod application_access_store;
mod artifact_publication;
mod browser_cors;
pub mod console;
pub mod control_service_boundary;
mod coordinator_component;
mod coordinator_persistence;
pub mod data_subject_boundary;
mod extraction_references;
#[cfg(test)]
mod inference_publication_tests;
mod managed_application;
mod managed_lifecycle;
pub mod mcp_export;
pub use managed_application::install_managed_agent_coordination;
pub use managed_lifecycle::{
    ManagedLifecycleCompositionError, install_managed_lifecycle_delivery,
    install_managed_lifecycle_delivery_with_deployments,
};
mod runtime_authority;
pub mod webhooks;
pub mod worker_fleet;
pub mod worker_observation_boundary;
pub mod worker_placement;
mod worker_registry;
pub mod workspace_path;

pub use artifact_publication::ClaimFencedArtifactPublisher;
pub use browser_cors::AiSdkBrowserCors;
pub use coordinator_component::{
    CoordinatorBuildError, CoordinatorComponent, CoordinatorDependencies,
    build_coordinator_component, register_application_access_retention,
    restore_deployment_application,
};
#[cfg(any(test, feature = "test-support"))]
pub use coordinator_persistence::init_scenario_runtime;
pub use coordinator_persistence::{
    CoordinatorPersistence, migrate_postgres_schema as migrate_postgres_coordinator_schema,
    open as open_coordinator_persistence, open_existing as open_existing_coordinator_persistence,
};
pub use extraction_references::ReferenceIndexedMemoryExtractions;
pub use runtime_authority::postgres_local_commit;

use std::sync::Arc;

mod dream;

use awaken_protocol_managed::{ManagedState, router};
use awaken_session_application_contract::RunApplication;
use axum::Router;

// This data plane depends on each authoritative owner directly.
pub use awaken_acp_application::{
    LocalAcpPreparation, PreparedAcpCapabilities, ensure_workspace_bindings,
};
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed::{
    ResourcesRouterInput, default_models, models_router, resources_router,
};

pub use awaken_dispatch_http::durable_ops_router;
pub use awaken_session_executor::{
    ExtMcpProbe, HostResume, ManagedHost, NoModelConfiguredExecutor, RunApplicationHost,
    SharedHost, ThreadEvent, ThreadEventHub, UNCONFIGURED_MODEL_REF, advertised_tools,
};
pub use awaken_sandbox_local::content_fingerprint;
pub use awaken_worker_registry::{WorkerDirectory, WorkerObservationSource};
pub use worker_registry::WorkerDirectoryHandle;
#[cfg(any(test, feature = "test-support"))]
pub use worker_registry::test_directory as test_worker_directory;

/// Canonical trusted-host ACP preparation for product processes. This
/// service-layer boundary joins reusable discovery with the production wire.
pub async fn prepare_local_acp(
    input: awaken_acp_application::LocalAcpPreparation,
) -> Result<awaken_acp_application::PreparedAcpCapabilities, String> {
    let adapters = awaken_acp_application::HostAcpAdapters::production(std::sync::Arc::new(
        awaken_protocol_acp::ProtocolAcpCapabilityHandshake,
    ));
    awaken_acp_application::prepare_host_acp_with(
        input,
        adapters.discovery,
        adapters.installer,
        adapters.negotiator,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformMemoryProjection {
    FusePreferred,
    CopyOnly,
}

fn platform_memory_projection(tier: awaken_session_executor::SandboxTier) -> PlatformMemoryProjection {
    use awaken_session_executor::SandboxTier;

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
    use awaken_session_executor::SandboxTier;

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
            .binding()
            .identity()
            .2
            .strip_prefix("a2a:")
            .filter(|endpoint| !endpoint.trim().is_empty())
            .ok_or_else(|| "A2A transport resolver received a non-remote candidate".to_string())?;
        let anonymous = awaken_run_executor_a2a::HttpTransport::new(endpoint);
        let ModelProvisioning::Remote {
            credential,
            security_fingerprint,
            ..
        } = candidate.provisioning()
        else {
            return match candidate.provisioning() {
                // Explicit scenario/test platforms may still install a
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
        let security = awaken_protocol_a2a::project_agent_card_security(&card)?;
        if security.fingerprint != *security_fingerprint {
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
) -> awaken_session_executor::RemoteAttemptInstallation {
    let credential_realization = credentials.as_ref().map_or_else(
        awaken_runtime_contract::CredentialRealizationCapabilities::default,
        awaken_credential_materializer::PinnedCredentialMaterializer::worker_relay_capabilities,
    );
    awaken_session_executor::RemoteAttemptInstallation {
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
    let catalog = ephemeral_resource_registry();
    let state = local_managed_state(host.clone(), catalog.clone());
    mount_with_managed_and_resource_registry_and_dreams(host, state, catalog, None).0
}

/// A Scenario composition's Managed adapter together with the one Session
/// application it narrows. The composition root that built the application
/// hands it to every other consumer (the lifecycle supervisor, Worker
/// transport, Dreams) from here; the adapter holds only its roles.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct ScenarioManagedState {
    pub managed: Arc<ManagedState>,
    pub sessions: Arc<awaken_session_application::SessionApplication>,
}

/// Assemble the local/single-process Managed adapter with one shared ephemeral
/// credential plane. Repository tokens are sealed immediately and runtime receives
/// only a credential reference. Production processes replace these
/// in-memory adapters with their durable equivalents; resource services remain
/// unaware of principals, API keys, roles, or authorization policy.
#[cfg(feature = "test-support")]
pub fn local_managed_state(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
) -> ScenarioManagedState {
    local_managed_state_over(host, catalog, None, None, None, None)
}

/// Scenario variant with the same Session model-publication resolver used by
/// production. It exists for provider-backed fixtures whose public model
/// reference differs from the built-in Agent's deterministic default.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_model_publication_resolver(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
) -> ScenarioManagedState {
    local_managed_state_over(host, catalog, None, None, Some(resolver), None)
}

/// Scenario variant that freezes both the immutable Agent publication and the
/// model-reference resolver. Model-routing fixtures need both authorities: the
/// resolver selects a candidate, while the Agent snapshot proves its backend.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_agent_source_and_model_publication_resolver(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
) -> ScenarioManagedState {
    local_managed_state_over(
        host,
        catalog,
        None,
        Some(agent_source),
        Some(resolver),
        None,
    )
}

/// [`local_managed_state`] with one immutable Agent projection source. Embedded
/// scenario hosts use this to exercise publication-owned routing without mounting
/// a second authoring plane.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_agent_source(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> ScenarioManagedState {
    local_managed_state_over(host, catalog, None, Some(agent_source), None, None)
}

/// [`local_managed_state_with_agent_source`] with the exact list-price
/// authority needed by budgeted Managed Session fixtures. The authority is
/// installed before sharing, so callers never unwrap or mutate an `Arc`.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_agent_source_and_list_prices(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    list_prices: Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
) -> ScenarioManagedState {
    local_managed_state_over(
        host,
        catalog,
        None,
        Some(agent_source),
        None,
        Some(list_prices),
    )
}

/// [`local_managed_state`] with the Environment registry/work queue installed on
/// the same Managed aggregate. Processes that mount `/v1/environments`
/// must pass that exact state here so Session environment pins resolve through the
/// registry that authored them.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_environments(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
) -> ScenarioManagedState {
    local_managed_state_over(host, catalog, Some(environments), None, None, None)
}

/// Scenario/local variant that installs the same Agent projection
/// Agent projection used by production before the Managed aggregate starts its supervisors.
#[cfg(feature = "test-support")]
pub fn local_managed_state_with_environments_and_agent_source(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> ScenarioManagedState {
    local_managed_state_over(
        host,
        catalog,
        Some(environments),
        Some(agent_source),
        None,
        None,
    )
}

/// The volatile Environment execution projection test/scenario startup uses
/// when the caller supplies none: one in-memory WorkQueue and a catalog seeded
/// with the built-in local Environment. The Coordinator composes these stores
/// itself rather than borrowing them through the Managed adapter.
#[cfg(feature = "test-support")]
fn volatile_environment_execution()
-> Arc<awaken_environment_execution_application::EnvironmentExecutionApplication> {
    let catalog =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    catalog
        .install_seed(awaken_environment_application::default_environment_registration())
        .expect("install built-in local Environment");
    Arc::new(
        awaken_environment_execution_application::EnvironmentExecutionApplication::new(
            Arc::new(awaken_work_store::InMemoryWorkQueue::new()),
            catalog,
        ),
    )
}

#[cfg(feature = "test-support")]
fn local_managed_state_over(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Option<
        Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
    >,
    agent_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    model_publication_resolver: Option<
        Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
    >,
    managed_list_price_provider: Option<
        Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
    >,
) -> ScenarioManagedState {
    // The Session application runs on the Host's clock, so Session facts and
    // the dispatch they drive share one timeline.
    let clock = host.clock();
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = Arc::new(awaken_protocol_managed::VaultState::new(
        secrets.clone(),
        credentials.clone(),
    ));
    // Keep the Session aggregate and its extraction intents in the same concrete
    // repository. The two application SPIs remain separate, while their local
    // durability boundary is shared at this process boundary.
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
    let session_repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        match durable_session_repo {
            Some(repo) => repo,
            None => Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .expect("open ephemeral managed Session repository"),
            ),
        };

    // Test/scenario startup must freeze the same topology fact as product
    // startup. Deriving placement from the canonical Host value eliminates
    // the former second path where ManagedState always selected LocalWorker even
    // when this process deliberately had no local dispatch pool.
    let execution_placement = if host.runs_local_dispatch_pool() {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    };
    let local_realization_owner = host.dispatch_owner().to_string();
    let environments = environments.unwrap_or_else(volatile_environment_execution);
    let repository_bindings = Arc::new(
        awaken_resource_application::RegistryRepositoryBindingVerifier::new(catalog.clone()),
    );
    let runtime = ManagedHost::new(host)
        .with_resource_validator(catalog.clone())
        .with_repository_binding_verifier(repository_bindings)
        .with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::new(credentials, secrets),
        );
    let runtime = runtime.install_dispatch_session_runtime();
    let runtime = Arc::new(runtime);
    let mut application = awaken_session_application::SessionApplication::new_with_configuration(
        runtime.clone(),
        runtime.clone(),
        session_repo,
        environments.clone(),
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement,
            local_realization_owner,
            ..Default::default()
        },
        clock,
    );
    application.set_credential_source(vaults.clone());
    application.set_credential_material_ingress(vaults);
    application.set_resource_registry(catalog);
    if let Some(source) = agent_source {
        application.set_config_source(source);
    }
    if let Some(resolver) = model_publication_resolver {
        application.set_model_publication_resolver(resolver);
    }
    if let Some(provider) = managed_list_price_provider {
        application.set_managed_list_price_provider(provider);
    }
    let application = Arc::new(application);
    install_managed_agent_coordination(&runtime, &application)
        .expect("scenario Session coordination application binds once");
    ScenarioManagedState {
        managed: Arc::new(ManagedState::from_application(
            application.clone(),
            environments,
        )),
        sessions: application,
    }
}

// The **worker** lifecycle moved to the production `awaken-worker` crate. In the
// current deployment it consumes only the publication-pinned model candidate
// through the injected `CredentialInferenceMaterializer`; its
// `NoModelConfiguredExecutor` is only an inert construction placeholder and is
// never a materialization fallback.
// The `awaken` binary's Worker role delegates to `awaken_worker::run`. The
// test-only echo-draining worker (for the worker-pool e2e) lives in
// `awaken-scenario-host::run_echo_worker`.

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
#[cfg(feature = "test-support")]
pub fn mount_with_managed(host: Arc<SharedHost>, managed_state: ScenarioManagedState) -> Router {
    mount_with_managed_over(
        host,
        managed_state,
        ephemeral_resource_registry(),
        ApplicationAccessMount::ExplicitlyUnguardedTest,
        awaken_service_lifecycle::ServiceLifecycle::new(),
        None,
    )
    .0
}

#[cfg(feature = "test-support")]
fn ephemeral_resource_registry() -> Arc<dyn awaken_resource_contract::ResourceRegistry> {
    awaken_resource_persistence::ephemeral(awaken_lease_contract::system_clock())
        .expect("open ephemeral Resources application")
        .authorities()
        .resource_registry()
}

#[cfg(feature = "test-support")]
fn scenario_dream_process_store(
    host: &SharedHost,
) -> Arc<dyn awaken_session_dream_contract::DreamProcessStore> {
    match host.storage_dir() {
        Some(root) => {
            std::fs::create_dir_all(root).expect("create durable Dream repository directory");
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &root.join("sessions.db").to_string_lossy(),
                )
                .expect("open durable Dream process repository"),
            )
        }
        None => Arc::new(awaken_dream_application::InMemoryDreamProcessStore::default()),
    }
}

#[cfg(feature = "test-support")]
fn with_scenario_session_lifecycle(
    router: Router,
    managed_state: ScenarioManagedState,
    host: Arc<SharedHost>,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
) -> Router {
    install_managed_lifecycle_delivery(
        &managed_state.managed,
        &managed_state.sessions,
        None,
        &lifecycle,
    )
    .expect("scenario Managed lifecycle delivery binds before traffic");
    coordinator_component::register_session_lifecycle(&lifecycle, managed_state.sessions);
    coordinator_component::register_runtime_background_drain(&lifecycle, host);
    // The Router owns the same process-lifecycle handle as the Scenario surface.
    // Dropping the test server drops the composition; the Tokio runtime then
    // tears down its registered tasks just as process shutdown does.
    router.layer(axum::Extension(lifecycle))
}

#[cfg(feature = "test-support")]
fn with_scenario_worker_transport(
    public: Router,
    worker_private: Router,
    remote_worker_required: bool,
) -> Router {
    // Production keeps the Worker transport on its private listener. The
    // single-listener Scenario surface exposes that exact router only for the
    // coordinator-only deployment axis whose local pool is disabled; ordinary
    // Scenario servers retain the same isolation instead of publishing an
    // unused private API.
    if remote_worker_required {
        public.merge(worker_private)
    } else {
        public
    }
}

/// Assemble the data plane with the same secret-free Resource Registry used by
/// the Managed Session ACL. Authorization remains an outer middleware concern;
/// this only shares resource identity/configuration/lifecycle truth.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_resource_registry(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
) -> Router {
    mount_with_managed_and_resource_registry_and_dreams(
        host,
        managed_state,
        resource_registry,
        None,
    )
    .0
}

/// Test-support decoration of the existing Dream execution port. The
/// Coordinator still constructs the sole production executor and Dream state.
#[cfg(feature = "test-support")]
pub type ScenarioDreamExecutorDecorator = Arc<
    dyn Fn(
            Arc<dyn awaken_dream_application::DreamExecutor>,
        ) -> Arc<dyn awaken_dream_application::DreamExecutor>
        + Send
        + Sync,
>;

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
pub fn mount_with_managed_and_resource_registry_and_dreams(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    mount_with_managed_and_resource_registry_and_dreams_on_lifecycle(
        host,
        managed_state,
        resource_registry,
        awaken_service_lifecycle::ServiceLifecycle::new(),
        dream_executor_decorator,
    )
}

/// Scenario composition with an outer-owned lifecycle. Process-level scenario
/// hosts use this entry point so graceful HTTP shutdown can join the exact
/// Session supervisors and Runtime background work mounted in the Router.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_resource_registry_and_dreams_on_lifecycle(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let local_workspace = host.local_workspace().to_string();
    let (data, dreams) = mount_with_managed_over(
        host,
        managed_state,
        resource_registry,
        ApplicationAccessMount::ExplicitlyUnguardedTest,
        lifecycle,
        dream_executor_decorator,
    );
    // The bare scenario/test mount has no separate management edge. Keep the
    // Awaken policy authoring projection reachable here so deterministic SDK and
    // Console E2E can exercise it. Production startup mounts this exact
    // router on `CoordinatorComponent::management_router`, behind IAM + audit.
    let router = data.merge(awaken_protocol_awaken::dream_policy_router(dreams.clone()));
    (with_local_workspace_scope(router, local_workspace), dreams)
}

/// Test-support data plane with application credentials enforced on browser-facing
/// AI SDK and AG-UI routes. Product startup uses the full dependency constructor.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_application_access(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
) -> Router {
    mount_with_managed_over(
        host,
        managed_state,
        resource_registry,
        ApplicationAccessMount::Guarded(application_access),
        awaken_service_lifecycle::ServiceLifecycle::new(),
        None,
    )
    .0
}

/// Test-support data plane with a live executable model directory.
#[cfg(feature = "test-support")]
pub fn mount_with_managed_and_application_access_and_models(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
    model_inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
) -> Router {
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let lifecycle_host = host.clone();
    let remote_worker_required = !host.runs_local_dispatch_pool();
    let dream_process_store = scenario_dream_process_store(host.as_ref());
    let (resources, memory_stores) =
        resource_management_router_from_host(&host, resource_registry.clone());
    let supervised_state = managed_state.clone();
    let session_application = managed_state.sessions;
    let worker_file_application = host
        .file_application()
        .expect("test-support File application");
    let worker_skill_bundles = Arc::new(awaken_resource_application::StoreSkillBundleSource::new(
        host.skill_store().expect("test-support Skill store"),
    ));
    let (managed, data, application, worker_private, _) = mount_with_managed_over_and_models(
        host,
        managed_state.managed,
        ApplicationAccessMount::Guarded(application_access),
        ManagedApplicationServices {
            session_application,
            resource_registry,
            model_inventory: Some(model_inventory),
            dream_process_store,
            executable_projection_refresh: None,
            #[cfg(feature = "test-support")]
            dream_executor_decorator: None,
        },
        ManagedRoutingExtensions {
            resource_management_router: resources,
            memory_stores,
            worker_file_application,
            worker_skill_bundles,
            worker_authenticator: Arc::new(
                awaken_worker_transport_security::HeaderWorkerAuthenticator,
            ),
            worker_placement_policy: None,
            worker_execution_bindings: None,
            worker_execution_stops: None,
            credential_deliveries: None,
            repository_transport_authorizer: None,
            worker_directory: test_worker_directory(),
            ai_sdk_browser_cors: AiSdkBrowserCors::default(),
            worker_clock: awaken_lease_contract::system_clock(),
        },
    )
    .expect("test-support Worker transport must assemble");
    let public = with_scenario_worker_transport(
        managed.merge(data).merge(application),
        worker_private,
        remote_worker_required,
    );
    with_scenario_session_lifecycle(public, supervised_state, lifecycle_host, lifecycle)
}

/// Router-owned services that must move together into the managed data plane.
pub struct ManagedRoutingExtensions {
    pub resource_management_router: Router,
    pub memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
    pub worker_file_application: Arc<dyn awaken_resource_contract::FileApplicationService>,
    pub worker_skill_bundles: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<awaken_dispatch::RunClaim>,
    >,
    pub worker_authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    pub worker_placement_policy: Option<Arc<dyn awaken_worker_contract::PlacementPolicy>>,
    pub worker_execution_bindings:
        Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    pub worker_execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    pub credential_deliveries:
        Option<Arc<dyn awaken_runtime_contract::AttemptCredentialDeliverySource>>,
    pub repository_transport_authorizer:
        Option<Arc<dyn awaken_resource_worker_http::RepositoryTransportAuthorizer>>,
    pub worker_directory: Arc<dyn awaken_worker_registry::WorkerDirectory>,
    /// Exact browser origins admitted only to the AI SDK application router.
    pub ai_sdk_browser_cors: AiSdkBrowserCors,
    /// The process wall clock the Worker transport judges leases, registry
    /// freshness and signed assertions with.
    pub worker_clock: Arc<dyn awaken_lease_contract::WallClock>,
}

/// Application-layer authorities mounted together by the one managed data-plane
/// composition. Keeping this dependency cluster explicit prevents production
/// and deterministic hosts from growing parallel assembly signatures.
pub struct ManagedApplicationServices {
    pub session_application: Arc<awaken_session_application::SessionApplication>,
    pub resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    pub model_inventory:
        Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>>,
    pub dream_process_store: Arc<dyn awaken_session_dream_contract::DreamProcessStore>,
    pub executable_projection_refresh:
        Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    #[cfg(feature = "test-support")]
    pub dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
}

enum ApplicationAccessMount {
    Guarded(Arc<application_access_store::ApplicationAccessStore>),
    #[cfg(feature = "test-support")]
    ExplicitlyUnguardedTest,
}

struct RefreshingExecutableAgentInventory {
    source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
    refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
}

#[async_trait::async_trait]
impl awaken_executable_agent_contract::ExecutableAgentInventorySource
    for RefreshingExecutableAgentInventory
{
    async fn current_registrations(
        &self,
        workspace_id: &str,
    ) -> Result<
        Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
        awaken_executable_agent_contract::ExecutableAgentRegistrationError,
    > {
        let admission = self.refresh.refresh().await.map_err(|error| {
            awaken_executable_agent_contract::ExecutableAgentRegistrationError::Unavailable(error)
        })?;
        admission
            .enter(|| self.source.current_registrations(workspace_id))
            .await
    }
}

struct RefreshingEnvironmentWarmups {
    source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::EnvironmentWarmupSource
    for RefreshingEnvironmentWarmups
{
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String> {
        let admission = self.refresh.refresh().await?;
        admission
            .enter(|| self.source.current_environment_warmups())
            .await
    }
}

#[cfg(test)]
mod executable_projection_refresh_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct ToggleRefresh {
        fail: AtomicBool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_session_application_contract::ExecutableAgentProjectionRefresh for ToggleRefresh {
        async fn refresh_agent_projection(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("projection unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_application_contract::ExecutableEnvironmentProjectionRefresh for ToggleRefresh {
        async fn refresh_environment_projection(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("projection unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct Inventory(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for Inventory {
        async fn current_registrations(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct Warmups(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_session_configuration_contract::EnvironmentWarmupSource for Warmups {
        async fn current_environment_warmups(
            &self,
        ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn failed_refresh_never_enters_shared_inventory_or_warmup_handlers() {
        // Causes: C1 executable refresh succeeds/fails; C2 Agent inventory is
        // consumed by Models/Dream; C3 Environment warmups are consumed by the
        // Worker. Effects: E1 failure reaches each consumer as unavailable and
        // its source is not read; E2 recovery delegates once. Constraints: K1
        // adapters own no cursor/cache; K2 Models and Dream share one wrapped
        // inventory. Decision table: D1 !C1+C2=>E1; D2 !C1+C3=>E1;
        // D3 C1+C2+C3=>E2. Each rule requires one Agent attempt and, only after
        // it succeeds, one Environment attempt per handler entry so a false
        // pass cannot come from bypassing the prerequisite altogether.
        let refresh = Arc::new(ToggleRefresh::default());
        let pair = Arc::new(
            awaken_session_application_contract::ExecutableProjectionRefresh::new(
                refresh.clone(),
                refresh.clone(),
            ),
        );
        let inventory = Arc::new(Inventory::default());
        let warmups = Arc::new(Warmups::default());
        let refreshing_inventory = RefreshingExecutableAgentInventory {
            source: inventory.clone(),
            refresh: pair.clone(),
        };
        let refreshing_warmups = RefreshingEnvironmentWarmups {
            source: warmups.clone(),
            refresh: pair,
        };
        refresh.fail.store(true, Ordering::SeqCst);
        assert!(
            awaken_executable_agent_contract::ExecutableAgentInventorySource::current_registrations(
                &refreshing_inventory,
                "workspace",
            )
            .await
            .is_err(),
            "D1/E1"
        );
        assert!(
            awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
                &refreshing_warmups,
            )
            .await
            .is_err(),
            "D2/E1"
        );
        assert_eq!(inventory.0.load(Ordering::SeqCst), 0, "D1/E1");
        assert_eq!(warmups.0.load(Ordering::SeqCst), 0, "D2/E1");
        assert_eq!(refresh.calls.load(Ordering::SeqCst), 2, "D1+D2 gate calls");

        refresh.fail.store(false, Ordering::SeqCst);
        awaken_executable_agent_contract::ExecutableAgentInventorySource::current_registrations(
            &refreshing_inventory,
            "workspace",
        )
        .await
        .unwrap();
        awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
            &refreshing_warmups,
        )
        .await
        .unwrap();
        assert_eq!(inventory.0.load(Ordering::SeqCst), 1, "D3/E2");
        assert_eq!(warmups.0.load(Ordering::SeqCst), 1, "D3/E2");
        assert_eq!(refresh.calls.load(Ordering::SeqCst), 6, "D3 gate calls");
    }
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
    dispatch: &awaken_dispatch::AnyDispatchStore,
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
    use awaken_dispatch::{AnyDispatchStore, Dispatch, MemoryDispatchStore};

    #[test]
    fn checkpoint_authority_selection_is_fail_closed() {
        // Cause/effect graph: C1 the dispatch store owns a paired checkpoint ->
        // E1 reuse it; otherwise C2 storage_dir exists and opens -> E2 durable FS;
        // C2 exists but cannot open -> E3 startup error; with neither, C3 explicit
        // test-support build -> E4 volatile test authority, otherwise E5 startup
        // refusal. Product startup cannot reach E4.
        //
        // | Rule | C1 paired | C2 dir/open | C3 test build | Effect |
        // | R1   | yes       | -           | -             | E1     |
        // | R2   | no        | yes/yes     | -             | E2     |
        // | R3   | no        | yes/no      | -             | E3     |
        // | R4   | no        | no          | yes           | E4     |
        // | R5   | no        | no          | no            | E5     |
        // R1 is covered by the PostgreSQL active/active acceptance test.
        let dispatch = AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(
            awaken_lease_contract::system_clock(),
        )) as Arc<dyn Dispatch>);
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
/// mounted in that same Router. Process startup uses the returned state for
/// periodic policies instead of constructing a second scheduler.
pub fn mount_with_managed_application_access_models_and_dreams(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
    applications: ManagedApplicationServices,
    routing: ManagedRoutingExtensions,
) -> Result<
    (
        Router,
        Router,
        Router,
        Router,
        Arc<awaken_dream_application::DreamApplication>,
    ),
    WorkerTransportBuildError,
> {
    mount_with_managed_over_and_models(
        host,
        managed_state,
        ApplicationAccessMount::Guarded(application_access),
        applications,
        routing,
    )
}

#[cfg(feature = "test-support")]
fn mount_with_managed_over(
    host: Arc<SharedHost>,
    managed_state: ScenarioManagedState,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    application_access: ApplicationAccessMount,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let lifecycle_host = host.clone();
    let remote_worker_required = !host.runs_local_dispatch_pool();
    // A scenario that opts into the same durable storage root as production
    // must not silently retain an in-memory Dream authority. Sessions and
    // Dreams share the concrete sessions.db aggregate in product startup; keep
    // crash/recovery fixtures on that same ownership boundary.
    let dream_process_store = scenario_dream_process_store(host.as_ref());
    let supervised_state = managed_state.clone();
    let session_application = managed_state.sessions;
    let (resources, memory_stores) =
        resource_management_router_from_host(&host, resource_registry.clone());
    let worker_file_application = host
        .file_application()
        .expect("test-support File application");
    let worker_skill_bundles = Arc::new(awaken_resource_application::StoreSkillBundleSource::new(
        host.skill_store().expect("test-support Skill store"),
    ));
    let (managed, public, application, worker_private, dreams) =
        mount_with_managed_over_and_models(
            host,
            managed_state.managed,
            application_access,
            ManagedApplicationServices {
                session_application,
                resource_registry,
                model_inventory: None,
                dream_process_store,
                executable_projection_refresh: None,
                dream_executor_decorator,
            },
            ManagedRoutingExtensions {
                resource_management_router: resources,
                memory_stores,
                worker_file_application,
                worker_skill_bundles,
                worker_authenticator: Arc::new(
                    awaken_worker_transport_security::HeaderWorkerAuthenticator,
                ),
                worker_placement_policy: None,
                worker_execution_bindings: None,
                worker_execution_stops: None,
                credential_deliveries: None,
                repository_transport_authorizer: None,
                worker_directory: test_worker_directory(),
                ai_sdk_browser_cors: AiSdkBrowserCors::default(),
                worker_clock: awaken_lease_contract::system_clock(),
            },
        )
        .expect("test-support Worker transport must assemble");
    let public = with_scenario_worker_transport(
        managed.merge(public).merge(application),
        worker_private,
        remote_worker_required,
    );
    (
        with_scenario_session_lifecycle(public, supervised_state, lifecycle_host, lifecycle),
        dreams,
    )
}

fn mount_with_managed_over_and_models(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    application_access: ApplicationAccessMount,
    applications: ManagedApplicationServices,
    routing: ManagedRoutingExtensions,
) -> Result<
    (
        Router,
        Router,
        Router,
        Router,
        Arc<awaken_dream_application::DreamApplication>,
    ),
    WorkerTransportBuildError,
> {
    let ManagedApplicationServices {
        session_application,
        resource_registry,
        model_inventory,
        dream_process_store,
        executable_projection_refresh,
        #[cfg(feature = "test-support")]
        dream_executor_decorator,
    } = applications;
    let ManagedRoutingExtensions {
        resource_management_router,
        memory_stores,
        worker_file_application,
        worker_skill_bundles,
        worker_authenticator,
        worker_placement_policy,
        worker_execution_bindings,
        worker_execution_stops,
        credential_deliveries,
        repository_transport_authorizer,
        worker_directory,
        ai_sdk_browser_cors,
        worker_clock,
    } = routing;
    // Bind the exact Coordinator Worker registry into the Session root-CAS
    // authority before any claim transport is exposed. HTTP still authenticates
    // each request, but it cannot become a second eligibility source or let an
    // in-process caller bypass the current registry incarnation.
    let worker_observations: Arc<dyn awaken_worker_contract::WorkerObservationSource> =
        worker_directory.clone();
    session_application
        .set_worker_observation_source(worker_observations)
        .map_err(|error| {
            WorkerTransportBuildError::Dispatch(format!(
                "bind Session Worker observation source: {error}"
            ))
        })?;
    let weak_session_application = Arc::downgrade(&session_application);
    host.install_committed_progress_wakeup(Arc::new(move || {
        if let Some(application) = weak_session_application.upgrade() {
            application.request_lifecycle_reconciliation();
        }
    }))
    .map_err(|error| {
        WorkerTransportBuildError::Dispatch(format!(
            "bind committed Runtime progress wakeup: {error}"
        ))
    })?;
    // The Worker warmup projection is part of the same private transport in
    // production and Scenario compositions. Its source is the exact Environment
    // execution application already used by Session admission; this adapter owns
    // no second catalog, WorkQueue, cache, or receipt state.
    let environment_warmup_source: Arc<
        dyn awaken_session_configuration_contract::EnvironmentWarmupSource,
    > = match executable_projection_refresh.clone() {
        Some(refresh) => Arc::new(RefreshingEnvironmentWarmups {
            source: managed_state.environment_execution(),
            refresh,
        }),
        None => managed_state.environment_execution(),
    };
    let environment_warmups = awaken_dispatch_http::worker_environment_warmup_router(
        environment_warmup_source,
        worker_directory.clone(),
        worker_authenticator.clone(),
        worker_clock.clone(),
    );
    let model_inventory =
        model_inventory.map(|source| match executable_projection_refresh.clone() {
            Some(refresh) => Arc::new(RefreshingExecutableAgentInventory { source, refresh })
                as Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
            None => source,
        });
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
    let dream_worker: Arc<dyn awaken_dream_application::DreamExecutor> =
        Arc::new(dream::BuiltInDreamAgent::new(
            session_application.clone(),
            host.memory_repository(),
            resource_registry.clone(),
            memory_stores,
            worker_file_application.clone(),
        ));
    #[cfg(feature = "test-support")]
    let dream_worker = match dream_executor_decorator {
        Some(decorate) => decorate(dream_worker),
        None => dream_worker,
    };
    let dream_application = Arc::new(
        awaken_dream_application::DreamApplication::with_store(dream_worker, dream_process_store)
            .expect("load durable Dream jobs"),
    );
    dream_application.bind_session_source(managed_state.clone());
    struct DreamModelReadiness {
        inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
        sessions: Arc<awaken_session_application::SessionApplication>,
    }
    #[async_trait::async_trait]
    impl awaken_dream_application::DreamModelReadiness for DreamModelReadiness {
        async fn is_ready(&self, workspace_id: &str, model_id: &str) -> Result<bool, String> {
            if !dream_model_can_consume_local_inputs(model_id)? {
                return Ok(false);
            }
            let current = awaken_executable_agent_contract::current_model_references(
                self.inventory.as_ref(),
                workspace_id,
            )
            .await
            .map_err(|error| error.to_string())?;
            if current.iter().any(|reference| reference == model_id) {
                return Ok(true);
            }
            // Force the same parser + publication resolver used by ordinary
            // Session model overrides. The sentinel can never equal a real
            // requested model, so this is resolution-only and persists nothing.
            match self
                .sessions
                .resolve_session_model_override(
                    workspace_id,
                    model_id,
                    "__awaken_dream_model_resolution_probe__",
                    Default::default(),
                )
                .await
            {
                Ok(_) => Ok(true),
                Err(error)
                    if error.kind == awaken_session_error_contract::RunErrorKind::BadRequest =>
                {
                    Ok(false)
                }
                Err(error) => Err(error.to_string()),
            }
        }
    }
    if let Some(inventory) = model_inventory.clone() {
        dream_application.bind_model_readiness(Arc::new(DreamModelReadiness {
            inventory,
            sessions: session_application.clone(),
        }));
    }
    dream_application.resume_incomplete();
    let dreams = awaken_protocol_managed::dreams_router(dream_application.clone());
    let resource_manifests = awaken_protocol_awaken::session_resource_manifest_router(
        awaken_protocol_managed::replace_resource_manifest,
    )
    .with_state(managed_state.clone());
    let session_extensions = awaken_protocol_awaken::profiled_session_router(
        awaken_protocol_managed::create_profiled_session,
    )
    .merge(awaken_protocol_awaken::profiled_session_run_router(
        awaken_protocol_managed::submit_profiled_session_run,
    ))
    .merge(awaken_protocol_awaken::profiled_session_release_router(
        awaken_protocol_managed::release_profiled_session,
    ))
    .merge(awaken_protocol_awaken::session_execution_evidence_router(
        awaken_protocol_managed::get_session_execution_evidence,
    ))
    .with_state(managed_state.clone());
    let managed = router(managed_state.clone())
        .merge(
            awaken_protocol_awaken::session_event_admission_router(
                awaken_protocol_managed::close_event_admission,
            )
            .with_state(managed_state.clone()),
        )
        .merge(dreams)
        .merge(awaken_protocol_awaken::live_inbox_router(
            session_application.clone(),
        ))
        .merge(resource_manifests)
        .merge(session_extensions);
    // One protocol-neutral Run application serves all three wire adapters, so
    // they share the Host with no per-protocol execution path.
    let host_runs: Arc<dyn RunApplication> = Arc::new(RunApplicationHost::new(host.clone()));
    let workspace_host = host.clone();
    let agent_host = host.clone();
    let session_runs = Arc::new(awaken_session_application::SessionRunApplication::new(
        host_runs,
        session_application.clone(),
        move |thread| workspace_host.thread_workspace(thread),
        move |thread| agent_host.thread_agent_projection(thread),
    ));
    let background_runs: Arc<
        dyn awaken_session_application_contract::SessionRunBackgroundApplication,
    > = session_runs.clone();
    ManagedHost::new(host.clone())
        .install_session_background_run_application(Arc::downgrade(&background_runs))
        .expect("Session background Run application binds once");
    let admitted_runs: Arc<dyn RunApplication> = session_runs.clone();
    let ai_sdk = awaken_protocol_ai_sdk::router_with_owner_scope(session_runs.clone());
    let ag_ui = awaken_protocol_ag_ui::router_with_owner_scope(session_runs.clone());
    let (ai_sdk, ag_ui) = match application_access {
        ApplicationAccessMount::Guarded(application_access) => {
            let application_authenticator: Arc<
                dyn awaken_authz_enforce::ApplicationAccessAuthenticator,
            > = application_access;
            (
                ai_sdk.layer(axum::middleware::from_fn_with_state(
                    application_authenticator.clone(),
                    awaken_authz_enforce::application_guard,
                )),
                ag_ui.layer(axum::middleware::from_fn_with_state(
                    application_authenticator,
                    awaken_authz_enforce::application_guard,
                )),
            )
        }
        #[cfg(feature = "test-support")]
        ApplicationAccessMount::ExplicitlyUnguardedTest => (ai_sdk, ag_ui),
    };
    // CORS wraps the already guarded AI SDK router so a matching browser
    // preflight is answered before bearer classification. It is deliberately
    // applied before merging AG-UI, keeping every sibling protocol closed.
    let ai_sdk = ai_sdk_browser_cors.apply(ai_sdk);
    let application = ai_sdk.merge(ag_ui);
    let a2a = awaken_protocol_a2a::router_with_storage_root(admitted_runs, host.storage_dir());
    // A coordinator-only Host owns both dispatch and committed Thread truth but
    // deliberately has no execution pool. Start its one environment-free repair
    // loop before exposing the Worker transport; local-pool embeddings already
    // perform the same maintenance inside that pool.
    host.ensure_terminal_dispatch_reconciliation();
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / manual quarantine + GC) over the same shared host.
    let durable_ops = awaken_dispatch_http::durable_ops_router_with_session_admission(
        host.clone(),
        session_runs.clone(),
        session_runs,
    );
    // The Worker-facing cross-node seam: a dispatch-store-isolated Worker claims/settles runs
    // over the dispatch transport and pushes committed facts to the commit ingest.
    let dispatch = host
        .dispatch_store()
        .map_err(|error| WorkerTransportBuildError::Dispatch(error.to_string()))?;
    let checkpoint = worker_checkpoint_authority(dispatch.as_ref(), host.storage_dir())?;
    let dispatch_router = awaken_dispatch_http::registered_dispatch_router(
        awaken_dispatch_http::RegisteredDispatchDependencies {
            execution_bindings: worker_execution_bindings,
            execution_stops: worker_execution_stops,
            dispatch: dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
            checkpoint,
            directory: worker_directory.clone(),
            policy: worker_placement_policy
                .unwrap_or_else(|| worker_placement::shared_worker_placement_policy()),
            sessions: session_application.clone(),
            session_environment_bindings: session_application.environment_binding_sink(),
            coordination: session_application.clone(),
            session_work: session_application.clone(),
            authenticator: worker_authenticator.clone(),
            recovery: host.worker_recovery_source(),
            completion: host.worker_completion_sink(),
            terminal_observer: host.worker_memory_settlement_observer(),
            stream_sink: host.worker_stream_sink(),
            credential_deliveries,
            clock: worker_clock,
        },
    );
    let file_content = awaken_resource_worker_http::worker_file_content_router(Arc::new(
        awaken_resource_worker_http::WorkerFileContentService::new(
            host.worker_file_content_source(),
            dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
            worker_authenticator.clone(),
        )
        .with_worker_directory(worker_directory.clone())
        .with_session_repository(session_application.session_repository_handle())
        .with_session_control(session_application.clone())
        .with_recovery(host.worker_recovery_source()),
    ));
    let artifact_publication =
        awaken_resource_worker_http::worker_artifact_publication_router(Arc::new(
            awaken_resource_worker_http::WorkerArtifactPublicationService::new(
                worker_file_application,
                dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
                worker_authenticator.clone(),
                worker_directory.clone(),
            )
            .with_session_control(session_application.clone()),
        ));
    let memory = awaken_resource_worker_http::worker_memory_router(Arc::new(
        awaken_resource_worker_http::WorkerMemoryService::new(
            host.memory_repository(),
            resource_registry.clone(),
            dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
            worker_authenticator.clone(),
            worker_directory.clone(),
        )
        .with_materialization_control(session_application.clone())
        .with_terminal_memory_control(session_application.clone()),
    ));
    let repository_service = awaken_resource_worker_http::WorkerRepositoryBindingService::new(
        resource_registry,
        dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
        worker_authenticator.clone(),
    )
    .with_worker_directory(worker_directory.clone())
    .with_session_control(session_application.clone());
    let repository_service = match repository_transport_authorizer {
        Some(authorizer) => repository_service.with_transport_authorizer(authorizer),
        None => repository_service,
    };
    let repositories =
        awaken_resource_worker_http::worker_repository_binding_router(Arc::new(repository_service));
    let resource_worker = file_content
        .merge(artifact_publication)
        .merge(memory)
        .merge(repositories)
        .merge(awaken_resource_worker_http::worker_skill_bundle_router(
            Arc::new(
                awaken_resource_worker_http::WorkerSkillBundleService::new(
                    worker_skill_bundles,
                    dispatch.clone() as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
                    worker_authenticator.clone(),
                )
                .with_worker_directory(worker_directory.clone())
                .with_session_control(session_application.clone()),
            ),
        ));
    let commit = Arc::new(awaken_dispatch_http::ClaimedCommitHttpService::new(
        Arc::new(awaken_session_executor::claimed_commit_service(
            dispatch as Arc<dyn awaken_dispatch::ControlDispatchQueue>,
            host.clone(),
        )),
        worker_directory.clone(),
        worker_authenticator,
    ));
    let worker_transport =
        awaken_dispatch_http::registered_worker_transport_router_with_services(
            dispatch_router,
            resource_worker,
            commit,
        )
        .merge(environment_warmups);
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = model_inventory.map_or_else(
        || models_router(std::sync::Arc::new(default_models())),
        awaken_protocol_managed::models_router_with_inventory,
    );
    let managed = managed.merge(resource_management_router).merge(models);
    let managed = awaken_protocol_managed::with_managed_response_context(managed);
    let local_workspace = host.local_workspace().to_string();
    // Fleet readiness is a deployment-wide read, so it joins the
    // service-authenticated product surface rather than the Managed data
    // plane, whose beta-header gate would reject a plain fleet question.
    let router = a2a
        .merge(durable_ops)
        .merge(crate::worker_fleet::router(worker_directory.clone()));
    let router = with_local_workspace_scope(router, local_workspace);
    let worker_transport =
        with_local_workspace_scope(worker_transport, host.local_workspace().to_string());
    Ok((
        managed,
        router,
        application,
        worker_transport,
        dream_application,
    ))
}

/// Dream's ordinary auxiliary Session consumes local Memory/File mounts. Native
/// and ACP backends share that Session environment; an outbound A2A reference
/// denotes a complete remote agent and is intentionally executor-only, so it
/// cannot receive those local authorities.
fn dream_model_can_consume_local_inputs(model_id: &str) -> Result<bool, String> {
    let selection = awaken_config_service::parse_managed_model_id(model_id)
        .map_err(|error| error.to_string())?;
    Ok(!matches!(
        selection,
        awaken_agent_config::ModelSelection::Pinned(ref binding)
            if matches!(
                binding.backend(),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            )
    ))
}

#[cfg(test)]
mod dream_model_runtime_tests {
    #[test]
    fn dream_runtime_admission_matches_the_local_input_boundary() {
        for model in [
            "claude-sonnet-5",
            "deepseek-v4;provider=deepseek;api=anthropic_messages",
            "qwen/qwen3;provider=anyrouter;api=open_ai_chat;executor=acp:opencode",
            "executor=acp:claude",
            "profile=dream-primary",
        ] {
            assert!(
                super::dream_model_can_consume_local_inputs(model).unwrap(),
                "{model}"
            );
        }
        assert!(
            !super::dream_model_can_consume_local_inputs(
                "executor=a2a:https://third-party.example/agent"
            )
            .unwrap()
        );
        assert!(super::dream_model_can_consume_local_inputs("executor=a2a:").is_err());
    }
}

fn with_local_workspace_scope(router: Router, local_workspace: String) -> Router {
    router.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let local_workspace = local_workspace.clone();
            async move {
                // A scope-less request is the local/single-tenant mode. Resolve
                // that mode once at the process edge so every adapter sees
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
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
) -> (
    Router,
    Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
) {
    // The Scenario composition's Resources clock: registration commands, the
    // Files API and the registry read one timeline.
    let clock = awaken_lease_contract::system_clock();
    let purge: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler> =
        Arc::new(awaken_resource_application::RepositoryPurgeScheduler::new(
            host.resource_reclamation()
                .expect("resource management requires lifecycle persistence"),
        ));
    let memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService> =
        Arc::new(awaken_resource_application::MemoryStoreApplication::new(
            catalog.clone(),
            purge.clone(),
            clock.clone(),
        ));
    (
        resources_router(ResourcesRouterInput {
            repositories: Arc::new(awaken_resource_application::RepositoryApplication::new(
                catalog,
                Arc::new(awaken_resource_contract::UnavailableRepositoryCredentialValidator),
                clock.clone(),
            )),
            files: host
                .file_application()
                .expect("resource management requires the File application"),
            memories: host.memory_repository(),
            memory_stores: memory_stores.clone(),
            skills: host.skill_store(),
            purge,
            clock,
        }),
        memory_stores,
    )
}

pub use awaken_credential_materializer::{
    ResolvedExecutorError, executor_from_materialized_endpoint,
};
