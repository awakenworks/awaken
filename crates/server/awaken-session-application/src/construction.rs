// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Consumes validated Session configuration and complete role values before sharing.
// @formal reason: Configuration validation remains in the application contract; this module selects
// @formal reason: the canonical Store/Clock and initializes application state once, without issuing
// @formal reason: credentials, changing durable authority, or starting effects. Constructor matrices
// @formal reason: in tests/construction.rs check refusal ordering and preserved capability identity.
//! Consume process configuration and complete feature capabilities before sharing.

use awaken_credential_contract::{CredentialMaterialIngress, SessionCredentialSource};
use awaken_executable_agent_contract::ExecutableAgentProfileSource;
use awaken_resource_contract::{
    FileCatalog, ResourcePurgeScheduler, ResourceReferenceIndex, ResourceRegistry,
};
use awaken_session_application_contract::ExecutableProjectionRefresh;
use awaken_session_budget_contract::ManagedListPriceProvider;
use awaken_session_configuration_contract::SessionModelPublicationResolver;
use awaken_tool_contract::ToolConfigurationCompiler;
use std::sync::Arc;

use crate::{
    SessionApplication, SessionApplicationBuildError, SessionApplicationConfiguration,
    SessionEnvironments, SessionExecutionAdmission, SessionExecutor, SessionPersistence,
};

/// Material ingress always includes the matching credential read capability.
///
/// O2/O11: the existing construction choice supplies one original authority
/// allocation. The command service owns its name and exposes no late capability
/// constructor. These examples check source capability, not secret access.
///
/// ```
/// use std::sync::Arc;
/// use awaken_credential_contract::{CredentialMaterialIngress, SessionCredentialSource};
/// use awaken_session_application::{SessionCredentialAuthorities};
/// fn read_only(source: Arc<dyn SessionCredentialSource>) -> SessionCredentialAuthorities {
///     SessionCredentialAuthorities::read_only(source)
/// }
/// fn read_write<T>(authority: Arc<T>) -> SessionCredentialAuthorities
/// where T: SessionCredentialSource + CredentialMaterialIngress + 'static {
///     SessionCredentialAuthorities::read_write(authority)
/// }
/// ```
///
/// ```compile_fail,E0599
/// use std::sync::Arc;
/// use awaken_credential_contract::{SessionCredentialSource};
/// use awaken_session_application::{SessionCredentials};
/// fn old_input(source: Arc<dyn SessionCredentialSource>) {
///     let _ = SessionCredentials::read_only(source);
/// }
/// ```
pub struct SessionCredentialAuthorities {
    source: Arc<dyn SessionCredentialSource>,
    ingress: Option<Arc<dyn CredentialMaterialIngress>>,
}

impl SessionCredentialAuthorities {
    #[must_use]
    pub fn read_only(source: Arc<dyn SessionCredentialSource>) -> Self {
        Self {
            source,
            ingress: None,
        }
    }

    #[must_use]
    pub fn read_write<T>(authority: Arc<T>) -> Self
    where
        T: SessionCredentialSource + CredentialMaterialIngress + 'static,
    {
        Self {
            source: authority.clone(),
            ingress: Some(authority),
        }
    }
}

/// The existing Resource catalog, optionally with its complete retention ports.
pub struct SessionResources {
    catalog: Arc<dyn ResourceRegistry>,
    retention: Option<ResourceRetention>,
}

struct ResourceRetention {
    references: Arc<dyn ResourceReferenceIndex>,
    files: Arc<dyn FileCatalog>,
    purge: Arc<dyn ResourcePurgeScheduler>,
}

impl SessionResources {
    #[must_use]
    pub fn catalog(catalog: Arc<dyn ResourceRegistry>) -> Self {
        Self {
            catalog,
            retention: None,
        }
    }

    /// Enable retention and reclamation together, preserving the catalog owner.
    #[must_use]
    pub fn with_retention(
        mut self,
        references: Arc<dyn ResourceReferenceIndex>,
        files: Arc<dyn FileCatalog>,
        purge: Arc<dyn ResourcePurgeScheduler>,
    ) -> Self {
        self.retention = Some(ResourceRetention {
            references,
            files,
            purge,
        });
        self
    }
}

