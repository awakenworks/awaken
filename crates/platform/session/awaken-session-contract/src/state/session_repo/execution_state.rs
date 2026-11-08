// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session_execution.terminal_state_never_reopens
//! Durable execution state of one Session aggregate.

/// The durable execution state of a Managed Session aggregate.
///
/// Retention and public visibility are deliberately owned by
/// [`super::SessionDisposition`]. Keeping the axes orthogonal allows an
/// activation failure or archived Session to be deleted without pretending
/// that deletion is another execution transition.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionState {
    Preparing,
    Activating,
    ActivationFailed,
    Running,
    Rescheduling,
    #[default]
    Idle,
    Terminated,
}

impl SessionExecutionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Activating => "activating",
            Self::ActivationFailed => "activation_failed",
            Self::Running => "running",
            Self::Rescheduling => "rescheduling",
            Self::Idle => "idle",
            Self::Terminated => "terminated",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::ActivationFailed | Self::Terminated)
    }

    /// Whether a driving Event may join or open the aggregate-owned activity
    /// interval. `Running` is ready because the monotonic `activity_epoch` and
    /// root-owned active epoch set fence overlapping and crash-recovery
    /// admissions; realization phases and terminal states are not ready.
    #[must_use]
    pub const fn admits_activity(self) -> bool {
        matches!(self, Self::Idle | Self::Running)
    }

    /// Whether the complete root still requires the initial realization driver.
    /// This classification is also the durable recovery-index cause: accepting
    /// revision 1 must never depend on a process-local Job or wake notification.
    #[must_use]
    pub const fn requires_initial_realization_recovery(self) -> bool {
        matches!(self, Self::Preparing | Self::Activating)
    }

    /// Whether the canonical Session state machine admits `next`.
    ///
    /// Replays are idempotent. Terminal states fail closed, and every
    /// non-terminal transition used by fresh execution, resume, and recovery is
    /// defined here rather than in those callers.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        match next {
            // Every live state may fail terminally: the Session's frozen
            // inputs can no longer admit execution. A realization failure
            // while Idle is absorbed by its caller instead (the resident
            // Session stays usable); from Idle this edge is taken only by a
            // permanently refused Run admission, which no retry can admit.
            Self::Terminated | Self::ActivationFailed => true,
            Self::Activating => {
                matches!(self, Self::Preparing | Self::Running | Self::Rescheduling)
            }
            Self::Idle => matches!(
                self,
                Self::Preparing | Self::Activating | Self::Running | Self::Rescheduling
            ),
            Self::Running => matches!(self, Self::Idle),
            Self::Rescheduling => matches!(self, Self::Idle | Self::Running),
            Self::Preparing => false,
        }
    }
}

#[cfg(test)]
#[test]
fn initial_realization_recovery_decision_table_is_closed() {
    // Cause/effect table mirrors the exhaustive test with concrete named rows. Only
    // Preparing and Activating retain the initial realization obligation; all
    // five complementary states rely on their own durable lifecycle causes.
    for (rule, state, expected) in [
        ("R1a", SessionExecutionState::Preparing, true),
        ("R1b", SessionExecutionState::Activating, true),
        ("R2a", SessionExecutionState::ActivationFailed, false),
        ("R2b", SessionExecutionState::Running, false),
        ("R2c", SessionExecutionState::Rescheduling, false),
        ("R2d", SessionExecutionState::Idle, false),
        ("R2e", SessionExecutionState::Terminated, false),
    ] {
        assert_eq!(
            state.requires_initial_realization_recovery(),
            expected,
            "{rule}"
        );
    }
}

impl std::fmt::Display for SessionExecutionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionExecutionStateError {
    #[error("unrecognized Session execution state `{value}`")]
    UnrecognizedState { value: String },
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid Session execution transition from `{from}` to `{to}`")]
pub struct SessionExecutionTransitionError {
    pub from: SessionExecutionState,
    pub to: SessionExecutionState,
}

impl std::str::FromStr for SessionExecutionState {
    type Err = SessionExecutionStateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "activating" => Ok(Self::Activating),
            "activation_failed" => Ok(Self::ActivationFailed),
            "running" => Ok(Self::Running),
            "rescheduling" => Ok(Self::Rescheduling),
            "idle" => Ok(Self::Idle),
            "terminated" => Ok(Self::Terminated),
            other => Err(SessionExecutionStateError::UnrecognizedState {
                value: other.to_string(),
            }),
        }
    }
}

#[cfg(test)]
pub(crate) mod exhaustive {
    use super::*;

    /// Every closed execution state, in declaration order.
    pub(crate) const EVERY_EXECUTION_STATE: [SessionExecutionState; 7] = [
        SessionExecutionState::Preparing,
        SessionExecutionState::Activating,
        SessionExecutionState::ActivationFailed,
        SessionExecutionState::Running,
        SessionExecutionState::Rescheduling,
        SessionExecutionState::Idle,
        SessionExecutionState::Terminated,
    ];

    const fn listed_execution_state(state: SessionExecutionState) -> usize {
        match state {
            SessionExecutionState::Preparing => 0,
            SessionExecutionState::Activating => 1,
            SessionExecutionState::ActivationFailed => 2,
            SessionExecutionState::Running => 3,
            SessionExecutionState::Rescheduling => 4,
            SessionExecutionState::Idle => 5,
            SessionExecutionState::Terminated => 6,
        }
    }

    #[test]
    fn every_execution_state_is_listed() {
        for (index, state) in EVERY_EXECUTION_STATE.into_iter().enumerate() {
            assert_eq!(listed_execution_state(state), index);
        }
    }

    /// @exhaustive SessionExecutionState(7) = 7
    /// @obligation session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
    #[test]
    fn initial_realization_recovery_is_exact_for_every_execution_state() {
        // Causes: C1 is each closed SessionExecutionState variant. Effects: E1
        // the complete root remains in the repository reconciliation index; E2
        // it does not gain initial-realization work. R1 Preparing/Activating =>
        // E1; R2 every other state => E2.
        for state in EVERY_EXECUTION_STATE {
            assert_eq!(
                state.requires_initial_realization_recovery(),
                matches!(
                    state,
                    SessionExecutionState::Preparing | SessionExecutionState::Activating
                )
            );
        }
    }
}
