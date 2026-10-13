// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Application construction validates process configuration and freezes complete Runtime,
// @formal reason: credential and Resource capabilities before sharing the Session authority. This product
// @formal reason: link does not claim a proof of provider I/O, plaintext custody or runtime execution.
//! Consume process configuration and complete feature capabilities before sharing.

use awaken_executable_agent_contract::ExecutableAgentProfileSource;
use awaken_resource_contract::{
    FileCatalog, ResourcePurgeScheduler, ResourceReferenceIndex, ResourceRegistry,
};
use awaken_session_application_contract::ExecutableProjectionRefresh;
use awaken_session_budget_contract::ManagedListPriceProvider;
use awaken_session_configuration_contract::SessionModelPublicationResolver;
use std::sync::Arc;

use crate::{
    CredentialMaterialIngress, McpAttachmentRealizer, SessionApplication,
    SessionApplicationBuildError, SessionApplicationConfiguration, SessionCredentialSource,
    SessionEnvironmentSource, SessionExecutor, SessionPersistence,
};

/// Material ingress always includes the matching credential read capability.
pub struct SessionCredentials {
    source: Arc<dyn SessionCredentialSource>,
    ingress: Option<Arc<dyn CredentialMaterialIngress>>,
}

impl SessionCredentials {
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
    mcp: Arc<dyn McpAttachmentRealizer>,
    persistence: SessionPersistence,
    environments: Arc<dyn SessionEnvironmentSource>,
    configuration: SessionApplicationConfiguration,
    credentials: Option<SessionCredentials>,
    resources: Option<SessionResources>,
    profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
    models: Option<Arc<dyn SessionModelPublicationResolver>>,
    prices: Option<Arc<dyn ManagedListPriceProvider>>,
    refresh: Option<Arc<ExecutableProjectionRefresh>>,
}

impl SessionApplicationBuilder {
    #[must_use]
    pub(super) fn new(
        runtime: impl Into<SessionExecutor>,
        mcp: Arc<dyn McpAttachmentRealizer>,
        persistence: SessionPersistence,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> Self {
        Self {
            runtime: runtime.into(),
            mcp,
            persistence,
            environments,
            configuration: SessionApplicationConfiguration::default(),
            credentials: None,
            resources: None,
            profiles: None,
            models: None,
            prices: None,
            refresh: None,
        }
    }
}

impl SessionApplicationBuilder {
    #[must_use]
    pub fn configuration(mut self, configuration: SessionApplicationConfiguration) -> Self {
        self.configuration = configuration;
        self
    }

    #[must_use]
    pub fn credentials(mut self, credentials: SessionCredentials) -> Self {
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
        let mut application = SessionApplication::new_inner(
            self.runtime,
            self.mcp,
            self.persistence,
            self.environments,
            configuration,
        );
        if let Some(credentials) = self.credentials {
            application.credential_source = Some(credentials.source);
            application.credential_material_ingress = credentials.ingress;
        }
        if let Some(resources) = self.resources {
            application.resource_registry = Some(resources.catalog);
            if let Some(retention) = resources.retention {
                application.resource_references = Some(retention.references);
                application.resource_files = Some(retention.files);
                application.resource_purge_scheduler = Some(retention.purge);
            }
        }
        application.config_source = self.profiles;
        application.model_publication_resolver = self.models;
        application.managed_list_prices = self.prices;
        application.executable_projection_refresh = self.refresh;
        Ok(application)
    }
}
