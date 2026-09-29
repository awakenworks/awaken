// @formal surface exclude=projection_vocabulary
// @formal owner crates/server/awaken-managed-session-events/src/index.rs
// @formal reason: The closed vocabulary of timeline fact roles: each role's wire-frozen id string and
// @formal reason: its canonical phase inside one commit. Constants only: no transition, admission,
// @formal reason: persistence, retry or effect; the order index that consumes them is the behavior
// @formal reason: owner and carries the ordering obligations.
//! What each timeline fact is, and where it sits inside one commit.
//!
//! Every fact the timeline publishes has one role. The role names the public
//! transition (a Thread went idle, the aggregate opened, a report arrived) and
//! owns its canonical phase: the position of that fact among the facts one
//! Runtime commit produces. Emitters used to spell the role as a string and the
//! order table spelled it again beside a phase number; a typo on either side
//! dropped the fact out of the order. One enum now carries both.
//!
//! Consumers: the protocol adapters that mint public event ids from
//! [`FactRole::as_wire_role`] and the canonical order, which reads
//! [`FactRole::phase`].

/// One fixed timeline role, or the wire type of a transcript-derived fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FactRole {
    ActivationFailedError,
    AdvisorLifecycleStatusTerminated,
    AdvisorMessageReceived,
    AggregateStatusIdle,
    AggregateStatusRunning,
    AggregateTerminalStatusRunning,
    AggregateUsage,
    ArchivedStatusTerminated,
    BudgetReachStatusIdle,
    BudgetReachUsage,
    ChildMessageReceived,
    CoordinationMessageSent,
    CoordinationStatusRunning,
    FailedLifecycleStatusTerminated,
    LifecycleError,
    LifecycleStatusIdle,
    LifecycleStatusRescheduled,
    LifecycleStatusRunning,
    LinkStatusRunning,
    LinkThreadCreated,
    ModelRequestEnd,
    ModelRequestStart,
    OutcomeError,
    OutcomeEvaluationEnd,
    OutcomeEvaluationOngoing,
    OutcomeEvaluationStart,
    ParentTerminalChildStatusTerminated,
    ParentTerminalPrimaryStatusTerminated,
    ParentTerminalSessionStatusTerminated,
    RescheduledLifecycleStatusRunning,
    RootLifecycleError,
    RootLifecycleSessionStatusRescheduled,
    RootLifecycleStatusIdle,
    RootLifecycleStatusRescheduled,
    RootLifecycleStatusRunning,
    RootRescheduledLifecycleStatusRunning,
    RootTerminalStatusRunning,
    RunContextCompacted,
    /// A fact folded from one committed transcript Message. Its public role is
    /// the wire type of the event it becomes.
    Transcript(TranscriptRole),
}

/// The transcript folds that are identified by their source Message rather
/// than by a Runtime call id. Tool uses carry their own reversible identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TranscriptRole {
    UserMessage,
    UserCustomToolResult,
    AgentMessage,
    AgentThinking,
    AgentToolResult,
    AgentMcpToolResult,
}

/// Where a fact sits among the facts of one commit. The aggregate opening
/// precedes the Thread transitions, which precede the Run's observations,
/// transcript, Outcome facts, errors, terminals, usage and the aggregate close.
pub mod phase {
    pub const AGGREGATE_OPENING: u16 = 10;
    pub const THREAD_OPENING: u16 = 20;
    pub const THREAD_RESCHEDULED: u16 = 21;
    pub const THREAD_REPLACEMENT_OPENING: u16 = 22;
    pub const MODEL_REQUEST_START: u16 = 40;
    pub const MODEL_REQUEST_END: u16 = 41;
    pub const CONTEXT_COMPACTED: u16 = 42;
    /// Transcript folds and the answerable tool event of a pause.
    pub const TRANSCRIPT: u16 = 50;
    pub const LINK_CREATED: u16 = 51;
    pub const LINK_OPENING: u16 = 52;
    pub const COORDINATION_OPENING: u16 = 54;
    pub const COORDINATION_MESSAGE: u16 = 55;
    pub const OUTCOME_EVALUATION_START: u16 = 60;
    pub const OUTCOME_EVALUATION_ONGOING: u16 = 61;
    pub const OUTCOME_EVALUATION_END: u16 = 62;
    pub const OUTCOME_ERROR: u16 = 63;
    pub const LIFECYCLE_ERROR: u16 = 70;
    pub const THREAD_TERMINAL: u16 = 80;
    pub const THREAD_CLOSED: u16 = 81;
    pub const AGGREGATE_USAGE: u16 = 90;
    pub const AGGREGATE_CLOSE: u16 = 100;
    pub const ACTIVATION_FAILED: u16 = 109;
    pub const PARENT_TERMINAL_CHILD: u16 = 110;
    pub const PARENT_TERMINAL_PRIMARY: u16 = 111;
    pub const PARENT_TERMINAL_SESSION: u16 = 112;
    /// An accepted inbound command, ordered at its admission fence.
    pub const INBOUND_ADMISSION: u16 = 120;
}

