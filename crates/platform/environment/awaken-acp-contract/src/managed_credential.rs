//! Neutral, secret-free description of ACP managed credential delivery.
//!
//! The ACP catalog selects one delivery form. Last-mile materializers consume
//! this description without importing the process executor or duplicating its
//! catalog decision.

/// Provider-owned credential artifact codecs supported by managed launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialArtifactCodec {
    CodexAuthJson,
}

/// One managed artifact selected from an ACP CLI profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialArtifactSpec {
    pub codec: CredentialArtifactCodec,
    pub relative_path: &'static str,
}

/// How an isolated, Awaken-managed launch receives provider credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCredentialDelivery {
    ProcessSecret,
    Artifact(CredentialArtifactSpec),
}

impl ManagedCredentialDelivery {
    /// Select an artifact only when this profile and the pinned credential shape
    /// require one.
    #[must_use]
    pub fn credential_artifact(self, _has_refresh: bool) -> Option<CredentialArtifactSpec> {
        match self {
            Self::Artifact(spec) => Some(spec),
            Self::ProcessSecret => None,
        }
    }

    #[must_use]
    pub fn allows_process_secret(self) -> bool {
        matches!(self, Self::ProcessSecret)
    }
}

/// One claim-fenced provider credential artifact. Only a broker reference and
/// provider-owned relative path cross into the executor, never material bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialArtifactRequirement {
    reference: String,
    relative_path: String,
}

impl CredentialArtifactRequirement {
    #[must_use]
    pub fn new(reference: impl Into<String>, relative_path: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            relative_path: relative_path.into(),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

impl std::fmt::Debug for CredentialArtifactRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialArtifactRequirement(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_delivery_and_reference_are_exact_and_secret_free() {
        // Causes: D1 process delivery, D2 artifact delivery, D3 an opaque
        // one-shot reference. Effects: only D2 selects a codec/path; D1
        // permits a process secret; Debug never reveals D3. The catalog is the
        // sole selector; this contract carries its choice without reselecting.
        let spec = CredentialArtifactSpec {
            codec: CredentialArtifactCodec::CodexAuthJson,
            relative_path: ".codex/auth.json",
        };
        assert!(ManagedCredentialDelivery::ProcessSecret.allows_process_secret());
        assert!(
            ManagedCredentialDelivery::ProcessSecret
                .credential_artifact(false)
                .is_none()
        );
        let artifact = ManagedCredentialDelivery::Artifact(spec);
        assert!(!artifact.allows_process_secret());
        assert_eq!(artifact.credential_artifact(true), Some(spec));
        let requirement = CredentialArtifactRequirement::new("opaque-one-shot", spec.relative_path);
        assert_eq!(requirement.reference(), "opaque-one-shot");
        assert_eq!(requirement.relative_path(), spec.relative_path);
        assert!(!format!("{requirement:?}").contains("opaque-one-shot"));
    }
}
