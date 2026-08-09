//! Owned storage for the Managed protocol adapter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use awaken_session_contract::{ManagedSessionRepository, SessionLifecycleSink};

use crate::routes::vaults::{RepositoryCredentialIngress, SessionCredentialSource};
use crate::types::StreamFrame;

use super::{SessionRecord, SessionRuntime};

/// The adapter's in-memory Session projection plus its runtime and persistence ports.
pub struct ManagedState {
    pub(super) runtime: Arc<dyn SessionRuntime>,
    /// Sole external realization port for initial and hot MCP generations.
    pub(super) mcp_realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    /// Optional vault-backed source for Session MCP credentials.
    pub(super) credential_source: Option<Arc<dyn SessionCredentialSource>>,
    /// Sole write-only path for Managed Repository authorization material.
    pub(super) repository_credential_ingress: Option<Arc<dyn RepositoryCredentialIngress>>,
    /// Authoritative Environment execution projection used by Session creation.
    pub(super) environments: Arc<crate::routes::environments::EnvironmentExecutionState>,
    /// Config-plane source of immutable executable Agent profiles.
    pub(super) config_source:
        Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    /// Resource identity and lifecycle authorities used by Managed adapters.
    pub(super) resource_catalog: Option<Arc<dyn awaken_resource_contract::ResourceCatalog>>,
    pub(super) resource_purge_scheduler:
        Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    /// Disposable per-process read projection over `sessions_repo`.
    pub(super) sessions: Mutex<HashMap<String, SessionRecord>>,
    /// Edge-owned Session-to-Workspace projection; the core remains tenancy-neutral.
    pub(super) owners: Mutex<HashMap<String, String>>,
    /// Durable source of truth for the Session aggregate.
    pub(super) sessions_repo: Arc<dyn ManagedSessionRepository>,
    /// Optional sink for committed Session lifecycle facts.
    pub(super) lifecycle_sink: Option<Arc<dyn SessionLifecycleSink>>,
    /// Unique process incarnation persisted in realization leases.
    pub(super) runtime_incarnation: String,
    /// Fence for the one canonical lifecycle supervisor.
    pub(super) lifecycle_supervisor_started: AtomicBool,
    pub(super) session_seq: AtomicU64,
    /// Shared with each turn's [`crate::preview::PreviewSink`] so preview and
    /// committed ids use one sequence.
    pub(super) event_seq: Arc<AtomicU64>,
    /// Lazily created per-Session live SSE broadcast channels.
    pub(super) live: Mutex<HashMap<String, broadcast::Sender<StreamFrame>>>,
}
