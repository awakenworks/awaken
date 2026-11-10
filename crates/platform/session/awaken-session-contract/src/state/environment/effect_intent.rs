// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the one provider-independent Session Environment effect intent, stable
// @formal reason: operation identity, and closed authorization result. Dynamic path: the parent aggregate
// @formal reason: authorizes and commits exact receipts while providers carry this immutable intent across
// @formal reason: I/O, so no adapter creates a second Environment operation identity or authorization
// @formal reason: path; this is a product-requirement classification, not a direct formal-proof claim.
//! Provider-independent Session Environment effect intent.
//!
//! The parent aggregate authorizes and commits receipts. This module owns the
//! one stable, binding-independent operation identity carried across provider
//! I/O and its closed authorization result.

use super::SessionEnvironmentReceiptError;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentEffectKind {
    Create,
    Adopt,
    /// Materialize the exact checkpoint target already requested by the root.
    /// Its logical operation id is reused; the intent's lease records the
    /// target's first physical admission, independently of later delivery.
    Restore {
        operation: Box<super::SessionEnvironmentOperation>,
        checkpoint_id: String,
    },
    /// Replace one unavailable substrate after the exact prior binding and
    /// immutable generation have been fenced by recovery policy.
    Rebuild {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_generation_id: Option<String>,
    },
    /// Write-ahead reservation of provider-owned paths for one exact Resource
    /// transition. The transition fingerprint binds both generations; durable
    /// Resource pending→active remains the completion authority.
    ResourceProjectionReservation {
        transition_fingerprint: String,
    },
}

impl SessionEnvironmentEffectKind {
    /// How this effect relates the Environment to its binding, in the
    /// transition function's vocabulary.
    #[must_use]
    pub const fn materialization_kind(&self) -> super::MaterializationKind {
        match self {
            Self::Create => super::MaterializationKind::Create,
            Self::Adopt => super::MaterializationKind::Adopt,
            Self::Rebuild { .. } => super::MaterializationKind::Rebuild,
            Self::Restore { .. } => super::MaterializationKind::Restore,
            Self::ResourceProjectionReservation { .. } => {
                super::MaterializationKind::ReserveResources
            }
        }
    }
}

/// Result of checking one Environment effect against the durable Session root.
/// `Unowned` is reserved for process-local threads without a Managed aggregate;
/// `AlreadyApplied` is the response-loss recovery path and must never re-run a
/// provider create or replacement effect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentEffectAuthorization {
    Unowned {
        intent: Box<SessionEnvironmentEffectIntent>,
    },
    Authorized {
        intent: Box<SessionEnvironmentEffectIntent>,
    },
    AlreadyApplied {
        binding: String,
        intent: Box<SessionEnvironmentEffectIntent>,
    },
}

impl SessionEnvironmentEffectAuthorization {
    /// Admit a process-local ordinary thread whose owner has established
    /// absence of a Managed Session root. The same aggregate decision owns
    /// this row and all durable materialization rows; a claimed realization
    /// can never fall back to Unowned merely because its root disappeared.
    pub fn admit_unowned(
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        intent.verify()?;
        super::transition_environment(
            super::SessionEnvironmentFacts::project_unowned(intent),
            super::SessionEnvironmentEvent::AdmitMaterialization {
                kind: intent.kind().materialization_kind(),
                already_applied: false,
            },
        )?;
        Ok(Self::Unowned {
            intent: Box::new(intent.clone()),
        })
    }

    /// A renewed request can receive the earlier admitted value, but never
    /// another Session or operation. The saved expiry is retained verbatim.
    pub fn verify_for(
        &self,
        requested: &SessionEnvironmentEffectIntent,
    ) -> Result<(), SessionEnvironmentReceiptError> {
        let original = self.intent();
        original.verify()?;
        if original.effect_id() != requested.effect_id()
            || original.session_id() != requested.session_id()
        {
            return Err(SessionEnvironmentReceiptError::Mismatch);
        }
        Ok(())
    }

    /// The exact value returned by the aggregate, including first expiry.
    #[must_use]
    pub fn intent(&self) -> &SessionEnvironmentEffectIntent {
        match self {
            Self::Unowned { intent }
            | Self::Authorized { intent }
            | Self::AlreadyApplied { intent, .. } => intent,
        }
    }
}

/// Secret-free, binding-independent intent authorized before provider I/O.
/// Its effect id is the sole operation identity later carried by the physical
/// provider fence and the completed [`super::SessionEnvironmentReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentEffectIntent {
    session_id: crate::state::SessionId,
    effect_id: String,
    kind: SessionEnvironmentEffectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment_fingerprint: Option<String>,
    realization: Option<crate::state::SessionRealizationLease>,
    /// A Rebuild's source evidence: an admission precondition the aggregate
    /// reads, never an identity coordinate. Rows written before it existed
    /// decode as `None`, which the aggregate reads as an open source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rebuild_source: Option<super::RebuildSourceEvidence>,
}

