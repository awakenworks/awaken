// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Closed failure classification at the Session realization Control boundary.
//!
//! Keeping the wire error and its one effect policy together prevents embedded,
//! HTTP, and remote Worker adapters from growing parallel retry decisions.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SessionRealizationControlFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is not ready for this realization phase")]
    NotReady,
    #[error("Session realization retired after its driving work settled")]
    Retired,
    #[error("Session realization is terminal")]
    Terminal,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session changed concurrently")]
    Conflict,
    #[error("Session realization command is invalid: {0}")]
    Invalid(String),
    #[error("Session realization service is unavailable: {0}")]
    Unavailable(String),
    #[error("Session realization durable state is invalid: {0}")]
    Internal(String),
}

/// Stable code preserving a Control deferral through the neutral Run boundary.
pub const SESSION_REALIZATION_NOT_READY_CODE: &str = "session_realization_not_ready";

/// Stable effect class for one Control failure at a Worker/Run boundary.
///
/// The Session contract owns this classification so HTTP, embedded, and remote
/// Workers cannot independently reinterpret the same durable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationControlDisposition {
    NotReady,
    Retryable,
    Terminal,
}

impl SessionRealizationControlFailure {
    #[must_use]
    pub const fn disposition(&self) -> SessionRealizationControlDisposition {
        match self {
            Self::NotReady => SessionRealizationControlDisposition::NotReady,
            Self::StaleOwnership | Self::Conflict | Self::Unavailable(_) => {
                SessionRealizationControlDisposition::Retryable
            }
            Self::NotFound
            | Self::Retired
            | Self::Terminal
            | Self::Invalid(_)
            | Self::Internal(_) => SessionRealizationControlDisposition::Terminal,
        }
    }

    /// Whether this reply conclusively denies the caller's exact current
    /// realization owner/fence. A retryable Run may be relinquished and
    /// executed by a newer owner, while the stale physical projection itself
    /// must stop immediately. NotReady, Conflict, and Unavailable do not prove
    /// that fact; their prior unexpired lease remains authoritative.
    #[must_use]
    pub const fn proves_current_realization_cannot_continue(&self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::Retired
                | Self::Terminal
                | Self::StaleOwnership
                | Self::Invalid(_)
                | Self::Internal(_)
        )
    }
}

/// Project one Control cause into a neutral Run fault without changing its
/// effect disposition. HTTP adapters retain the typed Control value for their
/// status/envelope; callers never need to parse its presentation string.
///
/// ```
/// use awaken_session_realization_contract::{SessionRealizationControlFailure as C, effect_failure_disposition};
/// use awaken_session_types::{RunError, RunErrorClassification};
/// let missing: RunError = C::NotFound.into();
/// assert_eq!(missing.classification(), RunErrorClassification::NotFound);
/// let corrupt = C::Internal("damaged row".into());
/// let expected = corrupt.disposition();
/// let run: RunError = corrupt.into();
/// assert_eq!(effect_failure_disposition(&run), expected);
/// assert!(!run.is_unclassified_internal());
/// ```
impl From<SessionRealizationControlFailure> for awaken_session_types::RunError {
    fn from(error: SessionRealizationControlFailure) -> Self {
        use SessionRealizationControlFailure as C;
        match error {
            C::NotReady => {
                Self::unavailable_classified(SESSION_REALIZATION_NOT_READY_CODE, error.to_string())
            }
            C::StaleOwnership | C::Conflict => Self::conflict(error.to_string()),
            C::NotFound => Self::bad_request_classified("not_found", error.to_string()),
            C::Retired | C::Terminal => Self::bad_request(error.to_string()),
            C::Invalid(message) => Self::bad_request(message),
            C::Unavailable(_) => Self::unavailable(error.to_string()),
            C::Internal(message) => Self::classified("session_realization_invalid", message),
        }
    }
}

