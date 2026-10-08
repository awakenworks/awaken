//! Why a committed prefix cannot be projected.

/// A committed prefix that the timeline cannot project without inventing
/// public facts. Every variant fails the whole candidate closed; the caller
/// keeps its previously published projection.
///
/// Consumer: the protocol adapter, which reports `Corrupt` as an internal
/// Runtime fault and `RecoveryRequired` as the recovery-required refusal, each
/// with the same message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionError {
    /// Committed evidence is malformed or contradicts itself.
    #[error("{0}")]
    Corrupt(String),
    /// Committed evidence is damaged in a way only an interrupt of the
    /// Session settles.
    #[error("{0}")]
    RecoveryRequired(String),
}

impl ProjectionError {
    #[must_use]
    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    #[must_use]
    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::RecoveryRequired(message.into())
    }
}
