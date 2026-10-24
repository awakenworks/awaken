//! Failure versus ordinary archive time at the sole parent-terminal owner.

use super::*;

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
