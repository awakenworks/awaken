// @formal surface obligation=credential.material_profile_admission_preserves_complete_tuples
// @formal reason: Pure ACP capability metadata projection preserves complete holder/source/mechanism
// @formal reason: tuples for one backend. The attempt decision table checks missing, foreign, split
// @formal reason: and contradictory profiles; no lifecycle transition or realization effect lives here.
//! ACP backend correlation over the existing credential capability contract.

use super::{CredentialRealizationCapabilities, CredentialRealizationKind};
use std::collections::BTreeSet;

/// Namespaced extension-consumer key for the exact ACP backend that consumes a
/// credential realization profile. This keeps independently installed ACP
/// adapters correlated with their last-mile mechanism after Worker profiles
/// are composed.
pub const ACP_CREDENTIAL_CONSUMER_PREFIX: &str = "awaken.acp.credential-delivery/";
pub const PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE: &str =
    "awaken.credential.process-secret-environment/v1";
pub const PRIVATE_SECRET_FILE_MATERIAL_TYPE: &str = "awaken.credential.private-secret-file/v1";
pub const PLATFORM_PROVIDER_ADAPTER_MATERIAL_TYPE: &str =
    "awaken.credential.platform-provider-adapter/v1";

impl CredentialRealizationCapabilities {
    /// Retain complete tuples from the exact ACP adapter only. A Native or
    /// foreign CLI profile must never supply missing dimensions to this binding.
    pub(crate) fn acp_backend_profiles(&self, backend_ref: &str) -> Self {
        let consumer_id = format!("{ACP_CREDENTIAL_CONSUMER_PREFIX}{backend_ref}");
        let mut profiles = self
            .alternatives
            .iter()
            .map(|profile| profile.acp_backend_profiles(backend_ref))
            .collect::<Vec<_>>();
        if self.extension_consumers.contains_key(&consumer_id) {
            let mut own = self.clone();
            own.alternatives.clear();
            profiles.push(own);
        }
        Self::alternatives(profiles)
    }

    /// Resolve the one last-mile credential mechanism advertised for an exact
    /// ACP backend. Missing evidence returns None and cannot imply a fallback;
    /// contradictory evidence fails closed.
    pub fn acp_backend_realization_kind(
        &self,
        backend_ref: &str,
    ) -> Result<Option<CredentialRealizationKind>, String> {
        let consumer_id = format!("{ACP_CREDENTIAL_CONSUMER_PREFIX}{backend_ref}");
        let mut kinds = BTreeSet::new();
        for (material_type, kind) in [
            (
                PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE,
                CredentialRealizationKind::ProcessSecretEnvironment,
            ),
            (
                PRIVATE_SECRET_FILE_MATERIAL_TYPE,
                CredentialRealizationKind::PrivateSecretFile,
            ),
            (
                PLATFORM_PROVIDER_ADAPTER_MATERIAL_TYPE,
                CredentialRealizationKind::PlatformProviderAdapter,
            ),
        ] {
            if self.supports_extension(&consumer_id, material_type) {
                kinds.insert(kind);
            }
        }
        match kinds.len() {
            0 => Ok(None),
            1 => Ok(kinds.into_iter().next()),
            _ => Err(format!(
                "ACP backend `{backend_ref}` advertises contradictory credential delivery mechanisms"
            )),
        }
    }
}
