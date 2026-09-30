// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Aggregate Session facts owned by admitted runtime intervals: each observed
//! interval's usage/idle close, and how the intervals hold the lifecycle
//! boundaries of a prefix.
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
/// Session-level Usage/Idle belongs to the root-owned interval. Its Running
/// edge is the lifecycle walk's (one per aggregate period, keyed by the
/// period's first commit), never the interval's, so closing an interval cannot
/// move an issued opening: only former interval-id Running facts are
/// withdrawn. A legacy row with no interval history, and independent budget
/// reach facts, retain their projections unchanged.
pub fn plan_interval_aggregate(
    persisted: &PersistedSession,
    lifecycle_events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
) -> Result<IntervalAggregatePlan, ProjectionError> {
    let session_id = persisted.session_id.as_str();
    let aggregate = |role, provenance| FactKey::new(session_id, role, provenance);
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
        if let Some(interval) = persisted.running_interval.as_ref() {
            replaced.push(interval_fact(
                FactRole::AggregateStatusRunning,
                &interval.interval_id,
            ));
        }
    }

    for &ClosedIntervalOpening {
        interval, close, ..
    } in &closed
    {
        if interval.observations.is_empty() {
            continue;
        }
        // The root lifecycle walk alone owns primary Thread Running, including
        // a boundary-only fast Run. This interval owns only aggregate
        // Usage/Idle; its opening selector cannot manufacture a second Thread
        // Running behind an already-issued ToolUse cursor.
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
        let budget_transition_owns_pause = stop == StopFact::BudgetReached
            && persisted
                .budget
                .reach_transitions()
                .iter()
                .any(|transition| {
                    budget_reach_projection_close_cursor(
                        persisted,
                        transition,
                        lifecycle_events,
                        historical_pending,
                    ) == Some(close)
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
    Ok(plan)
}

/// How admitted Session intervals hold the lifecycle boundaries of a prefix.
///
/// Consumer: the protocol adapter's aggregate close outside intervals, which
/// closes the aggregate at a boundary only when no interval holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntervalHold {
    /// Boundaries inside an observed closed interval, whose own usage/idle
    /// pair closes the aggregate for them.
    pub closed_by_interval: std::collections::HashSet<RunLifecycleCursor>,
    /// Boundaries at which a Session interval was still admitted: inside the
    /// running interval, or inside an interval that settled without an
    /// observation other than its last boundary, where it ended.
    pub session_active: std::collections::HashSet<RunLifecycleCursor>,
    /// The latest boundary of each observed closed interval, where its own
    /// pair closes the aggregate; the aggregate stays open at its earlier
    /// boundaries.
    pub interval_closes: std::collections::HashSet<RunLifecycleCursor>,
}

/// Where each admitted interval starts: after the Runtime commit cursor its
/// admission recorded. An interval recorded before that coordinate was kept
/// starts at its resolved opening, else after the fence of the batch that
/// admitted it, else after its predecessor's close (the legacy rule).
fn interval_start(
    admitted_after_commit_cursor: Option<u64>,
    opening: Option<&RunLifecycleEvent>,
    opened_revision: awaken_session_types::SessionRevision,
    batches: &[awaken_session_event_contract::SessionEventBatch],
    prior_close: u64,
) -> u64 {
    if let Some(admitted_after) = admitted_after_commit_cursor {
        return admitted_after.max(prior_close).saturating_add(1);
    }
    opening.map_or_else(
        || {
            crate::interval::interval_admission_fence(opened_revision, batches)
                .unwrap_or(prior_close)
                .max(prior_close)
                .saturating_add(1)
        },
        |opening| opening.source_commit_cursor,
    )
}

/// How the admitted intervals of `persisted` hold the boundaries of
/// `lifecycle_events` (see [`IntervalHold`]).
#[must_use]
pub fn interval_hold(
    persisted: &PersistedSession,
    lifecycle_events: &[RunLifecycleEvent],
) -> IntervalHold {
    let closed = closed_interval_openings(persisted, lifecycle_events);
    let mut prior_close = 0_u64;
    // (start, end, observed): an unobserved interval ends where the next
    // interval starts.
    let mut spans = Vec::<(u64, Option<u64>, bool)>::new();
    for interval in &closed {
        let start = interval_start(
            interval.interval.admitted_after_commit_cursor,
            interval.opening,
            interval.interval.opened_revision,
            &persisted.event_batches,
            prior_close,
        );
        let observed = !interval.interval.observations.is_empty();
        if let Some(previous) = spans.last_mut()
            && previous.1.is_none()
        {
            previous.1 = Some(start.saturating_sub(1));
        }
        spans.push((start, observed.then_some(interval.close), observed));
        prior_close = interval.close;
    }
    if let Some(interval) = persisted.running_interval.as_ref() {
        let opening = running_interval_lifecycle_open_event(
            persisted.session_id.as_str(),
            interval,
            prior_close,
            &persisted.event_batches,
            lifecycle_events,
        );
        let start = interval_start(
            interval.admitted_after_commit_cursor,
            opening,
            interval.opened_revision,
            &persisted.event_batches,
            prior_close,
        );
        if let Some(previous) = spans.last_mut()
            && previous.1.is_none()
        {
            previous.1 = Some(start.saturating_sub(1));
        }
        spans.push((start, None, true));
    }
    let boundaries = lifecycle_events
        .iter()
        .filter(|event| SessionAgentRunBoundary::is_boundary(event.kind))
        .collect::<Vec<_>>();
    let mut hold = IntervalHold::default();
    let running = persisted.running_interval.is_some();
    for (index, (start, end, observed)) in spans.iter().copied().enumerate() {
        let inside = boundaries
            .iter()
            .filter(|event| {
                event.source_commit_cursor >= start
                    && end.is_none_or(|end| event.source_commit_cursor <= end)
            })
            .map(|event| event.cursor)
            .collect::<Vec<_>>();
        let is_running = running && index + 1 == spans.len();
        if is_running {
            hold.session_active.extend(inside);
        } else if observed {
            hold.interval_closes.extend(inside.iter().max().copied());
            hold.closed_by_interval.extend(inside);
        } else if let Some((_, earlier)) = inside.split_last() {
            // Settled without an observation: the Session stayed active up
            // to its last boundary, where it ended.
            hold.session_active.extend(earlier.iter().copied());
        }
    }
    hold
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
