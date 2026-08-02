//! Construction and port wiring for the Managed protocol adapter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use awaken_session_contract::{ManagedSessionRepository, SessionLifecycleSink};
use awaken_session_store::SqliteManagedSessionRepository;

use super::mcp_attachment::UnsupportedMcpAttachmentRealizer;
use super::{ManagedState, SessionRuntime};
use crate::routes::vaults::{RepositoryCredentialIngress, SessionCredentialSource, VaultState};

/// Distinguishes multiple Managed adapters constructed inside one process tick
/// (tests and embedded multi-tenant composition). Production still normally has
/// one adapter per Coordinator process.
static MANAGED_STATE_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl ManagedState {
    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self::from_ports(
            Arc::new(runtime),
            Arc::new(UnsupportedMcpAttachmentRealizer),
        )
    }

    /// Compose one object that implements both independent application ports.
    /// The shared `Arc` preserves one adapter instance without merging the
    /// Session turn lifecycle with MCP attachment realization.
    pub fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: SessionRuntime + awaken_session_contract::McpAttachmentRealizer + 'static,
    {
        let runtime = Arc::new(runtime);
        Self::from_ports(runtime.clone(), runtime)
    }

    fn from_ports(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let sessions_repo: Arc<dyn ManagedSessionRepository> = Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral managed Session repository"),
        );
        runtime.install_environment_binding_sink(Arc::new(
            super::environment::RepositoryEnvironmentBindingSink::new(sessions_repo.clone()),
        ));
        Self {
            runtime,
            mcp_realizer,
            credential_source: None,
            repository_credential_ingress: None,
            environments: Arc::new(
                crate::routes::environments::EnvironmentExecutionState::default(),
            ),
            config_source: None,
            resource_catalog: None,
            resource_purge_scheduler: None,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            sessions_repo,
            lifecycle_sink: None,
            runtime_incarnation: format!(
                "managed:{}:{started_at}:{}",
                std::process::id(),
                MANAGED_STATE_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            session_seq: AtomicU64::new(0),
            event_seq: Arc::new(AtomicU64::new(0)),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Wire a projection sink (a webhook dispatcher) so committed session lifecycle
    /// facts fan out to workspace-scoped subscribers (ADR-0048). Default: none.
    #[must_use]
    pub fn with_lifecycle_sink(mut self, sink: Arc<dyn SessionLifecycleSink>) -> Self {
        self.lifecycle_sink = Some(sink);
        self
    }

    /// Wire the environments surface, so `POST /v1/sessions` resolves the session's
    /// `environment_id` to its networking policy (egress on/off). Share the same
    /// Coordinator's one executable Environment projection and WorkQueue.
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::routes::environments::EnvironmentExecutionState>,
    ) -> Self {
        self.environments = environments;
        self
    }

    /// Wire a durable session repository (e.g. SQLite alongside the transcript
    /// store) so a session's config survives a restart and is reported faithfully
    /// by another process. The default is in-memory (single-process behavior).
    #[must_use]
    pub fn with_session_repo(mut self, repo: Arc<dyn ManagedSessionRepository>) -> Self {
        self.runtime.install_environment_binding_sink(Arc::new(
            super::environment::RepositoryEnvironmentBindingSink::new(repo.clone()),
        ));
        self.sessions_repo = repo;
        self
    }

    /// Wire recoverable physical cleanup scheduling. Authorization has already
    /// completed at the edge; this port receives resource identity only.
    #[must_use]
    pub fn with_resource_purge_scheduler(
        mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) -> Self {
        self.resource_purge_scheduler = Some(scheduler);
        self
    }

    /// Wire the vault surface, so `POST /v1/sessions` binds each requested MCP
    /// server to a vault credential by URL (ADR-0043 Phase 3). Share the same
    /// `VaultState` with [`crate::vault_router`], or the sessions and the vault
    /// routes see different credentials.
    #[must_use]
    pub fn with_vaults(mut self, vaults: Arc<VaultState>) -> Self {
        self.credential_source = Some(vaults.clone());
        self.repository_credential_ingress = Some(vaults);
        self
    }

    /// Wire a split-service implementation of the write-only Repository token
    /// ingress independently from the secret-free credential selection port.
    #[must_use]
    pub fn with_repository_credential_ingress(
        mut self,
        ingress: Arc<dyn RepositoryCredentialIngress>,
    ) -> Self {
        self.repository_credential_ingress = Some(ingress);
        self
    }

    /// Wire the same secret-free credential-selection port through either the
    /// local VaultState adapter or the authenticated split-service adapter.
    #[must_use]
    pub fn with_credential_source(mut self, source: Arc<dyn SessionCredentialSource>) -> Self {
        self.credential_source = Some(source);
        self
    }

    /// Wire the config-plane agent projection source so a session inherits a
    /// published agent's authoritative `model`. Share the same
    /// [`awaken_executable_agent_contract::ExecutableAgentProfileSource`] that
    /// `/v1/agents` uses, or the Session and executable profile disagree.
    #[must_use]
    pub fn with_config_source(
        mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) -> Self {
        self.config_source = Some(source);
        self
    }

    /// Wire the platform Resource Catalog used to resolve Memory/Repository
    /// configuration once at Session creation.
    #[must_use]
    pub fn with_resource_catalog(
        mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    ) -> Self {
        self.resource_catalog = Some(catalog);
        self
    }
}