/// A process-local assembly value. Each enabled feature carries its required
/// ports; consuming `build` removes the configuration mutation surface.
pub struct SessionApplicationBuilder {
    persistence: SessionPersistence,
    execution: SessionExecutionAdmission,
    assembly: SessionAssembly,
}

impl SessionApplicationBuilder {
    /// Explicit partial assembly for fixtures only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture(
        runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
        mcp: Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>,
        persistence: SessionPersistence,
        environments: impl Into<SessionEnvironments>,
        tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    ) -> Self {
        Self::new(
            SessionExecutor::colocated(runtime, mcp),
            persistence,
            environments,
            SessionExecutionAdmission::fixture(),
            tool_configuration_compiler,
        )
    }

    #[must_use]
    pub fn new(
        runtime: impl Into<SessionExecutor>,
        persistence: SessionPersistence,
        environments: impl Into<SessionEnvironments>,
        execution: SessionExecutionAdmission,
        tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    ) -> Self {
        Self {
            persistence,
            execution,
            assembly: SessionAssembly {
                runtime: runtime.into(),
                tool_configuration_compiler,
                environments: environments.into(),
                configuration: SessionApplicationConfiguration::default(),
                credential_source: None,
                credential_material_ingress: None,
                resource_registry: None,
                resource_purge_scheduler: None,
                resource_references: None,
                resource_files: None,
                profiles: None,
                models: None,
                prices: None,
                refresh: None,
            },
        }
    }
    #[must_use]
    pub fn configuration(mut self, configuration: SessionApplicationConfiguration) -> Self {
        self.assembly.configuration = configuration;
        self
    }

    #[must_use]
    pub fn credentials(mut self, credentials: SessionCredentialAuthorities) -> Self {
        self.assembly.credential_source = Some(credentials.source);
        self.assembly.credential_material_ingress = credentials.ingress;
        self
    }

    #[must_use]
    pub fn resources(mut self, resources: SessionResources) -> Self {
        self.assembly.resource_registry = Some(resources.catalog);
        if let Some(retention) = resources.retention {
            self.assembly.resource_references = Some(retention.references);
            self.assembly.resource_files = Some(retention.files);
            self.assembly.resource_purge_scheduler = Some(retention.purge);
        }
        self
    }

    #[must_use]
    pub fn agent_profiles(mut self, profiles: Arc<dyn ExecutableAgentProfileSource>) -> Self {
        self.assembly.profiles = Some(profiles);
        self
    }

    #[must_use]
    pub fn model_overrides(mut self, models: Arc<dyn SessionModelPublicationResolver>) -> Self {
        self.assembly.models = Some(models);
        self
    }

    #[must_use]
    pub fn managed_prices(mut self, prices: Arc<dyn ManagedListPriceProvider>) -> Self {
        self.assembly.prices = Some(prices);
        self
    }

    #[must_use]
    pub fn executable_projection_refresh(
        mut self,
        refresh: Arc<ExecutableProjectionRefresh>,
    ) -> Self {
        self.assembly.refresh = Some(refresh);
        self
    }

    pub fn build(mut self) -> Result<SessionApplication, SessionApplicationBuildError> {
        self.assembly.configuration = self.assembly.configuration.validate()?;
        let crate::SessionPersistence {
            store,
            clock,
            environment_binding_sink,
        } = self.persistence;
        let started_at = clock.now().as_unix_nanos();
        let mutations = Arc::new(crate::SessionMutations::new(store.clone(), clock.clone()));
        let lifecycle = Arc::new(crate::SessionLifecycle::new(
            mutations.clone(),
            clock.clone(),
            store.clone(),
        ));
        Ok(self.assembly.assemble(SessionKernel {
            store,
            clock,
            mutations,
            lifecycle,
            environment_binding_sink,
            execution_admission: Arc::new(self.execution),
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                crate::APPLICATION_INCARNATION_SEQ
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            event_batch_cutover_validation: Arc::default(),
        }))
    }
}

/// What every component shares and a re-assembly never replaces: the selected
/// Store and Clock, the one CAS kernel and lifecycle signal, the process
/// incarnation and the Event-batch cutover validation it publishes.
pub(crate) struct SessionKernel {
    pub(crate) store: Arc<dyn awaken_session_repository_contract::SessionStore>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) environment_binding_sink:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    pub(crate) execution_admission: Arc<SessionExecutionAdmission>,
    pub(crate) runtime_incarnation: String,
    pub(crate) event_batch_cutover_validation: Arc<crate::SessionEventBatchCutoverValidationSource>,
}

