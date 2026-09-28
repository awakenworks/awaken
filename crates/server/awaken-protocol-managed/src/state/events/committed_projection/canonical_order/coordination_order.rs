// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Request-local joins reuse the existing committed coordination receipt selector and
// @formal reason: the timeline domain's order of each call and result. They preserve first eligible
// @formal reason: receipt, exact child identity, max(source,result) commit anchoring and terminal-report
// @formal reason: ordering inside the sole private candidate. Adjacent decision tables and warm/cold
// @formal reason: late-result integration tests cover behavior. This is reducer TCB linked to the
// @formal reason: existing paired-publication obligation, not a proof of HashMap joins, full
// @formal reason: canonicalizer equivalence, SQL isolation or performance.
//! Coordinate accepted call and terminal-report ordering from ordered facts.
use super::*;
use crate::state::events::coordination_receipts::CoordinationReceipts;
use awaken_managed_session_events::{FactKey, FactProvenance, FactRole};
use std::collections::{HashMap, HashSet};

fn accepted_order(
    receipts: &CoordinationReceipts,
    results: &HashMap<&str, CanonicalOrder>,
    orders: &HashMap<String, CanonicalOrder>,
    source: &Event,
    thread: &str,
) -> Option<CanonicalOrder> {
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

/// The Run operation behind every issued tool-use event whose source Message
/// is a committed assistant response of a projected Thread.
fn tool_operations(
    session_id: &str,
    events: &[Event],
    index: &OrderIndex<'_>,
) -> HashMap<String, String> {
    events
        .iter()
        .filter_map(|event| {
            let tool = decode_managed_tool_event_id(&event.id)?;
            let thread_id = internal_thread_id(session_id, tool.thread_id);
            let operation = index.tool_operation(&thread_id, tool.source_id, tool.call_id)?;
            Some((event.id.clone(), operation))
        })
        .collect()
}

/// Order every coordination fact of the candidate and return the highest
/// source cursor any coordination candidate occupies, issued or not.
pub(super) fn order_coordination_facts(
    record: &SessionRecord,
    index: &OrderIndex<'_>,
    links: &[CoordinatedThreadLink],
    facts: &[(String, FactKey)],
    orders: &mut HashMap<String, CanonicalOrder>,
) -> u64 {
    if links.is_empty() {
        return 0;
    }
    let session_id = record.session.id.as_str();
    let tool_operation_by_event_id = tool_operations(session_id, &record.events, index);
    let advisor_operation_orders = index.advisor_operation_orders();

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
    for event in index.lifecycle() {
        if matches!(
            event.kind,
            RunLifecycleEventKind::Completed
                | RunLifecycleEventKind::Failed
                | RunLifecycleEventKind::Cancelled
        ) {
            terminals.entry(&event.thread_id.0).or_default().push(event);
        }
    }

    let mut candidates = HashMap::<FactKey, CanonicalOrder>::new();
    for link in links {
        let thread = link.thread_id.0.as_str();
        let source = sources_by_operation
            .get(link.created_by_operation_id.as_str())
            .copied();
        let source_order = match &link.target {
            CoordinatedThreadTarget::Agent { .. } => source.and_then(|source| {
                accepted_order(&receipts, &result_orders, orders, source, thread)
            }),
            CoordinatedThreadTarget::Advisor { .. } => advisor_operation_orders
                .get(&link.created_by_operation_id)
                .copied(),
        };
        if let Some(source_order) = source_order {
            for role in [FactRole::LinkThreadCreated, FactRole::LinkStatusRunning] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::LinkOperation {
                            operation_id: link.created_by_operation_id.clone(),
                        },
                    ),
                    CanonicalOrder {
                        phase: role.phase(),
                        ..source_order
                    },
                );
            }
        }
        for source in calls_by_thread.get(thread).into_iter().flatten().copied() {
            let Some(source_order) =
                accepted_order(&receipts, &result_orders, orders, source, thread)
            else {
                continue;
            };
            for role in [
                FactRole::CoordinationStatusRunning,
                FactRole::CoordinationMessageSent,
            ] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::CoordinationCall {
                            event_id: source.id.clone(),
                        },
                    ),
                    CanonicalOrder {
                        phase: role.phase(),
                        ..source_order
                    },
                );
            }
        }
        for terminal in terminals.get(thread).into_iter().flatten().copied() {
            let Some(order) = index.report_order(terminal) else {
                continue;
            };
            for role in [
                FactRole::ChildMessageReceived,
                FactRole::AdvisorMessageReceived,
            ] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::AgentReport {
                            run_id: terminal.run_id.0.clone(),
                        },
                    ),
                    order,
                );
            }
        }
    }
    for (id, key) in facts {
        if let Some(order) = candidates.get(key) {
            orders.insert(id.clone(), *order);
        }
    }
    candidates
        .values()
        .map(|order| order.source_commit_cursor)
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

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
    fn tool_operation_join_uses_the_thread_qualified_fact_order() {
        // Cause/effect graph: C1 a root Assistant message owns one exact
        // source id; C2 another Thread has a qualified Event with that same
        // source id but no matching committed message; C3 unrelated Event
        // history grows; C4 the source/cursor vectors align. Effects: E1 only
        // the root Event receives the Run/step operation; E2 the foreign
        // Thread Event cannot manufacture an operation; E3 unrelated history
        // manufactures no operation either; E4 the Advisor order retains the
        // message's commit/time/block coordinate.
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
        let root = snapshot(session_id, &run_id, message);
        let children = HashMap::new();
        let historical = HashMap::new();
        let child_pending = HashMap::new();
        let index = OrderIndex::new(awaken_managed_session_events::OrderEvidence {
            session_id,
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: Default::default(),
            lifecycle: &[],
            root: Some(&root),
            children: &children,
            links: &[],
            outcomes: &[],
            historical_pending: &historical,
            root_pending: None,
            child_pending: &child_pending,
        });
        let operations = tool_operations(session_id, &events, &index);

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
        assert_eq!(operations.len(), 1, "A3/E3");
        let advisor_orders = index.advisor_operation_orders();
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
        let source_order = CanonicalOrder {
            source_commit_cursor: 2,
            phase: 50,
            ordinal: 0,
            processed_at_unix_ms: Some(200),
        };
        let result_order = CanonicalOrder {
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
