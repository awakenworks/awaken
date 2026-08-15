//! Managed protocol access to the canonical Session application.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use awaken_session_application::SessionApplication;
#[cfg(any(test, feature = "test-support"))]
use awaken_session_application::{RepositoryCredentialIngress, SessionCredentialSource};
#[cfg(any(test, feature = "test-support"))]
use awaken_session_contract::LifecycleFactNotifier;
#[cfg(any(test, feature = "test-support"))]
use awaken_session_contract::ManagedSessionRepository;
#[cfg(any(test, feature = "test-support"))]
use awaken_session_store::SqliteManagedSessionRepository;

use super::ManagedState;
#[cfg(any(test, feature = "test-support"))]
use super::SessionRuntime;
#[cfg(any(test, feature = "test-support"))]
use super::mcp_attachment::UnsupportedMcpAttachmentRealizer;
#[cfg(any(test, feature = "test-support"))]
use crate::routes::vaults::VaultState;

impl ManagedState {
    /// Volatile fixture constructor. Product startup must inject the durable
    /// Session repository and the canonical Environment execution projection via
    /// [`ManagedState::from_application`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self::from_runtime_fixture(
            Arc::new(runtime),
            Arc::new(UnsupportedMcpAttachmentRealizer),
            ephemeral_session_repository(),
            crate::test_support::environment_components().1,
        )
    }

    /// Use one fixture object that provides both independent application services.
    /// The shared `Arc` preserves one instance without merging the
    /// Session turn lifecycle with MCP attachment realization.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: SessionRuntime + awaken_session_contract::McpAttachmentRealizer + 'static,
    {
        let runtime = Arc::new(runtime);
        Self::from_runtime_fixture(
            runtime.clone(),
            runtime,
            ephemeral_session_repository(),
            crate::test_support::environment_components().1,
        )
    }

    /// Construct the disposable Managed wire projection over the one canonical
    /// Session application created during process startup.
    pub fn from_application(application: Arc<SessionApplication>) -> Self {
        Self {
            application,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            event_seq: Arc::new(AtomicU64::new(0)),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Canonical protocol-independent Session application used by private
    /// Worker transports and internal services.
    #[must_use]
    pub fn session_application(&self) -> Arc<SessionApplication> {
        self.application.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    fn application_mut(&mut self) -> &mut SessionApplication {
        Arc::get_mut(&mut self.application)
            .expect("ManagedState builders must finish before the application is shared")
    }

    #[cfg(any(test, feature = "test-support"))]
    fn from_runtime_fixture(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
        sessions_repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<
            awaken_environment_execution_application::EnvironmentExecutionApplication,
        >,
    ) -> Self {
        Self::from_application(Arc::new(SessionApplication::new(
            runtime,
            mcp_realizer,
            sessions_repo,
            environments,
        )))
    }

    /// Wire a projection sink (a webhook dispatcher) so committed session lifecycle
    /// facts fan out to workspace-scoped subscribers (ADR-0048). Default: none.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_lifecycle_notifier(mut self, notifier: Arc<dyn LifecycleFactNotifier>) -> Self {
        self.application_mut().set_lifecycle_notifier(notifier);
        self
    }

    /// Wire the environments surface, so `POST /v1/sessions` resolves the session's
    /// `environment_id` to its networking policy (egress on/off). Share the same
    /// Coordinator's one executable Environment projection and WorkQueue.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<
            awaken_environment_execution_application::EnvironmentExecutionApplication,
        >,
    ) -> Self {
        self.application_mut()
            .replace_environment_source(environments);
        self
    }

    /// Replace the already-explicit Session repository. This is primarily useful
    /// for fixture overrides applied before sharing; there is no implicit default.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_session_repo(mut self, repo: Arc<dyn ManagedSessionRepository>) -> Self {
        self.application_mut().replace_repository(repo);
        self
    }

    /// Inject the sole versioned Managed reference-price authority. Budgeted
    /// Session creation fails closed when this provider is absent.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_managed_list_price_provider(
        mut self,
        provider: Arc<dyn awaken_session_contract::ManagedListPriceProvider>,
    ) -> Self {
        self.application_mut()
            .set_managed_list_price_provider(provider);
        self
    }

    /// Wire recoverable physical cleanup scheduling. Authorization has already
    /// completed at the edge; this service receives resource identity only.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_purge_scheduler(
        mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) -> Self {
        self.application_mut()
            .set_resource_purge_scheduler(scheduler);
        self
    }

    /// Supply the selected Resource authorities' durable reference index and File
    /// catalog into Session retention projection. The protocol only supplies
    /// existing Resource services; it does not own Resource lifecycle behavior.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_reference_authority(
        mut self,
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
        files: Arc<dyn awaken_resource_contract::FileCatalog>,
    ) -> Self {
        self.application_mut()
            .set_resource_reference_authority(references, files);
        self
    }

    /// Wire the vault surface, so `POST /v1/sessions` binds each requested MCP
    /// server to a vault credential by URL (ADR-0043 Phase 3). Share the same
    /// `VaultState` with [`crate::vault_router`], or the sessions and the vault
    /// routes see different credentials.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_vaults(mut self, vaults: Arc<VaultState>) -> Self {
        self.application_mut().set_credential_source(vaults.clone());
        self.application_mut()
            .set_repository_credential_ingress(vaults);
        self
    }

    /// Wire a split-service implementation of the write-only Repository token
    /// ingress independently from secret-free credential selection.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_repository_credential_ingress(
        mut self,
        ingress: Arc<dyn RepositoryCredentialIngress>,
    ) -> Self {
        self.application_mut()
            .set_repository_credential_ingress(ingress);
        self
    }

    /// Wire the same secret-free credential selection through either the
    /// local VaultState adapter or the authenticated split-service adapter.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credential_source(mut self, source: Arc<dyn SessionCredentialSource>) -> Self {
        self.application_mut().set_credential_source(source);
        self
    }

    /// Wire the config-plane agent projection source so a session inherits a
    /// published agent's authoritative `model`. Share the same
    /// [`awaken_executable_agent_contract::ExecutableAgentProfileSource`] that
    /// `/v1/agents` uses, or the Session and executable profile disagree.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_config_source(
        mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) -> Self {
        self.application_mut().set_config_source(source);
        self
    }

    /// Wire the same canonical model-publication authority used by Agent
    /// publication. Test fixtures use this builder to prove override admission
    /// without creating a protocol-local catalog or parser.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_model_publication_resolver(
        mut self,
        resolver: Arc<dyn awaken_session_contract::SessionModelPublicationResolver>,
    ) -> Self {
        self.application_mut()
            .set_model_publication_resolver(resolver);
        self
    }

    /// Wire the platform Resource Catalog used to resolve Memory/Repository
    /// configuration once at Session creation.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_catalog(
        mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    ) -> Self {
        self.application_mut().set_resource_catalog(catalog);
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
fn ephemeral_session_repository() -> Arc<dyn ManagedSessionRepository> {
    Arc::new(
        SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository"),
    )
}
