//! The sole warm/cold projection from committed Session, Thread, Run, and Outcome truth.

use super::*;

mod canonical_order;
mod historical_pending;
mod refresh_entrypoint;
mod run_observation_projection;
#[cfg(test)]
pub(super) use canonical_order::budget_reach_close_cursor;
use canonical_order::*;
use historical_pending::historical_pending_by_lifecycle;

impl ManagedState {
    fn replace_interval_aggregate_projections(
        record: &mut SessionRecord,
        persisted: &awaken_session_contract::PersistedSession,
        lifecycle_events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<
            awaken_agent_contract::RunLifecycleCursor,
            Pending,
        >,
        price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
    ) -> Result<(), StateError> {
        let append_running_once = |record: &mut SessionRecord, id: String| {
            if record.events.iter().all(|event| event.id != id) {
                record.events.push(Event {
                    id,
                    kind: OutboundKind::SessionStatusRunning {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
        };
        // Session-level Usage/Idle belongs to the root-owned interval, while
        // Running belongs to the exact lifecycle opening already visible to a
        // client. Closing an interval must never replace that issued cursor.
        // Replace only non-canonical opening candidates and former interval-ID
        // Running projections. A legacy row with no interval history, and
        // independent BudgetReach facts, retain their projections unchanged.
        let has_interval_lineage = !persisted.closed_runtime_intervals.is_empty()
            || persisted
                .running_interval
                .as_ref()
                .is_some_and(|interval| interval.opened_revision.0 != 0);
        if has_interval_lineage {
            let mut replaced_ids = std::collections::HashSet::new();
            let mut prior_close = 0;
            for interval in &persisted.closed_runtime_intervals {
                let close = interval_close_cursor(interval).unwrap_or(prior_close);
                let opening =
                    interval_lifecycle_open_event(interval, prior_close, lifecycle_events);
                let open = opening
                    .map(|event| event.source_commit_cursor)
                    .unwrap_or(close);
                let interval_lifecycle = lifecycle_events
                    .iter()
                    .filter(|event| {
                        event.source_commit_cursor >= open && event.source_commit_cursor <= close
                    })
                    .collect::<Vec<_>>();
                for lifecycle in &interval_lifecycle {
                    if opening.is_some_and(|opening| opening.cursor == lifecycle.cursor) {
                        continue;
                    }
                    replaced_ids.insert(managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-status-running",
                        ManagedMultiagentEventProvenance::Lifecycle {
                            cursor: lifecycle.cursor.0,
                        },
                    ));
                }
                if let Some(terminal_cursor) = interval_lifecycle
                    .iter()
                    .filter(|event| {
                        matches!(
                            event.kind,
                            RunLifecycleEventKind::Awaiting
                                | RunLifecycleEventKind::Completed
                                | RunLifecycleEventKind::Failed
                                | RunLifecycleEventKind::Cancelled
                        )
                    })
                    .map(|event| event.cursor)
                    .max()
                {
                    for role in ["aggregate-usage", "aggregate-status-idle"] {
                        replaced_ids.insert(managed_multiagent_event_id(
                            &record.session.id,
                            &record.session.id,
                            role,
                            ManagedMultiagentEventProvenance::LifecyclePrefix {
                                cursor: terminal_cursor.0,
                            },
                        ));
                    }
                }
                for role in ["aggregate-usage", "aggregate-status-idle"] {
                    replaced_ids.insert(managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        role,
                        ManagedMultiagentEventProvenance::RuntimeInterval {
                            interval_id: &interval.interval_id,
                        },
                    ));
                }
                replaced_ids.insert(managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::RuntimeInterval {
                        interval_id: &interval.interval_id,
                    },
                ));
                prior_close = close;
            }
            if let Some(interval) = persisted.running_interval.as_ref()
                && let Some(opening) = running_interval_lifecycle_open_event(
                    interval,
                    prior_close,
                    &persisted.event_batches,
                    lifecycle_events,
                )
            {
                for lifecycle in lifecycle_events.iter().filter(|event| {
                    event.source_commit_cursor >= opening.source_commit_cursor
                        && event.cursor != opening.cursor
                }) {
                    replaced_ids.insert(managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-status-running",
                        ManagedMultiagentEventProvenance::Lifecycle {
                            cursor: lifecycle.cursor.0,
                        },
                    ));
                }
                replaced_ids.insert(managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::RuntimeInterval {
                        interval_id: &interval.interval_id,
                    },
                ));
            }
            record
                .events
                .retain(|event| !replaced_ids.contains(&event.id));
        }

