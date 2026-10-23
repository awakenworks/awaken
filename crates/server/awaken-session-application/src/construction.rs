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
    McpAttachmentRealizer, SessionApplication, SessionApplicationBuildError,
    SessionApplicationConfiguration, SessionEnvironments, SessionExecutionAdmission,
    SessionExecutor, SessionPersistence,
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
        runtime: impl Into<SessionExecutor>,
        mcp: Arc<dyn McpAttachmentRealizer>,
        persistence: SessionPersistence,
        environments: impl Into<SessionEnvironments>,
        tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    ) -> Self {
        Self::new(
            runtime,
            mcp,
            persistence,
            environments,
            SessionExecutionAdmission::fixture(),
            tool_configuration_compiler,
        )
    }

    #[must_use]
    pub fn new(
        runtime: impl Into<SessionExecutor>,
        mcp: Arc<dyn McpAttachmentRealizer>,
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
                mcp,
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
    pub(crate) mcp: Arc<dyn McpAttachmentRealizer>,
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
        let SessionKernel {
            store,
            clock,
            mutations,
            lifecycle,
            environment_binding_sink,
            execution_admission,
            runtime_incarnation,
            event_batch_cutover_validation,
        } = kernel;
        let credentials = Arc::new(crate::SessionCredentials::new(
            mutations.clone(),
            clock.clone(),
            self.credential_source.clone(),
            self.credential_material_ingress.clone(),
        ));
        let queries = Arc::new(crate::SessionQueries::new(
            store.clone(),
            &self.runtime,
            self.profiles.clone(),
        ));
        let inputs = Arc::new(crate::SessionInputs::new(
            store.clone(),
            self.runtime.live_inbox.clone(),
            self.resource_registry.as_ref().map(|registry| {
                registry.clone() as Arc<dyn awaken_resource_contract::ResourceInventory>
            }),
        ));
        let resource_effect_authorization = Arc::new(
            crate::SessionResourceEffectAuthorization::new(mutations.clone(), clock.clone()),
        );
        let resource_references = Arc::new(crate::SessionResourceReferences::new(
            mutations.clone(),
            self.resource_references.clone(),
            self.resource_files.clone(),
        ));
        let work_assignments = Arc::new(crate::SessionWorkAssignments::new(
            mutations.repository().clone(),
            self.environments.assignment.clone(),
            self.environments.leases.clone(),
            lifecycle.clone(),
            clock.clone(),
        ));
        let repository_participants = Arc::new(crate::SessionRepositoryParticipants::new(
            mutations.clone(),
            resource_references.clone(),
            credentials.clone(),
            self.resource_registry.clone(),
            self.resource_purge_scheduler.clone(),
            clock.clone(),
        ));
        let executable_projections = crate::ExecutableProjections(self.refresh.clone());
        let realizations = Arc::new(crate::SessionRealizations::new(
            crate::realization::SessionRealizationCollaborators {
                mutations: mutations.clone(),
                clock: clock.clone(),
                queries: queries.clone(),
                credentials: credentials.clone(),
                lifecycle: lifecycle.clone(),
                resource_references: resource_references.clone(),
                realization: self.runtime.realization.clone(),
                mcp: self.mcp.clone(),
                executable_projections: executable_projections.clone(),
                execution_admission: execution_admission.clone(),
                local_realization_owner: self.configuration.local_realization_owner.clone(),
                runtime_incarnation: runtime_incarnation.clone(),
                realization_retry_budget: self.configuration.realization_retry_budget,
            },
        ));
        SessionApplication {
            clock,
            configuration: self.configuration.clone(),
            runtime: self.runtime.clone(),
            tool_configuration_compiler: self.tool_configuration_compiler.clone(),
            credentials,
            environments: self.environments.clone(),
            model_publication_resolver: self.models.clone(),
            resource_registry: self.resource_registry.clone(),
            resource_references,
            work_assignments,
            repository_participants,
            resource_files: self.resource_files.clone(),
            managed_list_prices: self.prices.clone(),
            mutations,
            lifecycle,
            queries,
            inputs,
            resource_effect_authorization,
            environment_binding_sink,
            executable_projections,
            realizations,
            execution_admission,
            runtime_incarnation,
            event_batch_cutover_validation,
            #[cfg(any(test, feature = "test-support"))]
            store,
            #[cfg(any(test, feature = "test-support"))]
            fixture_assembly: self,
        }
    }
}

/// Change one composition input of a fixture that has not been shared and
/// rebuild every component from it over the same kernel. A component already
/// handed out would keep the ports it was built with while the application
/// moved on, so a shared fixture refuses before any change: each component
/// must have exactly the holders a fresh assembly of the same inputs gives it.
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
        event_batch_cutover_validation: application.event_batch_cutover_validation.clone(),
    };
    let holders = |application: &SessionApplication| {
        [
            Arc::strong_count(&application.credentials),
            Arc::strong_count(&application.queries),
            Arc::strong_count(&application.inputs),
            Arc::strong_count(&application.resource_effect_authorization),
            Arc::strong_count(&application.resource_references),
            Arc::strong_count(&application.work_assignments),
            Arc::strong_count(&application.repository_participants),
            Arc::strong_count(&application.realizations),
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
