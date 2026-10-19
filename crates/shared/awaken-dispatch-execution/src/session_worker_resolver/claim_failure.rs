// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Project canonical Session realization failures into Worker claim outcomes.

use super::{execution_error, terminal_resolution_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationWorkerEffect {
    /// Keep the claim parked while the Session Work slot is occupied.
    Defer,
    /// Relinquish the claim through the failed-attempt path, which spends one
    /// attempt of the dispatch retry budget (ADR-0015).
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
    claim_outcome(error.claim_disposition(), error.to_string(), None)
}

/// The claim outcome of one disposition, carrying the operator message. A
/// deferral keeps `retry_after`, the waited-on owner's bound on when to look
/// again; a failed or absorbed attempt has no use for it.
pub fn claim_outcome(
    disposition: awaken_session_realization_contract::SessionRealizationControlDisposition,
    message: String,
    retry_after: Option<std::time::Duration>,
) -> crate::Error {
    match session_realization_worker_effect(disposition) {
        SessionRealizationWorkerEffect::Defer => {
            crate::Error::ResolutionNotReady(crate::NotReady::after(message, retry_after))
        }
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
    let (disposition, message, retry_after) = match error {
        Drive::Effect(error) => (
            awaken_session_realization_contract::effect_failure_disposition(error.run_error()),
            error.to_string(),
            error.retry_after(),
        ),
        Drive::Control(control) => (control.disposition(), control.to_string(), None),
        retryable @ Drive::DidNotConverge => {
            return execution_error(retryable.to_string());
        }
    };
    claim_outcome(disposition, message, retry_after)
}

/// When a deferred claim is due again: the pool's backoff, shortened to the
/// waited-on owner's hint, so a deferral never sleeps past either.
#[must_use]
pub(crate) fn deferral_retry_after(
    backoff: std::time::Duration,
    hint: Option<std::time::Duration>,
) -> std::time::Duration {
    hint.map_or(backoff, |hint| hint.min(backoff))
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn a_deferral_is_due_no_later_than_its_owners_hint_or_the_backoff() {
    let backoff = std::time::Duration::from_millis(kani::any::<u64>());
    let hinted: bool = kani::any();
    let hint = hinted.then(|| std::time::Duration::from_millis(kani::any::<u64>()));
    let due = deferral_retry_after(backoff, hint);
    assert!(due <= backoff);
    match hint {
        // A provision deferred before its deadline is observed again no later
        // than that deadline.
        Some(hint) => assert!(due <= hint && (due == hint || due == backoff)),
        None => assert_eq!(due, backoff),
    }
}

#[cfg(test)]
mod retry_hint_tests {
    use super::*;
    use awaken_session_realization_contract::{
        ENVIRONMENT_PROVISIONING_CODE, SessionRealizationDriveError,
        SessionRealizationEffectFailure,
    };
    use awaken_session_types::RunError;
    use std::time::Duration;

    #[test]
    fn only_driver_deferrals_retain_a_provider_retry_hint() {
        // W1-W4 decision table. C1 original class is NotReady/Retryable/Terminal;
        // C2 hint is absent/zero/positive (including a misleading hint on a
        // failure); C3 pool backoff is shorter or longer than a real hint.
        // W1 NotReady => Defer and exact hint; W2 Retryable => failed-attempt
        // Retry with no hint delay; W3 Terminal => absorbing resolution;
        // W4 deferred wait stays <= original hint and existing pool backoff.
        let rows = [
            (
                (|| RunError::unavailable_classified(ENVIRONMENT_PROVISIONING_CODE, "original"))
                    as fn() -> RunError,
                "defer",
            ),
            (|| RunError::unavailable("original"), "retry"),
            (|| RunError::conflict("original"), "retry"),
            (|| RunError::classified("permanent", "original"), "absorb"),
        ];
        for (error, class) in rows {
            for hint in [None, Some(Duration::ZERO), Some(Duration::from_secs(6))] {
                let projected =
                    map_session_realization_drive_error(SessionRealizationDriveError::Effect(
                        SessionRealizationEffectFailure::new(error(), hint),
                    ));
                match (class, projected) {
                    ("defer", crate::Error::ResolutionNotReady(wait)) => {
                        assert_eq!(wait.retry_after(), hint, "W1");
                        for backoff in [Duration::from_secs(2), Duration::from_secs(10)] {
                            let due = deferral_retry_after(backoff, wait.retry_after());
                            assert!(due <= backoff, "W4");
                            if let Some(hint) = hint {
                                assert!(due <= hint, "W4");
                            }
                        }
                    }
                    ("retry", error @ crate::Error::Execution(_)) => assert_eq!(
                        error.claim_release(),
                        crate::error::ClaimRelease::Retry,
                        "W2 spends an attempt"
                    ),
                    ("absorb", error @ crate::Error::TerminalResolution(_)) => assert!(
                        error.is_terminal_resolution(),
                        "W3 absorbs before execution"
                    ),
                    (class, actual) => panic!("{class} had wrong projection {actual:?}"),
                }
            }
        }
    }
}
