// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Project canonical Session realization failures into Worker claim outcomes.

use super::{HostError, HostErrorKind, execution_error, terminal_resolution_error, to_run_error};

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

/// Preserve a deterministic Host admission refusal, and every classified
/// environment provider failure, through the Session realization
/// classification the realization driver uses, so both paths reach the
/// same claim outcome. Other Host failures keep their retryable behavior.
pub fn map_host_resolution_error(error: HostError) -> crate::Error {
    if error.kind == HostErrorKind::BadRequest
        || awaken_session_realization_contract::is_environment_effect_code(&error.code)
    {
        map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                to_run_error(error),
            ),
        )
    } else {
        execution_error(error.to_string())
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
    match session_realization_worker_effect(disposition) {
        SessionRealizationWorkerEffect::Defer => crate::Error::ResolutionNotReady(message),
        SessionRealizationWorkerEffect::Relinquish => execution_error(message),
        SessionRealizationWorkerEffect::Absorb => terminal_resolution_error(message),
    }
}
