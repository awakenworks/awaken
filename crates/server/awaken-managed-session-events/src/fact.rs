//! What one timeline fact says.

use awaken_agent_contract::audit::model_request::TokenUsage;

use crate::{FactKey, RunFault, StopFact};

/// One fact the timeline publishes: its identity and its content.
///
/// Consumer: the protocol adapter, which mints the public id from `key`,
/// encodes `kind` on its wire and appends the result to its event log. The
/// fact's canonical order is resolved from `key` by [`crate::OrderIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineFact {
    pub key: FactKey,
    pub kind: FactKind,
}

/// The content of one timeline fact. A Thread fact concerns the Thread named
/// by its key; the Session id names the primary Thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactKind {
    /// A coordinated child Thread was created by its link.
    ThreadCreated,
    ThreadRunning,
    ThreadRescheduled,
    ThreadIdle(StopFact),
    ThreadTerminated,
    /// The key's Thread's Run faulted. An Advisor consultation surfaces one
    /// fixed consultation failure instead of its internal cause.
    RunError {
        fault: RunFault,
        advisor: bool,
    },
    SessionRunning,
    SessionRescheduled,
    /// The aggregate paused or stopped; `calls_owner` is the Thread whose
    /// answerable calls a `RequiresAction` stop names.
    SessionIdle {
        stop: StopFact,
        calls_owner: String,
    },
    SessionTerminated,
    /// The usage one admitted interval closed with.
    IntervalUsage(crate::IntervalUsage),
    ContextCompacted,
    ModelRequestStart,
    ModelRequestEnd {
        start: FactKey,
        is_error: bool,
        usage: TokenUsage,
    },
    /// The parent sent this message to the key's child Thread.
    MessageSent {
        message: String,
    },
    /// The key's child Thread reported this text to its parent.
    ReportReceived {
        text: String,
    },
    /// Initial activation failed with this durable cause.
    ActivationFailed {
        message: String,
    },
}

impl TimelineFact {
    #[must_use]
    pub fn new(key: FactKey, kind: FactKind) -> Self {
        Self { key, kind }
    }
}
