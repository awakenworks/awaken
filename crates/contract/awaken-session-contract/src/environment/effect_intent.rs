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

/// Result of checking one Environment effect against the durable Session root.
/// `Unowned` is reserved for process-local threads without a Managed aggregate;
/// `AlreadyApplied` is the response-loss recovery path and must never re-run a
/// provider create or replacement effect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentEffectAuthorization {
    Unowned,
    Authorized,
    AlreadyApplied { binding: String },
}

/// Secret-free, binding-independent intent authorized before provider I/O.
/// Its effect id is the sole operation identity later carried by the physical
/// provider fence and the completed [`super::SessionEnvironmentReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentEffectIntent {
    session_id: String,
    effect_id: String,
    kind: SessionEnvironmentEffectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment_fingerprint: Option<String>,
    realization: Option<crate::SessionRealizationLease>,
}

impl SessionEnvironmentEffectIntent {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        kind: SessionEnvironmentEffectKind,
        realization: Option<crate::SessionRealizationLease>,
    ) -> Self {
        let mut intent = Self {
            session_id: session_id.into(),
            effect_id: String::new(),
            kind,
            source_binding: None,
            environment_fingerprint: None,
            realization,
        };
        intent.refresh_effect_id();
        intent
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
    pub fn session_id(&self) -> &str {
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
    pub fn realization(&self) -> Option<&crate::SessionRealizationLease> {
        self.realization.as_ref()
    }

    pub(super) fn with_receipt_context(
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
        realization: Option<crate::SessionRealizationLease>,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        let intent = Self {
            session_id,
            effect_id,
            kind,
            source_binding,
            environment_fingerprint,
            realization,
        };
        intent.verify()?;
        Ok(intent)
    }

    fn refresh_effect_id(&mut self) {
        self.effect_id = crate::stable_fingerprint(&(
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
        expected.refresh_effect_id();
        if self == &expected {
            Ok(())
        } else {
            Err(SessionEnvironmentReceiptError::Mismatch)
        }
    }
}