/// The validated composition inputs every use-case component is built from.
/// `build` consumes them once. A test-support application retains a copy so
/// a fixture can change one input before the application is shared; the
/// change re-assembles every component from the same kernel, so no component
/// keeps a port another one no longer sees.
#[derive(Clone)]
pub(crate) struct SessionAssembly {
    pub(crate) runtime: SessionExecutor,
    pub(crate) tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    pub(crate) environments: SessionEnvironments,
    pub(crate) configuration: SessionApplicationConfiguration,
    pub(crate) credential_source: Option<Arc<dyn SessionCredentialSource>>,
    pub(crate) credential_material_ingress: Option<Arc<dyn CredentialMaterialIngress>>,
    pub(crate) resource_registry: Option<Arc<dyn ResourceRegistry>>,
    pub(crate) resource_purge_scheduler: Option<Arc<dyn ResourcePurgeScheduler>>,
    pub(crate) resource_references: Option<Arc<dyn ResourceReferenceIndex>>,
    pub(crate) resource_files: Option<Arc<dyn FileCatalog>>,
    pub(crate) profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
    pub(crate) models: Option<Arc<dyn SessionModelPublicationResolver>>,
    pub(crate) prices: Option<Arc<dyn ManagedListPriceProvider>>,
    pub(crate) refresh: Option<Arc<ExecutableProjectionRefresh>>,
}

impl SessionAssembly {
    pub(crate) fn assemble(self, kernel: SessionKernel) -> SessionApplication {
        let foundation = self.foundation(&kernel);
        let lease = self.lease_protocol(&kernel, &foundation);
        let UseCases {
            event_batches,
            outcome_reconciliation,
            update_commands,
            termination_commands,
            resource_reconciliation,
            resource_manifests,
            creation_commands,
            run_admissions,
            coordination,
            recovery,
        } = self.use_cases(&kernel, &foundation, &lease);
        let SessionKernel {
            #[cfg(any(test, feature = "test-support"))]
            store,
            #[cfg(not(any(test, feature = "test-support")))]
                store: _,
            clock,
            mutations,
            lifecycle,
            environment_binding_sink,
            execution_admission,
            runtime_incarnation,
            event_batch_cutover_validation: _,
        } = kernel;
        let Foundation {
            credentials,
            observations,
            queries,
            inputs,
            resource_effect_authorization,
            resource_references: _,
            work_assignments,
            worker_occupancy,
            repository_participants,
            executable_projections: _,
        } = foundation;
        let LeaseProtocol {
            realizations,
            terminal_cleanups,
            continuations,
            activities,
        } = lease;
        SessionApplication {
            clock,
            configuration: self.configuration.clone(),
            credentials,
            work_assignments,
            worker_occupancy,
            repository_participants,
            mutations,
            lifecycle,
            queries,
            inputs,
            resource_effect_authorization,
            environment_binding_sink,
            realizations,
            terminal_cleanups,
            continuations,
            activities,
            termination_commands,
            update_commands,
            resource_reconciliation,
            resource_manifests,
            creation_commands,
            run_admissions,
            observations,
            coordination,
            event_batches,
            outcome_reconciliation,
            execution_admission,
            runtime_incarnation,
            recovery,
            #[cfg(any(test, feature = "test-support"))]
            store,
            #[cfg(any(test, feature = "test-support"))]
            fixture_assembly: self,
        }
    }

