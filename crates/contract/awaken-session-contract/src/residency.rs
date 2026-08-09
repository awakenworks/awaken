//! Durable Session activity and execution-environment residency decisions.
//!
//! The public Managed lifecycle remains `idle|running|rescheduling|terminated`.
//! These values fence a stale idle scan against newly admitted work and model
//! only residency states that current providers can truthfully realize.

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdleReason {
    EndTurn,
    AwaitingAction,
    RetryExhausted,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionActivityState {
    #[default]
    Unknown,
    Active,
    Idle {
        reason: SessionIdleReason,
        since_unix_ms: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionActivity {
    /// Monotonic admission/settlement fence. A stale completion cannot idle a
    /// Session after a newer event has acquired the next epoch.
    pub epoch: u64,
    pub state: SessionActivityState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionEnvironmentState {
    #[default]
    Unmaterialized,
    Resident {
        binding: String,
    },
}

impl SessionEnvironmentState {
    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding } => Some(binding),
            Self::Unmaterialized => None,
        }
    }

    pub fn set_resident(&mut self, binding: impl Into<String>) {
        *self = Self::Resident {
            binding: binding.into(),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionResidencyPolicy {
    pub hand_idle_after_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionResidencyAction {
    Keep,
    HibernateHand,
}

/// Pure decision kernel. External work remains in the Session application shell.
#[must_use]
pub fn decide_session_residency(
    activity: &SessionActivity,
    environment: &SessionEnvironmentState,
    now_unix_ms: u64,
    policy: SessionResidencyPolicy,
) -> SessionResidencyAction {
    match (environment, &activity.state) {
        (
            SessionEnvironmentState::Resident { .. },
            SessionActivityState::Idle {
                reason: SessionIdleReason::EndTurn,
                since_unix_ms,
            },
        ) if policy.hand_idle_after_ms > 0
            && now_unix_ms.saturating_sub(*since_unix_ms) >= policy.hand_idle_after_ms =>
        {
            SessionResidencyAction::HibernateHand
        }
        _ => SessionResidencyAction::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(state: SessionActivityState) -> SessionActivity {
        SessionActivity { epoch: 7, state }
    }

    fn resident() -> SessionEnvironmentState {
        SessionEnvironmentState::Resident {
            binding: "opaque".into(),
        }
    }

    /// Cause/effect decision table for the pure residency policy.
    /// C1=environment resident; C2=activity is Idle(EndTurn); C3=idle threshold
    /// enabled and reached. E1=hibernate Hand; E2=keep. Constraints: idle age
    /// applies only to C1+C2. Rules: R1 C1+C2+C3=>E1; R2 active=>E2;
    /// R3 awaiting action=>E2; R4 threshold not reached/disabled=>E2;
    /// R5 unmaterialized=>E2.
    #[test]
    fn residency_decision_table_covers_every_supported_cause() {
        let policy = SessionResidencyPolicy {
            hand_idle_after_ms: 60,
        };
        let idle = |reason, since| {
            activity(SessionActivityState::Idle {
                reason,
                since_unix_ms: since,
            })
        };
        let cases = vec![
            (
                idle(SessionIdleReason::EndTurn, 900),
                resident(),
                1_000,
                policy,
                SessionResidencyAction::HibernateHand,
                "R1 eligible",
            ),
            (
                activity(SessionActivityState::Active),
                resident(),
                1_000,
                policy,
                SessionResidencyAction::Keep,
                "R2 active",
            ),
            (
                idle(SessionIdleReason::AwaitingAction, 0),
                resident(),
                1_000,
                policy,
                SessionResidencyAction::Keep,
                "R3 awaiting",
            ),
            (
                idle(SessionIdleReason::EndTurn, 950),
                resident(),
                1_000,
                policy,
                SessionResidencyAction::Keep,
                "R4 grace",
            ),
            (
                idle(SessionIdleReason::EndTurn, 0),
                resident(),
                1_000,
                SessionResidencyPolicy {
                    hand_idle_after_ms: 0,
                },
                SessionResidencyAction::Keep,
                "R4 disabled",
            ),
            (
                idle(SessionIdleReason::EndTurn, 0),
                SessionEnvironmentState::Unmaterialized,
                1_000,
                policy,
                SessionResidencyAction::Keep,
                "R5 absent",
            ),
        ];
        for (activity, environment, now, policy, expected, rule) in cases {
            assert_eq!(
                decide_session_residency(&activity, &environment, now, policy),
                expected,
                "{rule}"
            );
        }
    }
}