impl FactRole {
    /// Every fixed (non-transcript) role, in wire-role order.
    pub const FIXED: [Self; 38] = [
        Self::ActivationFailedError,
        Self::AdvisorLifecycleStatusTerminated,
        Self::AdvisorMessageReceived,
        Self::AggregateStatusIdle,
        Self::AggregateStatusRunning,
        Self::AggregateTerminalStatusRunning,
        Self::AggregateUsage,
        Self::ArchivedStatusTerminated,
        Self::BudgetReachStatusIdle,
        Self::BudgetReachUsage,
        Self::ChildMessageReceived,
        Self::CoordinationMessageSent,
        Self::CoordinationStatusRunning,
        Self::FailedLifecycleStatusTerminated,
        Self::LifecycleError,
        Self::LifecycleStatusIdle,
        Self::LifecycleStatusRescheduled,
        Self::LifecycleStatusRunning,
        Self::LinkStatusRunning,
        Self::LinkThreadCreated,
        Self::ModelRequestEnd,
        Self::ModelRequestStart,
        Self::OutcomeError,
        Self::OutcomeEvaluationEnd,
        Self::OutcomeEvaluationOngoing,
        Self::OutcomeEvaluationStart,
        Self::ParentTerminalChildStatusTerminated,
        Self::ParentTerminalPrimaryStatusTerminated,
        Self::ParentTerminalSessionStatusTerminated,
        Self::RescheduledLifecycleStatusRunning,
        Self::RootLifecycleError,
        Self::RootLifecycleSessionStatusRescheduled,
        Self::RootLifecycleStatusIdle,
        Self::RootLifecycleStatusRescheduled,
        Self::RootLifecycleStatusRunning,
        Self::RootRescheduledLifecycleStatusRunning,
        Self::RootTerminalStatusRunning,
        Self::RunContextCompacted,
    ];

