//! Protocol-neutral services required by the Session application.

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding, CredentialSourceId,
    CredentialUsage, PlaintextHolder,
};
use awaken_environment_contract::EnvItem;
use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_contract::McpTarget;

/// Exact credential execution decision already selected by Session.
///
/// Source-row lookup, Workspace admission, compiler time, and deferred holder
/// selection stay with the credential authority; this request owns only the
/// exact consumer decision passed through the Session credential port.
pub struct SessionCredentialAccessRequest {
    pub target: awaken_credential_contract::CredentialTarget,
    pub usage: CredentialUsage,
    pub policy: CredentialExecutionPolicy,
    pub selected_holder: PlaintextHolder,
    pub binding: CredentialMaterialBinding,
}

/// Secret-free credential selection used while compiling a Session.
#[async_trait::async_trait]
pub trait SessionCredentialSource: Send + Sync {
    async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, String>;

    async fn mcp_credential_source_for_url(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String>;

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        selected_holder: &PlaintextHolder,
        binding: &CredentialMaterialBinding,
    ) -> Result<CredentialAccess, String>;

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        request: SessionCredentialAccessRequest,
    ) -> Result<CredentialAccess, String>;
}

/// Whether one external participant was created by the current Session command
/// or was adopted from exact durable truth left by an earlier attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionParticipantProvenance {
    Applied,
    Replayed,
}

/// Exact secret-free result of Repository credential ingress. The provenance
/// is transient command data; the Vault source remains the durable authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryCredentialEntry {
    pub credential: awaken_credential_contract::CredentialRef,
    pub provenance: SessionParticipantProvenance,
}

/// Write-only ingress for repository material. Implementations seal material
/// before returning the opaque source id used by Session compilation.
#[async_trait::async_trait]
pub trait RepositoryCredentialIngress: Send + Sync {
    async fn enter_repository_token(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        target: awaken_credential_contract::CredentialTarget,
        token: RedactedString,
    ) -> Result<RepositoryCredentialEntry, String>;

    /// Terminally archive one exact Session-owned Repository source and reclaim
    /// its material through the canonical credential WAL/CAS path.
    async fn retire_repository_token(
        &self,
        credential: &awaken_credential_contract::CredentialRef,
        workspace_id: &str,
    ) -> Result<(), String>;

    async fn rotate_repository_token(
        &self,
        source_id: &CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        target: awaken_credential_contract::CredentialTarget,
        token: RedactedString,
    ) -> Result<u64, String>;
}

/// Exact executable Environment projection resolved for one Session creation.
pub struct ResolvedSessionEnvironment {
    pub snapshot: awaken_session_contract::EnvironmentSnapshot,
}

/// Coordinator projection consumed by Session compilation. Environment HTTP
/// routes may use the same concrete adapter, but their wire types stay outside
/// this service contract.
#[async_trait::async_trait]
pub trait SessionEnvironmentSource: Send + Sync {
    async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn enqueue_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError>;

    async fn wake_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError>;

    async fn retire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::work_queue::WorkItem>,
        awaken_session_contract::work_queue::WorkQueueError,
    >;

    async fn acquire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<
        Option<awaken_session_contract::work_queue::SessionWorkLease>,
        awaken_session_contract::work_queue::WorkQueueError,
    >;

    async fn release_worker_session_work(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_session_contract::work_queue::WorkQueueError> {
        Ok(0)
    }
}
