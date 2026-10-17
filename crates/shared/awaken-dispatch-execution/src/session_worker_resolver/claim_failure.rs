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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::SandboxError;
    use awaken_session_realization::realization_error::{
        AuthorityLoss, PendingState, ProjectionConflict, RealizationError, RealizationLock,
    };

    /// One table row: rule, the failure it builds, and the expected outcome.
    type Row = (&'static str, fn() -> RealizationError, &'static str);

    fn outcome(error: &crate::Error) -> &'static str {
        match error {
            crate::Error::ResolutionNotReady(_) => "defer",
            crate::Error::Execution(_) => "relinquish",
            crate::Error::TerminalResolution(_) => "absorb",
            other => panic!("unexpected claim outcome {other:?}"),
        }
    }

    #[test]
    fn one_realization_failure_reaches_one_outcome_on_every_path() {
        // Decision table: cause = the realization failure class; paths = P1
        // the claim resolver (RealizationError -> AdmissionError -> claim),
        // P2 the projection synchronizer (its one Run projection) and P3 the
        // realization driver (the synchronizer's Run error as a drive error).
        // Effect: the claim outcome, identical on all three paths.
        // | Class                                   | outcome    |
        // | transient provider / authority / pending / poisoned / stale Run | relinquish |
        // | provider capacity                       | defer      |
        // | permanent provider / projection conflict / invariant / invalid | absorb |
        let rows: [Row; 10] = [
            (
                "transient provider",
                || SandboxError::timeout("t").into(),
                "relinquish",
            ),
            (
                "capacity",
                || SandboxError::capacity_unavailable("q").into(),
                "defer",
            ),
            (
                "permanent provider",
                || SandboxError::unauthorized("u").into(),
                "absorb",
            ),
            (
                "authority lost",
                || RealizationError::authority_lost(AuthorityLoss::ClaimStale, "stale claim"),
                "relinquish",
            ),
            (
                "pending",
                || RealizationError::pending(PendingState::TerminalHandNotQuiescent, "busy"),
                "relinquish",
            ),
            (
                "projection conflict",
                || {
                    RealizationError::conflict(
                        ProjectionConflict::PublicationConflict,
                        "foreign publication",
                    )
                },
                "absorb",
            ),
            (
                "invariant",
                || RealizationError::invariant("broken"),
                "absorb",
            ),
            (
                "invalid request",
                || RealizationError::invalid_request("bad"),
                "absorb",
            ),
            (
                "poisoned registry",
                || RealizationError::Poisoned(RealizationLock::InstalledSessions),
                "relinquish",
            ),
            (
                "stale Run lease",
                || {
                    awaken_session_types::RunError::unavailable_classified(
                        "session_realization_stale",
                        "stale",
                    )
                    .into()
                },
                "relinquish",
            ),
        ];
        for (rule, failure, expected) in rows {
            let claim = map_admission_error(failure());
            let synchronizer = awaken_session_types::RunError::from(failure());
            let driver = map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::from(failure()),
                ),
            );
            assert_eq!(outcome(&claim), expected, "{rule} P1 claim");
            assert_eq!(
                session_realization_worker_effect(
                    awaken_session_realization_contract::effect_failure_disposition(&synchronizer)
                ),
                session_realization_worker_effect(failure().disposition().contract_class()),
                "{rule} P2 synchronizer"
            );
            assert_eq!(outcome(&driver), expected, "{rule} P3 driver");
        }
    }
}
