//! Aggregate-derived two-stage Sandbox disposal authorization.

use serde::{Deserialize, Serialize};

use super::{SandboxEffectFence, SandboxError};

/// Immutable half of an aggregate-derived Sandbox disposal authorization.
///
/// Both Session terminal cleanup and checkpoint-source release project this
/// same value. It is not a second lifecycle state: it only preserves the exact
/// prepared effect and fingerprint needed to derive one stable physical
/// operation across a later lifecycle takeover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDisposalPreparation {
    prepared_effect_fence: SandboxEffectFence,
    preparation_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxDisposalPreparationWire {
    prepared_effect_fence: SandboxEffectFence,
    preparation_fingerprint: String,
}

impl<'de> Deserialize<'de> for SandboxDisposalPreparation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SandboxDisposalPreparationWire::deserialize(deserializer)?;
        Self::new(wire.prepared_effect_fence, wire.preparation_fingerprint)
            .map_err(serde::de::Error::custom)
    }
}

impl SandboxDisposalPreparation {
    pub fn new(
        prepared_effect_fence: SandboxEffectFence,
        preparation_fingerprint: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        let preparation = Self {
            prepared_effect_fence,
            preparation_fingerprint: preparation_fingerprint.into(),
        };
        preparation.operation_id()?;
        Ok(preparation)
    }

    /// Canonical physical-disposal identity. The successor is deliberately
    /// excluded so renewal, reassignment, and cross-lifecycle takeover all
    /// resume the same provider operation instead of minting another gate.
    pub fn operation_id(&self) -> Result<String, SandboxError> {
        self.prepared_effect_fence.validate_identity()?;
        if self.preparation_fingerprint.trim().is_empty() {
            return Err(SandboxError::new(
                "Sandbox disposal preparation requires a fingerprint",
            ));
        }
        Ok(awaken_agent_contract::stable_fingerprint(&(
            "sandbox-disposal-authorization-v1",
            self.prepared_effect_fence.operation_id.as_str(),
            self.prepared_effect_fence.owner.as_str(),
            self.prepared_effect_fence.runtime_incarnation.as_str(),
            self.prepared_effect_fence.epoch,
            self.prepared_effect_fence.expires_at_unix_ms,
            self.preparation_fingerprint.as_str(),
        )))
    }

    pub fn authorize(
        &self,
        effect_fence: SandboxEffectFence,
    ) -> Result<SandboxDisposalAuthorization, SandboxError> {
        SandboxDisposalAuthorization::new(
            self.prepared_effect_fence.clone(),
            effect_fence,
            self.preparation_fingerprint.clone(),
        )
    }

    #[must_use]
    pub const fn prepared_effect_fence(&self) -> &SandboxEffectFence {
        &self.prepared_effect_fence
    }

    #[must_use]
    pub fn preparation_fingerprint(&self) -> &str {
        &self.preparation_fingerprint
    }
}

/// Aggregate-derived authorization for the physical half of a two-stage
/// Sandbox disposal.
///
/// `prepared_effect_fence` and `preparation_fingerprint` are the one canonical
/// [`SandboxDisposalPreparation`]. `effect_fence` is the current live
/// realization allowed to finish deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDisposalAuthorization {
    prepared_effect_fence: SandboxEffectFence,
    effect_fence: SandboxEffectFence,
    preparation_fingerprint: String,
}

/// Deserialize-only transport shape. It owns no decisions: every decoded
/// value is immediately joined through [`SandboxDisposalAuthorization::new`],
/// the same canonical constructor used by in-process callers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxDisposalAuthorizationWire {
    prepared_effect_fence: SandboxEffectFence,
    effect_fence: SandboxEffectFence,
    preparation_fingerprint: String,
}

impl<'de> Deserialize<'de> for SandboxDisposalAuthorization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SandboxDisposalAuthorizationWire::deserialize(deserializer)?;
        Self::new(
            wire.prepared_effect_fence,
            wire.effect_fence,
            wire.preparation_fingerprint,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SandboxDisposalAuthorization {
    pub fn new(
        prepared_effect_fence: SandboxEffectFence,
        effect_fence: SandboxEffectFence,
        preparation_fingerprint: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        let authorization = Self {
            prepared_effect_fence,
            effect_fence,
            preparation_fingerprint: preparation_fingerprint.into(),
        };
        authorization.validate()?;
        Ok(authorization)
    }

    /// Revalidate a typed value at a provider boundary. This is deliberately
    /// the same owner used by construction and deserialization so adapters do
    /// not grow parallel A/fingerprint/B admission rules.
    pub fn validate(&self) -> Result<(), SandboxError> {
        let preparation = SandboxDisposalPreparation::new(
            self.prepared_effect_fence.clone(),
            self.preparation_fingerprint.clone(),
        )?;
        let expected_operation_id = preparation.operation_id()?;
        self.effect_fence.validate_identity()?;
        if self.effect_fence.operation_id != expected_operation_id {
            return Err(SandboxError::new(
                "Sandbox physical disposal operation does not bind its durable preparation",
            ));
        }
        if !self
            .prepared_effect_fence
            .authorizes_successor(&self.effect_fence)
        {
            return Err(SandboxError::new(
                "Sandbox physical disposal is not an authorized realization successor",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn prepared_effect_fence(&self) -> &SandboxEffectFence {
        &self.prepared_effect_fence
    }

    #[must_use]
    pub const fn effect_fence(&self) -> &SandboxEffectFence {
        &self.effect_fence
    }

    #[must_use]
    pub fn preparation_fingerprint(&self) -> &str {
        &self.preparation_fingerprint
    }

    #[must_use]
    pub fn preparation(&self) -> SandboxDisposalPreparation {
        SandboxDisposalPreparation {
            prepared_effect_fence: self.prepared_effect_fence.clone(),
            preparation_fingerprint: self.preparation_fingerprint.clone(),
        }
    }
}
