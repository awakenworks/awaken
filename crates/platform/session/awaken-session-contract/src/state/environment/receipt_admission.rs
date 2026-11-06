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

use super::SessionEnvironmentRejection;

/// A refused suspend/restore law. The step refusal is the Environment
/// decision's [`SessionEnvironmentRejection`], carried unchanged; the other
/// variants are evidence only the exact data law can judge.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentTransitionError {
    #[error(transparent)]
    Rejected(#[from] SessionEnvironmentRejection),
    #[error("Session environment receipt does not match its exact effect")]
    ReceiptMismatch,
    #[error("Session environment checkpoint is expired")]
    CheckpointExpired,
    #[error("Session environment checkpoint policy is invalid: {0}")]
    InvalidCheckpointPolicy(&'static str),
}

/// A refused Environment intent or receipt. The step refusal is the
/// Environment decision's [`SessionEnvironmentRejection`]; a lost lease keeps
/// its one name, `RealizationStale`, which the realization boundary reads as
/// lost authority. The other variants are exact-evidence refusals.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentReceiptError {
    #[error(transparent)]
    Rejected(SessionEnvironmentRejection),
    #[error("Session environment receipt does not match its exact effect")]
    Mismatch,
    #[error("Session environment receipt targets another frozen Environment")]
    EnvironmentMismatch,
    #[error("Session environment effect was fenced by another realization owner")]
    RealizationStale,
    #[error("Session environment Resource reservation does not monotonically extend one substrate")]
    InvalidReservation,
    #[error(
        "Session environment Resource reservation does not match the aggregate pending transition"
    )]
    ResourceTransitionMismatch,
    #[error("Session original materialization history is unknown")]
    MaterializationUnknown,
}

impl From<SessionEnvironmentRejection> for SessionEnvironmentReceiptError {
    fn from(rejection: SessionEnvironmentRejection) -> Self {
        match rejection {
            SessionEnvironmentRejection::LeaseNotCurrent => Self::RealizationStale,
            rejection => Self::Rejected(rejection),
        }
    }
}

/// Closed disposition for an Environment intent/receipt rejected by current
/// aggregate truth. Concurrent lease, lifecycle, and Resource-transition
/// movement is retryable; malformed or causally invalid evidence is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentReceiptRecoveryAction {
    Retry,
    Reject,
}

impl SessionEnvironmentRejection {
    /// Whether the refused event may succeed once the Session moves on: a
    /// lifecycle, lease or materialization that may still change retries;
    /// a causal refusal is final for the same evidence.
    #[must_use]
    pub const fn recovery_action(self) -> SessionEnvironmentReceiptRecoveryAction {
        match self {
            Self::Terminal
            | Self::NotTerminal
            | Self::ContinuationInProgress
            | Self::ActivitiesOpen
            | Self::WrongStep
            | Self::LeaseNotCurrent
            | Self::CheckpointLive
            | Self::MaterializationPending
            | Self::MemoryEvidenceUnproven => SessionEnvironmentReceiptRecoveryAction::Retry,
            Self::NotGenerated
            | Self::RequiresResident
            | Self::InvalidTransition
            | Self::GenerationExpired
            | Self::InconsistentEvidence
            | Self::UnsupportedContinuationTopology
            | Self::RebuildSourceUnavailable => SessionEnvironmentReceiptRecoveryAction::Reject,
        }
    }
}

impl SessionEnvironmentTransitionError {
    /// Whether the refused law may succeed once the Session moves on: a step
    /// refusal keeps its own classification; evidence only the data law
    /// judges (a mismatched receipt, an expired checkpoint, an invalid policy)
    /// is final for the same evidence.
    #[must_use]
    pub const fn recovery_action(&self) -> SessionEnvironmentReceiptRecoveryAction {
        match self {
            Self::Rejected(rejection) => rejection.recovery_action(),
            Self::ReceiptMismatch | Self::CheckpointExpired | Self::InvalidCheckpointPolicy(_) => {
                SessionEnvironmentReceiptRecoveryAction::Reject
            }
        }
    }
}

impl SessionEnvironmentReceiptError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionEnvironmentReceiptRecoveryAction {
        match self {
            Self::Rejected(rejection) => rejection.recovery_action(),
            Self::RealizationStale
            | Self::ResourceTransitionMismatch
            | Self::MaterializationUnknown => SessionEnvironmentReceiptRecoveryAction::Retry,
            Self::Mismatch | Self::EnvironmentMismatch | Self::InvalidReservation => {
                SessionEnvironmentReceiptRecoveryAction::Reject
            }
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
