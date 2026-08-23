use awaken_runtime_contract::resolved::{ResolvedModelCandidate, ToolDescriptor};
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use sha2::{Digest, Sha256};

use super::CompileError;
use crate::config::AgentConfig;

/// The canonical fingerprint: sha256 of the **behavioral** config serialization,
/// tool descriptors, and resolved publication metadata. Session resources are not
/// Agent publication inputs; they are composed later by `SessionInputResolver`.
///
/// Managed-Agent wire-identity metadata (`name` / `description` / `metadata`) is
/// **excluded**: none is executable snapshot behavior. Otherwise editing
/// presentation or moving an unchanged Agent would mint a new fingerprint for
/// byte-identical execution. Configs that never set these fields hash identically
/// to before because they skip empty values while serializing.
pub(super) fn fingerprint_of(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    metadata: &AgentSnapshotMetadata,
    primary: &ResolvedModelCandidate,
    candidates: &[ResolvedModelCandidate],
    advisor: Option<&ResolvedModelCandidate>,
) -> Result<String, CompileError> {
    let mut behavioral = config.clone();
    behavioral.name = None;
    behavioral.description = None;
    behavioral.metadata.clear();
    let mut bytes =
        serde_json::to_vec(&behavioral).map_err(|err| CompileError::Serialize(err.to_string()))?;
    if !metadata.is_legacy_default() {
        bytes.extend_from_slice(
            &serde_json::to_vec(tools).map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
        bytes.extend_from_slice(
            &serde_json::to_vec(metadata)
                .map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
        bytes.extend_from_slice(
            &serde_json::to_vec(&(primary, candidates))
                .map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
    }
    // Advisor is a new managed publication capability and therefore has no
    // legacy fingerprint to preserve. Its exact provider route and credential
    // revision are behavioral even when the public advisor model id is equal.
    if let Some(advisor) = advisor {
        bytes.extend_from_slice(
            &serde_json::to_vec(advisor).map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}