impl SessionEnvironmentEffectIntent {
    #[must_use]
    pub fn new(
        session_id: impl Into<crate::state::SessionId>,
        kind: SessionEnvironmentEffectKind,
        realization: Option<crate::state::SessionRealizationLease>,
    ) -> Self {
        let mut intent = Self {
            session_id: session_id.into(),
            effect_id: String::new(),
            kind,
            source_binding: None,
            environment_fingerprint: None,
            realization,
            rebuild_source: None,
        };
        intent.refresh_effect_id();
        intent
    }

    /// Attach the realization's evidence about the source a Rebuild replaces.
    /// The effect identity is unchanged: the evidence decides admission only.
    #[must_use]
    pub fn with_rebuild_source(mut self, evidence: super::RebuildSourceEvidence) -> Self {
        self.rebuild_source = Some(evidence);
        self
    }

    #[must_use]
    pub fn for_environment(mut self, environment_fingerprint: impl Into<String>) -> Self {
        self.environment_fingerprint = Some(environment_fingerprint.into());
        self.refresh_effect_id();
        self
    }

    #[must_use]
    pub fn from_binding(mut self, source_binding: impl Into<String>) -> Self {
        self.source_binding = Some(source_binding.into());
        self.refresh_effect_id();
        self
    }

    #[must_use]
    pub fn session_id(&self) -> &crate::state::SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    #[must_use]
    pub fn kind(&self) -> &SessionEnvironmentEffectKind {
        &self.kind
    }

    #[must_use]
    pub fn source_binding(&self) -> Option<&str> {
        self.source_binding.as_deref()
    }

    #[must_use]
    pub fn environment_fingerprint(&self) -> Option<&str> {
        self.environment_fingerprint.as_deref()
    }

    #[must_use]
    pub fn realization(&self) -> Option<&crate::state::SessionRealizationLease> {
        self.realization.as_ref()
    }

    /// The realization's evidence about the source a Rebuild replaces.
    #[must_use]
    pub fn rebuild_source(&self) -> Option<&super::RebuildSourceEvidence> {
        self.rebuild_source.as_ref()
    }

    /// Compile the complete frozen input coordinates in one identity refresh.
    /// This constructs a request; only the aggregate can admit it before I/O.
    #[must_use]
    pub fn for_frozen_coordinates(
        mut self,
        source_binding: Option<String>,
        environment_fingerprint: Option<String>,
    ) -> Self {
        self.source_binding = source_binding;
        self.environment_fingerprint = environment_fingerprint;
        self.refresh_effect_id();
        self
    }

    pub(super) fn from_persisted_receipt(
        session_id: String,
        effect_id: String,
        kind: SessionEnvironmentEffectKind,
        source_binding: Option<String>,
        environment_fingerprint: Option<String>,
        realization: Option<crate::state::SessionRealizationLease>,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        let intent = Self {
            session_id: crate::state::SessionId::from(session_id),
            effect_id,
            kind,
            source_binding,
            environment_fingerprint,
            realization,
            rebuild_source: None,
        };
        intent.verify()?;
        Ok(intent)
    }

    fn refresh_effect_id(&mut self) {
        if let SessionEnvironmentEffectKind::Restore { operation, .. } = &self.kind {
            self.effect_id = operation.effect_id().to_owned();
            return;
        }
        self.effect_id = crate::state::stable_fingerprint(&(
            "session-environment-v1",
            &self.session_id,
            &self.kind,
            self.source_binding.as_deref(),
            self.environment_fingerprint.as_deref(),
            self.realization.as_ref().map(|lease| {
                (
                    lease.owner.as_str(),
                    lease.runtime_incarnation.as_str(),
                    lease.epoch,
                )
            }),
        ));
    }

    pub fn verify(&self) -> Result<(), SessionEnvironmentReceiptError> {
        let mut expected = Self::new(
            self.session_id.clone(),
            self.kind.clone(),
            self.realization.clone(),
        );
        expected.source_binding.clone_from(&self.source_binding);
        expected
            .environment_fingerprint
            .clone_from(&self.environment_fingerprint);
        expected.rebuild_source = self.rebuild_source;
        expected.refresh_effect_id();
        if self == &expected {
            Ok(())
        } else {
            Err(SessionEnvironmentReceiptError::Mismatch)
        }
    }
}

#[cfg(test)]
mod tests;
