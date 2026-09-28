// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Reader-first restoration evidence is a closed, secret-free compatibility envelope;
// @formal reason: current constructors cannot write it, and provider activation remains behind the
// @formal reason: existing Sandbox capability boundary.
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
    pub(super) fn new(
        effect_id: String,
        generation_id: String,
        checkpoint_id: String,
        checkpoint_digest: String,
        sandbox_spec_fingerprint: String,
        checkpoint_exclusions_fingerprint: String,
    ) -> Self {
        Self {
            effect_id,
            generation_id,
            checkpoint_id,
            checkpoint_digest,
            sandbox_spec_fingerprint,
            checkpoint_exclusions_fingerprint,
        }
    }

    /// Reconstruct a complete substrate tuple after observing all exact fields.
    /// Providers must reject partial evidence before calling this constructor.
    pub fn from_exact_parts(
        effect_id: impl Into<String>,
        generation_id: impl Into<String>,
        checkpoint_id: impl Into<String>,
        checkpoint_digest: impl Into<String>,
        sandbox_spec_fingerprint: impl Into<String>,
        checkpoint_exclusions_fingerprint: impl Into<String>,
    ) -> Result<Self, super::SandboxError> {
        let evidence = Self::new(
            effect_id.into(),
            generation_id.into(),
            checkpoint_id.into(),
            checkpoint_digest.into(),
            sandbox_spec_fingerprint.into(),
            checkpoint_exclusions_fingerprint.into(),
        );
        if [
            evidence.effect_id.as_str(),
            evidence.generation_id.as_str(),
            evidence.checkpoint_id.as_str(),
            evidence.checkpoint_digest.as_str(),
            evidence.sandbox_spec_fingerprint.as_str(),
            evidence.checkpoint_exclusions_fingerprint.as_str(),
        ]
        .into_iter()
        .any(str::is_empty)
        {
            return Err(super::SandboxError::invalid(
                "sandbox restoration evidence fields must be non-empty",
            ));
        }
        Ok(evidence)
    }

    pub(super) fn verify(
        &self,
        request: &super::SandboxRestoreRequest,
        spec: &crate::SandboxSpec,
    ) -> Result<(), super::SandboxError> {
        request.validate_for_spec(spec)?;
        if self == &request.evidence(spec) {
            Ok(())
        } else {
            Err(super::SandboxError::conflict(
                "sandbox restoration evidence does not match the exact restore request",
            ))
        }
    }

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

    /// Complete collision-resistant key for the one physical target named by
    /// the authoritative operation effect. All other evidence remains an exact
    /// fence on that target rather than selecting a second target.
    pub fn physical_target_key(&self) -> Result<String, super::SandboxError> {
        awaken_agent_contract::collision_resistant_fingerprint_digest(&self.effect_id)
            .map(str::to_owned)
            .ok_or_else(|| {
                super::SandboxError::invalid(
                    "sandbox restoration effect must be canonical blake3 lowercase hex",
                )
            })
    }
}
