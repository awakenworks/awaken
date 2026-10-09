// @formal surface exclude=composition_wiring
// @formal owner crates/server/awaken-session-application/src/lib.rs
// @formal reason: Constructs the disposable Managed projection and installs application ports;
// @formal reason: test-support builders are configuration-gated and production behavior remains in
// @formal reason: SessionApplication.
//! Managed protocol access to the canonical Session application.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use awaken_session_application::SessionApplication;
#[cfg(any(test, feature = "test-support"))]
use awaken_session_application::{CredentialMaterialIngress, SessionCredentialSource};
#[cfg(any(test, feature = "test-support"))]
use awaken_session_application_contract::LifecycleFactNotifier;

use super::ManagedState;
#[cfg(any(test, feature = "test-support"))]
use super::mcp_attachment::UnsupportedMcpAttachmentRealizer;
#[cfg(any(test, feature = "test-support"))]
use crate::routes::vaults::VaultState;

impl ManagedState {
    /// Volatile fixture constructor for this crate's own tests. The stores come
    /// from `awaken-protocol-managed-testkit` (a dev-dependency); other crates
    /// use its `ManagedStateFixture`. Product startup injects the durable
    /// Session repository and the canonical Environment execution projection via
    /// [`ManagedState::from_application`].
    #[cfg(test)]
    pub async fn new(
        runtime: impl awaken_session_execution_contract::CompleteSessionRuntime + 'static,
    ) -> Self {
        let clock = awaken_time::system_clock();
        Self::from_fixture_parts(
            Arc::new(runtime),
            None,
            awaken_protocol_managed_testkit::ephemeral_session_repository().await,
            awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
            clock,
        )
    }

    /// Use one fixture object that provides both independent application services.
    /// The shared `Arc` preserves one instance without merging the
    /// Session Run lifecycle with MCP attachment realization.
    #[cfg(test)]
    pub async fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: awaken_session_execution_contract::CompleteSessionRuntime
            + awaken_session_execution_contract::McpAttachmentRealizer
            + 'static,
    {
        let runtime = Arc::new(runtime);
        let clock = awaken_time::system_clock();
        Self::from_fixture_parts(
            runtime.clone(),
            Some(runtime),
            awaken_protocol_managed_testkit::ephemeral_session_repository().await,
            awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
            clock,
        )
    }

    /// Construct the disposable Managed wire projection over the one canonical
    /// Session application created during process startup.
    pub fn from_application(
        application: Arc<SessionApplication>,
        environments: Arc<
            awaken_environment_execution_application::EnvironmentExecutionApplication,
        >,
    ) -> Self {
        Self {
            roles: super::SessionApplicationRoles::of(&application),
            #[cfg(any(test, feature = "test-support"))]
            application,
            environments,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            event_seq: Arc::new(AtomicU64::new(0)),
            live: Mutex::new(HashMap::new()),
            inference_geo_policy: None,
            #[cfg(test)]
            warm_cold_oracle: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Stop checking published refreshes against a cold rebuild. The check
    /// reads the committed prefix again, so only a test asserting the exact
    /// source-read trace of one refresh may turn it off.
    #[cfg(test)]
    pub(crate) fn disable_warm_cold_oracle(&self) {
        self.warm_cold_oracle
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Install the hosted Workspace policy used at Session creation and again
    /// before every Run. The latter is intentionally dynamic so narrowing an
    /// allowlist stops an already-created Session on its next Run.
    #[must_use]
    pub fn with_inference_geo_policy(
        mut self,
        policy: Arc<dyn crate::ManagedInferenceGeoPolicy>,
    ) -> Self {
        self.inference_geo_policy = Some(policy);
        self
    }

    /// The fixture composition's Session application, for tests that drive it
    /// beside this adapter. Product composition owns its application and hands
    /// it to every other consumer itself; this adapter holds only its roles.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn session_application(&self) -> Arc<SessionApplication> {
        self.application.clone()
    }

    /// Exact executable Environment application used by Session admission.
    /// Coordinator composition reuses this Arc for the Worker warmup adapter so
    /// test-support and production cannot observe different desired capacity.
    #[must_use]
    pub fn environment_execution(
        &self,
    ) -> Arc<awaken_environment_execution_application::EnvironmentExecutionApplication> {
        self.environments.clone()
    }

    /// Finish configuring the fixture application before it is shared. The
    /// roles are clones of the same `Arc`, so they are released first and
    /// narrowed again from the configured application.
    #[cfg(any(test, feature = "test-support"))]
    fn configure_application(self, configure: impl FnOnce(&mut SessionApplication)) -> Self {
        let Self {
            roles,
            mut application,
            environments,
            sessions,
            owners,
            session_seq,
            event_seq,
            live,
            inference_geo_policy,
            #[cfg(test)]
            warm_cold_oracle,
        } = self;
        drop(roles);
        configure(
            Arc::get_mut(&mut application)
                .expect("ManagedState builders must finish before the application is shared"),
        );
        Self {
            roles: super::SessionApplicationRoles::of(&application),
            application,
            environments,
            sessions,
            owners,
            session_seq,
            event_seq,
            live,
            inference_geo_policy,
            #[cfg(test)]
            warm_cold_oracle,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executable_projection_refresh(
        self,
        refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
    ) -> Self {
        self.configure_application(|application| {
            application
                .set_executable_projection_refresh(refresh)
                .expect("ManagedState fixture executable projection refresh binds once");
        })
    }

    /// Store-agnostic volatile constructor: the caller supplies the Session
    /// repository and Environment execution projection behind their ports. No
    /// MCP realizer means attachments are refused, never silently realized.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_fixture_parts(
        runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
        mcp_realizer: Option<Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
        sessions_repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
        environments: Arc<
            awaken_environment_execution_application::EnvironmentExecutionApplication,
        >,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self::from_application(
            Arc::new(SessionApplication::new(
                runtime,
                mcp_realizer.unwrap_or_else(|| Arc::new(UnsupportedMcpAttachmentRealizer)),
                awaken_session_application::SessionPersistence::new(sessions_repo, clock),
                environments.clone(),
            )),
            environments,
        )
    }

    /// Wire a projection sink (a webhook dispatcher) so committed session lifecycle
    /// facts fan out to workspace-scoped subscribers (ADR-0048). Default: none.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_lifecycle_notifier(self, notifier: Arc<dyn LifecycleFactNotifier>) -> Self {
        self.configure_application(|application| {
            application
                .set_lifecycle_notifier(notifier)
                .expect("ManagedState fixture lifecycle notifier is bound once");
        })
    }

    /// Wire the environments surface, so `POST /v1/sessions` resolves the session's
    /// `environment_id` to its networking policy (egress on/off). Share the same
    /// Coordinator's one executable Environment projection and WorkQueue.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_environments(
        self,
        environments: Arc<
            awaken_environment_execution_application::EnvironmentExecutionApplication,
        >,
    ) -> Self {
        let this = self.configure_application(|application| {
            application.replace_environment_source(environments.clone());
        });
        Self {
            environments,
            ..this
        }
    }

    /// Replace the already-explicit Session repository. This is primarily useful
    /// for fixture overrides applied before sharing; there is no implicit default.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_session_repo(
        self,
        repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    ) -> Self {
        self.configure_application(|application| {
            application.replace_repository(repo);
        })
    }

