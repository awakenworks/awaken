//! Protocol-neutral services required by the Session application.

use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};
use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding, CredentialRef,
    CredentialSourceId, CredentialTarget, CredentialUsage, PlaintextHolder,
};
use awaken_environment_contract::EnvItem;
use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_contract::McpTarget;

/// Exact credential execution decision already selected by Session.
///
/// Source-row lookup, Workspace admission, compiler time, and exact-holder
/// admission stay with the credential authority; this request carries only the
/// already-selected consumer decision through the Session credential port.
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
        target: &McpTarget,
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

/// Plaintext admitted at one write-only Credential boundary.
///
/// The consumer adapter owns only the provider-specific transformation into an
/// opaque or typed material document. Vault encoding, persistence, revisioning,
/// and retirement remain behind [`CredentialMaterialIngress`].
pub enum CredentialPlaintext {
    Opaque(RedactedString),
    Structured(StructuredCredentialMaterial),
}

/// Provider identity plus write-only material supplied by a protocol adapter.
/// Target and usage are deliberately absent: the consuming application owns
/// those facts and adds them when it constructs an ingress command.
pub struct CredentialMaterialInput {
    pub provider: String,
    pub plaintext: CredentialPlaintext,
}

impl CredentialMaterialInput {
    #[must_use]
    pub fn opaque(provider: impl Into<String>, value: RedactedString) -> Self {
        Self {
            provider: provider.into(),
            plaintext: CredentialPlaintext::Opaque(value),
        }
    }

    #[must_use]
    pub fn structured(provider: impl Into<String>, material: StructuredCredentialMaterial) -> Self {
        Self {
            provider: provider.into(),
            plaintext: CredentialPlaintext::Structured(material),
        }
    }
}

/// One exact, provider-neutral request to create and seal Credential material.
pub struct CredentialMaterialIngressCommand {
    pub source_id: CredentialSourceId,
    pub workspace_id: String,
    pub target: CredentialTarget,
    pub usage: CredentialUsage,
    pub material: CredentialMaterialInput,
}

/// One exact-revision Credential material rotation.
pub struct CredentialMaterialRotationCommand {
    pub source_id: CredentialSourceId,
    pub expected_revision: u64,
    pub workspace_id: String,
    pub target: CredentialTarget,
    pub usage: CredentialUsage,
    pub material: CredentialMaterialInput,
}

/// One terminal Credential retirement request.
pub struct CredentialMaterialRetirementCommand {
    pub credential: CredentialRef,
    pub workspace_id: String,
}

/// Exact secret-free result of Credential material ingress. The provenance
/// is transient command data; the Credential source remains durable authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialMaterialIngressReceipt {
    pub credential: CredentialRef,
    pub provenance: SessionParticipantProvenance,
}

/// Generic write-only Credential material authority.
///
/// Consumers declare provider material, target, and usage; implementations own
/// sealing, WAL/CAS publication, exact-revision rotation, and retirement. This
/// port must not select a Repository, MCP server, model, or execution holder.
#[async_trait::async_trait]
pub trait CredentialMaterialIngress: Send + Sync {
    async fn enter_material(
        &self,
        command: CredentialMaterialIngressCommand,
    ) -> Result<CredentialMaterialIngressReceipt, String>;

    async fn rotate_material(
        &self,
        command: CredentialMaterialRotationCommand,
    ) -> Result<u64, String>;

    /// Terminally archive one exact source and reclaim its material through the
    /// canonical Credential WAL/CAS path.
    async fn retire_material(
        &self,
        command: CredentialMaterialRetirementCommand,
    ) -> Result<(), String>;
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

    async fn release_session_work(
        &self,
        _lease: &awaken_session_contract::work_queue::SessionWorkLease,
    ) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
        Ok(false)
    }

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
