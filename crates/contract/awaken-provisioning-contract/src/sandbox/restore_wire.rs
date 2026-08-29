//! Reader-first wire evidence for a future exact physical restore target.

use serde::{Deserialize, Serialize};

/// Secret-free identity bound to a provider-owned physical restore target.
/// Phase A only reads and passes this evidence through; the checkpoint
/// composition activates writers after the fleet reader floor is raised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SandboxRestorationEvidence {
    effect_id: String,
    generation_id: String,
    checkpoint_id: String,
    checkpoint_digest: String,
    sandbox_spec_fingerprint: String,
    checkpoint_exclusions_fingerprint: String,
}

impl SandboxRestorationEvidence {
    #[must_use]
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    #[must_use]
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    #[must_use]
    pub fn checkpoint_digest(&self) -> &str {
        &self.checkpoint_digest
    }

    #[must_use]
    pub fn sandbox_spec_fingerprint(&self) -> &str {
        &self.sandbox_spec_fingerprint
    }

    #[must_use]
    pub fn checkpoint_exclusions_fingerprint(&self) -> &str {
        &self.checkpoint_exclusions_fingerprint
    }
}

/// Provider-owned host locator accepted by Phase-A readers but not constructible
/// through the typed API until the restoring composition enables its sole writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HostBindRestorationHandle {
    staging_root: String,
}

impl HostBindRestorationHandle {
    #[must_use]
    pub fn staging_root(&self) -> &str {
        &self.staging_root
    }
}
