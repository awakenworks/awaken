// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! One aggregate plan for every committed lifecycle period: the fallback
//! opening/close and admitted-interval usage/idle are selected together.
//!
//! Consumer: the protocol adapter's candidate builder, which renders the
//! selected facts on one fresh committed-prefix record.

use std::collections::{BTreeMap, HashMap, HashSet};

use awaken_agent_contract::RunLifecycleEventKind;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_runtime_contract::runs::Pending;
use awaken_session_contract::SessionAgentRunBoundary;
use awaken_session_contract::{PersistedSession, SessionRuntimeInterval};
use awaken_session_execution_contract::CoordinatedThreadLink;

use crate::interval::{
    budget_reach_projection_close_cursor, interval_close_cursor, interval_lifecycle_open_event,
    running_interval_lifecycle_open_event,
};
use crate::stop::terminal_stop;
use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, ProjectionError,
    SessionEventKind, StopFact,
};

/// One admitted interval's two closing facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateIntervalClose {
    pub usage: DerivedSessionEvent,
    pub idle: DerivedSessionEvent,
}

/// A fallback close for a period no admitted interval owns. Public usage is
/// calculated as of `source_commit_cursor`; its keys retain the old frozen
/// lifecycle-prefix identity regardless of when a replica first observes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateFallbackClose {
    pub source_commit_cursor: u64,
    pub usage: EventIdentity,
    pub idle: EventIdentity,
    pub stop: AggregateStop,
}

/// Neutral stop evidence for one aggregate fallback. The protocol resolves
/// the call identities to its already-issued public tool IDs when rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggregateStop {
    Run {
        calls_owner: String,
        stop: StopFact,
    },
    Calls {
        thread_calls: Vec<(String, crate::ToolResultSource)>,
    },
}

/// The sole aggregate projection plan over one committed prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggregatePlan {
    pub interval_closes: Vec<AggregateIntervalClose>,
    pub fallback_openings: Vec<EventIdentity>,
    pub fallback_closes: Vec<AggregateFallbackClose>,
}

/// The usage an interval closes with, before the protocol prices it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntervalUsage {
    pub usage: awaken_session_contract::SessionUsage,
    pub max_list_cost_minor: Option<u64>,
}

/// Plan interval-owned and fallback aggregate facts from the same accepted
/// lifecycle, topology, budget and interval admission evidence.
///
/// Session-level Usage/Idle belongs to the root-owned interval. Its Running
/// edge is the lifecycle walk's (one per aggregate period, keyed by the
/// period's first commit), never the interval's, so closing an interval cannot
/// move an issued opening. The fresh-prefix candidate never withdraws facts;
/// the plan withholds fallback closes owned by admitted intervals. A legacy
/// row with no interval history and independent budget reach facts retain
/// their projections unchanged.
pub fn plan_aggregate(
    persisted: &PersistedSession,
    lifecycle_events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
    links: &[CoordinatedThreadLink],
    index: &crate::OrderIndex<'_>,
) -> Result<AggregatePlan, ProjectionError> {
    let session_id = persisted.session_id.as_str();
    let aggregate = |role, provenance| EventIdentity::new(session_id, role, provenance);
    let interval_fact = |role, interval_id: &str| {
        aggregate(
            role,
            FactProvenance::RuntimeInterval {
                interval_id: interval_id.to_string(),
            },
        )
    };
    let mut plan = AggregatePlan::default();
    // One selection per closed interval supplies the interval-owned close
    // below and the fallback hold decision in the same plan. The candidate
    // starts without committed Events, so no prior interval fact is withdrawn.
    let closed = closed_interval_openings(persisted, lifecycle_events);

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
        let (stop, calls_owner) = match terminal {
            Some(terminal) => (
                terminal_stop(
                    terminal,
                    historical_pending.get(&terminal.cursor),
                    false,
                    index,
                )?,
                terminal.thread_id.0.clone(),
            ),
            None => (StopFact::EndTurn, session_id.to_string()),
        };
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
        plan.interval_closes.push(AggregateIntervalClose {
            usage: DerivedSessionEvent::new(
                interval_fact(ManagedEventType::AggregateUsage, &interval.interval_id),
                SessionEventKind::IntervalUsage(IntervalUsage {
                    usage,
                    max_list_cost_minor: interval.max_list_cost_minor,
                }),
            ),
            idle: DerivedSessionEvent::new(
                interval_fact(ManagedEventType::AggregateStatusIdle, &interval.interval_id),
                SessionEventKind::SessionIdle { stop, calls_owner },
            ),
        });
    }
    plan_fallback_aggregate(
        &mut plan,
        persisted,
        lifecycle_events,
        historical_pending,
        links,
        index,
    )?;
    Ok(plan)
}

