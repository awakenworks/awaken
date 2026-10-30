//! Failure versus ordinary archive time at the sole parent-terminal owner.

use super::*;

#[test]
fn terminal_after_idle_interrupt_preserves_the_admitted_prefix() {
    // Cause-effect table: C1 an admitted idle interrupt is already public;
    // C2 cleanup performs no new Runtime commit (equal cursor) or advances it;
    // C3 ordinary closure vs activation failure; C4 absent/forward/backward
    // clocks. R1 C1+C2 => interrupt precedes every new parent terminal;
    // R2 C3 => failure precedes its statuses; R3 C4 changes no ordering.
    // A terminal Root accepts no fresh configuration/event commands, so these
    // terminal facts close this admission prefix. The production position
    // kernel is used; no wall time or previously delivered order is an input.
    use crate::{AnchoredEvent, CoordinationEvents, EventAnchor, position_events};
    use awaken_session_contract::{
        SessionEventInput, SessionEventInterrupt, SessionEventProjectionAnchor,
    };
    let children = HashMap::new();
    let historical = HashMap::new();
    let links = [CoordinatedThreadLink {
        session_id: "session".into(),
        thread_id: awaken_agent_contract::agent::thread::Id("child".into()),
        target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
            agent_id: "writer".into(),
        },
        created_by_operation_id: "operation".into(),
        latest_run_id: None,
    }];
    let coordination = CoordinationEvents {
        cross_posts: vec![],
        coordination_orders: HashMap::new(),
    };
    for admitted_cursor in [0, 372] {
        for cleanup_cursor in [admitted_cursor, admitted_cursor + 1] {
            for failed in [false, true] {
                for archive_time in [None, Some(3000), Some(1)] {
                    let mut batch = SessionEventBatch::compile(
                        &awaken_session_types::SessionId::from("session"),
                        "idle-interrupt",
                        vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                            requested_target: None,
                            targets: vec![
                                awaken_session_execution_contract::SessionThreadTarget::Primary,
                            ],
                        })],
                    )
                    .unwrap();
                    batch.admission_projection_anchor = Some(SessionEventProjectionAnchor {
                        source_commit_cursor: admitted_cursor,
                    });
                    let operation = batch.events[0].event.operation_id().to_owned();
                    let batches = [batch];
                    let index = OrderIndex::new(OrderEvidence {
                        updates: None,
                        session_id: "session",
                        batches: &batches,
                        closed_intervals: &[],
                        budget_reach: &[],
                        lifecycle: &[],
                        root: None,
                        children: &children,
                        links: &links,
                        outcomes: &[],
                        historical_pending: &historical,
                        parent_terminal: ParentTerminalEvidence {
                            cleanup_commit_cursor: Some(cleanup_cursor),
                            activation_failed: failed,
                            failed_at_unix_ms: Some(2000),
                            archived_at_unix_ms: archive_time,
                        },
                    });
                    let keys = [
                        EventIdentity::new(
                            "child",
                            ManagedEventType::ParentTerminalChildStatusTerminated,
                            FactProvenance::ParentTerminal,
                        ),
                        EventIdentity::new(
                            "session",
                            ManagedEventType::ParentTerminalPrimaryStatusTerminated,
                            FactProvenance::ParentTerminal,
                        ),
                        EventIdentity::new(
                            "session",
                            ManagedEventType::ParentTerminalSessionStatusTerminated,
                            FactProvenance::ParentTerminal,
                        ),
                        EventIdentity::new(
                            "session",
                            ManagedEventType::ActivationFailedError,
                            FactProvenance::ParentTerminal,
                        ),
                    ];
                    let mut events = vec![AnchoredEvent {
                        id: "interrupt",
                        wire_type: "user.interrupt",
                        anchor: EventAnchor::Inbound {
                            operation_id: &operation,
                        },
                    }];
                    for (id, key) in ["child", "primary", "session", "failure"]
                        .into_iter()
                        .zip(&keys)
                    {
                        if id != "failure" || failed {
                            events.push(AnchoredEvent {
                                id,
                                wire_type: "terminal",
                                anchor: EventAnchor::Fact(key),
                            });
                        }
                    }
                    let positions = position_events(&index, &coordination, &events).unwrap();
                    let mut ordered = positions.into_iter().collect::<Vec<_>>();
                    ordered.sort_by_key(|(_, position)| *position);
                    let expected = if failed {
                        vec!["interrupt", "failure", "child", "primary", "session"]
                    } else {
                        vec!["interrupt", "child", "primary", "session"]
                    };
                    assert_eq!(
                        ordered.into_iter().map(|(id, _)| id).collect::<Vec<_>>(),
                        expected,
                        "admitted={admitted_cursor}, cleanup={cleanup_cursor}, failed={failed}, time={archive_time:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn parent_terminal_time_has_one_durable_owner_for_every_status_role() {
    // C1 activation failure versus ordinary terminal; C2 failure time known or
    // historical unknown; C3 archive absent or later, including backwards time;
    // C4 primary, aggregate or retained-child terminal status. E1 C1 => only C2;
    // E2 !C1 => existing archive observation; E3 cursor/phase/ordinal unchanged.
    // R1=C1+known+C3+C4=>E1(100)+E3; R2=C1+unknown+C3+C4=>E1(null)+E3;
    // R3=!C1+C2+C3+C4=>E2+E3. Cross every rule with all C3/C4 values; no wall
    // time can select a different identity or position. Runtime fallback without
    // archive remains covered by the existing index's fact-position table.
    let children = HashMap::new();
    let historical = HashMap::new();
    let links = [CoordinatedThreadLink {
        session_id: "session".into(),
        thread_id: awaken_agent_contract::agent::thread::Id("child".into()),
        target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
            agent_id: "coder".into(),
        },
        created_by_operation_id: "operation".into(),
        latest_run_id: None,
    }];
    for failed in [true, false] {
        for failure_time in [Some(100), None] {
            for archive_time in [None, Some(900), Some(1)] {
                let index = OrderIndex::new(OrderEvidence {
                    updates: None,
                    session_id: "session",
                    batches: &[],
                    closed_intervals: &[],
                    budget_reach: &[],
                    lifecycle: &[],
                    root: None,
                    children: &children,
                    links: &links,
                    outcomes: &[],
                    historical_pending: &historical,
                    parent_terminal: ParentTerminalEvidence {
                        cleanup_commit_cursor: Some(9),
                        activation_failed: failed,
                        failed_at_unix_ms: failure_time,
                        archived_at_unix_ms: archive_time,
                    },
                });
                for (thread, role) in [
                    (
                        "session",
                        ManagedEventType::ParentTerminalPrimaryStatusTerminated,
                    ),
                    (
                        "session",
                        ManagedEventType::ParentTerminalSessionStatusTerminated,
                    ),
                    (
                        "child",
                        ManagedEventType::ParentTerminalChildStatusTerminated,
                    ),
                ] {
                    let key = EventIdentity::new(thread, role, FactProvenance::ParentTerminal);
                    let order = index.parent_terminal_order(&key, 9).unwrap();
                    assert_eq!(
                        order.processed_at_unix_ms,
                        if failed { failure_time } else { archive_time },
                        "R1/R2/R3"
                    );
                    assert_eq!(order.source_commit_cursor, 9, "E3 source");
                    assert_eq!(order.phase, role.phase(), "E3 phase");
                    assert_eq!(order.ordinal, Ordinal::FIRST, "E3 ordinal");
                }
            }
        }
    }
}