    /// The role hashed into public event ids. These strings are wire-frozen:
    /// changing one changes every public id minted for that role.
    #[must_use]
    pub const fn as_wire_role(self) -> &'static str {
        match self {
            Self::ActivationFailedError => "activation-failed-error",
            Self::AdvisorLifecycleStatusTerminated => "advisor-lifecycle-status-terminated",
            Self::AdvisorMessageReceived => "advisor-message-received",
            Self::AggregateStatusIdle => "aggregate-status-idle",
            Self::AggregateStatusRunning => "aggregate-status-running",
            Self::AggregateTerminalStatusRunning => "aggregate-terminal-status-running",
            Self::AggregateUsage => "aggregate-usage",
            Self::ArchivedStatusTerminated => "archived-status-terminated",
            Self::BudgetReachStatusIdle => "budget-reach-status-idle",
            Self::BudgetReachUsage => "budget-reach-usage",
            Self::ChildMessageReceived => "child-message-received",
            Self::CoordinationMessageSent => "coordination-message-sent",
            Self::CoordinationStatusRunning => "coordination-status-running",
            Self::FailedLifecycleStatusTerminated => "failed-lifecycle-status-terminated",
            Self::LifecycleError => "lifecycle-error",
            Self::LifecycleStatusIdle => "lifecycle-status-idle",
            Self::LifecycleStatusRescheduled => "lifecycle-status-rescheduled",
            Self::LifecycleStatusRunning => "lifecycle-status-running",
            Self::LinkStatusRunning => "link-status-running",
            Self::LinkThreadCreated => "link-thread-created",
            Self::ModelRequestEnd => "model-request-end",
            Self::ModelRequestStart => "model-request-start",
            Self::OutcomeError => "outcome-error",
            Self::OutcomeEvaluationEnd => "outcome-evaluation-end",
            Self::OutcomeEvaluationOngoing => "outcome-evaluation-ongoing",
            Self::OutcomeEvaluationStart => "outcome-evaluation-start",
            Self::ParentTerminalChildStatusTerminated => "parent-terminal-child-status-terminated",
            Self::ParentTerminalPrimaryStatusTerminated => {
                "parent-terminal-primary-status-terminated"
            }
            Self::ParentTerminalSessionStatusTerminated => {
                "parent-terminal-session-status-terminated"
            }
            Self::RescheduledLifecycleStatusRunning => "rescheduled-lifecycle-status-running",
            Self::RootLifecycleError => "root-lifecycle-error",
            Self::RootLifecycleSessionStatusRescheduled => {
                "root-lifecycle-session-status-rescheduled"
            }
            Self::RootLifecycleStatusIdle => "root-lifecycle-status-idle",
            Self::RootLifecycleStatusRescheduled => "root-lifecycle-status-rescheduled",
            Self::RootLifecycleStatusRunning => "root-lifecycle-status-running",
            Self::RootRescheduledLifecycleStatusRunning => {
                "root-rescheduled-lifecycle-status-running"
            }
            Self::RootTerminalStatusRunning => "root-terminal-status-running",
            Self::RunContextCompacted => "run-context-compacted",
            Self::Transcript(role) => role.as_wire_type(),
        }
    }

    /// The canonical phase of this role inside one commit.
    #[must_use]
    pub const fn phase(self) -> u16 {
        match self {
            Self::AggregateStatusRunning | Self::AggregateTerminalStatusRunning => {
                phase::AGGREGATE_OPENING
            }
            Self::RootLifecycleStatusRunning
            | Self::RootLifecycleSessionStatusRescheduled
            | Self::RootTerminalStatusRunning
            | Self::LifecycleStatusRunning => phase::THREAD_OPENING,
            Self::RootLifecycleStatusRescheduled | Self::LifecycleStatusRescheduled => {
                phase::THREAD_RESCHEDULED
            }
            Self::RootRescheduledLifecycleStatusRunning
            | Self::RescheduledLifecycleStatusRunning => phase::THREAD_REPLACEMENT_OPENING,
            Self::ModelRequestStart => phase::MODEL_REQUEST_START,
            Self::ModelRequestEnd => phase::MODEL_REQUEST_END,
            Self::RunContextCompacted => phase::CONTEXT_COMPACTED,
            Self::Transcript(_) => phase::TRANSCRIPT,
            Self::LinkThreadCreated => phase::LINK_CREATED,
            Self::LinkStatusRunning => phase::LINK_OPENING,
            Self::CoordinationStatusRunning => phase::COORDINATION_OPENING,
            Self::CoordinationMessageSent
            | Self::ChildMessageReceived
            | Self::AdvisorMessageReceived => phase::COORDINATION_MESSAGE,
            Self::OutcomeEvaluationStart => phase::OUTCOME_EVALUATION_START,
            Self::OutcomeEvaluationOngoing => phase::OUTCOME_EVALUATION_ONGOING,
            Self::OutcomeEvaluationEnd => phase::OUTCOME_EVALUATION_END,
            Self::OutcomeError => phase::OUTCOME_ERROR,
            Self::RootLifecycleError | Self::LifecycleError => phase::LIFECYCLE_ERROR,
            Self::RootLifecycleStatusIdle
            | Self::LifecycleStatusIdle
            | Self::FailedLifecycleStatusTerminated => phase::THREAD_TERMINAL,
            Self::AdvisorLifecycleStatusTerminated | Self::ArchivedStatusTerminated => {
                phase::THREAD_CLOSED
            }
            Self::AggregateUsage | Self::BudgetReachUsage => phase::AGGREGATE_USAGE,
            Self::AggregateStatusIdle | Self::BudgetReachStatusIdle => phase::AGGREGATE_CLOSE,
            Self::ActivationFailedError => phase::ACTIVATION_FAILED,
            Self::ParentTerminalChildStatusTerminated => phase::PARENT_TERMINAL_CHILD,
            Self::ParentTerminalPrimaryStatusTerminated => phase::PARENT_TERMINAL_PRIMARY,
            Self::ParentTerminalSessionStatusTerminated => phase::PARENT_TERMINAL_SESSION,
        }
    }

    /// Roles whose facts are ordered at one Run lifecycle commit.
    pub const LIFECYCLE: [Self; 14] = [
        Self::RootLifecycleStatusRunning,
        Self::RootLifecycleSessionStatusRescheduled,
        Self::RootLifecycleStatusRescheduled,
        Self::RootRescheduledLifecycleStatusRunning,
        Self::RootTerminalStatusRunning,
        Self::LifecycleStatusRunning,
        Self::LifecycleStatusRescheduled,
        Self::RescheduledLifecycleStatusRunning,
        Self::RootLifecycleError,
        Self::LifecycleError,
        Self::RootLifecycleStatusIdle,
        Self::LifecycleStatusIdle,
        Self::FailedLifecycleStatusTerminated,
        Self::AdvisorLifecycleStatusTerminated,
    ];
}