/// Select fallback periods only where no admitted Session interval owns their
/// close. This is part of `plan_aggregate`, never an adapter-side second walk.
fn plan_fallback_aggregate(
    plan: &mut AggregatePlan,
    persisted: &PersistedSession,
    lifecycle_events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
    links: &[CoordinatedThreadLink],
    index: &crate::OrderIndex<'_>,
) -> Result<(), ProjectionError> {
    let session_id = persisted.session_id.as_str();
    let known_children = links
        .iter()
        .map(|link| link.thread_id.0.as_str())
        .collect::<HashSet<_>>();
    let hold = interval_hold(persisted, lifecycle_events);
    let budget_reach =
        crate::BudgetReachAnchor::of(persisted, lifecycle_events, historical_pending);
    let mut root_running = false;
    let mut running_children = HashSet::<&str>::new();
    let mut paused = BTreeMap::<&str, crate::ToolResultSource>::new();
    let mut deferred: Option<(Option<&RunId>, AggregateStop)> = None;
    let mut aggregate_open = false;
    for event in lifecycle_events {
        let thread = event.thread_id.0.as_str();
        let is_root = thread == session_id;
        if !is_root && !known_children.contains(thread) {
            continue;
        }
        if crate::interval::is_opening(event.kind) {
            if !aggregate_open {
                plan.fallback_openings.push(EventIdentity::new(
                    session_id,
                    ManagedEventType::AggregateStatusRunning,
                    FactProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                ));
                aggregate_open = true;
            }
            if is_root {
                root_running = true;
            } else {
                running_children.insert(thread);
            }
            paused.remove(thread);
            continue;
        }
        if !SessionAgentRunBoundary::is_boundary(event.kind) {
            continue;
        }
        if is_root && !aggregate_open {
            // A fast Run may first become visible at its boundary. Keep the
            // frozen lifecycle-prefix identity and its phase ordering.
            plan.fallback_openings.push(EventIdentity::new(
                session_id,
                ManagedEventType::AggregateTerminalStatusRunning,
                FactProvenance::LifecyclePrefix {
                    cursor: event.cursor.0,
                },
            ));
            aggregate_open = true;
        }
        if is_root {
            root_running = false;
        } else {
            running_children.remove(thread);
        }
        let pending = historical_pending
            .get(&event.cursor)
            .filter(|_| event.kind == RunLifecycleEventKind::Awaiting);
        let stop = crate::terminal_stop(event, pending, false, index)?;
        // The one boundary classification owns its exact answerable source
        // for both the paused set and deferred close; do not select it twice.
        if let StopFact::RequiresAction { calls } = &stop
            && let Some(source) = calls.first()
        {
            paused.insert(thread, source.clone());
        } else {
            paused.remove(thread);
        }
        let budget_pause = event.await_reason.as_ref() == Some(&AwaitReason::BudgetReached);
        let idle = !root_running
            && running_children.is_empty()
            && !hold.session_active.contains(&event.cursor);
        if is_root {
            let replace = match &deferred {
                None => true,
                Some((
                    _,
                    AggregateStop::Run {
                        stop: StopFact::BudgetReached,
                        ..
                    },
                )) => false,
                Some((Some(owner), _)) => *owner == &event.run_id,
                Some((None, _)) => false,
            };
            if replace || stop == StopFact::BudgetReached {
                deferred = Some((
                    Some(&event.run_id),
                    AggregateStop::Run {
                        calls_owner: session_id.to_string(),
                        stop,
                    },
                ));
            }
        } else if budget_pause {
            deferred = Some((
                None,
                AggregateStop::Run {
                    calls_owner: thread.to_string(),
                    stop: StopFact::BudgetReached,
                },
            ));
        } else if !idle {
            deferred.get_or_insert((
                None,
                AggregateStop::Run {
                    calls_owner: thread.to_string(),
                    stop,
                },
            ));
        }
        if hold.closed_by_interval.contains(&event.cursor) {
            deferred = None;
            if hold.interval_closes.contains(&event.cursor) {
                aggregate_open = false;
            }
            continue;
        }
        if !idle {
            continue;
        }
        aggregate_open = false;
        let budget_reached = budget_reach.iter().any(|anchor| {
            anchor
                .close_cursor
                .is_some_and(|close| close <= event.source_commit_cursor)
        });
        let stop = if paused.is_empty() {
            deferred.take().map_or_else(
                || AggregateStop::Run {
                    calls_owner: session_id.to_string(),
                    stop: StopFact::EndTurn,
                },
                |(_, stop)| stop,
            )
        } else {
            let thread_calls = paused
                .iter()
                .map(|(thread, source)| ((*thread).to_string(), source.clone()))
                .collect();
            deferred = None;
            AggregateStop::Calls { thread_calls }
        };
        if matches!(
            &stop,
            AggregateStop::Run {
                stop: StopFact::BudgetReached,
                ..
            }
        ) || budget_reached
        {
            continue;
        }
        let at = |role| {
            EventIdentity::new(
                session_id,
                role,
                FactProvenance::LifecyclePrefix {
                    cursor: event.cursor.0,
                },
            )
        };
        plan.fallback_closes.push(AggregateFallbackClose {
            source_commit_cursor: event.source_commit_cursor,
            usage: at(ManagedEventType::AggregateUsage),
            idle: at(ManagedEventType::AggregateStatusIdle),
            stop,
        });
    }
    Ok(())
}

/// How admitted Session intervals hold the lifecycle boundaries of a prefix.
///
/// Consumer: the sole aggregate planner, which closes the aggregate at a
/// boundary only when no admitted interval holds it.
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
    batches: &[awaken_session_contract::SessionEventBatch],
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