/// The one Control projection of an Environment decision's refusal: a terminal
/// Session is terminal, a lost lease is stale ownership, movement the Session
/// may still make is not ready, and every causal refusal is invalid.
impl From<awaken_session_contract::SessionEnvironmentRejection>
    for SessionRealizationControlFailure
{
    fn from(rejection: awaken_session_contract::SessionEnvironmentRejection) -> Self {
        use awaken_session_contract::SessionEnvironmentRejection as Rejection;
        match rejection {
            Rejection::Terminal => Self::Terminal,
            Rejection::LeaseNotCurrent => Self::StaleOwnership,
            Rejection::NotTerminal
            | Rejection::ContinuationInProgress
            | Rejection::ActivitiesOpen
            | Rejection::WrongStep
            | Rejection::CheckpointLive
            | Rejection::MaterializationPending
            | Rejection::MemoryEvidenceUnproven => Self::NotReady,
            Rejection::NotGenerated
            | Rejection::RequiresResident
            | Rejection::InvalidTransition
            | Rejection::GenerationExpired
            | Rejection::InconsistentEvidence
            | Rejection::UnsupportedContinuationTopology => Self::Invalid(rejection.to_string()),
        }
    }
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn session_realization_control_failure_disposition_is_total_exact_and_fail_closed() {
    let selector = kani::any::<u8>() % 9;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        7 => SessionRealizationControlFailure::Unavailable(String::new()),
        _ => SessionRealizationControlFailure::Internal(String::new()),
    };
    // This oracle is deliberately derived from the symbolic variant selector,
    // not from `failure.disposition()`: changing the production table cannot
    // change the expected result at the same time.
    let expected = match selector {
        1 => SessionRealizationControlDisposition::NotReady,
        4 | 5 | 7 => SessionRealizationControlDisposition::Retryable,
        _ => SessionRealizationControlDisposition::Terminal,
    };

    assert_eq!(failure.disposition(), expected);
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn realization_ownership_loss_proof_is_total_and_exact() {
    let selector = kani::any::<u8>() % 9;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        7 => SessionRealizationControlFailure::Unavailable(String::new()),
        _ => SessionRealizationControlFailure::Internal(String::new()),
    };
    assert_eq!(
        failure.proves_current_realization_cannot_continue(),
        matches!(selector, 0 | 2 | 3 | 4 | 6 | 8)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realization_control_failures_round_trip_without_semantic_loss() {
        // Cause/effect graph: C1 transient backpressure, C2 retryable authority
        // or dependency failure, and C3 an absent/terminal/invalid aggregate.
        // Effects: E1 defer without crash accounting, E2 relinquish for the
        // existing retry path, and E3 absorb the Run instead of hot-looping.
        // Every cause also round-trips over the existing typed transport.
        //
        // | Rule | cause | disposition | current owner disproved |
        // | T1 | not ready | NotReady | no |
        // | T2a | stale | Retryable | yes |
        // | T2b | conflict/unavailable | Retryable | no |
        // | T3 | not found/retired/terminal/invalid/internal | Terminal | yes |
        for (rule, failure, disposition, owner_disproved) in [
            (
                "T3 not found",
                SessionRealizationControlFailure::NotFound,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T1 not ready",
                SessionRealizationControlFailure::NotReady,
                SessionRealizationControlDisposition::NotReady,
                false,
            ),
            (
                "T3 retired",
                SessionRealizationControlFailure::Retired,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T3 terminal",
                SessionRealizationControlFailure::Terminal,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2a stale",
                SessionRealizationControlFailure::StaleOwnership,
                SessionRealizationControlDisposition::Retryable,
                true,
            ),
            (
                "T2b conflict",
                SessionRealizationControlFailure::Conflict,
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
            (
                "T3 invalid",
                SessionRealizationControlFailure::Invalid("bad target".into()),
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2b unavailable",
                SessionRealizationControlFailure::Unavailable("control offline".into()),
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
            (
                "T3 corrupt durable state",
                SessionRealizationControlFailure::Internal("control offline".into()),
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
        ] {
            let wire = serde_json::to_value(&failure).expect(rule);
            let decoded: SessionRealizationControlFailure =
                serde_json::from_value(wire).expect(rule);
            assert_eq!(decoded, failure, "{rule}");
            assert_eq!(decoded.disposition(), disposition, "{rule}");
            let direct: awaken_session_types::RunError = decoded.clone().into();
            let run: awaken_session_types::RunError =
                super::super::SessionRealizationDriveError::Control(decoded.clone()).into();
            assert_eq!(
                (&run.kind, &run.code, &run.message),
                (&direct.kind, &direct.code, &direct.message),
                "{rule} Drive adapter"
            );
            assert_eq!(
                super::super::effect_failure_disposition(&run),
                disposition,
                "{rule} neutral Run projection"
            );
            assert!(!run.is_unclassified_internal(), "{rule} known cause");
            assert_eq!(
                decoded.proves_current_realization_cannot_continue(),
                owner_disproved,
                "{rule}"
            );
        }
    }

    #[test]
    fn environment_rejections_project_onto_one_control_failure() {
        // Decision table R1-R4: R1 a terminal Session => Terminal; R2 a lost
        // realization lease => StaleOwnership; R3 movement the Session may
        // still make (another step, open activities, a live checkpoint, a
        // pending materialization or Memory evidence) => NotReady; R4 a causal
        // refusal => Invalid carrying the rejection.
        use awaken_session_contract::SessionEnvironmentRejection as X;
        for (rule, rejection, expected) in [
            (
                "R1",
                X::Terminal,
                SessionRealizationControlFailure::Terminal,
            ),
            (
                "R2",
                X::LeaseNotCurrent,
                SessionRealizationControlFailure::StaleOwnership,
            ),
            (
                "R3",
                X::NotTerminal,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::ContinuationInProgress,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::ActivitiesOpen,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::WrongStep,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::CheckpointLive,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::MaterializationPending,
                SessionRealizationControlFailure::NotReady,
            ),
            (
                "R3",
                X::MemoryEvidenceUnproven,
                SessionRealizationControlFailure::NotReady,
            ),
        ] {
            assert_eq!(
                SessionRealizationControlFailure::from(rejection),
                expected,
                "{rule}"
            );
        }
        for rejection in [
            X::NotGenerated,
            X::RequiresResident,
            X::InvalidTransition,
            X::GenerationExpired,
            X::InconsistentEvidence,
            X::UnsupportedContinuationTopology,
        ] {
            assert_eq!(
                SessionRealizationControlFailure::from(rejection),
                SessionRealizationControlFailure::Invalid(rejection.to_string()),
                "R4 {rejection:?}"
            );
        }
    }
}