    /// The Session use cases built on the shared components and the lease
    /// protocol: update, termination, Resource commands, creation, Run
    /// admission and Agent coordination.
    fn use_cases(
        &self,
        kernel: &SessionKernel,
        foundation: &Foundation,
        lease: &LeaseProtocol,
    ) -> UseCases {
        let update_commands = Arc::new(crate::SessionUpdateCommands::new(
            crate::update::SessionUpdateCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                credentials: foundation.credentials.clone(),
                lifecycle: kernel.lifecycle.clone(),
                realizations: lease.realizations.clone(),
                activities: lease.activities.clone(),
                executable_projections: foundation.executable_projections.clone(),
                tool_configuration_compiler: self.tool_configuration_compiler.clone(),
                runtime: self.runtime.clone(),
            },
        ));
        let termination_commands = Arc::new(crate::SessionTerminationCommands::new(
            kernel.mutations.clone(),
            kernel.clock.clone(),
            kernel.lifecycle.clone(),
            lease.terminal_cleanups.clone(),
            lease.activities.clone(),
            &self.runtime,
        ));
        let resource_reconciliation = Arc::new(crate::SessionResourceReconciliation::new(
            kernel.mutations.clone(),
            kernel.clock.clone(),
            foundation.credentials.clone(),
            foundation.resource_references.clone(),
            foundation.repository_participants.clone(),
            lease.terminal_cleanups.clone(),
            &self.runtime,
        ));
        let resource_manifests = Arc::new(crate::SessionResourceManifests::new(
            crate::resource_manifests::SessionResourceManifestCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                resource_references: foundation.resource_references.clone(),
                work_assignments: foundation.work_assignments.clone(),
                repository_participants: foundation.repository_participants.clone(),
                resource_reconciliation: resource_reconciliation.clone(),
                resource_registry: self.resource_registry.clone(),
                placement: self.runtime.placement.clone(),
            },
        ));
        let creation_commands = Arc::new(crate::SessionCreationCommands::new(
            crate::creation::SessionCreationCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                lifecycle: kernel.lifecycle.clone(),
                queries: foundation.queries.clone(),
                inputs: foundation.inputs.clone(),
                credentials: foundation.credentials.clone(),
                repository_participants: foundation.repository_participants.clone(),
                work_assignments: foundation.work_assignments.clone(),
                realizations: lease.realizations.clone(),
                terminal_cleanups: lease.terminal_cleanups.clone(),
                executable_projections: foundation.executable_projections.clone(),
                tool_configuration_compiler: self.tool_configuration_compiler.clone(),
                resource_registry: self.resource_registry.clone(),
                environments: self.environments.clone(),
                model_publication_resolver: self.models.clone(),
                managed_list_prices: self.prices.clone(),
                execution_placement: self.configuration.execution_placement,
                runtime: self.runtime.clone(),
            },
        ));
        let run_admissions = Arc::new(crate::SessionRunAdmissions::new(
            crate::run_admission::SessionRunAdmissionCollaborators {
                mutations: kernel.mutations.clone(),
                queries: foundation.queries.clone(),
                activities: lease.activities.clone(),
                creation_commands: creation_commands.clone(),
                resource_reconciliation: resource_reconciliation.clone(),
                realizations: lease.realizations.clone(),
                work_assignments: foundation.work_assignments.clone(),
                executable_projections: foundation.executable_projections.clone(),
                execution_admission: kernel.execution_admission.clone(),
                runtime: self.runtime.clone(),
            },
        ));
        let coordination = Arc::new(crate::SessionCoordination::new(
            crate::coordination::SessionCoordinationCollaborators {
                mutations: kernel.mutations.clone(),
                lifecycle: kernel.lifecycle.clone(),
                queries: foundation.queries.clone(),
                activities: lease.activities.clone(),
                run_admissions: run_admissions.clone(),
                realizations: lease.realizations.clone(),
                observations: foundation.observations.clone(),
                runtime: self.runtime.clone(),
            },
        ));
        let event_batches = Arc::new(crate::SessionEventBatchCommands::new(
            crate::event_batches::SessionEventBatchCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                lifecycle: kernel.lifecycle.clone(),
                queries: foundation.queries.clone(),
                activities: lease.activities.clone(),
                run_admissions: run_admissions.clone(),
                coordination: coordination.clone(),
                runtime: self.runtime.clone(),
            },
        ));
        let outcome_reconciliation = Arc::new(crate::SessionOutcomeReconciliation::new(
            kernel.mutations.clone(),
            lease.realizations.clone(),
            &self.runtime,
        ));
        let recovery = Arc::new(crate::SessionRecovery::new(
            crate::recovery::SessionRecoveryCollaborators {
                repository: kernel.mutations.repository().clone(),
                clock: kernel.clock.clone(),
                lifecycle: kernel.lifecycle.clone(),
                work_assignments: foundation.work_assignments.clone(),
                resource_reconciliation: resource_reconciliation.clone(),
                continuations: lease.continuations.clone(),
                realizations: lease.realizations.clone(),
                event_batches: event_batches.clone(),
                outcome_reconciliation: outcome_reconciliation.clone(),
                event_batch_cutover_validation: kernel.event_batch_cutover_validation.clone(),
            },
        ));
        UseCases {
            event_batches,
            outcome_reconciliation,
            update_commands,
            termination_commands,
            resource_reconciliation,
            resource_manifests,
            creation_commands,
            run_admissions,
            coordination,
            recovery,
        }
    }

    /// Components every use case shares: the reads, credential and Resource
    /// components over the one CAS kernel, and the Session Work projection.
    fn foundation(&self, kernel: &SessionKernel) -> Foundation {
        let credentials = Arc::new(crate::SessionCredentials::new(
            kernel.mutations.clone(),
            kernel.clock.clone(),
            self.credential_source.clone(),
            self.credential_material_ingress.clone(),
        ));
        let resource_references = Arc::new(crate::SessionResourceReferences::new(
            kernel.mutations.clone(),
            self.resource_references.clone(),
            self.resource_files.clone(),
        ));
        let queries = Arc::new(crate::SessionQueries::new(
            kernel.store.clone(),
            &self.runtime,
            self.profiles.clone(),
        ));
        let worker_occupancy = Arc::new(crate::SessionWorkerOccupancy::new(
            kernel.mutations.repository().clone(),
            queries.clone(),
            &self.runtime,
        ));
        Foundation {
            worker_occupancy,
            observations: Arc::new(crate::SessionObservations::new(
                kernel.mutations.clone(),
                kernel.clock.clone(),
                queries.clone(),
                self.resource_files.clone(),
                &self.runtime,
            )),
            queries,
            inputs: Arc::new(crate::SessionInputs::new(
                kernel.store.clone(),
                self.runtime.live_inbox.clone(),
                self.resource_registry.as_ref().map(|registry| {
                    registry.clone() as Arc<dyn awaken_resource_contract::ResourceInventory>
                }),
            )),
            resource_effect_authorization: Arc::new(
                crate::SessionResourceEffectAuthorization::new(
                    kernel.mutations.clone(),
                    kernel.clock.clone(),
                ),
            ),
            work_assignments: Arc::new(crate::SessionWorkAssignments::new(
                kernel.mutations.repository().clone(),
                self.environments.assignment.clone(),
                self.environments.leases.clone(),
                kernel.lifecycle.clone(),
                kernel.clock.clone(),
            )),
            repository_participants: Arc::new(crate::SessionRepositoryParticipants::new(
                kernel.mutations.clone(),
                resource_references.clone(),
                credentials.clone(),
                self.resource_registry.clone(),
                self.resource_purge_scheduler.clone(),
                kernel.clock.clone(),
            )),
            executable_projections: crate::ExecutableProjections(self.refresh.clone()),
            credentials,
            resource_references,
        }
    }

    /// The realization lease protocol and what is built on it: terminal
    /// cleanup, the Environment continuation saga and activity fencing.
    fn lease_protocol(&self, kernel: &SessionKernel, foundation: &Foundation) -> LeaseProtocol {
        let realizations = Arc::new(crate::SessionRealizations::new(
            crate::realization::SessionRealizationCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                queries: foundation.queries.clone(),
                credentials: foundation.credentials.clone(),
                lifecycle: kernel.lifecycle.clone(),
                resource_references: foundation.resource_references.clone(),
                placement: self.runtime.placement.clone(),
                executable_projections: foundation.executable_projections.clone(),
                execution_admission: kernel.execution_admission.clone(),
                local_realization_owner: self.configuration.local_realization_owner.clone(),
                runtime_incarnation: kernel.runtime_incarnation.clone(),
                realization_retry_budget: self.configuration.realization_retry_budget,
            },
        ));
        let terminal_cleanups = Arc::new(crate::SessionTerminalCleanups::new(
            crate::terminal_cleanup::SessionTerminalCleanupCollaborators {
                mutations: kernel.mutations.clone(),
                clock: kernel.clock.clone(),
                queries: foundation.queries.clone(),
                lifecycle: kernel.lifecycle.clone(),
                resource_references: foundation.resource_references.clone(),
                repository_participants: foundation.repository_participants.clone(),
                work_assignments: foundation.work_assignments.clone(),
                realizations: realizations.clone(),
                executable_projections: foundation.executable_projections.clone(),
                execution_admission: kernel.execution_admission.clone(),
                runtime: self.runtime.clone(),
            },
        ));
        let continuations = Arc::new(crate::SessionContinuations::new(
            kernel.mutations.clone(),
            &self.runtime,
            realizations.clone(),
        ));
        let activities = Arc::new(crate::SessionActivities::new(
            kernel.mutations.clone(),
            kernel.clock.clone(),
            &self.runtime,
            continuations.clone(),
            &kernel.runtime_incarnation,
        ));
        LeaseProtocol {
            realizations,
            terminal_cleanups,
            continuations,
            activities,
        }
    }
}

