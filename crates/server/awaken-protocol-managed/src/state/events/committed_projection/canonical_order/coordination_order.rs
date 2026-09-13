//! Coordinate accepted call and terminal-report ordering from indexed facts.
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

pub(super) fn index_coordination_orders(
    record: &SessionRecord,
    links: &[CoordinatedThreadLink],
    root_snapshot: Option<&RunRecoverySnapshot>,
    child_snapshots: &HashMap<String, RunRecoverySnapshot>,
    lifecycle_events: &[RunLifecycleEvent],
    orders: &mut HashMap<String, CanonicalEventOrder>,
    time_for: &impl Fn(&str, u64) -> Option<u64>,
) {
    if links.is_empty() {
        return;
    }
    let mut tool_operation_by_event_id = std::collections::HashMap::<String, String>::new();
    let mut advisor_operation_orders =
        std::collections::HashMap::<String, CanonicalEventOrder>::new();
    let mut index_tool_operations =
        |snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
            if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
                return;
            }
            let run_ids = snapshot
                .runs
                .iter()
                .map(|run| run.id.clone())
                .collect::<Vec<_>>();
            let coordinates =
                ManagedState::assistant_response_coordinates(&snapshot.messages, &run_ids);
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
            for event in &record.events {
                let Some(identity) = decode_managed_tool_event_id(&event.id) else {
                    continue;
                };
                let Some((run_id, step, _)) = coordinates.get(identity.source_id) else {
                    continue;
                };
                tool_operation_by_event_id.insert(
                    event.id.clone(),
                    awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                        &awaken_agent_contract::agent::run::Id(run_id.clone()),
                        *step,
                        identity.call_id,
                    ),
                );
            }
        };
    if let Some(snapshot) = root_snapshot {
        index_tool_operations(snapshot);
    }
    for snapshot in child_snapshots.values() {
        index_tool_operations(snapshot);
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
            let report_source = child_snapshots
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
