//! What one timeline fact says.

use awaken_agent_contract::audit::model_request::TokenUsage;

use crate::{EventIdentity, RunFault, StopFact};

/// One fact the timeline publishes: its identity and its content.
///
/// Consumer: the protocol adapter, which mints the public id from `key`,
/// encodes `kind` on its wire and appends the result to its event log. The
/// fact's canonical order is resolved from `key` by [`crate::OrderIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedSessionEvent {
    pub key: EventIdentity,
    pub kind: SessionEventKind,
}

/// The content of one timeline fact. A Thread fact concerns the Thread named
/// by its key; the Session id names the primary Thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEventKind {
    /// One immutable Session-root authoring update.
    SessionUpdated,
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
    /// The usage a budget reach was counted at, its rounded public list cost
    /// and the cap it crossed.
    BudgetReachUsage {
        usage: awaken_session_contract::SessionUsage,
        list_cost_minor: Option<u64>,
        max_list_cost_minor: u64,
    },
    ContextCompacted,
    ModelRequestStart,
    ModelRequestEnd {
        start: EventIdentity,
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
    /// One evaluation iteration of an Outcome began grading.
    OutcomeEvaluationStart {
        outcome_id: String,
        iteration: u32,
    },
    /// The iteration's grading is in progress.
    OutcomeEvaluationOngoing {
        outcome_id: String,
        iteration: u32,
    },
    /// The iteration was graded.
    OutcomeEvaluationEnd(Box<OutcomeVerdict>),
    /// The Outcome aggregate committed an infrastructure failure.
    OutcomeError {
        code: String,
        message: String,
    },
}

/// What one graded Outcome evaluation iteration concluded. `start` names the
/// iteration's start fact; `usage` is what grading cost, absent when the round
/// was never graded or its Run recorded none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeVerdict {
    pub outcome_id: String,
    pub iteration: u32,
    pub start: EventIdentity,
    pub result: String,
    pub explanation: String,
    pub usage: Option<TokenUsage>,
}

impl DerivedSessionEvent {
    #[must_use]
    pub fn new(key: EventIdentity, kind: SessionEventKind) -> Self {
        Self { key, kind }
    }
}
