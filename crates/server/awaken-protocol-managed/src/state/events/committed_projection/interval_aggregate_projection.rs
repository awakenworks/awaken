//! Canonical aggregate projections for one persisted Session runtime interval.

use super::canonical_order::{
    interval_close_cursor, interval_lifecycle_open_event, running_interval_lifecycle_open_event,
};
use super::*;

impl ManagedState {
    pub(super) fn replace_interval_aggregate_projections(
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
                let opening = interval_lifecycle_open_event(
                    &record.session.id,
                    interval,
                    prior_close,
                    &persisted.event_batches,
                    lifecycle_events,
                );
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
                    &record.session.id,
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
            let running_id = interval_lifecycle_open_event(
                &record.session.id,
                interval,
                prior_close,
                &persisted.event_batches,
                lifecycle_events,
            )
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
            // A BudgetReachTransition owns the aggregate usage + idle pair for
            // a cap crossing. The interval still owns its Running edge, but
            // projecting its terminal pair as well would publish the same
            // budget pause twice. If the transition commit is momentarily
            // behind the lifecycle/interval prefix, leave the aggregate pause
            // absent until that durable authority becomes visible.
            if matches!(stop_reason, StopReason::BudgetReached) {
                prior_close = close;
                continue;
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
                &record.session.id,
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
}
