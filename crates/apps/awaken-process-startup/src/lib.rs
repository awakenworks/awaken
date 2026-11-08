// @formal entrypoint credential.restart_recovery_compensates_unpublished_material
//! `awaken-process-startup`: the product-process startup every service
//! executable composes.
//!
//! Control (`awaken-control`) and Coordinator (`awaken-coordinator`) do not depend on
//! each other. This crate starts their product processes: it opens deployment
//! stores, builds shared adapters, asks each owner for its router, and exposes
//! exactly the API selected by `config::Role`. AllInOne merges those same routers;
//! it does not maintain a second implementation. The role-named Control and
//! Coordinator executables terminate in the same lifecycle, while Worker keeps
//! its independent authority-free bootstrap.
//!
//! The crate declares no executable and links no Worker. The `awaken`
//! launcher (`awaken-cli`) supplies the Worker an AllInOne process co-locates
//! through [`ColocatedWorkerPreparation`]; the Coordinator process package
//! (`awaken-coordinator-process`) declares `awaken-coordinator` over this same
//! startup and supplies none.
mod assistant_selection;
mod brokered_catalog;
#[cfg(any(test, feature = "test-support"))]
mod colocated_test_process;
mod colocated_worker;
pub mod config;
pub mod console;
mod console_assets;
mod control;
mod control_component;
#[cfg(any(test, feature = "test-support"))]
pub use colocated_test_process::{
    ColocatedTestProcess, ColocatedTestStores, ColocatedTestWorkerInputs,
    start_all_in_one_with_worker,
};
mod coordinator_execution;
mod credential_probe;
mod deployment_process;
#[cfg(any(test, feature = "test-support"))]
mod exact_host_model;
mod executable_agent_registration;
mod executable_environment_registration;
mod executable_projection_refresh;
mod hosted_model_publication;
mod identity;
mod installation_binding;
#[cfg(any(test, feature = "test-support"))]
mod local_process_stores;
mod observation_reconcile;
mod postgres_pools;
mod process_admin;
mod process_startup;
mod process_stores;
mod process_surface;
mod runtime_process_router;
#[cfg(any(test, feature = "test-support"))]
mod scenario_runtime;
mod service;
mod web_search_publication;
mod worker_observation_wiring;
mod worker_transport;
mod worker_transport_security;

pub use coordinator_execution::{
    ExecutionCompositionError, SessionCommitOwner, verify_composition,
};
#[cfg(any(test, feature = "test-support"))]
pub use scenario_runtime::scenario_runtime_authority;
pub use worker_transport::WorkerTransport;

use std::sync::Arc;

use awaken_config_service::ManagementAuditPlane;
use awaken_control::ExtMcpProbe;
use awaken_protocol_managed::ManagedState;
#[cfg(any(test, feature = "test-support"))]
use awaken_runtime_contract::llm::LlmExecutor;
use axum::Router;
#[cfg(any(test, feature = "test-support"))]
use exact_host_model::ExactHostModelPublicationResolver;
#[cfg(any(test, feature = "test-support"))]
use local_process_stores::in_memory_process_stores;
#[cfg(test)]
use local_process_stores::open_local_process_stores;
#[cfg(test)]
use local_process_stores::{
    in_memory_control_stores, in_memory_split_coordinator, process_stores_for_runtime_storage,
};

pub use crate::process_admin::{
    DrainController, process_admin_router, register_active_streams_gauge, with_connection_metric,
    with_process_admin,
};
pub use awaken_coordinator::{
    ColocatedExecution, ColocatedExecutionInputs, InferenceSelection, PreparedColocatedExecution,
};
pub use colocated_worker::{ColocatedWorker, ColocatedWorkerPreparation};
pub use console_assets::mount as mount_console;
pub use console_assets::mount_with_navigation as mount_console_with_navigation;
pub use control::{
    build_control_router, build_control_router_with_publication_resolver,
    build_control_router_with_publication_resolver_and_web_search, prepare_control_process,
    prepare_control_process_with_managed_services,
    prepare_control_process_with_publication_resolver,
    prepare_control_process_with_publication_resolver_and_web_search,
    prepare_control_process_with_publication_resolver_web_search_and_lifecycle_delivery,
};
use control_component::{control_component_for_process, prepare_control_routers};
mod credential_execution;
pub use deployment_process::migrate_deployment_schema;
use deployment_process::{
    prepare_runtime_process, prepare_runtime_process_with_coordinator_services,
};
use hosted_model_publication::{
    HostedAndDirectModelPublicationResolver, PublicationModelSupply, brokered_inference_client,
    local_client_instance_id,
};
use identity::identity_wiring;
pub use installation_binding::{PreparedDeploymentInstallations, verify_deployment_installations};
pub use managed_platform::{
    CoordinatorServiceAdapters, ManagedBackgroundService, ManagedServiceAdapters,
};
pub use process_startup::block_on_service;
use process_startup::{ProcessStartup, local_model_supply};
#[cfg(test)]
use process_stores::role_hosts_resources;
use process_stores::{
    CoordinatorStores, MigrationComponent, PostgresSchemaMode, ProcessStoreOpenOptions,
    ProcessStores, migration_manifest, open_process_stores,
};
#[cfg(test)]
use process_stores::{role_owns_control_component, role_owns_managed_execution};
use runtime_process_router::prepare_runtime_routers;
pub use service::{
    ServiceRole, migrate_service, migrate_service_with_request, run_all_in_one,
    run_all_in_one_with_services, run_service, run_service_binary, serve_prepared_control,
    serve_prepared_coordinator,
};

// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, HttpOrdinaryWorkspaceAdmission,
    HttpPrivateWorkspaceContext, ManagementAuthz, ManagementIdentityMode, RemoteManagementAuthz,
    TokenSpec, embedded_iam,
};
mod live_runtime_capabilities;
mod managed_platform;
use live_runtime_capabilities::LiveRuntimeCapabilities;

