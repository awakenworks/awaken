//! The wire-stability pin of the role coordinates hashed into Managed
//! multi-agent event ids.
//!
//! The roles themselves are [`ManagedEventType`] in the timeline domain, which owns
//! both the wire string and the canonical phase of each. This test-only module
//! keeps the literal ids those strings produced before they moved: a changed
//! row is a public-id break. Consumer: `managed_event_role_ids_are_wire_stable`.

use awaken_managed_session_events::ManagedEventType;

pub(crate) const ACTIVATION_FAILED_ERROR: &str =
    ManagedEventType::ActivationFailedError.as_wire_role();
pub(crate) const ADVISOR_LIFECYCLE_STATUS_TERMINATED: &str =
    ManagedEventType::AdvisorLifecycleStatusTerminated.as_wire_role();
pub(crate) const ADVISOR_MESSAGE_RECEIVED: &str =
    ManagedEventType::AdvisorMessageReceived.as_wire_role();
pub(crate) const AGGREGATE_STATUS_IDLE: &str = ManagedEventType::AggregateStatusIdle.as_wire_role();
pub(crate) const AGGREGATE_STATUS_RUNNING: &str =
    ManagedEventType::AggregateStatusRunning.as_wire_role();
pub(crate) const AGGREGATE_TERMINAL_STATUS_RUNNING: &str =
    ManagedEventType::AggregateTerminalStatusRunning.as_wire_role();
pub(crate) const AGGREGATE_USAGE: &str = ManagedEventType::AggregateUsage.as_wire_role();
pub(crate) const ARCHIVED_STATUS_TERMINATED: &str =
    ManagedEventType::ArchivedStatusTerminated.as_wire_role();
pub(crate) const BUDGET_REACH_STATUS_IDLE: &str =
    ManagedEventType::BudgetReachStatusIdle.as_wire_role();
pub(crate) const BUDGET_REACH_USAGE: &str = ManagedEventType::BudgetReachUsage.as_wire_role();
pub(crate) const CHILD_MESSAGE_RECEIVED: &str =
    ManagedEventType::ChildMessageReceived.as_wire_role();
pub(crate) const COORDINATION_MESSAGE_SENT: &str =
    ManagedEventType::CoordinationMessageSent.as_wire_role();
pub(crate) const COORDINATION_STATUS_RUNNING: &str =
    ManagedEventType::CoordinationStatusRunning.as_wire_role();
pub(crate) const FAILED_LIFECYCLE_STATUS_TERMINATED: &str =
    ManagedEventType::FailedLifecycleStatusTerminated.as_wire_role();
pub(crate) const LIFECYCLE_ERROR: &str = ManagedEventType::LifecycleError.as_wire_role();
pub(crate) const LIFECYCLE_STATUS_IDLE: &str = ManagedEventType::LifecycleStatusIdle.as_wire_role();
pub(crate) const LIFECYCLE_STATUS_RESCHEDULED: &str =
    ManagedEventType::LifecycleStatusRescheduled.as_wire_role();
pub(crate) const LIFECYCLE_STATUS_RUNNING: &str =
    ManagedEventType::LifecycleStatusRunning.as_wire_role();
pub(crate) const LINK_STATUS_RUNNING: &str = ManagedEventType::LinkStatusRunning.as_wire_role();
pub(crate) const LINK_THREAD_CREATED: &str = ManagedEventType::LinkThreadCreated.as_wire_role();
pub(crate) const MODEL_REQUEST_END: &str = ManagedEventType::ModelRequestEnd.as_wire_role();
pub(crate) const MODEL_REQUEST_START: &str = ManagedEventType::ModelRequestStart.as_wire_role();
pub(crate) const OUTCOME_ERROR: &str = ManagedEventType::OutcomeError.as_wire_role();
pub(crate) const OUTCOME_EVALUATION_END: &str =
    ManagedEventType::OutcomeEvaluationEnd.as_wire_role();
pub(crate) const OUTCOME_EVALUATION_ONGOING: &str =
    ManagedEventType::OutcomeEvaluationOngoing.as_wire_role();
pub(crate) const OUTCOME_EVALUATION_START: &str =
    ManagedEventType::OutcomeEvaluationStart.as_wire_role();
pub(crate) const PARENT_TERMINAL_CHILD_STATUS_TERMINATED: &str =
    ManagedEventType::ParentTerminalChildStatusTerminated.as_wire_role();
pub(crate) const PARENT_TERMINAL_PRIMARY_STATUS_TERMINATED: &str =
    ManagedEventType::ParentTerminalPrimaryStatusTerminated.as_wire_role();
pub(crate) const PARENT_TERMINAL_SESSION_STATUS_TERMINATED: &str =
    ManagedEventType::ParentTerminalSessionStatusTerminated.as_wire_role();
pub(crate) const RESCHEDULED_LIFECYCLE_STATUS_RUNNING: &str =
    ManagedEventType::RescheduledLifecycleStatusRunning.as_wire_role();
