// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
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

impl HostWorkerResolver {
    /// Preserve a deterministic Host admission refusal, and every classified
    /// environment provider failure, through the Session realization
    /// classification the realization driver uses, so both paths reach the
    /// same claim outcome. Other Host failures keep their retryable behavior.
    pub(super) fn map_host_resolution_error(
        error: crate::HostError,
    ) -> awaken_dispatch_execution::Error {
        if error.kind == crate::HostErrorKind::BadRequest
            || awaken_session_realization_contract::is_environment_effect_code(&error.code)
        {
            Self::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    crate::managed_adapter_error::to_run_error(error),
                ),
            )
        } else {
            Self::execution_error(error.to_string())
        }
    }

    pub(super) fn map_session_realization_drive_error(
        error: awaken_session_realization_contract::SessionRealizationDriveError,
    ) -> awaken_dispatch_execution::Error {
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
                return Self::execution_error(retryable.to_string());
            }
        };
        match session_realization_worker_effect(disposition) {
            SessionRealizationWorkerEffect::Defer => {
                awaken_dispatch_execution::Error::ResolutionNotReady(message)
            }
            SessionRealizationWorkerEffect::Relinquish => Self::execution_error(message),
            SessionRealizationWorkerEffect::Absorb => Self::terminal_resolution_error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resolution_preserves_admission_and_retry_classes_by_kind() {
        // Cause/effect table: H1 deterministic BadRequest -> the existing
        // absorbing Session effect; H2-H4 Unavailable/Internal/Conflict -> the
        // existing claim-local retry. Every row uses identical detail text so
        // an operator message cannot become a second classification policy.
        for (error, terminal) in [
            (crate::HostError::bad_request("same detail"), true),
            (crate::HostError::unavailable("same detail"), false),
            (crate::HostError::internal("same detail"), false),
            (crate::HostError::conflict("same detail"), false),
        ] {
            let actual = HostWorkerResolver::map_host_resolution_error(error);
            assert_eq!(actual.is_terminal_resolution(), terminal);
            if !terminal {
                assert!(matches!(
                    actual,
                    awaken_dispatch_execution::Error::Execution(_)
                ));
            }
        }
    }

    fn claim_outcome(error: &awaken_dispatch_execution::Error) -> &'static str {
        match error {
            awaken_dispatch_execution::Error::ResolutionNotReady(_) => "defer",
            awaken_dispatch_execution::Error::Execution(_) => "relinquish",
            awaken_dispatch_execution::Error::TerminalResolution(_) => "absorb",
            other => panic!("unexpected claim outcome {other:?}"),
        }
    }

    #[test]
    fn sandbox_failure_class_reaches_the_same_claim_outcome_on_both_paths() {
        // End-to-end decision table (ADR-0092 step 0): SandboxError class x
        // path -> claim outcome and Session state. Causes: C1 the provider's
        // SandboxError variant (its Classify disposition, or the capacity
        // class); C2 the path that observes it: P1 the realization driver
        // (synchronizer -> RunError -> FailSessionRealization + drive error)
        // or P2 the claim resolver (HostError -> map_host_resolution_error).
        // Effects: the claim outcome (the dispatch pool defers every
        // non-terminal outcome request-locally with the claim's backoff and
        // keeps its retry budget; `absorb` settles the Run), and the
        // FailSessionRealization report, which decides the Session state
        // (session-application `failure_budget` table):
        // | Rule | class | claim (P1 = P2) | report (retryable, capacity) | Session state | Session budget consumed? | dispatch budget consumed? |
        // | X1 | Transient: NotReady/Transport/Timeout/Backend | relinquish (request-local defer) | (true, false) | Preparing; ActivationFailed once the budget is spent | yes, one attempt | no |
        // | X2 | CapacityUnavailable | defer (request-local) | (true, true) | Preparing, never ActivationFailed | no, attempt refunded | no |
        // | X3 | Permanent: NotFound/Gone/Unauthorized/FenceRejected/Invalid/Unsupported/Conflict | absorb (TerminalResolution) | (false, false) | ActivationFailed | n/a (absorbing) | n/a (Run settled) |
        // Session-state and budget columns are proven on the durable aggregate by
        // session-application `capacity_budget` and the contract's
        // `capacity_failures_retry_without_consuming_and_others_keep_the_budget`;
        // the dispatch column by the pool's ClaimDeferred path (defer_claim keeps
        // attempt_count, run_ingress obligations).
        use awaken_provisioning_contract::SandboxError;
        let rows = [
            (
                "X1",
                SandboxError::not_ready("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::transport("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::timeout("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::backend("op", "d"),
                "relinquish",
                (true, false),
            ),
            (
                "X2",
                SandboxError::capacity_unavailable("exceeded quota"),
                "defer",
                (true, true),
            ),
            ("X3", SandboxError::not_found("d"), "absorb", (false, false)),
            ("X3", SandboxError::gone("d"), "absorb", (false, false)),
            (
                "X3",
                SandboxError::unauthorized("d"),
                "absorb",
                (false, false),
            ),
            (
                "X3",
                SandboxError::fence_rejected("d"),
                "absorb",
                (false, false),
            ),
            ("X3", SandboxError::invalid("d"), "absorb", (false, false)),
            (
                "X3",
                SandboxError::unsupported("d"),
                "absorb",
                (false, false),
            ),
            ("X3", SandboxError::conflict("d"), "absorb", (false, false)),
        ];
        for (rule, sandbox, claim, report) in rows {
            // P2: the resolver sees the Host error directly.
            let host_error = || {
                crate::host_error::from_run_error(
                    awaken_session_realization_contract::environment_effect_error(&sandbox),
                )
            };
            let resolver = HostWorkerResolver::map_host_resolution_error(host_error());
            // P1: the driver sees the same Host error after the synchronizer's
            // Run projection, reports it to Control, then returns it.
            let run_error = crate::host_error::to_run_error(host_error());
            let command = awaken_session_realization_contract::FailSessionRealization::for_effect(
                "s".into(),
                awaken_session_resource_contract::SessionRealizationLease {
                    owner: "w".into(),
                    runtime_incarnation: "w/1".into(),
                    epoch: 1,
                    expires_at_unix_ms: 1,
                },
                None,
                None,
                &run_error,
            );
            let reported = (command.retryable, command.capacity_unavailable);
            let driver = HostWorkerResolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    run_error,
                ),
            );
            assert_eq!(claim_outcome(&resolver), claim, "{rule} P2 {sandbox}");
            assert_eq!(claim_outcome(&driver), claim, "{rule} P1 {sandbox}");
            assert_eq!(reported, report, "{rule} report {sandbox}");
            assert!(
                resolver.to_string().contains(&sandbox.to_string()),
                "{rule}: the provider detail survives the boundary"
            );
        }
        // Fail closed: an unclassified Host failure keeps the resolver's
        // existing retry, and a capacity code on a non-Unavailable fault is
        // not a capacity refusal.
        assert_eq!(
            claim_outcome(&HostWorkerResolver::map_host_resolution_error(
                crate::HostError::internal("unclassified")
            )),
            "relinquish"
        );
        assert_eq!(
            claim_outcome(&HostWorkerResolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::classified(
                        awaken_session_realization_contract::ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                        "mislabelled",
                    ),
                ),
            )),
            "absorb"
        );
    }

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
                awaken_session_types::RunError::unavailable_classified(
                    "mcp_material_source_unavailable",
                    "materializer unavailable",
                ),
            ),
        );
        assert!(matches!(
            retryable_effect,
            awaken_dispatch_execution::Error::Execution(_)
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
            awaken_dispatch_execution::Error::Execution(_)
        ));

        let not_ready = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            ),
        );
        assert!(matches!(
            not_ready,
            awaken_dispatch_execution::Error::ResolutionNotReady(_)
        ));

        for permanent in [
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                awaken_session_types::RunError::classified(
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
                awaken_dispatch_execution::Error::TerminalResolution(_)
            ));
        }
    }
}
