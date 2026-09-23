//! Project canonical Session realization failures into Worker claim outcomes.

use super::HostWorkerResolver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionRealizationWorkerEffect {
    /// Keep the claim parked while the Session Work slot is occupied.
    Defer,
    /// Relinquish the claim through the retryable execution-failure path.
    Relinquish,
    /// Settle the still-current claim with an absorbing Run failure.
    Absorb,
}

pub(super) const fn session_realization_worker_effect(
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

impl HostWorkerResolver {
    pub(super) fn map_session_realization_drive_error(
        error: awaken_session_realization_contract::SessionRealizationDriveError,
    ) -> awaken_dispatch::Error {
        match error {
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(error)
                if error.kind == awaken_session_error_contract::RunErrorKind::Unavailable =>
            {
                Self::execution_error(error.to_string())
            }
            awaken_session_realization_contract::SessionRealizationDriveError::Control(control) => {
                match session_realization_worker_effect(control.disposition()) {
                    SessionRealizationWorkerEffect::Defer => {
                        awaken_dispatch::Error::ResolutionNotReady(
                            "Session realization is not ready".into(),
                        )
                    }
                    SessionRealizationWorkerEffect::Relinquish => {
                        Self::execution_error(control.to_string())
                    }
                    SessionRealizationWorkerEffect::Absorb => {
                        Self::terminal_resolution_error(control.to_string())
                    }
                }
            }
            retryable @ awaken_session_realization_contract::SessionRealizationDriveError::DidNotConverge => {
                Self::execution_error(retryable.to_string())
            }
            error => Self::terminal_resolution_error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_realization_preserves_retryable_and_absorbing_failure_classes() {
        // Cause/effect graph: C1 the canonical driver classifies an effect as
        // Unavailable, C2 Control reports transient ownership/readiness, or C3
        // the phase is permanently invalid. Effects: C1/C2 relinquish or defer
        // the WorkQueue claim; only C3 becomes TerminalResolution. This preserves
        // the Session aggregate's already-persisted retry decision instead of
        // creating a second host-side policy.
        //
        // | Rule | driver cause | effect |
        // | R1 | unavailable effect | retryable Execution |
        // | R2 | Control unavailable/conflict | retryable Execution |
        // | R3 | Control not ready | ResolutionNotReady |
        // | R4 | permanent effect/invalid Control | TerminalResolution |
        let retryable_effect = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                awaken_session_error_contract::RunError::unavailable_classified(
                    "mcp_material_source_unavailable",
                    "materializer unavailable",
                ),
            ),
        );
        assert!(matches!(
            retryable_effect,
            awaken_dispatch::Error::Execution(_)
        ));

        let retryable_control = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "repository unavailable".into(),
                ),
            ),
        );
        assert!(matches!(
            retryable_control,
            awaken_dispatch::Error::Execution(_)
        ));

        let not_ready = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            ),
        );
        assert!(matches!(
            not_ready,
            awaken_dispatch::Error::ResolutionNotReady(_)
        ));

        for permanent in [
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                awaken_session_error_contract::RunError::classified(
                    "mcp_credential_revision_mismatch",
                    "invalid revision",
                ),
            ),
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    "invalid phase".into(),
                ),
            ),
        ] {
            assert!(matches!(
                HostWorkerResolver::map_session_realization_drive_error(permanent),
                awaken_dispatch::Error::TerminalResolution(_)
            ));
        }
    }
}
