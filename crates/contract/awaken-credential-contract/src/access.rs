// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: These existing credential read and material command declarations carry the
// @formal reason: unchanged authority tuples to the same Vault owner; the relocation adds no
// @formal reason: validation, state, proof grade or material delivery behavior.
//! Credential access roles and consumer-neutral material commands.

use crate::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding, CredentialRef,
    CredentialSourceId, CredentialTarget, CredentialUsage, MaterialEntry, PlaintextHolder,
};
use awaken_agent_contract::McpTarget;
use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};

/// Exact credential execution decision already selected by Session.
///
/// Source-row lookup, Workspace admission, compiler time, and exact-holder
/// admission stay with the credential authority; this request carries only the
/// already-selected consumer decision through the Session credential port.
pub struct SessionCredentialAccessRequest {
    pub target: CredentialTarget,
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
    ) -> Result<MaterialEntry, String>;

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
