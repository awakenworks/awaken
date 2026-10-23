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
/// Cloning preserves the existing redacted leaf ownership; it carries no write
/// attempt, receipt or permission to enter material again.
///
/// S3-S4: neither variant may cross a durable Serde boundary.
///
/// ```compile_fail,E0277
/// use awaken_credential_contract::CredentialPlaintext;
/// fn durable<T: serde::Serialize>() {}
/// durable::<CredentialPlaintext>();
/// ```
///
/// ```compile_fail,E0277
/// use awaken_credential_contract::CredentialPlaintext;
/// fn durable<T: serde::de::DeserializeOwned>() {}
/// durable::<CredentialPlaintext>();
/// ```
#[derive(Debug, Clone)]
pub enum CredentialPlaintext {
    Opaque(RedactedString),
    Structured(StructuredCredentialMaterial),
}

/// Provider identity plus write-only material supplied by a protocol adapter.
/// Target and usage are deliberately absent: the consuming application owns
/// those facts and adds them when it constructs an ingress command.
/// Debug exposes provider/type/field-name metadata, with every material value
/// redacted by its existing [`RedactedString`] leaf.
///
/// S5-S6: copying an input must not add a durable Serde representation.
///
/// ```compile_fail,E0277
/// use awaken_credential_contract::CredentialMaterialInput;
/// fn durable<T: serde::Serialize>() {}
/// durable::<CredentialMaterialInput>();
/// ```
///
/// ```compile_fail,E0277
/// use awaken_credential_contract::CredentialMaterialInput;
/// fn durable<T: serde::de::DeserializeOwned>() {}
/// durable::<CredentialMaterialInput>();
/// ```
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_material_preserves_values_and_redaction_across_clone_and_drop() {
        // Cause/effect graph: C1 opaque/structured, C2 original/clone after
        // dropping original. E1 provider/type/every value remain exact; E2 Debug
        // contains no secret value. Four exhaustive rows V1=opaque+original,
        // V2=opaque+clone/drop, V3=structured+original, V4=structured+clone/drop.
        // This proves independent valid ownership, not heap-wide erasure or a
        // shorter lifetime. Provider/type/field-name metadata is not secret.
        for structured in [false, true] {
            for clone_after_drop in [false, true] {
                let input = if structured {
                    CredentialMaterialInput::structured(
                        "adapter/provider",
                        StructuredCredentialMaterial {
                            type_id: "adapter.material/v1".into(),
                            fields: std::collections::BTreeMap::from([
                                ("first".into(), RedactedString::new("first-secret-value")),
                                ("second".into(), RedactedString::new("second-secret-value")),
                            ]),
                        },
                    )
                } else {
                    CredentialMaterialInput::opaque(
                        "adapter/provider",
                        RedactedString::new("opaque-secret-value"),
                    )
                };
                let retained = if clone_after_drop {
                    let cloned = input.clone();
                    drop(input);
                    cloned
                } else {
                    input
                };
                assert_eq!(retained.provider, "adapter/provider", "V1-V4/E1");
                let debug = format!("{retained:?}");
                assert!(!debug.contains("opaque-secret-value"), "V1-V2/E2");
                assert!(!debug.contains("first-secret-value"), "V3-V4/E2");
                assert!(!debug.contains("second-secret-value"), "V3-V4/E2");
                match retained.plaintext {
                    CredentialPlaintext::Opaque(value) => {
                        assert!(!structured, "V1-V2 variant");
                        assert_eq!(value.expose_secret(), "opaque-secret-value", "V1-V2/E1");
                    }
                    CredentialPlaintext::Structured(material) => {
                        assert!(structured, "V3-V4 variant");
                        assert_eq!(material.type_id, "adapter.material/v1", "V3-V4/E1");
                        assert_eq!(material.fields.len(), 2, "V3-V4/E1");
                        assert_eq!(
                            material.fields["first"].expose_secret(),
                            "first-secret-value",
                            "V3-V4/E1"
                        );
                        assert_eq!(
                            material.fields["second"].expose_secret(),
                            "second-secret-value",
                            "V3-V4/E1"
                        );
                    }
                }
            }
        }
    }
}
