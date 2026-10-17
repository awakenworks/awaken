// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Project canonical Session realization failures into Worker claim outcomes.

use super::{execution_error, terminal_resolution_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationWorkerEffect {
    /// Keep the claim parked while the Session Work slot is occupied.
    Defer,
    /// Relinquish the claim through the retryable execution-failure path.
    Relinquish,
    /// Settle the still-current claim with an absorbing Run failure.
    Absorb,
}

pub const fn session_realization_worker_effect(
    disposition: awaken_session_realization_contract::SessionRealizationControlDisposition,
) -> SessionRealizationWorkerEffect {
    match disposition {
        awaken_session_realization_contract::SessionRealizationControlDisposition::NotReady => {
            SessionRealizationWorkerEffect::Defer
        }
        awaken_session_realization_contract::SessionRealizationControlDisposition::Retryable => {
            SessionRealizationWorkerEffect::Relinquish
        }
        awaken_session_realization_contract::SessionRealizationControlDisposition::Terminal => {
            SessionRealizationWorkerEffect::Absorb
        }
    }
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn session_realization_control_disposition_projects_exact_worker_effect() {
    let selector = kani::any::<u8>() % 3;
    let (disposition, expected) = match selector {
        0 => (
            awaken_session_realization_contract::SessionRealizationControlDisposition::NotReady,
            SessionRealizationWorkerEffect::Defer,
        ),
        1 => (
            awaken_session_realization_contract::SessionRealizationControlDisposition::Retryable,
            SessionRealizationWorkerEffect::Relinquish,
        ),
        _ => (
            awaken_session_realization_contract::SessionRealizationControlDisposition::Terminal,
            SessionRealizationWorkerEffect::Absorb,
        ),
    };

    assert_eq!(session_realization_worker_effect(disposition), expected);
}

/// Project one admission or realization failure onto the claim outcome its
/// disposition names. The realization driver projects the same disposition
/// through [`map_session_realization_drive_error`], so both paths agree.
pub fn map_admission_error(error: impl Into<crate::AdmissionError>) -> crate::Error {
    let error = error.into();
    claim_outcome(error.claim_disposition(), error.to_string())
}

/// The claim outcome of one disposition, carrying the operator message.
pub fn claim_outcome(
    disposition: awaken_session_realization_contract::SessionRealizationControlDisposition,
    message: String,
) -> crate::Error {
    match session_realization_worker_effect(disposition) {
        SessionRealizationWorkerEffect::Defer => crate::Error::ResolutionNotReady(message),
        SessionRealizationWorkerEffect::Relinquish => execution_error(message),
        SessionRealizationWorkerEffect::Absorb => terminal_resolution_error(message),
    }
}

pub fn map_session_realization_drive_error(
    error: awaken_session_realization_contract::SessionRealizationDriveError,
) -> crate::Error {
    use awaken_session_realization_contract::SessionRealizationDriveError as Drive;
    // An effect and a Control failure share one Worker projection; a
    // capacity refusal is NotReady, the request-local deferral (ADR-0092).
    let (disposition, message) = match error {
        Drive::Effect(error) => (
            awaken_session_realization_contract::effect_failure_disposition(&error),
            error.to_string(),
        ),
        Drive::Control(control) => (control.disposition(), control.to_string()),
        retryable @ Drive::DidNotConverge => {
            return execution_error(retryable.to_string());
        }
    };
    claim_outcome(disposition, message)
}
