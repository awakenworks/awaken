//! Durable identity of the execution environment bound to a Session.
//!
//! Rebuildable process capabilities such as a container Hand belong to the
//! Runtime Host and are deliberately absent from this durable aggregate state.

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionEnvironmentState {
    #[default]
    Unmaterialized,
    Resident {
        binding: String,
        /// Stable identity of the create/adopt effect that last proved this
        /// binding. Legacy rows omit it and are upgraded on the next receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_id: Option<String>,
    },
}

impl SessionEnvironmentState {
    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding, .. } => Some(binding),
            Self::Unmaterialized => None,
        }
    }

    #[must_use]
    pub fn effect_id(&self) -> Option<&str> {
        match self {
            Self::Resident { effect_id, .. } => effect_id.as_deref(),
            Self::Unmaterialized => None,
        }
    }

    pub fn set_resident(&mut self, binding: impl Into<String>) {
        *self = Self::Resident {
            binding: binding.into(),
            effect_id: None,
        };
    }

    pub fn apply_receipt(&mut self, receipt: &SessionEnvironmentReceipt) {
        *self = Self::Resident {
            binding: receipt.binding.clone(),
            effect_id: Some(receipt.effect_id.clone()),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentEffectKind {
    Create,
    Adopt,
}

/// Secret-free evidence that one exact owner created or adopted the Session
/// environment. The durable binding is committed only after this receipt passes
/// the aggregate's realization fence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentReceipt {
    pub session_id: String,
    pub effect_id: String,
    pub kind: SessionEnvironmentEffectKind,
    pub binding: String,
    pub realization: Option<crate::SessionRealizationLease>,
    pub receipt_fingerprint: String,
}

impl SessionEnvironmentReceipt {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        kind: SessionEnvironmentEffectKind,
        binding: impl Into<String>,
        realization: Option<crate::SessionRealizationLease>,
    ) -> Self {
        let session_id = session_id.into();
        let binding = binding.into();
        let effect_id = crate::stable_fingerprint(&(
            "session-environment-v1",
            &session_id,
            kind,
            realization.as_ref().map(|lease| {
                (
                    lease.owner.as_str(),
                    lease.runtime_incarnation.as_str(),
                    lease.epoch,
                )
            }),
        ));
        let receipt_fingerprint =
            crate::stable_fingerprint(&(&session_id, &effect_id, kind, &binding, &realization));
        Self {
            session_id,
            effect_id,
            kind,
            binding,
            realization,
            receipt_fingerprint,
        }
    }

    pub fn verify(&self) -> Result<(), SessionEnvironmentReceiptError> {
        let expected = Self::new(
            self.session_id.clone(),
            self.kind,
            self.binding.clone(),
            self.realization.clone(),
        );
        if self == &expected {
            Ok(())
        } else {
            Err(SessionEnvironmentReceiptError::Mismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentReceiptError {
    #[error("Session environment receipt does not match its exact effect")]
    Mismatch,
}