    /// Inject the sole versioned Managed reference-price authority. Budgeted
    /// Session creation fails closed when this provider is absent.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_managed_list_price_provider(
        self,
        provider: Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_managed_list_price_provider(provider);
        })
    }

    /// Wire recoverable physical cleanup scheduling. Authorization has already
    /// completed at the edge; this service receives resource identity only.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_purge_scheduler(
        self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_resource_purge_scheduler(scheduler);
        })
    }

    /// Supply the selected Resource authorities' durable reference index and File
    /// catalog into Session retention projection. The protocol only supplies
    /// existing Resource services; it does not own Resource lifecycle behavior.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_reference_authority(
        self,
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
        files: Arc<dyn awaken_resource_contract::FileCatalog>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_resource_reference_authority(references, files);
        })
    }

    /// Wire the vault surface, so `POST /v1/sessions` binds each requested MCP
    /// server to a vault credential by URL (ADR-0043 Phase 3). Share the same
    /// `VaultState` with [`crate::vault_router`], or the sessions and the vault
    /// routes see different credentials.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_vaults(self, vaults: Arc<VaultState>) -> Self {
        self.configure_application(|application| {
            application.set_credential_source(vaults.clone());
            application.set_credential_material_ingress(vaults);
        })
    }

    /// Wire a split-service implementation of generic write-only Credential
    /// material ingress independently from secret-free credential selection.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credential_material_ingress(
        self,
        ingress: Arc<dyn CredentialMaterialIngress>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_credential_material_ingress(ingress);
        })
    }

    /// Wire the same secret-free credential selection through either the
    /// local VaultState adapter or the authenticated split-service adapter.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credential_source(self, source: Arc<dyn SessionCredentialSource>) -> Self {
        self.configure_application(|application| {
            application.set_credential_source(source);
        })
    }

    /// Wire the config-plane agent projection source so a session inherits a
    /// published agent's authoritative `model`. Share the same
    /// [`awaken_executable_agent_contract::ExecutableAgentProfileSource`] that
    /// `/v1/agents` uses, or the Session and executable profile disagree.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_config_source(
        self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_config_source(source);
        })
    }

    /// Wire the same canonical model-publication authority used by Agent
    /// publication. Test fixtures use this builder to prove override admission
    /// without creating a protocol-local catalog or parser.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_model_publication_resolver(
        self,
        resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_model_publication_resolver(resolver);
        })
    }

    /// Wire the platform Resource Registry used to resolve Memory/Repository
    /// configuration once at Session creation.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_registry(
        self,
        catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ) -> Self {
        self.configure_application(|application| {
            application.set_resource_registry(catalog);
        })
    }
}
