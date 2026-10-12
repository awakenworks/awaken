// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the closed error and recovery disposition for exact Session Environment
// @formal reason: receipts plus the provider-neutral monotonic reservation check. Dynamic path: the parent
// @formal reason: aggregate remains the only lifecycle authority, while retryable concurrent movement and
// @formal reason: malformed evidence are classified without a second receipt store or transition path;
// @formal reason: this is a product-requirement classification, not a direct formal-proof claim.
//! Closed receipt rejection and recovery policy for Session Environments.
//!
//! The parent aggregate owns lifecycle transitions. This module owns only the
//! exact classification of rejected receipt evidence and the provider-neutral
//! monotonic reservation check reused by those transitions.

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentTransitionError {
    #[error("Session environment is not a generated resident environment")]
    NotResident,
    #[error("Session environment is not suspending")]
    NotSuspending,
    #[error("Session environment is not hibernated")]
    NotHibernated,
    #[error("Session environment is not restoring")]
    NotRestoring,
    #[error("Session environment operation is in the wrong phase")]
    WrongPhase,
    #[error("Session environment receipt does not match its exact effect")]
    ReceiptMismatch,
    #[error("Session environment checkpoint is expired")]
    CheckpointExpired,
    #[error("Session environment checkpoint policy is invalid: {0}")]
    InvalidCheckpointPolicy(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentReceiptError {
    #[error("Session environment receipt does not match its exact effect")]
    Mismatch,
    #[error("Session environment receipt targets another frozen Environment")]
    EnvironmentMismatch,
    #[error("Session environment effect was fenced by another realization owner")]
    RealizationStale,
    #[error("Session environment receipt is not valid in the current lifecycle phase")]
    WrongPhase,
    #[error("Session environment receipt requires a resident source binding")]
    RequiresResident,
    #[error("Session environment receipt does not prove an allowed state transition")]
    InvalidTransition,
    #[error("Session environment Resource reservation does not monotonically extend one substrate")]
    InvalidReservation,
    #[error(
        "Session environment Resource reservation does not match the aggregate pending transition"
    )]
    ResourceTransitionMismatch,
    #[error("Session original materialization history is unknown")]
    MaterializationUnknown,
    #[error("Session materialization remains pending its exact original effect and source closure")]
    MaterializationPending,
}

/// Closed disposition for an Environment intent/receipt rejected by current
/// aggregate truth. Concurrent lease, lifecycle, and Resource-transition
/// movement is retryable; malformed or causally invalid evidence is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentReceiptRecoveryAction {
    Retry,
    Reject,
}

impl SessionEnvironmentReceiptError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionEnvironmentReceiptRecoveryAction {
        match self {
            Self::RealizationStale
            | Self::WrongPhase
            | Self::ResourceTransitionMismatch
            | Self::MaterializationUnknown
            | Self::MaterializationPending => SessionEnvironmentReceiptRecoveryAction::Retry,
            Self::Mismatch
            | Self::EnvironmentMismatch
            | Self::RequiresResident
            | Self::InvalidTransition
            | Self::InvalidReservation => SessionEnvironmentReceiptRecoveryAction::Reject,
        }
    }
}

pub(super) fn reservation_binding_is_monotonic(
    current: &str,
    next: &str,
) -> Result<bool, SessionEnvironmentReceiptError> {
    let current = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(current)
        .map_err(|_| SessionEnvironmentReceiptError::InvalidReservation)?;
    let next = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(next)
        .map_err(|_| SessionEnvironmentReceiptError::InvalidReservation)?;
    Ok(current.owned_paths_are_monotonic_to(&next))
}
