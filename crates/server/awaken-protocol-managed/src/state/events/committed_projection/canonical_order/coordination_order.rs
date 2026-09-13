//! Coordinate accepted call and terminal-report ordering from indexed facts.
use super::message_index::MessageEventIndex;
use super::*;
use crate::state::events::coordination_receipts::CoordinationReceipts;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use std::collections::{HashMap, HashSet};

fn accepted_order(
    receipts: &CoordinationReceipts,
    results: &HashMap<&str, CanonicalEventOrder>,
    orders: &HashMap<String, CanonicalEventOrder>,
    source: &Event,
    thread: &str,
) -> Option<CanonicalEventOrder> {
    if receipts.get(&source.id) != Some(thread) {
        return None;
    }
    Some(
        orders
            .get(&source.id)
            .copied()?
            .max(*results.get(source.id.as_str())?),
    )
}

fn index_snapshot_tool_operations(
    session_id: &str,
    thread_id: &str,
    snapshot: &RunRecoverySnapshot,
    event_index: &MessageEventIndex<'_>,
    tool_operation_by_event_id: &mut HashMap<String, String>,
    advisor_operation_orders: &mut HashMap<String, CanonicalEventOrder>,
) {
    if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
        return;
    }
    let run_ids = snapshot
        .runs
        .iter()
        .map(|run| run.id.clone())
        .collect::<Vec<_>>();
    let coordinates = ManagedState::assistant_response_coordinates(&snapshot.messages, &run_ids);
    for (message_index, (message, source_commit_cursor)) in snapshot
        .messages
        .iter()
        .zip(snapshot.message_commit_cursors.iter().copied())
        .enumerate()
    {
        let Some((run_id, step, _)) = coordinates.get(&message.id.0) else {
            continue;
        };
        for (block_ordinal, block) in message.content.iter().enumerate() {
            let ContentBlock::ToolUse {
                id: call_id, name, ..
            } = block
            else {
                continue;
            };
            if name != awaken_runtime_contract::resolved::ADVISOR_TOOL_ID {
                continue;
            }
            let operation_id =
                awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                    &awaken_agent_contract::agent::run::Id(run_id.clone()),
                    *step,
                    call_id,
                );
            let order = CanonicalEventOrder {
                processed_at_unix_ms: snapshot
                    .commit_timestamps
                    .get(&source_commit_cursor)
                    .copied(),
                source_commit_cursor,
                phase: 50,
                ordinal: message_index
                    .saturating_mul(message.content.len().saturating_add(1))
                    .saturating_add(block_ordinal),
            };
            advisor_operation_orders
                .entry(operation_id)
                .and_modify(|current| *current = (*current).min(order))
                .or_insert(order);
        }
    }

    let public_thread = public_thread_id(session_id, thread_id);
    for (source_id, (run_id, step, _)) in &coordinates {
        for (event, call_id) in event_index.tool_events_for_source(&public_thread, source_id) {
            tool_operation_by_event_id.insert(
                event.id.clone(),
                awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                    &awaken_agent_contract::agent::run::Id(run_id.clone()),
                    *step,
                    call_id,
                ),
            );
        }
    }
}

pub(super) struct CoordinationSnapshots<'a> {
    pub(super) root: Option<&'a RunRecoverySnapshot>,
    pub(super) children: &'a HashMap<String, RunRecoverySnapshot>,
}