/// The components every use case shares.
struct Foundation {
    credentials: Arc<crate::SessionCredentials>,
    observations: Arc<crate::SessionObservations>,
    queries: Arc<crate::SessionQueries>,
    inputs: Arc<crate::SessionInputs>,
    resource_effect_authorization: Arc<crate::SessionResourceEffectAuthorization>,
    resource_references: Arc<crate::SessionResourceReferences>,
    worker_occupancy: Arc<crate::SessionWorkerOccupancy>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    executable_projections: crate::ExecutableProjections,
}

/// The Session use-case components.
struct UseCases {
    event_batches: Arc<crate::SessionEventBatchCommands>,
    outcome_reconciliation: Arc<crate::SessionOutcomeReconciliation>,
    update_commands: Arc<crate::SessionUpdateCommands>,
    termination_commands: Arc<crate::SessionTerminationCommands>,
    resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    resource_manifests: Arc<crate::SessionResourceManifests>,
    creation_commands: Arc<crate::SessionCreationCommands>,
    run_admissions: Arc<crate::SessionRunAdmissions>,
    coordination: Arc<crate::SessionCoordination>,
    recovery: Arc<crate::SessionRecovery>,
}

/// The realization lease protocol and the components built directly on it.
struct LeaseProtocol {
    realizations: Arc<crate::SessionRealizations>,
    terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    continuations: Arc<crate::SessionContinuations>,
    activities: Arc<crate::SessionActivities>,
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reassemble(
    application: &mut SessionApplication,
    change: impl FnOnce(&mut SessionAssembly),
) {
    let kernel = || SessionKernel {
        store: application.store.clone(),
        clock: application.clock.clone(),
        mutations: application.mutations.clone(),
        lifecycle: application.lifecycle.clone(),
        environment_binding_sink: application.environment_binding_sink.clone(),
        execution_admission: application.execution_admission.clone(),
        runtime_incarnation: application.runtime_incarnation.clone(),
        event_batch_cutover_validation: application
            .recovery
            .session_event_batch_cutover_validation_source(),
    };
    let holders = |application: &SessionApplication| {
        [
            Arc::strong_count(&application.credentials),
            Arc::strong_count(&application.queries),
            Arc::strong_count(&application.inputs),
            Arc::strong_count(&application.resource_effect_authorization),
            Arc::strong_count(&application.work_assignments),
            Arc::strong_count(&application.worker_occupancy),
            Arc::strong_count(&application.repository_participants),
            Arc::strong_count(&application.realizations),
            Arc::strong_count(&application.terminal_cleanups),
            Arc::strong_count(&application.continuations),
            Arc::strong_count(&application.activities),
            Arc::strong_count(&application.termination_commands),
            Arc::strong_count(&application.update_commands),
            Arc::strong_count(&application.resource_reconciliation),
            Arc::strong_count(&application.resource_manifests),
            Arc::strong_count(&application.creation_commands),
            Arc::strong_count(&application.run_admissions),
            Arc::strong_count(&application.observations),
            Arc::strong_count(&application.coordination),
            Arc::strong_count(&application.event_batches),
            Arc::strong_count(&application.outcome_reconciliation),
            Arc::strong_count(&application.recovery),
        ]
    };
    let unshared = application.fixture_assembly.clone().assemble(kernel());
    assert_eq!(
        holders(application),
        holders(&unshared),
        "fixture ports must be configured before the application is shared"
    );
    drop(unshared);
    let mut assembly = application.fixture_assembly.clone();
    change(&mut assembly);
    let kernel = kernel();
    *application = assembly.assemble(kernel);
}