/// Publication policy plus a deferred runtime choice. Standalone Control uses
/// only the resolver; execution adapters are realized only by runtime process.
struct ResolvedModelServices {
    publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    runtime: InferenceSelection,
}

#[derive(Clone)]
struct ControlServices {
    audit: Arc<dyn awaken_config_service::ManagementAuditRepository>,
    credentials: Arc<dyn awaken_credential_contract::SessionCredentialSource>,
    repository_credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    webhooks: Arc<dyn awaken_session_contract::LifecycleFactDelivery>,
    capture_policy: awaken_data_governance_contract::CapturePolicySources,
}

impl ControlServices {
    fn remote(
        client: Arc<awaken_coordinator::control_service_boundary::HttpControlServiceClient>,
    ) -> Self {
        Self {
            audit: client.clone(),
            credentials: client.clone(),
            repository_credentials: client.clone(),
            webhooks: client.clone(),
            capture_policy: awaken_data_governance_contract::CapturePolicySources {
                consent: client.clone(),
                narrowing: client,
            },
        }
    }
}
mod prepared_process;
use prepared_process::ProcessRouters;
pub use prepared_process::{CoordinatorAuthorityHandles, PreparedProcess};

/// Build all-in-one from the standard typed deployment configuration, its
/// local claim pool running Sessions through `execution`.
pub async fn prepare_all_in_one_process_with_execution(
    execution: Box<dyn ColocatedExecution>,
    observability: &awaken_observability::ObservabilityConfig,
) -> PreparedProcess {
    prepare_all_in_one_process_with_model_supply(
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
        execution,
        observability,
    )
    .await
}

/// One volatile management-only composition, retaining its lifecycle and a
/// borrowed Session read authority for the in-crate management test. It runs
/// no Session, so it starts no Worker; its Coordinator owns no claim pool.
#[cfg(test)]
async fn prepare_ephemeral_all_in_one_process() -> (
    ProcessRouters,
    Arc<dyn awaken_session_contract::SessionRepository>,
) {
    let stores = in_memory_process_stores().await;
    let sessions = stores
        .coordinator
        .as_ref()
        .expect("AllInOne borrows its selected Session authority")
        .sessions
        .clone();
    let options = exact_host_model::registered_test_process_options(&stores);
    let process = prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        options,
    )
    .await
    .expect("prepare ephemeral all-in-one process");
    (process, sessions)
}

/// Canonical product process from the command's one resolved configuration.
pub async fn prepare_all_in_one_process(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<PreparedProcess, String> {
    prepare_runtime_process(
        deployment,
        Some(key),
        config::Role::AllInOne,
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Prepare the canonical AllInOne process with product-supplied managed and
/// Coordinator infrastructure adapters.
///
/// Commercial compositions use this seam to retain Awaken's complete
/// Managed Agents and Environment surface while replacing only narrow
/// infrastructure ports. Routers, stores, migrations, and lifecycle remain
/// owned by Awaken.
pub async fn prepare_all_in_one_process_with_services(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    managed_services: ManagedServiceAdapters,
    coordinator_services: CoordinatorServiceAdapters,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process_with_coordinator_services(
        deployment,
        Some(key),
        config::Role::AllInOne,
        PublicationModelSupply::PublishedProviders,
        managed_services,
        coordinator_services,
    )
    .await
}

/// Coordinator-only process. It reuses the exact same Session, Run,
/// Dispatch, and Worker-coordination construction as all-in-one, while the
/// shared role selector keeps Control routes outside the exposed API.
pub async fn prepare_coordinator_process(
    deployment: &config::ResolvedDeployment,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process(
        deployment,
        None,
        config::Role::Coordinator,
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Prepare the one canonical Coordinator with hosted infrastructure adapters.
/// The returned routers are still built exclusively by
/// `awaken-coordinator`; callers may merge only non-overlapping product routes.
pub async fn prepare_coordinator_process_with_services(
    deployment: &config::ResolvedDeployment,
    managed_services: ManagedServiceAdapters,
    coordinator_services: CoordinatorServiceAdapters,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process_with_coordinator_services(
        deployment,
        None,
        config::Role::Coordinator,
        PublicationModelSupply::PublishedProviders,
        managed_services,
        coordinator_services,
    )
    .await
}

/// Build the real env-selected management surface with one explicit in-process
/// scenario executor. This is a dev/e2e startup, not a provider fallback:
/// its exact host candidate is published by a dedicated resolver and no
/// credential/provider materializer is installed.
#[cfg(any(test, feature = "test-support"))]
pub async fn prepare_all_in_one_process_with_scenario_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: String,
    managed_services: ManagedServiceAdapters,
    execution: Box<dyn ColocatedExecution>,
    observability: &awaken_observability::ObservabilityConfig,
) -> PreparedProcess {
    prepare_all_in_one_process_with_model_supply(
        PublicationModelSupply::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "default",
            ),
        },
        managed_services,
        execution,
        observability,
    )
    .await
}

async fn prepare_all_in_one_process_with_model_supply(
    model_supply: PublicationModelSupply,
    managed_services: ManagedServiceAdapters,
    execution: Box<dyn ColocatedExecution>,
    observability: &awaken_observability::ObservabilityConfig,
) -> PreparedProcess {
    let mut deployment = config::ResolvedDeployment::load(
        config::ConfigOverrides::default(),
        deployment_process::process_clock_for(None),
    )
    .unwrap_or_else(|error| panic!("deployment configuration: {error}"));
    // This caller already selected and initialized its exporter. Scenario roots
    // can override the file input; a second config load cannot replace that fact.
    deployment.observability = observability.clone();
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("control seal key: {error}"));
    prepare_runtime_process_with_coordinator_services(
        &deployment,
        Some(&key),
        config::Role::AllInOne,
        model_supply,
        managed_services,
        CoordinatorServiceAdapters::default().with_colocated_execution(execution),
    )
    .await
    .unwrap_or_else(|error| panic!("prepare all-in-one process: {error}"))
}

/// [`build_all_in_one_router_with_model`] plus explicit test-only process inputs.
/// `deployment` is the caller's one typed runtime configuration; this seam must
/// not rediscover or replace storage, sandbox, or topology state while composing
/// the managed plane.
/// `web_search_providers` is selected before Control and Host are assembled, so
/// publication, capability discovery, and dispatch all receive the same registry.
/// The supplied constructor owns every execution-specific selection, including
/// any scenario backend customization, before shared startup receives its roles.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_all_in_one_service_with_execution_inputs(
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    deployment: awaken_deployment_config::DeploymentConfig,
    web_search_providers: Option<awaken_ext_builtin_tools::WebSearchProviderRegistry>,
    execution: Box<dyn ColocatedExecution>,
    trace_file_present: bool,
) -> (Router, awaken_service_lifecycle::ServiceLifecycle) {
    let stores = in_memory_process_stores().await;
    let mut options = exact_host_model::local_test_process_options(&stores, execution);
    options.content_capture_ceiling = awaken_data_governance_contract::clamp_for_trace_file(
        deployment.content_capture.level,
        trace_file_present,
    );
    options.trace_file_present = trace_file_present;
    options.deployment = Some(deployment);
    options.web_search_providers = web_search_providers;
    let prepared = prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::Host {
            executor: model,
            binding,
        },
        options,
    )
    .await
    .expect("prepare test-support customized all-in-one process");
    (prepared.public_router, prepared.service_lifecycle)
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected, its Sessions running on this process's local claim pool.
///
/// Retained only for the MCP tests whose behaviour exists solely on that
/// placement (inline initial-realization failure at create, mid-connect OAuth
/// refresh); every other test runs the shipped topology through
/// [`start_all_in_one_with_worker`].
#[cfg(any(test, feature = "test-support"))]
pub async fn build_all_in_one_router_with_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    execution: Box<dyn ColocatedExecution>,
) -> Router {
    let stores = in_memory_process_stores().await;
    let options = exact_host_model::local_test_process_options(&stores, execution);
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "genai",
            ),
        },
        options,
    )
    .await
    .expect("prepare test-support modeled all-in-one process")
    .public_router
}