impl TranscriptRole {
    /// The public event type this fold becomes.
    #[must_use]
    pub const fn as_wire_type(self) -> &'static str {
        match self {
            Self::UserMessage => "user.message",
            Self::UserCustomToolResult => "user.custom_tool_result",
            Self::AgentMessage => "agent.message",
            Self::AgentThinking => "agent.thinking",
            Self::AgentToolResult => "agent.tool_result",
            Self::AgentMcpToolResult => "agent.mcp_tool_result",
        }
    }

    /// Classify a public event type as a Message-identified fold. Tool uses
    /// and every non-transcript type have no Message identity.
    #[must_use]
    pub fn from_wire_type(wire_type: &str) -> Option<Self> {
        [
            Self::UserMessage,
            Self::UserCustomToolResult,
            Self::AgentMessage,
            Self::AgentThinking,
            Self::AgentToolResult,
            Self::AgentMcpToolResult,
        ]
        .into_iter()
        .find(|role| role.as_wire_type() == wire_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_roles_are_distinct_sorted_and_phase_matches_the_former_table() {
        // Causes: each of the 38 fixed roles. Effects: E1 wire roles are unique
        // and in the published order; E2 every phase equals the value the
        // retired order table assigned; E3 transcript folds share the
        // transcript phase and round-trip their wire type.
        let wire = FactRole::FIXED.map(FactRole::as_wire_role);
        let mut sorted = wire.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, wire.to_vec(), "E1");
        let table = [
            ("activation-failed-error", 109),
            ("advisor-lifecycle-status-terminated", 81),
            ("advisor-message-received", 55),
            ("aggregate-status-idle", 100),
            ("aggregate-status-running", 10),
            ("aggregate-terminal-status-running", 10),
            ("aggregate-usage", 90),
            ("archived-status-terminated", 81),
            ("budget-reach-status-idle", 100),
            ("budget-reach-usage", 90),
            ("child-message-received", 55),
            ("coordination-message-sent", 55),
            ("coordination-status-running", 54),
            ("failed-lifecycle-status-terminated", 80),
            ("lifecycle-error", 70),
            ("lifecycle-status-idle", 80),
            ("lifecycle-status-rescheduled", 21),
            ("lifecycle-status-running", 20),
            ("link-status-running", 52),
            ("link-thread-created", 51),
            ("model-request-end", 41),
            ("model-request-start", 40),
            ("outcome-error", 63),
            ("outcome-evaluation-end", 62),
            ("outcome-evaluation-ongoing", 61),
            ("outcome-evaluation-start", 60),
            ("parent-terminal-child-status-terminated", 110),
            ("parent-terminal-primary-status-terminated", 111),
            ("parent-terminal-session-status-terminated", 112),
            ("rescheduled-lifecycle-status-running", 22),
            ("root-lifecycle-error", 70),
            ("root-lifecycle-session-status-rescheduled", 20),
            ("root-lifecycle-status-idle", 80),
            ("root-lifecycle-status-rescheduled", 21),
            ("root-lifecycle-status-running", 20),
            ("root-rescheduled-lifecycle-status-running", 22),
            ("root-terminal-status-running", 20),
            ("run-context-compacted", 42),
        ];
        assert_eq!(
            FactRole::FIXED
                .map(|role| (role.as_wire_role(), role.phase()))
                .to_vec(),
            table.to_vec(),
            "E2"
        );
        for role in [
            TranscriptRole::UserMessage,
            TranscriptRole::UserCustomToolResult,
            TranscriptRole::AgentMessage,
            TranscriptRole::AgentThinking,
            TranscriptRole::AgentToolResult,
            TranscriptRole::AgentMcpToolResult,
        ] {
            assert_eq!(FactRole::Transcript(role).phase(), phase::TRANSCRIPT, "E3");
            assert_eq!(
                TranscriptRole::from_wire_type(role.as_wire_type()),
                Some(role),
                "E3"
            );
        }
        for untyped in ["agent.tool_use", "session.status_idle", ""] {
            assert_eq!(TranscriptRole::from_wire_type(untyped), None, "E3");
        }
    }
}
