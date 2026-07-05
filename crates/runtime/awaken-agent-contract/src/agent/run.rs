use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

/// The committed phase of a run at a checkpoint: the single stored authority for
/// where the run stands. A run is either paused on a waiting ticket, or ended
/// through exactly one [`EndCause`].
///
/// Anything coarser — a published outcome, an `is_error` flag, a retry ruling —
/// is *derived* from this value the moment a consumer needs it, never stored
/// beside it. There is no consumer of such a projection yet, so none is
/// materialized; that keeps this enum the sole authority and makes a run record
/// unable to drift from its own classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// The run is executing. Committed at step boundaries so completed steps
    /// are durable (and visible to readers) before the run reaches a
    /// terminus. Not a pause — a running run carries no waiting ticket and
    /// cannot be resumed; a run left `Running` by a crashed process is an
    /// orphan a host terminalizes (cancel/stop) or redelivers.
    Running,
    /// The run paused on a waiting ticket. A pause is not a terminus.
    Waiting,
    /// The run reached a terminus through one mechanism.
    Ended(EndCause),
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
    pub phase: Phase,
}