pub(crate) const ROOT_LIFECYCLE_ERROR: &str = ManagedEventType::RootLifecycleError.as_wire_role();
pub(crate) const ROOT_LIFECYCLE_SESSION_STATUS_RESCHEDULED: &str =
    ManagedEventType::RootLifecycleSessionStatusRescheduled.as_wire_role();
pub(crate) const ROOT_LIFECYCLE_STATUS_IDLE: &str =
    ManagedEventType::RootLifecycleStatusIdle.as_wire_role();
pub(crate) const ROOT_LIFECYCLE_STATUS_RESCHEDULED: &str =
    ManagedEventType::RootLifecycleStatusRescheduled.as_wire_role();
pub(crate) const ROOT_LIFECYCLE_STATUS_RUNNING: &str =
    ManagedEventType::RootLifecycleStatusRunning.as_wire_role();
pub(crate) const ROOT_RESCHEDULED_LIFECYCLE_STATUS_RUNNING: &str =
    ManagedEventType::RootRescheduledLifecycleStatusRunning.as_wire_role();
pub(crate) const ROOT_TERMINAL_STATUS_RUNNING: &str =
    ManagedEventType::RootTerminalStatusRunning.as_wire_role();
pub(crate) const RUN_CONTEXT_COMPACTED: &str = ManagedEventType::RunContextCompacted.as_wire_role();
pub(crate) const SESSION_UPDATED: &str = ManagedEventType::SessionUpdated.as_wire_role();

/// Every fixed role, for the wire-stability pin.
pub(crate) const ALL: &[&str] = &[
    ACTIVATION_FAILED_ERROR,
    ADVISOR_LIFECYCLE_STATUS_TERMINATED,
    ADVISOR_MESSAGE_RECEIVED,
    AGGREGATE_STATUS_IDLE,
    AGGREGATE_STATUS_RUNNING,
    AGGREGATE_TERMINAL_STATUS_RUNNING,
    AGGREGATE_USAGE,
    ARCHIVED_STATUS_TERMINATED,
    BUDGET_REACH_STATUS_IDLE,
    BUDGET_REACH_USAGE,
    CHILD_MESSAGE_RECEIVED,
    COORDINATION_MESSAGE_SENT,
    COORDINATION_STATUS_RUNNING,
    FAILED_LIFECYCLE_STATUS_TERMINATED,
    LIFECYCLE_ERROR,
    LIFECYCLE_STATUS_IDLE,
    LIFECYCLE_STATUS_RESCHEDULED,
    LIFECYCLE_STATUS_RUNNING,
    LINK_STATUS_RUNNING,
    LINK_THREAD_CREATED,
    MODEL_REQUEST_END,
    MODEL_REQUEST_START,
    OUTCOME_ERROR,
    OUTCOME_EVALUATION_END,
    OUTCOME_EVALUATION_ONGOING,
    OUTCOME_EVALUATION_START,
    PARENT_TERMINAL_CHILD_STATUS_TERMINATED,
    PARENT_TERMINAL_PRIMARY_STATUS_TERMINATED,
    PARENT_TERMINAL_SESSION_STATUS_TERMINATED,
    RESCHEDULED_LIFECYCLE_STATUS_RUNNING,
    ROOT_LIFECYCLE_ERROR,
    ROOT_LIFECYCLE_SESSION_STATUS_RESCHEDULED,
    ROOT_LIFECYCLE_STATUS_IDLE,
    ROOT_LIFECYCLE_STATUS_RESCHEDULED,
    ROOT_LIFECYCLE_STATUS_RUNNING,
    ROOT_RESCHEDULED_LIFECYCLE_STATUS_RUNNING,
    ROOT_TERMINAL_STATUS_RUNNING,
    RUN_CONTEXT_COMPACTED,
    SESSION_UPDATED,
];

#[cfg(test)]
mod tests {
    use super::super::{ManagedMultiagentEventProvenance, managed_multiagent_event_id};

