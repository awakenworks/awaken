// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Consumes validated Session configuration and complete role values before sharing.
// @formal reason: Configuration validation remains in the application contract; this module selects
// @formal reason: the canonical Store/Clock and initializes application state once, without issuing
// @formal reason: credentials, changing durable authority, or starting effects. Constructor matrices
// @formal reason: in tests/construction.rs check refusal ordering and preserved capability identity.
//! Consume process configuration and complete feature capabilities before sharing.

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
    CredentialMaterialIngress, McpAttachmentRealizer, SessionApplication,
    SessionApplicationBuildError, SessionApplicationConfiguration, SessionCredentialSource,
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
/// use awaken_session_application::{
///     CredentialMaterialIngress, SessionCredentialAuthorities, SessionCredentialSource,
/// };
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
/// use awaken_session_application::{SessionCredentials, SessionCredentialSource};
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
    runtime: SessionExecutor,
    tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    mcp: Arc<dyn McpAttachmentRealizer>,
    persistence: SessionPersistence,
    environments: SessionEnvironments,
    execution: SessionExecutionAdmission,
    configuration: SessionApplicationConfiguration,
    credentials: Option<SessionCredentialAuthorities>,
    resources: Option<SessionResources>,
    profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
    models: Option<Arc<dyn SessionModelPublicationResolver>>,
    prices: Option<Arc<dyn ManagedListPriceProvider>>,
    refresh: Option<Arc<ExecutableProjectionRefresh>>,
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
            runtime: runtime.into(),
            tool_configuration_compiler,
            mcp,
            persistence,
            environments: environments.into(),
            execution,
            configuration: SessionApplicationConfiguration::default(),
            credentials: None,
            resources: None,
            profiles: None,
            models: None,
            prices: None,
            refresh: None,
        }
    }
    #[must_use]
    pub fn configuration(mut self, configuration: SessionApplicationConfiguration) -> Self {
        self.configuration = configuration;
        self
    }

    #[must_use]
    pub fn credentials(mut self, credentials: SessionCredentialAuthorities) -> Self {
        self.credentials = Some(credentials);
        self
    }

    #[must_use]
    pub fn resources(mut self, resources: SessionResources) -> Self {
        self.resources = Some(resources);
        self
    }

    #[must_use]
    pub fn agent_profiles(mut self, profiles: Arc<dyn ExecutableAgentProfileSource>) -> Self {
        self.profiles = Some(profiles);
        self
    }

    #[must_use]
    pub fn model_overrides(mut self, models: Arc<dyn SessionModelPublicationResolver>) -> Self {
        self.models = Some(models);
        self
    }

    #[must_use]
    pub fn managed_prices(mut self, prices: Arc<dyn ManagedListPriceProvider>) -> Self {
        self.prices = Some(prices);
        self
    }

    #[must_use]
    pub fn executable_projection_refresh(
        mut self,
        refresh: Arc<ExecutableProjectionRefresh>,
    ) -> Self {
        self.refresh = Some(refresh);
        self
    }

    pub fn build(self) -> Result<SessionApplication, SessionApplicationBuildError> {
        let configuration = self.configuration.validate()?;
        let crate::SessionPersistence {
            store,
            clock,
            environment_binding_sink,
        } = self.persistence;
        let (credential_source, credential_material_ingress) = self
            .credentials
            .map_or((None, None), |value| (Some(value.source), value.ingress));
        let (resource_registry, retention) = self
            .resources
            .map_or((None, None), |value| (Some(value.catalog), value.retention));
        let (resource_references, resource_files, resource_purge_scheduler) = retention
            .map_or((None, None, None), |value| {
                (Some(value.references), Some(value.files), Some(value.purge))
            });
        let started_at = clock.now().as_unix_nanos();
        let mutations = Arc::new(crate::SessionMutations::new(store.clone(), clock.clone()));
        let credentials = Arc::new(crate::SessionCredentials::new(
            mutations.clone(),
            clock.clone(),
            credential_source,
            credential_material_ingress,
        ));
        let lifecycle = Arc::new(crate::SessionLifecycle::new(
            mutations.clone(),
            clock.clone(),
            store.clone(),
        ));
        let queries = Arc::new(crate::SessionQueries::new(
            store.clone(),
            self.runtime.projection_handle(),
            self.runtime.capability.clone(),
            self.profiles.clone(),
        ));
        let inputs = Arc::new(crate::SessionInputs::new(
            store.clone(),
            self.runtime.live_inbox.clone(),
        ));
        Ok(SessionApplication {
            clock,
            configuration,
            runtime: self.runtime,
            tool_configuration_compiler: self.tool_configuration_compiler,
            mcp_realizer: self.mcp,
            credentials,
            environments: self.environments,
            config_source: self.profiles,
            model_publication_resolver: self.models,
            resource_registry,
            resource_purge_scheduler,
            resource_references,
            resource_files,
            managed_list_prices: self.prices,
            mutations,
            lifecycle,
            queries,
            inputs,
            environment_binding_sink,
            executable_projection_refresh: self.refresh,
            execution_admission: self.execution,
            session_preparation_cursors: tokio::sync::Mutex::new(std::collections::BTreeMap::new()),
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                crate::APPLICATION_INCARNATION_SEQ
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            local_realization_locks: Arc::default(),
            event_batch_cutover_validation: Arc::default(),
        })
    }
}