pub(super) fn index_coordination_orders(
    record: &SessionRecord,
    links: &[CoordinatedThreadLink],
    snapshots: CoordinationSnapshots<'_>,
    lifecycle_events: &[RunLifecycleEvent],
    event_index: &MessageEventIndex<'_>,
    orders: &mut HashMap<String, CanonicalEventOrder>,
    time_for: &impl Fn(&str, u64) -> Option<u64>,
) {
    if links.is_empty() {
        return;
    }
    let mut tool_operation_by_event_id = HashMap::<String, String>::new();
    let mut advisor_operation_orders = HashMap::<String, CanonicalEventOrder>::new();
    if let Some(snapshot) = snapshots.root {
        index_snapshot_tool_operations(
            &record.session.id,
            &record.session.id,
            snapshot,
            event_index,
            &mut tool_operation_by_event_id,
            &mut advisor_operation_orders,
        );
    }
    for (thread_id, snapshot) in snapshots.children {
        index_snapshot_tool_operations(
            &record.session.id,
            thread_id,
            snapshot,
            event_index,
            &mut tool_operation_by_event_id,
            &mut advisor_operation_orders,
        );
    }

    let mut sources_by_operation = HashMap::new();
    for event in &record.events {
        if matches!(event.kind, OutboundKind::AgentToolUse { .. })
            && let Some(operation) = tool_operation_by_event_id.get(&event.id)
        {
            sources_by_operation
                .entry(operation.as_str())
                .or_insert(event);
        }
    }
    let mut wanted = record.events.iter().filter(|event|
        matches!(&event.kind, OutboundKind::AgentToolUse { name, .. } if name == SEND_MESSAGE))
        .map(|event|event.id.as_str()).collect::<HashSet<_>>();
    for link in links {
        if matches!(link.target, CoordinatedThreadTarget::Agent { .. })
            && let Some(source) = sources_by_operation.get(link.created_by_operation_id.as_str())
        {
            wanted.insert(source.id.as_str());
        }
    }
    let receipts =
        CoordinationReceipts::for_calls(&record.events, &record.event_thread_owners, &wanted);
    let mut result_orders = HashMap::new();
    let mut calls_by_thread = HashMap::<&str, Vec<&Event>>::new();
    for event in &record.events {
        if let OutboundKind::AgentToolResult {
            tool_use_id,
            is_error,
            ..
        } = &event.kind
            && *is_error != Some(true)
            && wanted.contains(tool_use_id.as_str())
            && let Some(order) = orders.get(&event.id)
        {
            result_orders.entry(tool_use_id.as_str()).or_insert(*order);
        }
        if matches!(&event.kind, OutboundKind::AgentToolUse { name, .. } if name == SEND_MESSAGE)
            && let Some(thread) = receipts.get(&event.id)
        {
            calls_by_thread.entry(thread).or_default().push(event);
        }
    }
    let mut terminals = HashMap::<&str, Vec<&RunLifecycleEvent>>::new();
    for event in lifecycle_events {
        if matches!(
            event.kind,
            RunLifecycleEventKind::Completed
                | RunLifecycleEventKind::Failed
                | RunLifecycleEventKind::Cancelled
        ) {
            terminals.entry(&event.thread_id.0).or_default().push(event);
        }
    }

    for link in links {
        let source = sources_by_operation
            .get(link.created_by_operation_id.as_str())
            .copied();
        let source_order = match &link.target {
            CoordinatedThreadTarget::Agent { .. } => source.and_then(|source| {
                accepted_order(&receipts, &result_orders, orders, source, &link.thread_id.0)
            }),
            CoordinatedThreadTarget::Advisor { .. } => advisor_operation_orders
                .get(&link.created_by_operation_id)
                .copied(),
        };
        if let Some(source_order) = source_order {
            for (role, phase) in [("link-thread-created", 51), ("link-status-running", 52)] {
                retain_earliest_order(
                    orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &link.thread_id.0,
                        role,
                        ManagedMultiagentEventProvenance::LinkOperation {
                            operation_id: &link.created_by_operation_id,
                        },
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: source_order.processed_at_unix_ms,
                        source_commit_cursor: source_order.source_commit_cursor,
                        phase,
                        ordinal: source_order.ordinal,
                    },
                );
            }
        }
        for source in calls_by_thread
            .get(link.thread_id.0.as_str())
            .into_iter()
            .flatten()
            .copied()
        {
            let Some(source_order) =
                accepted_order(&receipts, &result_orders, orders, source, &link.thread_id.0)
            else {
                continue;
            };
            for (role, phase) in [
                ("coordination-status-running", 54),
                ("coordination-message-sent", 55),
            ] {
                retain_earliest_order(
                    orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &link.thread_id.0,
                        role,
                        ManagedMultiagentEventProvenance::CoordinationCall {
                            event_id: &source.id,
                        },
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: source_order.processed_at_unix_ms,
                        source_commit_cursor: source_order.source_commit_cursor,
                        phase,
                        ordinal: source_order.ordinal,
                    },
                );
            }
        }
        for terminal in terminals
            .get(link.thread_id.0.as_str())
            .into_iter()
            .flatten()
            .copied()
        {
            let report_source = snapshots
                .children
                .get(&link.thread_id.0)
                .and_then(|snapshot| report_commit_cursor(snapshot, &terminal.run_id));
            let Some(source_commit_cursor) = report_source else {
                continue;
            };
            for role in ["child-message-received", "advisor-message-received"] {
                retain_earliest_order(
                    orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &link.thread_id.0,
                        role,
                        ManagedMultiagentEventProvenance::AgentReport {
                            run_id: &terminal.run_id.0,
                        },
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: time_for(
                            &terminal.thread_id.0,
                            terminal.source_commit_cursor,
                        ),
                        source_commit_cursor: source_commit_cursor
                            .max(terminal.source_commit_cursor),
                        phase: 55,
                        ordinal: 0,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn tool_event(id: String) -> Event {
        Event {
            id,
            processed_at: None,
            kind: OutboundKind::AgentToolUse {
                name: SEND_MESSAGE.into(),
                input: serde_json::json!({}),
                evaluated_permission: None,
                evaluation: None,
                session_thread_id: None,
            },
        }
    }

    fn snapshot(thread_id: &str, run_id: &RunId, message: Message) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id: ThreadId(thread_id.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run_id.clone(),
                thread_id: ThreadId(thread_id.into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run_id.clone()),
            messages: vec![message],
            message_commit_cursors: vec![7],
            commit_timestamps: [(7, 1_800_000_000_123)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 7,
            next_commit_ordinal: 1,
        }
    }

    #[test]
    fn tool_operation_join_uses_the_shared_thread_qualified_event_index() {
        // Cause/effect graph: C1 a root Assistant message owns one exact
        // source id; C2 another Thread has a qualified Event with that same
        // source id but no matching committed message; C3 unrelated Event
        // history grows; C4 the source/cursor vectors align. Effects: E1 only
        // the root Event receives the Run/step operation; E2 the foreign
        // Thread Event cannot manufacture an operation; E3 candidate visits
        // remain one after the shared index's single build; E4 the Advisor
        // order retains the message's commit/time/block coordinate.
        // Decision table: A1=C1+C4=>E1,E4; A2=C1+C2+C4=>E1,E2;
        // A3=A2+C3(1,000)=>E1-E3. Missing C4 is covered by the caller's
        // established malformed-snapshot tests and returns before any effect.
        let session_id = "session";
        let run_id = RunId("root-run".into());
        let message = Message::new(
            MessageId::assistant(&run_id, 2),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "root-call",
                awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                serde_json::json!({}),
            )],
        );
        let source_id = message.id.0.clone();
        let root_thread = public_thread_id(session_id, session_id);
        let root_event_id = managed_tool_event_id(&root_thread, &source_id, "root-call");
        let foreign_event_id = managed_tool_event_id("child", &source_id, "foreign-call");
        let mut events = vec![
            tool_event(root_event_id.clone()),
            tool_event(foreign_event_id.clone()),
        ];
        events.extend((0..1_000).map(|index| {
            tool_event(managed_tool_event_id(
                "child",
                &format!("unrelated-{index}"),
                "call",
            ))
        }));
        let event_index = MessageEventIndex::new(&events);
        let mut operations = HashMap::new();
        let mut advisor_orders = HashMap::new();
        index_snapshot_tool_operations(
            session_id,
            session_id,
            &snapshot(session_id, &run_id, message),
            &event_index,
            &mut operations,
            &mut advisor_orders,
        );

        let expected_operation =
            awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                &run_id,
                2,
                "root-call",
            );
        assert_eq!(
            operations.get(&root_event_id),
            Some(&expected_operation),
            "A1/E1"
        );
        assert!(!operations.contains_key(&foreign_event_id), "A2/E2");
        assert_eq!(event_index.examined(), 1, "A3/E3");
        let advisor_order = advisor_orders.get(&expected_operation).expect("A1/E4");
        assert_eq!(
            (
                advisor_order.source_commit_cursor,
                advisor_order.processed_at_unix_ms,
                advisor_order.phase,
                advisor_order.ordinal,
            ),
            (7, Some(1_800_000_000_123), 50, 0),
            "A1/E4",
        );
    }

    #[test]
    fn accepted_order_requires_the_exact_receipt_and_both_committed_anchors() {
        // C1 primary non-error receipt accepts the exact child; C2 source has
        // an anchor; C3 result has an anchor; C4 result committed later despite
        // an earlier wall clock. R1 all C1-C3 => max COMMIT order; R2 !C1 or
        // !C2 or !C3 => no manufactured coordination order; R3 C4 => result's
        // commit/time stays paired. The live late-result test exercises this
        // same function through canonicalization, warm reads and cold recovery.
        let source = Event {
            id: "source".into(),
            processed_at: None,
            kind: OutboundKind::AgentToolUse {
                name: SEND_MESSAGE.into(),
                input: serde_json::json!({}),
                evaluated_permission: None,
                evaluation: None,
                session_thread_id: None,
            },
        };
        let result = Event {
            id: "result".into(),
            processed_at: None,
            kind: OutboundKind::AgentToolResult {
                tool_use_id: "source".into(),
                content: vec![ContentBlock::text(
                    r#"{"accepted":true,"session_thread_id":"child"}"#,
                )],
                is_error: Some(false),
            },
        };
        let receipts =
            CoordinationReceipts::for_calls(&[result], &HashMap::new(), &HashSet::from(["source"]));
        let source_order = CanonicalEventOrder {
            source_commit_cursor: 2,
            phase: 50,
            ordinal: 0,
            processed_at_unix_ms: Some(200),
        };
        let result_order = CanonicalEventOrder {
            source_commit_cursor: 7,
            phase: 50,
            ordinal: 1,
            processed_at_unix_ms: Some(100),
        };
        let orders = HashMap::from([("source".into(), source_order)]);
        let results = HashMap::from([("source", result_order)]);
        let accepted = accepted_order(&receipts, &results, &orders, &source, "child").unwrap();
        assert_eq!(accepted.source_commit_cursor, 7);
        assert_eq!(accepted.processed_at_unix_ms, Some(100));
        assert!(accepted_order(&receipts, &results, &orders, &source, "foreign").is_none());
        assert!(accepted_order(&receipts, &HashMap::new(), &orders, &source, "child").is_none());
        assert!(accepted_order(&receipts, &results, &HashMap::new(), &source, "child").is_none());
    }
}