        let mut prior_close = 0;
        for interval in &persisted.closed_runtime_intervals {
            let close = interval_close_cursor(interval).unwrap_or(prior_close);
            if interval.observations.is_empty() {
                prior_close = close;
                continue;
            }
            let running_id = interval_lifecycle_open_event(interval, prior_close, lifecycle_events)
                .map(|opening| {
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-status-running",
                        ManagedMultiagentEventProvenance::Lifecycle {
                            cursor: opening.cursor.0,
                        },
                    )
                });
            let usage_id = managed_multiagent_event_id(
                &record.session.id,
                &record.session.id,
                "aggregate-usage",
                ManagedMultiagentEventProvenance::RuntimeInterval {
                    interval_id: &interval.interval_id,
                },
            );
            let idle_id = managed_multiagent_event_id(
                &record.session.id,
                &record.session.id,
                "aggregate-status-idle",
                ManagedMultiagentEventProvenance::RuntimeInterval {
                    interval_id: &interval.interval_id,
                },
            );
            let mut usage = session_usage_value(
                interval
                    .usage
                    .to_session_usage()
                    .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?,
                price_snapshot,
            )
            .map_err(StateError::Run)?;
            let budget = interval.max_list_cost_minor.map(|amount| {
                let max_list_cost = crate::types::MonetaryAmount {
                    amount: amount.to_string(),
                    currency: crate::types::Currency::USD,
                };
                if usage.list_cost.is_none() {
                    usage.list_cost = Some(crate::types::MonetaryAmount {
                        amount: "0".to_string(),
                        currency: crate::types::Currency::USD,
                    });
                }
                crate::types::BudgetLimit::Limit { max_list_cost }
            });
            let stop_reason = interval
                .observations
                .last()
                .and_then(|observation| {
                    lifecycle_events.iter().find(|event| {
                        event.cursor == observation.lifecycle_cursor
                            && event.thread_id == observation.thread_id
                            && event.run_id == observation.run_id
                    })
                })
                .map(|terminal| {
                    let owner = (terminal.thread_id.0 != record.session.id)
                        .then_some(terminal.thread_id.0.as_str());
                    Self::public_run_stop_reason(
                        record,
                        owner,
                        &terminal.state,
                        historical_pending.get(&terminal.cursor),
                        terminal.await_reason.as_ref(),
                    )
                })
                .transpose()?
                .unwrap_or(StopReason::EndTurn);
            if let Some(running_id) = running_id {
                append_running_once(record, running_id);
            }
            record.events.extend([
                Event {
                    id: usage_id,
                    kind: OutboundKind::SessionUsage { usage, budget },
                    processed_at: Some(PROCESSED_AT.to_string()),
                },
                Event {
                    id: idle_id,
                    kind: OutboundKind::SessionStatusIdle { stop_reason },
                    processed_at: Some(PROCESSED_AT.to_string()),
                },
            ]);
        }
        if let Some(interval) = persisted.running_interval.as_ref() {
            let prior_close = persisted
                .closed_runtime_intervals
                .iter()
                .filter_map(interval_close_cursor)
                .max()
                .unwrap_or_default();
            if let Some(opening) = running_interval_lifecycle_open_event(
                interval,
                prior_close,
                &persisted.event_batches,
                lifecycle_events,
            ) {
                let running_id = managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: opening.cursor.0,
                    },
                );
                append_running_once(record, running_id);
            }
        }
        Ok(())
    }

    fn canonicalize_committed_events(
        record: &mut SessionRecord,
        persisted: &awaken_session_contract::PersistedSession,
        lifecycle_events: &[RunLifecycleEvent],
        root_snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        child_snapshots: &std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
        links: &[CoordinatedThreadLink],
    ) -> Result<(), StateError> {
        let mut orders = std::collections::HashMap::<String, CanonicalEventOrder>::new();
        let batch_stride = persisted
            .event_batches
            .iter()
            .map(|batch| batch.events.len())
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        for (batch_index, batch) in persisted.event_batches.iter().enumerate() {
            for (ordinal, entry) in batch.events.iter().enumerate() {
                let source_commit_cursor = if let Some(anchor) = entry.projection_anchor {
                    anchor.source_commit_cursor
                } else if entry.processed {
                    // Isolated legacy lineage: rows written before projection
                    // anchors remain visible as one fixed prefix, but are not
                    // included in the new cross-replica ordering guarantee.
                    0
                } else {
                    continue;
                };
                retain_earliest_order(
                    &mut orders,
                    durable_inbound_event_id(&record.session.id, entry.event.operation_id()),
                    CanonicalEventOrder {
                        source_commit_cursor,
                        phase: 0,
                        ordinal: batch_index
                            .saturating_mul(batch_stride)
                            .saturating_add(ordinal),
                    },
                );
            }
        }

        index_lifecycle_orders(&mut orders, &record.session.id, lifecycle_events);

        for interval in &persisted.closed_runtime_intervals {
            let Some(close) = interval_close_cursor(interval) else {
                continue;
            };
            for (role, phase) in [("aggregate-usage", 90), ("aggregate-status-idle", 100)] {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        role,
                        ManagedMultiagentEventProvenance::RuntimeInterval {
                            interval_id: &interval.interval_id,
                        },
                    ),
                    CanonicalEventOrder {
                        source_commit_cursor: close,
                        phase,
                        ordinal: 0,
                    },
                );
            }
        }

        let mut index_messages = |thread_id: &str,
                                  snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
            if snapshot.message_commit_cursors.len() != snapshot.messages.len() {
                return;
            }
            let run_ids = snapshot.runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
            let response_coordinates = Self::assistant_response_coordinates(&snapshot.messages, &run_ids);
            let public_owner_thread_id = public_thread_id(&record.session.id, thread_id);
            let event_count = record.events.len();
            for (message_index, (message, source_commit_cursor)) in snapshot
                .messages
                .iter()
                .zip(snapshot.message_commit_cursors.iter().copied())
                .enumerate()
            {
                for event in &record.events {
                    let tool_source_ordinal = decode_managed_tool_event_id(&event.id)
                        .filter(|identity| {
                            identity.thread_id == public_owner_thread_id.as_str()
                                && identity.source_id == message.id.0
                        })
                        .and_then(|identity| {
                            message.content.iter().position(|block| {
                                matches!(block, ContentBlock::ToolUse { id, .. } if id == identity.call_id)
                                    || matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == identity.call_id)
                            })
                        });
                    let generic_source_ordinal = (0..=event_count).find(|ordinal| {
                        event.id == managed_multiagent_event_id(
                                &record.session.id,
                                thread_id,
                                event.kind.type_str(),
                                ManagedMultiagentEventProvenance::Message {
                                    message_id: &message.id.0,
                                    ordinal: *ordinal,
                                },
                            )
                    });
                    let assistant_source_ordinal = response_coordinates
                        .get(&message.id.0)
                        .and_then(|(run_id, step, response)| {
                            (event.id
                                == managed_assistant_event_id(
                                    &record.session.id,
                                    thread_id,
                                    run_id,
                                    *step,
                                    *response,
                                    event.kind.type_str(),
                                ))
                            .then(|| match &event.kind {
                                OutboundKind::AgentThinking {} => 0,
                                OutboundKind::AgentMessage { .. } => usize::from(
                                    message
                                        .content
                                        .iter()
                                        .any(|block| matches!(block, ContentBlock::Thinking { .. })),
                                ),
                                _ => 0,
                            })
                        });
                    if tool_source_ordinal.is_some()
                        || generic_source_ordinal.is_some()
                        || assistant_source_ordinal.is_some()
                    {
                        retain_earliest_order(
                            &mut orders,
                            event.id.clone(),
                            CanonicalEventOrder {
                                source_commit_cursor,
                                phase: 50,
                                ordinal: message_index
                                    .saturating_mul(event_count.saturating_add(1))
                                    .saturating_add(
                                        tool_source_ordinal
                                            .or(generic_source_ordinal)
                                            .or(assistant_source_ordinal)
                                            .unwrap_or_default(),
                                    ),
                            },
                        );
                    }
                }
            }
        };
        if let Some(snapshot) = root_snapshot {
            index_messages(&record.session.id, snapshot);
        }
        for (thread_id, snapshot) in child_snapshots {
            index_messages(thread_id, snapshot);
        }

        let mut index_snapshot_facts = |thread_id: &str,
                                        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
            for audit in &snapshot.events {
                let source_commit_cursor = awaken_agent_contract::decode_run_lifecycle_cursor(
                    awaken_agent_contract::RunLifecycleCursor(audit.sequence),
                )
                .0;
                for (role, phase) in [
                    ("model-request-start", 40),
                    ("model-request-end", 41),
                ] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            role,
                            ManagedMultiagentEventProvenance::Audit {
                                run_id: &audit.run_id.0,
                                sequence: audit.sequence,
                            },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor,
                            phase,
                            ordinal: usize::try_from(audit.sequence).unwrap_or(usize::MAX),
                        },
                    );
                }
            }
            for run in &snapshot.runs {
                if let Some(source_commit_cursor) =
                    awaken_runtime_contract::compaction::RunCompactionMarker::recorded_commit_cursor(
                        &snapshot.state,
                        &snapshot.state_commit_cursors,
                        &run.id.0,
                    )
                {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "run-context-compacted",
                            ManagedMultiagentEventProvenance::RunState {
                                run_id: &run.id.0,
                            },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor,
                            phase: 42,
                            ordinal: 0,
                        },
                    );
                }
            }
            if let Some(source_commit_cursor) =
                awaken_agent_contract::archived_thread_commit_cursor(
                    &snapshot.state,
                    &snapshot.state_commit_cursors,
                )
            {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        thread_id,
                        "archived-status-terminated",
                        ManagedMultiagentEventProvenance::ArchivedDisposition,
                    ),
                    CanonicalEventOrder {
                        source_commit_cursor,
                        phase: 81,
                        ordinal: 0,
                    },
                );
            }
        };
        if let Some(snapshot) = root_snapshot {
            index_snapshot_facts(&record.session.id, snapshot);
        }
        for (thread_id, snapshot) in child_snapshots {
            index_snapshot_facts(thread_id, snapshot);
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
                    Self::assistant_response_coordinates(&snapshot.messages, &run_ids);
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

        for link in links {
            let source = record.events.iter().find(|event| {
                matches!(event.kind, OutboundKind::AgentToolUse { .. })
                    && tool_operation_by_event_id.get(&event.id)
                        == Some(&link.created_by_operation_id)
            });
            let source_order = match &link.target {
                CoordinatedThreadTarget::Agent { .. } => source.and_then(|source| {
                    accepted_coordination_order(record, &orders, source, &link.thread_id.0)
                }),
                CoordinatedThreadTarget::Advisor { .. } => advisor_operation_orders
                    .get(&link.created_by_operation_id)
                    .copied(),
            };
            if let Some(source_order) = source_order {
                for (role, phase) in [("link-thread-created", 15), ("link-status-running", 16)] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::LinkOperation {
                                operation_id: &link.created_by_operation_id,
                            },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor: source_order.source_commit_cursor,
                            phase,
                            ordinal: source_order.ordinal,
                        },
                    );
                }
            }
            for source in record.events.iter().filter(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::AgentToolUse { name, .. } if name == SEND_TO_AGENT
                )
            }) {
                let Some(source_order) =
                    accepted_coordination_order(record, &orders, source, &link.thread_id.0)
                else {
                    continue;
                };
                for (role, phase) in [
                    ("coordination-status-running", 54),
                    ("coordination-message-sent", 55),
                ] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::CoordinationCall {
                                event_id: &source.id,
                            },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor: source_order.source_commit_cursor,
                            phase,
                            ordinal: source_order.ordinal,
                        },
                    );
                }
            }
            for terminal in lifecycle_events.iter().filter(|event| {
                event.thread_id == link.thread_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Completed
                            | RunLifecycleEventKind::Failed
                            | RunLifecycleEventKind::Cancelled
                    )
            }) {
                let report_source = child_snapshots
                    .get(&link.thread_id.0)
                    .and_then(|snapshot| report_commit_cursor(snapshot, &terminal.run_id));
                let Some(source_commit_cursor) = report_source else {
                    continue;
                };
                for role in ["child-message-received", "advisor-message-received"] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::AgentReport {
                                run_id: &terminal.run_id.0,
                            },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor: source_commit_cursor
                                .max(terminal.source_commit_cursor),
                            phase: 55,
                            ordinal: 0,
                        },
                    );
                }
            }
        }

        for (batch_index, batch) in persisted.event_batches.iter().enumerate() {
            for entry in &batch.events {
                let SessionEventCommand::DefineOutcome { outcome_id, .. } = &entry.event else {
                    continue;
                };
                for iteration in 0..=record.events.len() {
                    let state_key = format!("outcome/{outcome_id}/evaluation/{iteration}");
                    let source_commit_cursor = root_snapshot.and_then(|snapshot| {
                        (snapshot.state.len() == snapshot.state_commit_cursors.len())
                            .then(|| {
                                snapshot
                                    .state
                                    .iter()
                                    .zip(snapshot.state_commit_cursors.iter().copied())
                                    .find_map(|(command, cursor)| {
                                        (command.key.0 == state_key).then_some(cursor)
                                    })
                            })
                            .flatten()
                    });
                    let Some(source_commit_cursor) = source_commit_cursor else {
                        continue;
                    };
                    for (role, phase) in [
                        ("outcome-evaluation-start", 60),
                        ("outcome-evaluation-ongoing", 61),
                        ("outcome-evaluation-end", 62),
                    ] {
                        retain_earliest_order(
                            &mut orders,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                role,
                                ManagedMultiagentEventProvenance::OutcomeEvaluation {
                                    outcome_id,
                                    iteration: u32::try_from(iteration).unwrap_or(u32::MAX),
                                },
                            ),
                            CanonicalEventOrder {
                                source_commit_cursor,
                                phase,
                                ordinal: batch_index
                                    .saturating_mul(batch_stride)
                                    .saturating_add(iteration),
                            },
                        );
                    }
                }
                let state_key = format!("outcome/{outcome_id}/state");
                if let Some(source_commit_cursor) = root_snapshot.and_then(|snapshot| {
                    (snapshot.state.len() == snapshot.state_commit_cursors.len())
                        .then(|| {
                            snapshot
                                .state
                                .iter()
                                .zip(snapshot.state_commit_cursors.iter().copied())
                                .rev()
                                .find_map(|(command, cursor)| {
                                    (command.key.0 == state_key).then_some(cursor)
                                })
                        })
                        .flatten()
                }) {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &record.session.id,
                            "outcome-error",
                            ManagedMultiagentEventProvenance::Outcome { outcome_id },
                        ),
                        CanonicalEventOrder {
                            source_commit_cursor,
                            phase: 63,
                            ordinal: batch_index.saturating_mul(batch_stride),
                        },
                    );
                }
            }
        }

        for (index, transition) in persisted.budget.reach_transitions().iter().enumerate() {
            let Some(source_commit_cursor) = budget_reach_close_cursor(persisted, transition)
            else {
                continue;
            };
            for (role, phase) in [
                ("budget-reach-usage", 90),
                ("budget-reach-status-idle", 100),
            ] {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        role,
                        ManagedMultiagentEventProvenance::BudgetReach {
                            generation: transition.generation,
                        },
                    ),
                    CanonicalEventOrder {
                        source_commit_cursor,
                        phase,
                        ordinal: index,
                    },
                );
            }
        }

        let terminal_source = persisted.terminal_cleanup.runtime_commit_cursor();
        for (terminal_source, thread_id, role, phase) in
            terminal_source.into_iter().flat_map(|terminal_source| {
                links
                    .iter()
                    .map(|link| {
                        (
                            link.thread_id.0.as_str(),
                            "parent-terminal-child-status-terminated",
                            110,
                        )
                    })
                    .chain([
                        (
                            record.session.id.as_str(),
                            "parent-terminal-primary-status-terminated",
                            111,
                        ),
                        (
                            record.session.id.as_str(),
                            "parent-terminal-session-status-terminated",
                            112,
                        ),
                    ])
                    .map(move |(thread_id, role, phase)| (terminal_source, thread_id, role, phase))
            })
        {
            retain_earliest_order(
                &mut orders,
                managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    role,
                    ManagedMultiagentEventProvenance::ParentTerminal,
                ),
                CanonicalEventOrder {
                    source_commit_cursor: terminal_source,
                    phase,
                    ordinal: 0,
                },
            );
        }

        let visible_durable_ids = record
            .events
            .iter()
            .filter(|event| !is_transient_event_id(&event.id))
            .map(|event| event.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut still_pending = Vec::new();
        for (event, predecessor) in std::mem::take(&mut record.pending_transient_events) {
            if visible_durable_ids.contains(&predecessor) {
                record.events.push(event);
            } else {
                still_pending.push((event, predecessor));
            }
        }
        record.pending_transient_events = still_pending;

        // Canonicalization is a disposable projection transaction: validate
        // every immutable anchor against a snapshot of the issued prefix, then
        // replace the cache once. A late/malformed fact must fail closed without
        // erasing receipts and cursors that callers already observed.
        let original_events = record.events.clone();
        let mut durable = Vec::new();
        for event in original_events.iter().cloned() {
            if is_transient_event_id(&event.id) {
                continue;
            } else {
                let order = orders.get(&event.id).copied().ok_or_else(|| {
                    StateError::Run(RunError::internal(format!(
                        "committed Managed event {} ({}) has no immutable projection anchor",
                        event.id,
                        event.type_str(),
                    )))
                })?;
                durable.push((order, event));
            }
        }
        durable.sort_by(|(left_order, left), (right_order, right)| {
            left_order
                .cmp(right_order)
                .then_with(|| left.id.cmp(&right.id))
        });
        let durable_ids = durable
            .iter()
            .map(|(_, event)| event.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut overlays = std::collections::HashMap::<Option<String>, Vec<Event>>::new();
        for (index, event) in original_events
            .iter()
            .enumerate()
            .filter(|(_, event)| is_transient_event_id(&event.id))
        {
            let anchor = record
                .transient_event_anchors
                .get(&event.id)
                .filter(|candidate| durable_ids.contains(candidate.as_str()))
                .cloned()
                .or_else(|| {
                    original_events[..index]
                        .iter()
                        .rev()
                        .find(|candidate| durable_ids.contains(candidate.id.as_str()))
                        .map(|candidate| candidate.id.clone())
                });
            overlays.entry(anchor).or_default().push(event.clone());
        }
        let mut canonical = overlays.remove(&None).unwrap_or_default();
        for (_, event) in durable {
            let id = event.id.clone();
            canonical.push(event);
            if let Some(mut anchored) = overlays.remove(&Some(id)) {
                canonical.append(&mut anchored);
            }
        }
        debug_assert!(
            overlays.is_empty(),
            "every transient overlay has a durable anchor"
        );
        record.events = canonical;
        Ok(())
    }

    async fn refresh_committed_projection_once(
        &self,
        session_id: &str,
    ) -> Result<bool, StateError> {
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Refresh its disposable
        // wire projection before reading runtime events so GET cannot retain a
        // stale preparing/idle status as a parallel lifecycle authority.
        let persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        if persisted.event_batches.iter().any(|batch| {
            batch
                .events
                .iter()
                .any(|entry| !entry.processed && entry.projection_anchor.is_none())
        }) {
            // The accepted response remains available to the caller, but a
            // listable Runtime suffix cannot cross the earliest command whose
            // effect has not yet supplied its immutable anchor. Holding the
            // prior disposable prefix prevents the eventual input from being
            // inserted before an already-issued lifecycle cursor.
            return Ok(true);
        }
        let persisted_status = Self::wire_session_status(persisted.execution);
        let price_snapshot = persisted.budget.price_snapshot().cloned();
        let budget_reach_projections = persisted
            .budget
            .reach_transitions()
            .iter()
            .filter(|transition| budget_reach_close_cursor(&persisted, transition).is_some())
            .map(|transition| {
                let mut usage = session_usage_value(
                    transition
                        .usage_cursor
                        .to_session_usage()
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?,
                    None,
                )
                .map_err(StateError::Run)?;
                usage.list_cost = transition.public_list_cost_minor().map(|amount| {
                    crate::types::MonetaryAmount {
                        amount: amount.to_string(),
                        currency: crate::types::Currency::USD,
                    }
                });
                Ok((
                    transition.generation,
                    usage,
                    crate::types::BudgetLimit::Limit {
                        max_list_cost: crate::types::MonetaryAmount {
                            amount: transition.max_list_cost_minor.to_string(),
                            currency: crate::types::Currency::USD,
                        },
                    },
                ))
            })
            .collect::<Result<Vec<_>, StateError>>()?;
        self.refresh_cached_projection(&persisted)?;
        let initial_cursor = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .ok_or(StateError::NotFound)?
            .projected_lifecycle_cursor;
        // Read the root fence before the later relationship snapshot. This
        // ordering makes an unknown lifecycle event classifiable without
        // guessing from its Thread id: a relationship read taken after the
        // root fence must include every coordinated link committed at or before
        // that fence. An event beyond the fence remains unconsumed until the
        // next refresh, when its matching root/link snapshot can catch up.
        let root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let mut durable_outcome_projections = Vec::new();
        for outcome_id in retained_outcome_ids(&persisted.event_batches) {
            if let Some(terminal) = self
                .application
                .committed_outcome_projection(session_id, &outcome_id)
                .await
                .map_err(StateError::Run)?
            {
                durable_outcome_projections
                    .push(validate_durable_outcome_projection(outcome_id, terminal)?);
            }
        }
        let (links, child_snapshots) = self.coordinated_projection_prefix(session_id).await?;
        // The lifecycle feed may advance after the snapshots. Each event carries
        // its source commit cursor, so the recovery snapshot's store cursor can
        // fence it to the exact prefix whose transcript, ticket and disposition
        // were read atomically. A terminal can therefore never close SSE before
        // that same prefix exposes its final output; opaque lifecycle event
        // cursors are never compared with commit-sequence cursors.
        const LIFECYCLE_PAGE_SIZE: usize = 256;
        // The disposable cursor is only a live-delivery optimization. Canonical
        // list reconstruction always folds the complete retained lifecycle so a
        // cold replica and a warm replica derive the same interval buckets.
        let mut read_cursor = awaken_agent_contract::RunLifecycleCursor::default();
        let mut lifecycle_events = Vec::new();
        loop {
            let page = self
                .application
                .committed_run_lifecycle(session_id, read_cursor, LIFECYCLE_PAGE_SIZE)
                .await
                .map_err(StateError::Run)?;
            let count = page.events.len();
            lifecycle_events.extend(page.events);
            if page.next_cursor == read_cursor || count < LIFECYCLE_PAGE_SIZE {
                break;
            }
            read_cursor = page.next_cursor;
        }
        let mut transcripts = child_snapshots
            .iter()
            .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.messages.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let child_latest_run_states = child_snapshots
            .iter()
            .filter_map(|(thread_id, snapshot)| {
                let latest_run_id = snapshot.latest_run_id.as_ref()?;
                snapshot
                    .runs
                    .iter()
                    .find(|run| &run.id == latest_run_id)
                    .map(|run| (thread_id.clone(), run.state.clone()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut child_pending = std::collections::HashMap::new();
        for (thread_id, snapshot) in &child_snapshots {
            if let Some(pending) = Self::pending_from_recovery_snapshot(snapshot)? {
                child_pending.insert(thread_id.clone(), pending);
            }
        }
        let child_dispositions = child_snapshots
            .iter()
            .map(|(thread_id, snapshot)| {
                awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                    .map(|disposition| (thread_id.clone(), disposition))
                    .map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "recover coordinated Thread disposition: {error}"
                        )))
                    })
            })
            .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
        let root_pending = root_snapshot
            .as_ref()
            .map(Self::pending_from_recovery_snapshot)
            .transpose()?
            .flatten();
        let messages = root_snapshot
            .as_ref()
            .map(|snapshot| snapshot.messages.clone())
            .unwrap_or_default();
        let root_run_ids = root_snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .runs
                    .iter()
                    .map(|run| run.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let root_response_coordinates =
            Self::assistant_response_coordinates(&messages, &root_run_ids);
        let durable_inbound_projections =
            durable_inbound_projections(session_id, &persisted.event_batches);
        let usage = self
            .application
            .session_usage(session_id)
            .await
            .map_err(StateError::Run)?;
        let primary_thread_usage = self
            .application
            .session_thread_usage(session_id, session_id)
            .await
            .map_err(StateError::Run)?;
        let primary_thread_usage =
            session_thread_usage_value(primary_thread_usage, price_snapshot.as_ref())
                .map_err(StateError::Run)?;
        let mut child_thread_usage = std::collections::HashMap::new();
        for link in &links {
            let usage = self
                .application
                .session_thread_usage(session_id, &link.thread_id.0)
                .await
                .map_err(StateError::Run)?;
            let usage = session_thread_usage_value(usage, price_snapshot.as_ref())
                .map_err(StateError::Run)?;
            child_thread_usage.insert(link.thread_id.0.clone(), usage);
        }

        let known_children = links
            .iter()
            .map(|link| link.thread_id.0.as_str())
            .collect::<std::collections::HashSet<_>>();
        let root_store_cursor = root_snapshot.as_ref().map(|snapshot| snapshot.store_cursor);
        let snapshot_for_thread = |thread_id: &str| {
            if thread_id == session_id {
                root_snapshot.as_ref()
            } else {
                child_snapshots.get(thread_id)
            }
        };
        let latest_cursor_by_thread = lifecycle_events
            .iter()
            .filter(|event| {
                snapshot_for_thread(&event.thread_id.0)
                    .is_some_and(|snapshot| event.source_commit_cursor <= snapshot.store_cursor)
            })
            .fold(std::collections::HashMap::new(), |mut latest, event| {
                latest.insert(event.thread_id.0.clone(), event.cursor);
                latest
            });
        let mut accepted_cursor = initial_cursor;
        let mut accepted_lifecycle = Vec::new();
        let mut blocked_transcript_threads = std::collections::HashSet::new();
        for event in lifecycle_events {
            let thread_id = event.thread_id.0.as_str();
            let is_root = thread_id == session_id;
            let is_known_child = known_children.contains(thread_id);
            if !is_root && !is_known_child {
                // The relationship read happened after `root_store_cursor`.
                // Therefore an event inside that prefix which still has no
                // relationship is out-of-scope lifecycle (another Postgres
                // Session or invisible auxiliary work) and can advance the
                // feed cursor without creating a public Thread. An event beyond
                // the prefix may be a just-created child whose link/snapshot is
                // not visible yet, so it remains replayable. Thread-id spelling
                // is deliberately never used as provenance.
                if root_store_cursor.is_some_and(|cursor| event.source_commit_cursor <= cursor) {
                    accepted_cursor = event.cursor;
                    continue;
                }
                break;
            }
            let snapshot_is_behind = snapshot_for_thread(thread_id)
                .is_none_or(|snapshot| event.source_commit_cursor > snapshot.store_cursor);
            if snapshot_is_behind {
                break;
            }
            let is_current_awaiting = event.kind == RunLifecycleEventKind::Awaiting
                && latest_cursor_by_thread.get(thread_id) == Some(&event.cursor);
            let has_pending_ticket = if is_root {
                root_pending.is_some()
            } else {
                child_pending.contains_key(thread_id)
            };
            let is_budget_pause = event.await_reason.as_ref()
                == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
            // A current Awaiting transition cannot be published without its
            // exact answerable ticket. Historical Awaiting transitions may be
            // followed by a committed resume and therefore need no live ticket.
            if is_current_awaiting && !has_pending_ticket && !is_budget_pause {
                // Transcript and lifecycle are one public classification. If the
                // message were marked projected before its ticket became visible,
                // a ToolUse would be irreversibly downgraded to allow and could
                // never become the answerable requires-action event.
                blocked_transcript_threads.insert(thread_id.to_string());
                break;
            }
            accepted_cursor = event.cursor;
            accepted_lifecycle.push(event);
        }
        let all_accepted_lifecycle = accepted_lifecycle.clone();
        let historical_pending =
            historical_pending_by_lifecycle(&all_accepted_lifecycle, |thread_id| {
                if thread_id == session_id {
                    root_snapshot.as_ref()
                } else {
                    child_snapshots.get(thread_id)
                }
            })?;
        for thread_id in &blocked_transcript_threads {
            transcripts.remove(thread_id);
        }

        // Session revision does not change for Runtime commits. Fence the root
        // Thread prefix again after child/link reads, while ignoring the
        // backend-wide diagnostic cursor advanced by unrelated Threads.
        let (final_links, final_child_snapshots) =
            self.coordinated_projection_prefix(session_id).await?;
        if links != final_links
            || child_snapshots.len() != final_child_snapshots.len()
            || child_snapshots.iter().any(|(thread_id, initial)| {
                !same_thread_projection_prefix(Some(initial), final_child_snapshots.get(thread_id))
            })
        {
            return Ok(false);
        }
        let final_root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        if !same_thread_projection_prefix(root_snapshot.as_ref(), final_root_snapshot.as_ref()) {
            return Ok(false);
        }

        let final_persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        if final_persisted.session_id != persisted.session_id
            || final_persisted.revision != persisted.revision
        {
            return Ok(false);
        }

        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let previous_event_ids = record
            .events
            .iter()
            .map(|event| event.id.clone())
            .collect::<std::collections::HashSet<_>>();
        merge_durable_inbound_projections(record, durable_inbound_projections);
        record.primary_thread_usage = primary_thread_usage;
        let unseen_budget_reach = budget_reach_projections
            .iter()
            .filter(|(generation, _, _)| *generation > record.projected_budget_reach_generation)
            .cloned()
            .collect::<Vec<_>>();
        let current_cursor = record.projected_lifecycle_cursor;
        accepted_lifecycle.retain(|event| event.cursor > current_cursor);
        if accepted_cursor > record.projected_lifecycle_cursor {
            record.projected_lifecycle_cursor = accepted_cursor;
        }
        let root_pending = root_pending.as_ref();
        let transcript_evidence = project::committed_transcript_evidence(&messages, root_pending);
        let new_messages = if blocked_transcript_threads.contains(session_id) {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| !record.message_was_projected(session_id, &message.id.0))
                .cloned()
                .collect::<Vec<_>>()
        };
        let root_terminal = accepted_lifecycle.iter().rev().find(|event| {
            event.thread_id.0 == session_id
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        });
        let unseen_root_terminal = root_terminal
            .filter(|event| !record.projected_terminal_cursors.contains(&event.cursor));
        // The public aggregate opening belongs to the exact committed root
        // opening transition, not to this Coordinator's process-local
        // observation order. A warm replica can see Running before a cold peer
        // first sees the same prefix at terminal; a resumed Run can also open a
        // later interval under the same Run id. The lifecycle cursor is the one
        // durable coordinate that is both known at the opening and unique for
        // every reopen.
        let aggregate_running_event_id = accepted_lifecycle
            .iter()
            .rev()
            .find(|event| {
                event.thread_id.0 == session_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Running
                            | RunLifecycleEventKind::Resumed
                            | RunLifecycleEventKind::Rescheduled
                    )
            })
            .map(|event| {
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                )
            });
        let aggregate_running_transition =
            Self::should_append_aggregate_running(record, persisted_status);
        if aggregate_running_transition && let Some(id) = aggregate_running_event_id.clone() {
            record.events.push(Event {
                id,
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // A warm reader or a restarted replica may first observe a Run only
        // after its terminal commit and the aggregate has already returned to
        // Idle. Reconstruct the missing opening edge before projecting output;
        // otherwise cold recovery yields agent output before
        // `session.status_running`, unlike the ordinary foreground path. The
        // terminal lifecycle cursor plus the aggregate Event tail are the
        // existing idempotency authorities, so no create-specific marker is
        // needed. An active child would keep the durable aggregate Running; an
        // Idle root is therefore already the closed-interval case.
        if unseen_root_terminal.is_some()
            && persisted_status == SessionStatus::Idle
            && !aggregate_running_transition
            && Self::needs_aggregate_running_edge(record)
            && let Some(id) = aggregate_running_event_id
        {
            record.events.push(Event {
                id,
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Root Thread lifecycle is projected from the same accepted lifecycle
        // prefix as the aggregate Session edges. A foreground command already
        // inserted the bracket when its terminal cursor is present in the one
        // terminal set; otherwise warm/cold recovery emits every newly accepted
        // Running/Rescheduled edge. If a cold prefix starts at a terminal, the
        // event tail reconstructs the missing opening edge before output.
        let project_root_transitions = root_terminal.is_none() || unseen_root_terminal.is_some();
        let mut projected_root_opening = false;
        if project_root_transitions {
            for lifecycle in accepted_lifecycle.iter().filter(|event| {
                event.thread_id.0 == session_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Running
                            | RunLifecycleEventKind::Resumed
                            | RunLifecycleEventKind::Rescheduled
                    )
            }) {
                match lifecycle.kind {
                    RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed => {
                        let status = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-status-running",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Running,
                        );
                        record.events.push(status);
                        projected_root_opening = true;
                    }
                    RunLifecycleEventKind::Rescheduled => {
                        let session_rescheduled = Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-session-status-rescheduled",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            kind: OutboundKind::SessionStatusRescheduled {},
                            processed_at: Some(PROCESSED_AT.to_string()),
                        };
                        let thread_rescheduled = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-status-rescheduled",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Rescheduled,
                        );
                        let replacement_running = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-rescheduled-lifecycle-status-running",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Running,
                        );
                        record.events.extend([
                            session_rescheduled,
                            thread_rescheduled,
                            replacement_running,
                        ]);
                        projected_root_opening = true;
                    }
                    _ => unreachable!("filtered root lifecycle transition"),
                }
            }
        }
        if !projected_root_opening
            && !Self::primary_thread_status_is_open(record)
            && let Some(terminal) = unseen_root_terminal
        {
            let reconstructed = primary_thread_status_event(
                record,
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "root-terminal-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                PrimaryThreadStatusProjection::Running,
            );
            record.events.push(reconstructed);
        }
        if !blocked_transcript_threads.contains(session_id)
            && let Some(snapshot) = root_snapshot.as_ref()
        {
            Self::append_run_observation_projections(record, session_id, snapshot)?;
        }
        let root_pending_source_run_id = root_terminal
            .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
            .map(|event| &event.run_id);
        let mut pending_is_in_new_message = false;
        for message in &new_messages {
            let historical_message_boundary =
                historical_pending.iter().find_map(|(cursor, pending)| {
                    message
                        .content
                        .iter()
                        .any(|block| {
                            matches!(
                                block,
                                ContentBlock::ToolUse { id, .. } if id == &pending.tool_use_id
                            )
                        })
                        .then(|| {
                            all_accepted_lifecycle
                                .iter()
                                .find(|event| event.cursor == *cursor)
                                .map(|event| (pending, &event.run_id))
                        })
                        .flatten()
                });
            let message_source_run_id = historical_message_boundary
                .map(|(_, run_id)| run_id)
                .or(root_pending_source_run_id);
            let tool_index = record.projected_tool_index_for(None);
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                message_source_run_id.map(|run_id| run_id.0.as_str()),
            );
            pending_is_in_new_message |= selected.pending_occurrence;
            let message_pending = historical_message_boundary
                .map(|(pending, _)| pending)
                .or_else(|| {
                    selected
                        .retained_pending_occurrence
                        .then_some(root_pending)
                        .flatten()
                });
            let mut projected = project_messages_with_mcp_ids(
                std::slice::from_ref(&selected.message),
                message_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
            );
            self.qualify_tool_projection(
                record,
                None,
                std::slice::from_ref(&selected.message),
                message_source_run_id,
                &mut projected,
            )?;
            for (ordinal, projected) in projected.into_iter().enumerate() {
                let kind = projected.kind;
                let id = projected.id.unwrap_or_else(|| {
                    match (&kind, root_response_coordinates.get(&message.id.0)) {
                        (
                            OutboundKind::AgentMessage { .. } | OutboundKind::AgentThinking {},
                            Some((run_id, step, response)),
                        ) => managed_assistant_event_id(
                            &record.session.id,
                            session_id,
                            run_id,
                            *step,
                            *response,
                            kind.type_str(),
                        ),
                        _ => managed_multiagent_event_id(
                            &record.session.id,
                            session_id,
                            kind.type_str(),
                            ManagedMultiagentEventProvenance::Message {
                                message_id: &message.id.0,
                                ordinal,
                            },
                        ),
                    }
                });
                if record.events.iter().any(|event| event.id == id) {
                    continue;
                }
                record.events.push(Event {
                    id,
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            if selected.fully_classified {
                record.consume_message(session_id, &message.id.0);
            }
        }
        // A committed ticket may precede its ToolUse transcript block. Retain
        // the existing synthetic path, qualified by the Awaiting Run, but only
        // when no revisitable source Message already owns that occurrence.
        if root_pending.is_some() && !pending_is_in_new_message {
            let tool_index = record.projected_tool_index_for(None);
            let mut projected = project_messages_with_mcp_ids(
                &[],
                root_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
            );
            self.qualify_tool_projection(
                record,
                None,
                &[],
                root_pending_source_run_id,
                &mut projected,
            )?;
            for projected in projected {
                let id = projected.id.ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "synthetic root pending projection has no qualified tool id",
                    ))
                })?;
                if !record.events.iter().any(|event| event.id == id) {
                    record.events.push(Event {
                        id,
                        kind: projected.kind,
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
            }
        }

        // Outcome observation is an immediate, side-effect-free committed fact.
        // The retained DefineOutcome selects the aggregate: Completed adds only
        // evaluation facts, while Errored adds one infrastructure error and no
        // partial evaluation. General transcript/lifecycle projection remains
        // the sole owner unless an exact failure source Run is correlated below.
        append_durable_outcome_projections(record, &durable_outcome_projections);

        self.append_delegation_projections(
            record,
            DelegationProjectionEvidence {
                links: &links,
                snapshots: &child_snapshots,
                transcripts: &transcripts,
                lifecycle_events: &accepted_lifecycle,
                latest_run_states: &child_latest_run_states,
                pending: &child_pending,
                historical_pending: &historical_pending,
                dispositions: &child_dispositions,
                usage: &child_thread_usage,
            },
        )?;

        let child_terminal = accepted_lifecycle.iter().rev().find(|event| {
            known_children.contains(event.thread_id.0.as_str())
                && child_dispositions.get(event.thread_id.0.as_str())
                    != Some(&awaken_agent_contract::ThreadDisposition::Archived)
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        });
        let child_terminal_reached_budget = child_terminal.is_some_and(|event| {
            event.kind == RunLifecycleEventKind::Awaiting
                && event.await_reason.as_ref()
                    == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached)
        });
        let active_child = record.child_threads.iter().any(|child| {
            matches!(
                child.status,
                SessionThreadStatus::Running | SessionThreadStatus::Rescheduling
            )
        });
        let can_idle = persisted_status == SessionStatus::Idle && !active_child;
        let aggregate_budget_reached = !persisted.budget.can_admit_model_request();
        if let Some(terminal) = unseen_root_terminal {
            let thread_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                root_pending,
                terminal.await_reason.as_ref(),
            )?;
            let aggregate_reason = match thread_reason {
                reason @ StopReason::RequiresAction { .. } | reason @ StopReason::BudgetReached => {
                    reason
                }
                _ if aggregate_budget_reached => StopReason::BudgetReached,
                reason => reason,
            };
            // A scheduled action can commit Awaiting while the Session activity
            // is still Running, then autonomously resume and commit a newer
            // terminal for the same root Run before the aggregate may idle. The
            // newer same-Run reason supersedes that disposable continuation;
            // BudgetReached remains absorbing across every later terminal.
            let replace = match record.deferred_session_stop_reason.as_ref() {
                None => true,
                Some((_, StopReason::BudgetReached)) => false,
                Some((Some(owner), _)) => owner == &terminal.run_id,
                Some((None, _)) => false,
            };
            if replace || matches!(aggregate_reason, StopReason::BudgetReached) {
                record.deferred_session_stop_reason =
                    Some((Some(terminal.run_id.clone()), aggregate_reason));
            }
        }
        if !can_idle && let Some(terminal) = child_terminal {
            let reason = if child_terminal_reached_budget {
                StopReason::BudgetReached
            } else {
                Self::public_run_stop_reason(
                    record,
                    Some(terminal.thread_id.0.as_str()),
                    &terminal.state,
                    child_pending.get(terminal.thread_id.0.as_str()),
                    terminal.await_reason.as_ref(),
                )?
            };
            if matches!(reason, StopReason::BudgetReached) {
                record.deferred_session_stop_reason = Some((None, reason));
            } else {
                record
                    .deferred_session_stop_reason
                    .get_or_insert((None, reason));
            }
        }
        if can_idle && child_terminal_reached_budget {
            record.deferred_session_stop_reason = Some((None, StopReason::BudgetReached));
        }
        if let Some(terminal) = unseen_root_terminal {
            record.projected_terminal_cursors.insert(terminal.cursor);
            if let awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Error(failure),
            ) = &terminal.state
                && !durable_outcome_projections
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id))
            {
                record.events.push(Event {
                    id: managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "root-lifecycle-error",
                        ManagedMultiagentEventProvenance::Lifecycle {
                            cursor: terminal.cursor.0,
                        },
                    ),
                    kind: OutboundKind::SessionError {
                        error: SessionError::classify(failure.code(), failure.message()),
                    },
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            let stop_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                root_pending,
                terminal.await_reason.as_ref(),
            )?;
            let thread_idle = primary_thread_status_event(
                record,
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "root-lifecycle-status-idle",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                PrimaryThreadStatusProjection::Idle { stop_reason },
            );
            record.events.push(thread_idle);
        }
        let projected_usage =
            session_usage_value(usage, price_snapshot.as_ref()).map_err(StateError::Run)?;
        record.session.usage = projected_usage.clone();
        let mut pending_event_ids = Vec::new();
        if let Some(pending) = root_pending {
            pending_event_ids.push(
                record
                    .projected_tool_index_for(None)
                    .latest
                    .get(&pending.tool_use_id)
                    .cloned()
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "root pending ticket has no public Managed tool event",
                        ))
                    })?,
            );
        }
        for (thread_id, pending) in &child_pending {
            pending_event_ids.push(
                record
                    .projected_tool_index_for(Some(thread_id))
                    .latest
                    .get(&pending.tool_use_id)
                    .cloned()
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "child pending ticket has no public Managed tool event",
                        ))
                    })?,
            );
        }
        pending_event_ids.sort();
        pending_event_ids.dedup();
        if can_idle
            && (unseen_root_terminal.is_some()
                || child_terminal.is_some()
                || record.deferred_session_stop_reason.is_some()
                || (!unseen_budget_reach.is_empty() && pending_event_ids.is_empty()))
        {
            if pending_event_ids.is_empty() && !unseen_budget_reach.is_empty() {
                for (generation, usage, budget) in unseen_budget_reach {
                    record.events.extend([
                        Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "budget-reach-usage",
                                ManagedMultiagentEventProvenance::BudgetReach { generation },
                            ),
                            kind: OutboundKind::SessionUsage {
                                usage,
                                budget: Some(budget),
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        },
                        Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "budget-reach-status-idle",
                                ManagedMultiagentEventProvenance::BudgetReach { generation },
                            ),
                            kind: OutboundKind::SessionStatusIdle {
                                stop_reason: StopReason::BudgetReached,
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        },
                    ]);
                    record.projected_budget_reach_generation = generation;
                }
            } else {
                let stop_reason = if pending_event_ids.is_empty() {
                    record
                        .deferred_session_stop_reason
                        .clone()
                        .map(|(_, reason)| reason)
                        .unwrap_or_else(|| {
                            root_terminal.map_or(StopReason::EndTurn, |event| {
                                Self::child_stop_reason(event, None)
                            })
                        })
                } else {
                    StopReason::RequiresAction {
                        event_ids: pending_event_ids,
                    }
                };
                // Only a terminal lifecycle prefix not yet represented by an
                // aggregate idle is replayable here. The shared terminal-cursor
                // set covers root and children.
                let aggregate_terminal_cursor = record
                    .projected_terminal_cursors
                    .iter()
                    .max()
                    .copied()
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "idle aggregate has no committed terminal lifecycle boundary",
                        ))
                    })?;
                let aggregate_usage_id = managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-usage",
                    ManagedMultiagentEventProvenance::LifecyclePrefix {
                        cursor: aggregate_terminal_cursor.0,
                    },
                );
                let aggregate_idle_id = managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-idle",
                    ManagedMultiagentEventProvenance::LifecyclePrefix {
                        cursor: aggregate_terminal_cursor.0,
                    },
                );
                record.events.extend([
                    Event {
                        id: aggregate_usage_id,
                        kind: OutboundKind::SessionUsage {
                            usage: projected_usage,
                            budget: record.session.budget.clone(),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                    Event {
                        id: aggregate_idle_id,
                        kind: OutboundKind::SessionStatusIdle { stop_reason },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                ]);
            }
            record.deferred_session_stop_reason = None;
        }
        // Parent terminality is a durable Session fact and absorbs every
        // derived child lifecycle. Keeping this at the end of the sole warm/cold
        // projector preserves child output/lifecycle ordering, closes each child
        // stream, then closes only the aggregate primary stream.
        if persisted.terminal_cleanup.runtime_commit_cursor().is_some() {
            self.append_parent_terminal_projection(record);
        }
        // A cold prefix can contain multiple completed root turns. The legacy
        // projector lowered only its latest terminal, so repair every retained
        // root boundary by the same lifecycle identity before interval ordering.
        for terminal in all_accepted_lifecycle.iter().filter(|event| {
            event.thread_id.0 == session_id
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        }) {
            let idle_id = managed_multiagent_event_id(
                &record.session.id,
                &record.session.id,
                "root-lifecycle-status-idle",
                ManagedMultiagentEventProvenance::Lifecycle {
                    cursor: terminal.cursor.0,
                },
            );
            if record.events.iter().any(|event| event.id == idle_id) {
                continue;
            }
            if let awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Error(failure),
            ) = &terminal.state
                && !durable_outcome_projections
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id))
            {
                let error_id = managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "root-lifecycle-error",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                );
                if !record.events.iter().any(|event| event.id == error_id) {
                    record.events.push(Event {
                        id: error_id,
                        kind: OutboundKind::SessionError {
                            error: SessionError::classify(failure.code(), failure.message()),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
            }
            let stop_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                historical_pending
                    .get(&terminal.cursor)
                    .or(root_pending.filter(|_| root_terminal == Some(terminal))),
                terminal.await_reason.as_ref(),
            )?;
            record.events.push(primary_thread_status_event(
                record,
                idle_id,
                PrimaryThreadStatusProjection::Idle { stop_reason },
            ));
            record.projected_terminal_cursors.insert(terminal.cursor);
        }
        Self::replace_interval_aggregate_projections(
            record,
            &persisted,
            &all_accepted_lifecycle,
            &historical_pending,
            price_snapshot.as_ref(),
        )?;
        Self::canonicalize_committed_events(
            record,
            &persisted,
            &all_accepted_lifecycle,
            root_snapshot.as_ref(),
            &child_snapshots,
            &links,
        )?;
        self.broadcast_new_event_ids(session_id, record, &previous_event_ids);
        Ok(true)
    }
}
