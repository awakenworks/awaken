use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

/// The committed lifecycle state of a run: the single stored authority for
/// where the run stands. A run is either executing, awaiting with a resume
/// ticket, or ended through exactly one [`EndCause`].
///
/// Anything coarser — a published outcome, an `is_error` flag, a retry ruling —
/// is *derived* from this value the moment a consumer needs it, never stored
/// beside it. There is no consumer of such a projection yet, so none is
/// materialized; that keeps this enum the sole authority and makes a run record
/// unable to drift from its own classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    /// The run is executing. Committed at step boundaries so completed steps
    /// are durable (and visible to readers) before the run reaches a
    /// end. A running run carries no resume ticket and
    /// cannot be resumed; a run left `Running` by a crashed process is an
    /// orphan a host terminalizes (cancel/stop) or redelivers.
    Running,
    /// The run awaits external input under a resume ticket.
    #[serde(alias = "Waiting")]
    Awaiting,
    /// The run ended through one mechanism.
    Ended(EndCause),
}

impl RunState {
    /// Whether a committed run may accept another lifecycle commit.
    ///
    /// `Running` and `Awaiting` may move to any next state, while `Ended` is
    /// absorbing. The relation is pure and total so it can be exhaustively
    /// verified and reused by every persistence backend.
    #[must_use]
    pub fn permits(&self, _next: &Self) -> bool {
        !matches!(self, Self::Ended(_))
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ended(_))
    }
}

/// The closed set of mechanisms by which a run ends. A run ends through exactly
/// one of these; the variant is the terminal authority, the only place a
/// terminal classification lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndCause {
    /// The model produced a final text turn with no further tool calls.
    NaturalEnd,
    /// The model/tool loop hit its step ceiling without ending naturally.
    MaxSteps,
    /// The run was cancelled from outside.
    Cancelled,
    /// A stop policy ended the run with a terminal reason (e.g. a budget or step
    /// ceiling enforced by the host), distinct from an external cancel (ADR-0026).
    Stopped(String),
    /// An execution fault ended the run.
    Error(Failure),
    /// External execution dispatched work asynchronously; the actual outcome
    /// cannot be determined at the point of return. Callers should poll or
    /// wait for a subsequent status update rather than treating the run as
    /// successfully completed. Never silently projected as success (G26).
    Indeterminate,
}

/// The classified cause of an [`EndCause::Error`]. The runtime owns these
/// neutral fault kinds; it never keeps a free-form status string as authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// Model inference failed permanently or exhausted its retries. `code` is
    /// the provider error's stable snake_case classification (for example
    /// `rate_limited`, `context_overflow`, `unauthorized`), so a host can
    /// categorize a fault the loop could not handle without parsing `message`.
    Inference { code: String, message: String },
    /// A plugin contributed beyond its declared capability bound.
    CapabilityBound,
    /// A staged state batch held an exclusive-key conflict.
    StateConflict,
}

impl Failure {
    /// The stable snake_case code classifying this fault, for hosts that
    /// categorize failures without parsing messages.
    pub fn code(&self) -> &str {
        match self {
            Failure::Inference { code, .. } => code,
            Failure::CapabilityBound => "capability_bound",
            Failure::StateConflict => "state_conflict",
        }
    }

    /// A human-readable description of the fault.
    pub fn message(&self) -> String {
        match self {
            Failure::Inference { message, .. } => message.clone(),
            Failure::CapabilityBound => {
                "a plugin contributed beyond its declared capability bound".to_string()
            }
            Failure::StateConflict => {
                "a staged state batch held an exclusive-key conflict".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: Id,
    pub thread_id: crate::agent::thread::Id,
    pub state: RunState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_code_is_the_stable_snake_case_classification() {
        assert_eq!(
            Failure::Inference {
                code: "rate_limited".into(),
                message: "429".into(),
            }
            .code(),
            "rate_limited"
        );
        assert_eq!(Failure::CapabilityBound.code(), "capability_bound");
        assert_eq!(Failure::StateConflict.code(), "state_conflict");
    }

    #[test]
    fn failure_message_is_the_provider_text_or_a_fixed_description() {
        assert_eq!(
            Failure::Inference {
                code: "unauthorized".into(),
                message: "bad api key".into(),
            }
            .message(),
            "bad api key"
        );
        assert!(
            Failure::CapabilityBound
                .message()
                .contains("capability bound")
        );
        assert!(
            Failure::StateConflict
                .message()
                .contains("exclusive-key conflict")
        );
    }

    #[test]
    fn ended_variants_round_trip_through_serde() {
        for cause in [
            EndCause::NaturalEnd,
            EndCause::MaxSteps,
            EndCause::Cancelled,
            EndCause::Stopped("budget".into()),
            EndCause::Error(Failure::CapabilityBound),
            EndCause::Indeterminate,
        ] {
            let state = RunState::Ended(cause.clone());
            let json = serde_json::to_string(&state).unwrap();
            let back: RunState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back, "{cause:?} must survive a serde round trip");
        }
    }

    fn model_states() -> Vec<RunState> {
        vec![
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
            RunState::Ended(EndCause::MaxSteps),
            RunState::Ended(EndCause::Cancelled),
            RunState::Ended(EndCause::Stopped("budget".into())),
            RunState::Ended(EndCause::Error(Failure::Inference {
                code: "fault".into(),
                message: "fault".into(),
            })),
            RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
            RunState::Ended(EndCause::Error(Failure::StateConflict)),
            RunState::Ended(EndCause::Indeterminate),
        ]
    }

    /// Exhaustive over every lifecycle class and every closed terminal cause.
    #[test]
    fn transition_relation_is_total_and_terminal_is_absorbing() {
        let states = model_states();
        for current in &states {
            for next in &states {
                assert_eq!(
                    current.permits(next),
                    !current.is_terminal(),
                    "unexpected transition ruling: {current:?} -> {next:?}",
                );
            }
        }
    }

    #[test]
    fn legacy_waiting_state_deserializes_as_awaiting_but_never_serializes_back() {
        let state: RunState = serde_json::from_str("\"Waiting\"").unwrap();
        assert_eq!(state, RunState::Awaiting);
        assert_eq!(serde_json::to_string(&state).unwrap(), "\"Awaiting\"");
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn symbolic_failure(tag: u8) -> Failure {
        match tag % 3 {
            0 => Failure::Inference {
                code: String::new(),
                message: String::new(),
            },
            1 => Failure::CapabilityBound,
            _ => Failure::StateConflict,
        }
    }

    fn symbolic_end_cause(tag: u8, failure_tag: u8) -> EndCause {
        match tag % 6 {
            0 => EndCause::NaturalEnd,
            1 => EndCause::MaxSteps,
            2 => EndCause::Cancelled,
            3 => EndCause::Stopped(String::new()),
            4 => EndCause::Error(symbolic_failure(failure_tag)),
            _ => EndCause::Indeterminate,
        }
    }

    fn symbolic_state(state_tag: u8, cause_tag: u8, failure_tag: u8) -> RunState {
        match state_tag % 3 {
            0 => RunState::Running,
            1 => RunState::Awaiting,
            _ => RunState::Ended(symbolic_end_cause(cause_tag, failure_tag)),
        }
    }

    /// Covers every lifecycle class, end cause, failure kind, and next-state
    /// class against the production transition relation.
    #[kani::proof]
    fn ended_is_absorbing_for_every_next_state() {
        let current = symbolic_state(kani::any(), kani::any(), kani::any());
        let next = symbolic_state(kani::any(), kani::any(), kani::any());
        assert_eq!(current.permits(&next), !current.is_terminal());
    }
}
