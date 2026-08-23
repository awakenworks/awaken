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
            Self::Terminated => true,
            Self::ActivationFailed => !matches!(self, Self::Idle),
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

impl std::fmt::Display for SessionExecutionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown Session execution state `{0}`")]
pub struct SessionExecutionStateError(pub String);

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
            other => Err(SessionExecutionStateError(other.to_string())),
        }
    }
}
