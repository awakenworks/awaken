// @formal surface exclude=projection_vocabulary
// @formal owner crates/server/awaken-protocol-managed/src/state/events.rs
// @formal owner crates/server/awaken-protocol-managed/src/state/events/committed_projection/canonical_order.rs
// @formal owner crates/server/awaken-protocol-managed/src/state/events/lifecycle_projection.rs
// @formal reason: Wire-frozen role strings hashed into Managed multi-agent event ids, shared by the
// @formal reason: committed-projection emitters and the canonical order table. Constants only: no
// @formal reason: transition, admission, persistence, retry or effect; the projection modules that
// @formal reason: consume them remain the behavior owners.
//! Stable role coordinates hashed into Managed multi-agent event ids.
//!
//! Consumers: every committed-projection emitter (lifecycle, transcript,
//! interval aggregate, run observation, Outcome, activation failure) and the
//! canonical order table, which rebuilds the same ids to assign their order.
//! Each side used to spell these strings itself, so a typo on either side
//! silently dropped an event out of the order table. The values are hashed
//! into public event ids: they are wire-frozen and pinned by
//! `managed_event_role_ids_are_wire_stable`.

pub(crate) const ACTIVATION_FAILED_ERROR: &str = "activation-failed-error";
pub(crate) const ADVISOR_LIFECYCLE_STATUS_TERMINATED: &str = "advisor-lifecycle-status-terminated";
pub(crate) const ADVISOR_MESSAGE_RECEIVED: &str = "advisor-message-received";
pub(crate) const AGGREGATE_STATUS_IDLE: &str = "aggregate-status-idle";
pub(crate) const AGGREGATE_STATUS_RUNNING: &str = "aggregate-status-running";
pub(crate) const AGGREGATE_TERMINAL_STATUS_RUNNING: &str = "aggregate-terminal-status-running";
pub(crate) const AGGREGATE_USAGE: &str = "aggregate-usage";
pub(crate) const ARCHIVED_STATUS_TERMINATED: &str = "archived-status-terminated";
pub(crate) const BUDGET_REACH_STATUS_IDLE: &str = "budget-reach-status-idle";
pub(crate) const BUDGET_REACH_USAGE: &str = "budget-reach-usage";
pub(crate) const CHILD_MESSAGE_RECEIVED: &str = "child-message-received";
pub(crate) const COORDINATION_MESSAGE_SENT: &str = "coordination-message-sent";
pub(crate) const COORDINATION_STATUS_RUNNING: &str = "coordination-status-running";
pub(crate) const FAILED_LIFECYCLE_STATUS_TERMINATED: &str = "failed-lifecycle-status-terminated";
pub(crate) const LIFECYCLE_ERROR: &str = "lifecycle-error";
pub(crate) const LIFECYCLE_STATUS_IDLE: &str = "lifecycle-status-idle";
pub(crate) const LIFECYCLE_STATUS_RESCHEDULED: &str = "lifecycle-status-rescheduled";
pub(crate) const LIFECYCLE_STATUS_RUNNING: &str = "lifecycle-status-running";
pub(crate) const LINK_STATUS_RUNNING: &str = "link-status-running";
pub(crate) const LINK_THREAD_CREATED: &str = "link-thread-created";
pub(crate) const MODEL_REQUEST_END: &str = "model-request-end";
pub(crate) const MODEL_REQUEST_START: &str = "model-request-start";
pub(crate) const OUTCOME_ERROR: &str = "outcome-error";
pub(crate) const OUTCOME_EVALUATION_END: &str = "outcome-evaluation-end";
pub(crate) const OUTCOME_EVALUATION_ONGOING: &str = "outcome-evaluation-ongoing";
pub(crate) const OUTCOME_EVALUATION_START: &str = "outcome-evaluation-start";
pub(crate) const PARENT_TERMINAL_CHILD_STATUS_TERMINATED: &str =
    "parent-terminal-child-status-terminated";
pub(crate) const PARENT_TERMINAL_PRIMARY_STATUS_TERMINATED: &str =
    "parent-terminal-primary-status-terminated";
pub(crate) const PARENT_TERMINAL_SESSION_STATUS_TERMINATED: &str =
    "parent-terminal-session-status-terminated";
pub(crate) const RESCHEDULED_LIFECYCLE_STATUS_RUNNING: &str =
    "rescheduled-lifecycle-status-running";
pub(crate) const ROOT_LIFECYCLE_ERROR: &str = "root-lifecycle-error";
pub(crate) const ROOT_LIFECYCLE_SESSION_STATUS_RESCHEDULED: &str =
    "root-lifecycle-session-status-rescheduled";
pub(crate) const ROOT_LIFECYCLE_STATUS_IDLE: &str = "root-lifecycle-status-idle";
pub(crate) const ROOT_LIFECYCLE_STATUS_RESCHEDULED: &str = "root-lifecycle-status-rescheduled";
pub(crate) const ROOT_LIFECYCLE_STATUS_RUNNING: &str = "root-lifecycle-status-running";
pub(crate) const ROOT_RESCHEDULED_LIFECYCLE_STATUS_RUNNING: &str =
    "root-rescheduled-lifecycle-status-running";
pub(crate) const ROOT_TERMINAL_STATUS_RUNNING: &str = "root-terminal-status-running";
pub(crate) const RUN_CONTEXT_COMPACTED: &str = "run-context-compacted";

/// Every fixed role, for the wire-stability pin.
#[cfg(test)]
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
}