fn resolve_model_services(
    supply: PublicationModelSupply,
    stores: &ProcessStores,
    cloud_models_enabled: bool,
    credential_material_delivery: Option<awaken_credential_contract::CredentialMaterialDelivery>,
    worker_observations: Arc<dyn awaken_coordinator::WorkerObservationSource>,
) -> ResolvedModelServices {
    let control = stores
        .control
        .as_ref()
        .expect("model publication requires Control stores");
    let acp_capabilities = Arc::new(awaken_acp_catalog::known_acp_publication_capabilities());
    let direct_resolver = || {
        awaken_control::model_publication::CatalogModelPublicationResolver::from_repo(
            control.catalog.clone(),
            control.credentials.clone(),
            stores.clock.clone(),
        )
        .with_credential_material_delivery(
            control.secrets.clone(),
            credential_material_delivery.clone(),
        )
        .with_acp_capabilities(acp_capabilities.clone())
        .with_profiles(control.profiles.clone())
        .with_worker_observations(worker_observations.clone())
    };
    match supply {
        PublicationModelSupply::PublishedProviders => ResolvedModelServices {
            publication_resolver: Arc::new(
                direct_resolver()
                    .with_brokered_access(cloud_models_enabled)
                    // Freeze the same exact backend-specific recipients used by
                    // Session execution: Native/A2A at Worker, ACP at Workload.
                    // A Native envelope cannot be reused for an ACP candidate.
                    .with_direct_provider_credential_execution(
                        awaken_agent_contract::credential::CredentialExecutionPolicy::self_hosted_provider(),
                        (
                            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native().inference_holder,
                            awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp().inference_holder,
                        ),
                    ),
            ),
            runtime: InferenceSelection::PublishedProviders,
        },
        PublicationModelSupply::HostedPublication {
            resolver,
            direct_credential_execution,
        } => {
            let publication_resolver = if let Some(execution) = direct_credential_execution {
                let direct = direct_resolver()
                    .with_brokered_access(cloud_models_enabled)
                    .with_brokered_publication(resolver.clone())
                    .with_direct_provider_credential_execution(execution.policy, execution.holder);
                Arc::new(HostedAndDirectModelPublicationResolver {
                    hosted: resolver,
                    direct: Arc::new(direct),
                }) as Arc<dyn awaken_config_service::ModelPublicationResolver>
            } else {
                resolver
            };
            ResolvedModelServices {
                publication_resolver,
                runtime: InferenceSelection::NoModelConfigured,
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        PublicationModelSupply::Host { executor, binding } => {
            let model_ref = binding.identity().1.to_owned();
            ResolvedModelServices {
                publication_resolver: Arc::new(ExactHostModelPublicationResolver { binding }),
                runtime: InferenceSelection::Host {
                    executor,
                    model_ref,
                },
            }
        }
    }
}

/// Prepare Coordinator and, for AllInOne, include the canonical Control service.
/// The data plane comes from [`awaken_coordinator::mount_with_managed`]; this
/// process boundary joins their HTTP surfaces and supervises lifecycle without
/// rebuilding either domain application.
/// Resolve the hidden local Org from one process-startup seam. Self-managed
/// deployments may explicitly configure it; single-machine mode never asks the
/// user and consistently uses the default Org.
fn local_org_id() -> String {
    awaken_control::DEFAULT_ORG_ID.to_owned()
}

// Seal-key parsing and its decision tests live with the authoritative AEAD
// adapter in `awaken_credential_store::sealed`.

#[cfg(test)]
mod runtime_session_store_tests {
    use super::*;
    use awaken_agent_contract::stable_fingerprint;
    use awaken_config_service::ModelPublicationResolver;
    use awaken_environment_contract::EnvironmentRevision;
    use awaken_session_configuration_contract::{
        ControlSessionCreationInputs, EnvironmentFingerprint, EnvironmentSnapshot,
        SessionBaselineState, SessionCreationIntent, SessionNetworkPolicy,
    };
    use awaken_session_contract::{IdempotencyRecord, PersistedSession};

    fn creation_intent() -> SessionCreationIntent {
        SessionCreationIntent {
            control: ControlSessionCreationInputs {
                mutation_policy:
                    awaken_session_configuration_contract::resource::SessionMutationPolicy::Managed,
                application_origin: None,
                environment: EnvironmentSnapshot {
                    environment_id: awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
                        .into(),
                    revision: EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: EnvironmentFingerprint("env-local".into()),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: SessionNetworkPolicy::Unrestricted,
                    credential_realization:
                        awaken_credential_contract::CredentialRealizationProfile {
                            inference_holder:
                                awaken_agent_contract::credential::PlaintextHolder::new(
                                    awaken_agent_contract::credential::PlaintextBoundary::Workload,
                                    "awaken.workload.acp",
                                ),
                            mcp_holder: awaken_agent_contract::credential::PlaintextHolder::new(
                                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder:
                                awaken_agent_contract::credential::PlaintextHolder::new(
                                    awaken_agent_contract::credential::PlaintextBoundary::Worker,
                                    "awaken.worker",
                                ),
                        },
                },
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
                agent_id: "assistant".into(),
                agent_revision: None,
                model_override: None,
                system_prompt:
                    awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
                model: "test-model".into(),
                execution_model_ref: "test-model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
        }
    }

    pub(super) fn session(id: &awaken_session_types::SessionId) -> PersistedSession {
        let intent = creation_intent();
        let mut legacy = PersistedSession::frozen_with_budget(
            id.clone(),
            intent.clone().finalize().unwrap().baseline,
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        legacy.baseline = SessionBaselineState::Preparing(intent);
        legacy.execution = awaken_session_contract::SessionExecutionState::Idle;
        // Interrupted preparation is historical wire input, never a new Create.
        let mut wire = serde_json::to_value(legacy).unwrap();
        wire.as_object_mut()
            .unwrap()
            .remove("environment_materialization");
        serde_json::from_value(wire).unwrap()
    }

    #[tokio::test]
    async fn runtime_storage_reopens_the_session_and_its_owner_fence() {
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        {
            let stores = process_stores_for_runtime_storage(Some(dir.path())).await;
            let execution = stores
                .coordinator
                .as_ref()
                .expect("test startup owns Managed Execution");
            let value = session(&awaken_session_types::SessionId::from("sesn-restart"));
            let payload_hash = stable_fingerprint(&value);
            execution
                .sessions
                .create(
                    "workspace-a",
                    value,
                    IdempotencyRecord {
                        key: "test:runtime-session-restart".into(),
                        payload_hash,
                    },
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        let reopened = process_stores_for_runtime_storage(Some(dir.path())).await;
        let execution = reopened
            .coordinator
            .as_ref()
            .expect("test startup owns Managed Execution");
        assert_eq!(
            execution
                .sessions
                .owner(&awaken_session_types::SessionId::from("sesn-restart"))
                .await
                .as_deref(),
            Ok("workspace-a")
        );
        assert_eq!(
            execution
                .sessions
                .get(&awaken_session_types::SessionId::from("sesn-restart"))
                .await
                .expect("session survives restart")
                .baseline,
            SessionBaselineState::Preparing(creation_intent())
        );
    }

    #[tokio::test]
    async fn local_store_startup_publishes_one_workspace_after_session_authority() {
        /* Lower-level post-admission opener design: C1 an explicitly authorized
         * migration supplies absent sessions.db (first install) or a canonical
         * marker-free Session DB (store-first retry); C2 canonical Session open
         * succeeds. Effects: E1 store startup completes;
         * E2 one platform-workspace-id is published and the exact value is
         * retained in ProcessStores for IAM/routers; E3 no sibling store owns a
         * second publication. Decision table: SO1 absent+C2=>E1+E2+E3;
         * SO2 canonical marker-free+C2=>E1+E2+E3. The ordinary entry cannot call
         * these rows: installation_binding owns its exact-only preflight. */
        for (rule, preopen_session) in [("SO1", false), ("SO2", true)] {
            let directory = tempfile::tempdir().unwrap();
            let sessions = directory.path().join("sessions.db");
            if preopen_session {
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &sessions.to_string_lossy(),
                )
                .await
                .unwrap()
                .close()
                .await;
            }
            let marker = directory.path().join("platform-workspace-id");
            assert!(!marker.exists(), "{rule} precondition");

            let stores = open_local_process_stores(directory.path(), &[0x61; 32])
                .await
                .unwrap_or_else(|error| panic!("{rule} startup: {error}"));
            let persisted = std::fs::read_to_string(&marker).unwrap();
            assert_eq!(persisted.trim(), stores.platform_workspace, "{rule}/E2");
            assert!(sessions.exists(), "{rule}/E1");
        }
    }

    #[tokio::test]
    async fn session_and_deployment_ports_share_one_physical_repository() {
        // Cause/effect decision table: D1 durable runtime storage -> Session and
        // Deployment typed ports point at one concrete repository allocation;
        // D2 ephemeral startup -> the same single-allocation invariant holds.
        // A different address would expose a parallel Deployment truth before
        // the repository-backed Deployment component is prepared.
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        let durable = process_stores_for_runtime_storage(Some(dir.path())).await;
        let durable = durable
            .coordinator
            .as_ref()
            .expect("durable test startup owns Managed Execution");
        assert_eq!(
            Arc::as_ptr(&durable.sessions) as *const (),
            Arc::as_ptr(&durable.deployments) as *const (),
            "D1"
        );

        let ephemeral = process_stores_for_runtime_storage(None).await;
        let ephemeral = ephemeral
            .coordinator
            .as_ref()
            .expect("ephemeral test startup owns Managed Execution");
        assert_eq!(
            Arc::as_ptr(&ephemeral.sessions) as *const (),
            Arc::as_ptr(&ephemeral.deployments) as *const (),
            "D2"
        );
    }

    #[tokio::test]
    async fn scenario_host_resolves_a_pinned_binding_only_when_it_names_the_host_route() {
        // C1 exact pin == host binding -> E1 published; a model-only pin is refused by the binding.
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::new("default", "scenario", "default");
        let resolver = ExactHostModelPublicationResolver {
            binding: binding.clone(),
        };
        let resolved = resolver
            .resolve_models(
                &awaken_agent_contract::ScopeId::from("workspace-a"),
                &awaken_agent_config::ModelSelection::Pinned(binding.clone()),
                &[],
            )
            .await
            .expect("C1: an exact pin resolves through the host adapter");
        assert_eq!(resolved.primary.binding(), &binding);
        for backend in ["", "default"] {
            let pin =
                awaken_runtime_contract::resolved::ModelBinding::parse("", "scenario", backend);
            assert!(
                pin.is_err(),
                "model-only pin {backend:?} is unrepresentable"
            );
        }
    }

    #[tokio::test]
    async fn scenario_host_rejects_a_conflicting_provider_qualified_binding() {
        let resolver = ExactHostModelPublicationResolver {
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", "scenario", "default",
            ),
        };

        let error = resolver
            .resolve_models(
                &awaken_agent_contract::ScopeId::from("workspace-a"),
                &awaken_agent_config::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        "other", "scenario", "default",
                    ),
                ),
                &[],
            )
            .await
            .expect_err("an exact provider selection cannot drift to the host executor");

        assert!(error.to_string().contains("cannot publish model"));
    }
}

#[cfg(test)]
mod process_role_surface_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_agent_contract::stable_fingerprint;
    use awaken_session_contract::IdempotencyRecord;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    use super::*;

    fn executable_agent_registration_body() -> Vec<u8> {
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .fingerprint("fp-a")
            .build();
        snapshot.metadata = awaken_runtime_contract::AgentSnapshotMetadata {
            source: awaken_runtime_contract::AgentConfigRevisionRef {
                agent_id: awaken_runtime_contract::snapshot::AgentId("agent-a".into()),
                revision: 1,
            },
            publication_version: awaken_runtime_contract::AgentPublicationVersion("v1".into()),
            resolution: Default::default(),
            fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint("fp-a".into()),
        };
        serde_json::to_vec(
            &awaken_executable_agent_contract::ExecutableAgentRegistration {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                source_revision: 1,
                published_at_unix_ms: None,
                publication_sequence: None,
                publication_order_verified: false,
                snapshot,
                session_profile: Default::default(),
            },
        )
        .unwrap()
    }

    #[test]
    fn control_component_has_exactly_the_authoring_process_owners() {
        // Cause/effect decision table:
        // R1 Control and R2 AllInOne contain the authoring authority, so they
        // build the canonical Control component (including Vault recovery).
        // R3 Coordinator and R4 Worker consume only explicit projections and
        // must not construct a parallel ConfigService or Control router.
        assert!(role_owns_control_component(config::Role::Control), "R1");
        assert!(role_owns_control_component(config::Role::AllInOne), "R2");
        assert!(
            !role_owns_control_component(config::Role::Coordinator),
            "R3"
        );
        assert!(!role_owns_control_component(config::Role::Worker), "R4");
    }

    #[test]
    fn managed_execution_stores_have_only_execution_process_owners() {
        // Cause/effect decision table:
        // R1 Coordinator owns the distributed execution boundary and R2
        // AllInOne configures it locally, so both open the Session/Deployment store
        // group. R3 Control and R4 Worker must never create a second local group.
        assert!(role_owns_managed_execution(config::Role::Coordinator), "R1");
        assert!(role_owns_managed_execution(config::Role::AllInOne), "R2");
        assert!(!role_owns_managed_execution(config::Role::Control), "R3");
        assert!(!role_owns_managed_execution(config::Role::Worker), "R4");
    }

    #[test]
    fn resource_authorities_have_only_resource_serving_process_owners() {
        // Cause/effect decision table:
        // R1 AllInOne and R2 Coordinator serve claim-fenced Resource APIs and
        // therefore host the canonical authorities. R3 Control consumes only
        // Resource authoring/read services; R4 Worker consumes authenticated per-kind
        // clients, so neither may open Resource authorities.
        assert!(role_hosts_resources(config::Role::AllInOne), "R1");
        assert!(role_hosts_resources(config::Role::Coordinator), "R2");
        assert!(!role_hosts_resources(config::Role::Control), "R3");
        assert!(!role_hosts_resources(config::Role::Worker), "R4");
    }

    /// Cause/effect decision table:
    ///
    /// | role/listener | authoring API | Session/Deployment | Worker transport | service API |
    /// | --- | --- | --- | --- | --- |
    /// | Control/public | mounted | absent | absent | absent |
    /// | Control/private | absent | absent | absent | authenticated |
    /// | Coordinator/public | absent | mounted | absent | absent |
    /// | Coordinator/private | absent | absent | authenticated once | authenticated |
    ///
    /// AllInOne local-adapter startup is covered by the existing full-surface integration
    /// suites; this test owns the two exclusion rules that those suites cannot
    /// prove. Moving the startup body into `runtime_process_router` adds no
    /// new condition or outcome, so a separate decision table is inapplicable:
    /// these same route-presence/absence effects are the structural-extraction
    /// regression coverage. The hosted custom-publication entry point delegates
    /// to this same process and only substitutes publication SPIs, so these
    /// listener-presence and listener-absence rules cover that projection too.
    /// The Worker column includes dispatch, resources, commit, and Environment
    /// warmup. Building the split Coordinator proves the common returned Worker
    /// router is merged once: a second warmup route merge would make Axum reject
    /// the duplicate route during construction rather than reach these assertions.
    #[tokio::test]
    async fn service_roles_expose_only_their_owned_api() {
        let control_routers = prepare_control_routers(
            in_memory_control_stores().await,
            None,
            None,
            None,
            PublicationModelSupply::PublishedProviders,
            ProcessStartup {
                role: config::Role::Control,
                executable_environment_wiring: Some(
                    executable_environment_registration::local_test_wiring(),
                ),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local(),
                ),
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(
                        awaken_coordinator::test_worker_directory(),
                    ),
                ),
                control_service_authenticator: Some(
                    awaken_service_auth_contract::static_token_authenticator("control-token")
                        .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("Control MCP exports have matching descriptors and executors");
        let app = control_routers.public_router;

        let control = app
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control.status(), StatusCode::OK);

        let registration = app
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::NOT_FOUND);

        for path in [
            awaken_executable_environment_catalog::EXECUTABLE_ENVIRONMENT_REGISTER_PATH,
            awaken_coordinator::worker_observation_boundary::WORKER_OBSERVATIONS_PATH,
            awaken_coordinator::data_subject_boundary::ERASE_COORDINATOR_CONTENT_PATH,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer registration-token")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let internal_control = app
            .clone()
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer control-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(internal_control.status(), StatusCode::NOT_FOUND);

        let deployment = app
            .clone()
            .oneshot(Request::get("/v1/deployments").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(deployment.status(), StatusCode::NOT_FOUND);

        let session = app
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::NOT_FOUND);

        let private_control = control_routers.private_router;
        let public_on_private = private_control
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public_on_private.status(), StatusCode::NOT_FOUND);
        let unauthorized = private_control
            .clone()
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let authenticated = private_control
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer control-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(authenticated.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(authenticated.status(), StatusCode::NOT_FOUND);

        let (coordinator_stores, control_service) = in_memory_split_coordinator().await;
        let coordinator_data = tempfile::tempdir().expect("coordinator test data");
        let mut coordinator_deployment =
            config::local_test_deployment(coordinator_data.path().to_owned());
        // Session execution ownership decision table:
        // C1=split Coordinator, C2=local pool disabled, C3=the namespace Hand
        // is explicitly absent. R1(C1+C2+C3)->startup serves Coordinator APIs
        // without probing/constructing local execution. Registered Workers keep
        // the Session physical-effect roles. Container source observation has
        // separate protocol-backed coverage in local_execution::tests.
        coordinator_deployment.runtime.disable_local_pool = true;
        coordinator_deployment.runtime.sandbox_tier =
            awaken_deployment_config::SandboxTier::Namespace;
        let absent_hand = coordinator_data.path().join("uninstalled-namespace-hand");
        assert!(
            !absent_hand.exists(),
            "role fixture must have no installed Hand"
        );
        let executable_environment_wiring =
            executable_environment_registration::ExecutableEnvironmentWiring::local(
                coordinator_stores
                    .coordinator
                    .as_ref()
                    .expect("Coordinator test owns Environment work")
                    .environment_work
                    .clone(),
            )
            .expect("configure Coordinator test Environment wiring");
        let domain_work = coordinator_stores
            .coordinator
            .as_ref()
            .expect("Coordinator test owns WorkQueue")
            .environment_work
            .clone();
        let domain_sessions = coordinator_stores
            .coordinator
            .as_ref()
            .expect("Coordinator test owns Session roots")
            .sessions
            .clone();
        let worker_directory = awaken_coordinator::test_worker_directory();
        // Domain-observation decision table: C1 private service token valid,
        // C2 exact incarnation exists, C3 Dispatch, WorkQueue and Session
        // realization-owner reads are empty.
        // R1 no token -> 401 before query parsing; R2 valid token + exact
        // record + empty owners -> one read-only 200 projection; R3 missing
        // record -> 409, never a false empty-domain proof. R4 claimed Work,
        // even past its lease deadline, -> occupied; R5 ordinary owner release
        // -> empty. R6 exact expired Session realization with unfinished
        // cleanup -> occupied. Public listener remains absent in the role matrix.
        let mut domain_manifest = awaken_worker_contract::WorkerManifest::default();
        domain_manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        domain_manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        let domain_identity = worker_directory
            .register(
                awaken_worker_contract::WorkerRegistration {
                    worker_id: "domain-test".into(),
                    incarnation_id: "domain-boot".into(),
                    manifest: domain_manifest,
                    create_ledger: None,
                },
                100,
                10_000,
            )
            .await
            .expect("register exact domain test Worker")
            .snapshot
            .identity;
        let coordinator_routers = prepare_runtime_routers(
            coordinator_stores,
            None,
            None,
            None,
            PublicationModelSupply::PublishedProviders,
            ProcessStartup {
                role: config::Role::Coordinator,
                namespace_hand_path: Some(absent_hand),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local_server_with_observer(
                        "registration-token",
                    ),
                ),
                executable_environment_wiring: Some(executable_environment_wiring),
                control_service: Some(control_service),
                deployment: Some(coordinator_deployment.runtime),
                worker_directory: Some(worker_directory.clone()),
                runtime_authority: None,
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(worker_directory),
                ),
                worker_transport: Some(worker_transport_security::WorkerTransport::unbound(
                    Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator),
                )),
                relay_destination_authenticator: Some(
                    awaken_service_auth_contract::static_token_authenticator("relay-token")
                        .unwrap(),
                ),
                ..Default::default()
            })
        .await
        .expect("prepare split Coordinator test process");
        let app = coordinator_routers.public_router;
        let control = app
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control.status(), StatusCode::NOT_FOUND);

        let session = app
            .clone()
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::OK);
        let registration = app
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::NOT_FOUND);

        for path in [
            awaken_executable_environment_catalog::EXECUTABLE_ENVIRONMENT_REGISTER_PATH,
            awaken_coordinator::worker_observation_boundary::WORKER_OBSERVATIONS_PATH,
            awaken_coordinator::data_subject_boundary::ERASE_COORDINATOR_CONTENT_PATH,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer registration-token")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let deployments = app
            .clone()
            .oneshot(Request::get("/v1/deployments").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(deployments.status(), StatusCode::OK);

        let worker_paths = [
            "/v1/worker/dispatch/claim",
            "/v1/worker/environment/warmups",
            "/v1/worker/resources/files/content",
            "/v1/worker/commit-claimed",
        ];
        for path in worker_paths {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header(
                            awaken_worker_transport_security::WORKER_ID_HEADER,
                            "worker-test",
                        )
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "public {path}");
        }

        let retired_private_launch = app
            .clone()
            .oneshot(
                Request::post("/internal/v1/deployment-sessions/launch")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retired_private_launch.status(), StatusCode::NOT_FOUND);

        let private_coordinator = coordinator_routers.private_router;
        // Relay destination causes: C1 public/private listener; C2 observer
        // dedicated relay authentication; C3 exact query shape; C4 Pod target
        // observer wired; C5 broad Coordinator token used instead of relay token.
        // Effects: E1 public route absent; E2 unauthenticated request refused
        // before query parsing; E3 invalid query refused; E4 no target evidence
        // fails closed; E5 broad token cannot read relay destinations.
        // Decision rules: R1 public -> E1; R2 private + !C2 -> E2;
        // R3 private + C2 + !C3 -> E3; R4 private + C2 + C3 + !C4 -> E4;
        // R5 C5 -> E5 before query parsing.
        let relay_path = format!(
            "{}?session_id=domain-session&attachment_id=mcp&version=1",
            awaken_coordinator::worker_observation_boundary::WORKER_RELAY_DESTINATION_PATH
        );
        let public_relay = app
            .clone()
            .oneshot(Request::get(&relay_path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(public_relay.status(), StatusCode::NOT_FOUND, "R1/E1");
        let unauthenticated_relay = private_coordinator
            .clone()
            .oneshot(
                Request::get("/internal/v1/workers/mcp-relay-destination?version=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unauthenticated_relay.status(),
            StatusCode::UNAUTHORIZED,
            "R2/E2"
        );
        let malformed_relay = private_coordinator
            .clone()
            .oneshot(
                Request::get("/internal/v1/workers/mcp-relay-destination?version=0")
                    .header("authorization", "Bearer relay-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed_relay.status(), StatusCode::BAD_REQUEST, "R3/E3");
        let broad_token_relay = private_coordinator
            .clone()
            .oneshot(
                Request::get(&relay_path)
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            broad_token_relay.status(),
            StatusCode::UNAUTHORIZED,
            "R5/E5"
        );
        let unbound_relay = private_coordinator
            .clone()
            .oneshot(
                Request::get(&relay_path)
                    .header("authorization", "Bearer relay-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unbound_relay.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "R4/E4"
        );
        let domain_path = format!(
            "/internal/v1/workers/domain?worker_id={}&incarnation_id={}&generation={}",
            domain_identity.worker_id, domain_identity.incarnation_id, domain_identity.generation
        );
        let no_token = private_coordinator
            .clone()
            .oneshot(Request::get(&domain_path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED, "R1");
        let relay_token_domain = private_coordinator
            .clone()
            .oneshot(
                Request::get(&domain_path)
                    .header("authorization", "Bearer relay-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            relay_token_domain.status(),
            StatusCode::UNAUTHORIZED,
            "R5/E5"
        );
        let observed = private_coordinator
            .clone()
            .oneshot(
                Request::get(&domain_path)
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(observed.status(), StatusCode::OK, "R2");
        let observed: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(observed.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            observed["registered"]["snapshot"]["identity"]["worker_id"],
            "domain-test"
        );
        assert_eq!(observed["physical_attempt_active"], false);
        assert_eq!(observed["work_claims_active"], false);
        assert_eq!(observed["session_realizations_active"], false);
        // R7 the actual composed private Router consumes the qualification
        // selector and emits its optional result. A Ready/unanchored Worker
        // cannot retire even with empty owners. Merely declaring the DTO or
        // parsing the selector in isolation cannot satisfy this wire check.
        for (selector, expected) in [("false", None), ("true", Some(false))] {
            let response = private_coordinator
                .clone()
                .oneshot(
                    Request::get(format!("{domain_path}&qualify_retirement={selector}"))
                        .header("authorization", "Bearer registration-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "R7 {selector}");
            let response: awaken_coordinator::worker_observation_boundary::WorkerDomainObservation =
                serde_json::from_slice(
                    &axum::body::to_bytes(response.into_body(), 1_000_000)
                        .await
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(response.retirement_permitted, expected, "R7 {selector}");
        }
        domain_work
            .assignment()
            .enqueue_session(
                "domain-env",
                &awaken_session_types::SessionId::from("domain-session"),
            )
            .await
            .unwrap();
        domain_work
            .consumer()
            .claim("domain-env", &domain_identity.lease_owner())
            .await
            .unwrap()
            .expect("R4 Work claim");
        let occupied = private_coordinator
            .clone()
            .oneshot(
                Request::get(&domain_path)
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(occupied.status(), StatusCode::OK, "R4");
        let occupied: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(occupied.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(occupied["work_claims_active"], true, "R4");
        assert_eq!(
            domain_work
                .worker_leases()
                .release_owner(&domain_identity.lease_owner())
                .await
                .unwrap(),
            1
        );
        let released = private_coordinator
            .clone()
            .oneshot(
                Request::get(&domain_path)
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK, "R5");
        let released: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(released.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(released["work_claims_active"], false, "R5");
        let missing = private_coordinator
            .clone()
            .oneshot(
                Request::get("/internal/v1/workers/domain?worker_id=missing&incarnation_id=boot&generation=1")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::CONFLICT, "R3");
        let mut realized = super::runtime_session_store_tests::session(
            &awaken_session_types::SessionId::from("domain-affinity"),
        );
        realized.realization = Some(
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: domain_identity.worker_id.clone(),
                runtime_incarnation: "domain-test:old-pod".into(),
                epoch: 1,
                expires_at_unix_ms: 1,
            },
        );
        let payload_hash = stable_fingerprint(&realized);
        domain_sessions
            .create(
                "workspace",
                realized,
                IdempotencyRecord {
                    key: "test:worker-domain-affinity".into(),
                    payload_hash,
                },
                Vec::new(),
            )
            .await
            .expect("R6 durable Session realization owner");
        let session_occupied = private_coordinator
            .clone()
            .oneshot(
                Request::get(&domain_path)
                    .header("authorization", "Bearer registration-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session_occupied.status(), StatusCode::OK, "R6");
        let session_occupied: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(session_occupied.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(session_occupied["session_realizations_active"], true, "R6");
        let session_on_private = private_coordinator
            .clone()
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session_on_private.status(), StatusCode::NOT_FOUND);
        for path in worker_paths {
            let unauthorized = private_coordinator
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                unauthorized.status(),
                StatusCode::UNAUTHORIZED,
                "private unauthenticated {path}"
            );
            let authenticated = private_coordinator
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header(
                            awaken_worker_transport_security::WORKER_ID_HEADER,
                            "worker-test",
                        )
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                authenticated.status(),
                StatusCode::UNAUTHORIZED,
                "private authenticated {path}"
            );
            assert_ne!(
                authenticated.status(),
                StatusCode::NOT_FOUND,
                "private mounted {path}"
            );
        }
        let unauthorized = private_coordinator
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(executable_agent_registration_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let authenticated = private_coordinator
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from(executable_agent_registration_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);
    }

    #[derive(Debug)]
    struct RecordingHostedResolver {
        called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl awaken_config_service::ModelPublicationResolver for RecordingHostedResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_agent_contract::ScopeId,
            _selection: &awaken_agent_config::ModelSelection,
            _candidates: &[awaken_runtime_contract::resolved::ModelBinding],
        ) -> Result<
            awaken_config_service::ResolvedPublicationModels,
            awaken_config_service::PublicationResolutionError,
        > {
            self.called.store(true, Ordering::SeqCst);
            Err(awaken_config_service::PublicationResolutionError::MissingPrimary)
        }
    }

    #[test]
    fn hosted_publication_does_not_require_an_interactive_broker_client() {
        // Cause/effect decision table: C1=published-provider composition,
        // C2=hosted resolver composition. R1(C1)->interactive broker fallback;
        // R2(C2)->no fallback because the host-supplied resolver/catalog are
        // authoritative and a projected workload token is not a user token.
        assert!(
            PublicationModelSupply::PublishedProviders.needs_interactive_brokered_client(),
            "R1"
        );
        assert!(
            !PublicationModelSupply::HostedPublication {
                resolver: Arc::new(RecordingHostedResolver {
                    called: Arc::new(AtomicBool::new(false)),
                }),
                direct_credential_execution: None,
            }
            .needs_interactive_brokered_client(),
            "R2"
        );
    }

    /// Causal graph:
    /// hosted resolver injection -> canonical ConfigService publication
    /// -> the injected resolver is the only model-selection authority
    /// -> its fail-closed result is returned without consulting the local catalog.
    ///
    /// Decision table:
    /// | control startup | resolver result | local catalog | outcome |
    /// | --- | --- | --- | --- |
    /// | default open | any | authoritative | catalog resolver decides |
    /// | hosted injection | success | irrelevant | injected candidate publishes |
    /// | hosted injection | failure | populated/empty | publication fails closed |
    #[tokio::test]
    async fn hosted_control_uses_the_injected_publication_resolver() {
        let called = Arc::new(AtomicBool::new(false));
        let app = prepare_control_routers(
            in_memory_control_stores().await,
            None,
            None,
            None,
            PublicationModelSupply::HostedPublication {
                resolver: Arc::new(RecordingHostedResolver {
                    called: called.clone(),
                }),
                direct_credential_execution: None,
            },
            ProcessStartup {
                role: config::Role::Control,
                executable_environment_wiring: Some(
                    executable_environment_registration::local_test_wiring(),
                ),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local(),
                ),
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(
                        awaken_coordinator::test_worker_directory(),
                    ),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("Control MCP exports have matching descriptors and executors")
        .public_router;

        let authored = app
            .clone()
            .oneshot(
                Request::put("/v1/config/agents/hosted-agent")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Hosted",
                            "model": {
                                "provider_identity_ref": "provider-account-a",
                                "model_ref": "model-a",
                                "backend_ref": "hosted"
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authored.status(), StatusCode::OK);

        let published = app
            .oneshot(
                Request::post("/v1/config/agents/hosted-agent/publish")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::CONFLICT);
        assert!(called.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod management_surface_tests;
