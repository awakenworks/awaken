// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Aggregate Session facts owned by admitted runtime intervals: each interval's
//! exact opening edge, and its usage/idle close.
//!
//! Consumer: the protocol adapter's candidate builder, which withdraws the
//! replaced facts, then appends the planned ones in order.

use std::collections::HashMap;

use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_session_application_contract::Pending;
use awaken_session_event_contract::SessionAgentRunBoundary;
use awaken_session_state_contract::{PersistedSession, SessionRuntimeInterval};

use crate::interval::{
    budget_reach_projection_close_cursor, interval_close_cursor, interval_lifecycle_open_event,
    running_interval_lifecycle_open_event,
};
use crate::stop::terminal_stop;
use crate::{FactKey, FactKind, FactProvenance, FactRole, ProjectionError, StopFact, TimelineFact};

/// One planned aggregate fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntervalStep {
    /// Publish unless the fact is already published.
    AppendOnce(TimelineFact),
    /// Publish the interval's close pair.
    Close {
        usage: TimelineFact,
        idle: TimelineFact,
    },
}

/// The aggregate projection of every admitted interval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntervalAggregatePlan {
    /// Previously published candidates this lineage supersedes; `None` for a
    /// legacy Session without interval lineage, whose projections stand.
    pub replaced: Option<Vec<FactKey>>,
    pub steps: Vec<IntervalStep>,
}

/// The usage an interval closes with, before the protocol prices it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntervalUsage {
    pub usage: awaken_session_budget_contract::SessionUsage,
    pub max_list_cost_minor: Option<u64>,
}

/// Plan the aggregate facts of `persisted`'s intervals over the accepted
/// lifecycle.
///
/// Session-level Usage/Idle belongs to the root-owned interval, while Running
/// belongs to the exact lifecycle opening already visible to a client.
/// Closing an interval must never replace that issued cursor: only
/// non-canonical opening candidates and former interval-id Running facts are
/// withdrawn. A legacy row with no interval history, and independent budget
/// reach facts, retain their projections unchanged.
pub fn plan_interval_aggregate(
    persisted: &PersistedSession,
    lifecycle_events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
) -> Result<IntervalAggregatePlan, ProjectionError> {
    let session_id = persisted.session_id.as_str();
    let aggregate = |role, provenance| FactKey::new(session_id, role, provenance);
    let lifecycle_running = |cursor: RunLifecycleCursor| {
        aggregate(
            FactRole::AggregateStatusRunning,
            FactProvenance::Lifecycle { cursor: cursor.0 },
        )
    };
    let interval_fact = |role, interval_id: &str| {
        aggregate(
            role,
            FactProvenance::RuntimeInterval {
                interval_id: interval_id.to_string(),
            },
        )
    };
    let mut plan = IntervalAggregatePlan::default();
    // One selection per closed interval, shared by the withdrawal and the
    // emission pass below: each interval's opening is chosen against the close
    // of the interval before it, exactly as the live selection of the running
    // interval chose it, so withdrawal cannot keep one opening while emission
    // publishes another.
    let closed = closed_interval_openings(persisted, lifecycle_events);
    let has_interval_lineage = !persisted.closed_runtime_intervals.is_empty()
        || persisted
            .running_interval
            .as_ref()
            .is_some_and(|interval| interval.opened_revision.0 != 0);
    if has_interval_lineage {
        let replaced = plan.replaced.insert(Vec::new());
        for &ClosedIntervalOpening {
            interval,
            close,
            opening,
            ..
        } in &closed
        {
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
                replaced.push(lifecycle_running(lifecycle.cursor));
            }
            if let Some(terminal_cursor) = interval_lifecycle
                .iter()
                .filter(|event| SessionAgentRunBoundary::is_boundary(event.kind))
                .map(|event| event.cursor)
                .max()
            {
                for role in [FactRole::AggregateUsage, FactRole::AggregateStatusIdle] {
                    replaced.push(aggregate(
                        role,
                        FactProvenance::LifecyclePrefix {
                            cursor: terminal_cursor.0,
                        },
                    ));
                }
            }
            for role in [FactRole::AggregateUsage, FactRole::AggregateStatusIdle] {
                replaced.push(interval_fact(role, &interval.interval_id));
            }
            replaced.push(interval_fact(
                FactRole::AggregateStatusRunning,
                &interval.interval_id,
            ));
        }
        if let Some(interval) = persisted.running_interval.as_ref()
            && let Some(opening) = running_interval_lifecycle_open_event(
                session_id,
                interval,
                closed.last().map_or(0, |last| last.close),
                &persisted.event_batches,
                lifecycle_events,
            )
        {
            for lifecycle in lifecycle_events.iter().filter(|event| {
                event.source_commit_cursor >= opening.source_commit_cursor
                    && event.cursor != opening.cursor
            }) {
                replaced.push(lifecycle_running(lifecycle.cursor));
            }
            replaced.push(interval_fact(
                FactRole::AggregateStatusRunning,
                &interval.interval_id,
            ));
        }
    }

    for &ClosedIntervalOpening {
        interval,
        close,
        opening,
    } in &closed
    {
        if interval.observations.is_empty() {
            continue;
        }
        let running = opening.map(|opening| lifecycle_running(opening.cursor));
        // A fast interval may commit only Awaiting/Ended. Its terminal-prefix
        // bracket was public before any later interval existed and must be
        // rebuilt on a cold reader using that same exact terminal, never a
        // Running commit beyond this interval's retained close.
        if running.is_none()
            && let Some(terminal) = interval.observations.iter().find_map(|observation| {
                lifecycle_events.iter().find(|event| {
                    event.cursor == observation.lifecycle_cursor
                        && event.thread_id == observation.thread_id
                        && event.run_id == observation.run_id
                        && event.thread_id.0 == session_id
                        && SessionAgentRunBoundary::is_boundary(event.kind)
                })
            })
        {
            plan.steps.push(IntervalStep::AppendOnce(TimelineFact::new(
                aggregate(
                    FactRole::AggregateTerminalStatusRunning,
                    FactProvenance::LifecyclePrefix {
                        cursor: terminal.cursor.0,
                    },
                ),
                FactKind::SessionRunning,
            )));
            plan.steps.push(IntervalStep::AppendOnce(TimelineFact::new(
                aggregate(
                    FactRole::RootTerminalStatusRunning,
                    FactProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                FactKind::ThreadRunning,
            )));
        }
        let usage = interval
            .usage
            .to_session_usage()
            .map_err(|error| ProjectionError::corrupt(error.to_string()))?;
        let terminal = interval.observations.last().and_then(|observation| {
            lifecycle_events.iter().find(|event| {
                event.cursor == observation.lifecycle_cursor
                    && event.thread_id == observation.thread_id
                    && event.run_id == observation.run_id
            })
        });
        let (stop, calls_owner) = terminal.map_or_else(
            || (StopFact::EndTurn, session_id.to_string()),
            |terminal| {
                (
                    terminal_stop(
                        &terminal.state,
                        historical_pending.get(&terminal.cursor),
                        terminal.await_reason.as_ref(),
                        false,
                    ),
                    terminal.thread_id.0.clone(),
                )
            },
        );
        if let Some(running) = running {
            plan.steps.push(IntervalStep::AppendOnce(TimelineFact::new(
                running,
                FactKind::SessionRunning,
            )));
        }
        let budget_transition_owns_pause = stop == StopFact::BudgetReached
            && persisted
                .budget
                .reach_transitions()
                .iter()
                .any(|transition| {
                    budget_reach_projection_close_cursor(persisted, transition, lifecycle_events)
                        == Some(close)
                });
        if budget_transition_owns_pause {
            continue;
        }
        plan.steps.push(IntervalStep::Close {
            usage: TimelineFact::new(
                interval_fact(FactRole::AggregateUsage, &interval.interval_id),
                FactKind::IntervalUsage(IntervalUsage {
                    usage,
                    max_list_cost_minor: interval.max_list_cost_minor,
                }),
            ),
            idle: TimelineFact::new(
                interval_fact(FactRole::AggregateStatusIdle, &interval.interval_id),
                FactKind::SessionIdle { stop, calls_owner },
            ),
        });
    }
    if let Some(interval) = persisted.running_interval.as_ref()
        && let Some(opening) = running_interval_lifecycle_open_event(
            session_id,
            interval,
            closed.last().map_or(0, |last| last.close),
            &persisted.event_batches,
            lifecycle_events,
        )
    {
        plan.steps.push(IntervalStep::AppendOnce(TimelineFact::new(
            lifecycle_running(opening.cursor),
            FactKind::SessionRunning,
        )));
    }
    Ok(plan)
}