    /// Public ids of every fixed role over one fixed coordinate. Captured from
    /// the pre-constant literals; a changed row is a public-id break.
    const PINNED: &[(&str, &str)] = &[
        (
            "activation-failed-error",
            "magent_v1_fnv1a64:61a532fa057a19f1",
        ),
        (
            "advisor-lifecycle-status-terminated",
            "magent_v1_fnv1a64:09355ba9c4963e80",
        ),
        (
            "advisor-message-received",
            "magent_v1_fnv1a64:3ea1aed168a9ce2c",
        ),
        (
            "aggregate-status-idle",
            "magent_v1_fnv1a64:eb809a664559cef5",
        ),
        (
            "aggregate-status-running",
            "magent_v1_fnv1a64:931007ba37bc67fc",
        ),
        (
            "aggregate-terminal-status-running",
            "magent_v1_fnv1a64:3e362402be977a05",
        ),
        ("aggregate-usage", "magent_v1_fnv1a64:fac3b5afc8264d8d"),
        (
            "archived-status-terminated",
            "magent_v1_fnv1a64:10955f77013edd4d",
        ),
        (
            "budget-reach-status-idle",
            "magent_v1_fnv1a64:05b1303fd58c9db3",
        ),
        ("budget-reach-usage", "magent_v1_fnv1a64:3fb9fb472619e25b"),
        (
            "child-message-received",
            "magent_v1_fnv1a64:b9ba6adbedc7e412",
        ),
        (
            "coordination-message-sent",
            "magent_v1_fnv1a64:5f27ddbaaba68e32",
        ),
        (
            "coordination-status-running",
            "magent_v1_fnv1a64:499e5114e01129d2",
        ),
        (
            "failed-lifecycle-status-terminated",
            "magent_v1_fnv1a64:79cb2986f7d8c417",
        ),
        ("lifecycle-error", "magent_v1_fnv1a64:cd5e627b4001f00f"),
        (
            "lifecycle-status-idle",
            "magent_v1_fnv1a64:4d1b454cdd0e1b76",
        ),
        (
            "lifecycle-status-rescheduled",
            "magent_v1_fnv1a64:d35147dde173b856",
        ),
        (
            "lifecycle-status-running",
            "magent_v1_fnv1a64:67bf79972c509729",
        ),
        ("link-status-running", "magent_v1_fnv1a64:221e7abadb089777"),
        ("link-thread-created", "magent_v1_fnv1a64:517f28fd849e2b2e"),
        ("model-request-end", "magent_v1_fnv1a64:305d3207cab63259"),
        ("model-request-start", "magent_v1_fnv1a64:6062492cf1d0610c"),
        ("outcome-error", "magent_v1_fnv1a64:bde36d06f7fccc63"),
        (
            "outcome-evaluation-end",
            "magent_v1_fnv1a64:1e9a43d9275e2f77",
        ),
        (
            "outcome-evaluation-ongoing",
            "magent_v1_fnv1a64:8683316539246eff",
        ),
        (
            "outcome-evaluation-start",
            "magent_v1_fnv1a64:6649bf38cbe13bb6",
        ),
        (
            "parent-terminal-child-status-terminated",
            "magent_v1_fnv1a64:2b83d2a0fd72f64d",
        ),
        (
            "parent-terminal-primary-status-terminated",
            "magent_v1_fnv1a64:3867f5dd7e282399",
        ),
        (
            "parent-terminal-session-status-terminated",
            "magent_v1_fnv1a64:d1dedd99adffb283",
        ),
        (
            "rescheduled-lifecycle-status-running",
            "magent_v1_fnv1a64:ed203b0ca84b8446",
        ),
        ("root-lifecycle-error", "magent_v1_fnv1a64:fa1be80cbb95db30"),
        (
            "root-lifecycle-session-status-rescheduled",
            "magent_v1_fnv1a64:bb08dc0b3a7931d4",
        ),
        (
            "root-lifecycle-status-idle",
            "magent_v1_fnv1a64:19d2a484e9d4c865",
        ),
        (
            "root-lifecycle-status-rescheduled",
            "magent_v1_fnv1a64:abecedd1e0d2c0b3",
        ),
        (
            "root-lifecycle-status-running",
            "magent_v1_fnv1a64:61c6e98d45ca580c",
        ),
        (
            "root-rescheduled-lifecycle-status-running",
            "magent_v1_fnv1a64:f85f91fa9fee48cf",
        ),
        (
            "root-terminal-status-running",
            "magent_v1_fnv1a64:59ea96441be49d80",
        ),
        (
            "run-context-compacted",
            "magent_v1_fnv1a64:7f71a099a106e092",
        ),
        ("session-updated", "magent_v1_fnv1a64:31db6be586bded8a"),
    ];

    #[test]
    fn managed_event_role_ids_are_wire_stable() {
        let actual = super::ALL
            .iter()
            .map(|role| {
                (
                    *role,
                    managed_multiagent_event_id(
                        "sesn_pin",
                        "sthr_pin",
                        role,
                        ManagedMultiagentEventProvenance::Lifecycle { cursor: 7 },
                    ),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual
                .iter()
                .map(|(role, id)| (*role, id.as_str()))
                .collect::<Vec<_>>(),
            PINNED.to_vec()
        );
    }

    #[test]
    fn every_fixed_fact_role_is_pinned() {
        // ID1 every fixed role has a literal pin; adding SessionUpdated must
        // preserve all existing rows. ID2 its real Root/revision provenance
        // yields this exact id; changing the retained revision yields another.
        let update_id = |revision| {
            managed_multiagent_event_id(
                "sesn_pin",
                "sesn_pin",
                super::SESSION_UPDATED,
                ManagedMultiagentEventProvenance::SessionUpdate { revision },
            )
        };
        assert_eq!(update_id(7), "magent_v1_fnv1a64:7fdbf02a99fd91a5");
        assert_ne!(update_id(7), update_id(8));
        assert_eq!(
            super::ALL.to_vec(),
            super::ManagedEventType::FIXED
                .map(super::ManagedEventType::as_wire_role)
                .to_vec()
        );
    }
}