/// The latest interval opening the Session has resolved: the running
/// interval's when it is resolved, otherwise the latest closed interval's.
/// Each is selected against the same predecessor close as
/// [`plan_interval_aggregate`], so the aggregate Running key a reader derives
/// from it is the one the plan keeps.
#[must_use]
pub fn latest_interval_opening<'a>(
    persisted: &'a PersistedSession,
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let closed = closed_interval_openings(persisted, lifecycle_events);
    persisted
        .running_interval
        .as_ref()
        .and_then(|interval| {
            running_interval_lifecycle_open_event(
                persisted.session_id.as_str(),
                interval,
                closed.last().map_or(0, |last| last.close),
                &persisted.event_batches,
                lifecycle_events,
            )
        })
        .or_else(|| closed.last().and_then(|last| last.opening))
}

/// A closed interval with the opening selected against its predecessor.
struct ClosedIntervalOpening<'a> {
    interval: &'a SessionRuntimeInterval,
    /// Where the interval closed; its predecessor's close when it recorded no
    /// observation.
    close: u64,
    opening: Option<&'a RunLifecycleEvent>,
}

fn closed_interval_openings<'a>(
    persisted: &'a PersistedSession,
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Vec<ClosedIntervalOpening<'a>> {
    let mut prior_close = 0;
    persisted
        .closed_runtime_intervals
        .iter()
        .map(|interval| {
            let opening = interval_lifecycle_open_event(
                persisted.session_id.as_str(),
                interval,
                prior_close,
                &persisted.event_batches,
                lifecycle_events,
            );
            prior_close = interval_close_cursor(interval).unwrap_or(prior_close);
            ClosedIntervalOpening {
                interval,
                close: prior_close,
                opening,
            }
        })
        .collect()
}
