// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Aggregate close of one projection candidate: the primary Thread's terminal
//! idles, the budget-reach pause, and the aggregate Running edge and
//! usage/idle pair of every period that no admitted interval owns.
//!
//! Consumer: the candidate builder, after the root and delegation projectors.

use super::candidate_evidence::CandidateEvidence;
use super::root_projection::RootProjection;
use super::*;
use awaken_managed_session_events::{FactKey, FactKind, FactProvenance, FactRole, TimelineFact};

impl ManagedState {
    pub(super) fn close_aggregate(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        persisted_status: SessionStatus,
        evidence: &CandidateEvidence,
        root: &RootProjection,
    ) -> Result<(), StateError> {
        let RootProjection {
            projected_budget_reach,
        } = root;
        let historical_pending = &evidence.historical_pending;
        // Every root Run boundary publishes its Thread idle (and fault) once,
        // from its own committed lifecycle and audit.
        for terminal in evidence.acceptance.accepted.iter().filter(|event| {
            event.thread_id.0 == session_id
                && awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind)
        }) {
            let idle_key = FactKey::new(
                record.session.id.clone(),
                FactRole::RootLifecycleStatusIdle,
                FactProvenance::Lifecycle {
                    cursor: terminal.cursor.0,
                },
            );
            let idle_id = record.fact_id(&idle_key);
            if record.has_event_id(&idle_id) {
                continue;
            }
            Self::append_root_terminal_error(
                record,
                terminal,
                evidence
                    .outcomes
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id)),
                evidence.root.as_ref(),
            )?;
            let stop = awaken_managed_session_events::terminal_stop(
                &terminal.state,
                historical_pending.get(&terminal.cursor),
                terminal.await_reason.as_ref(),
                false,
            );
            record.publish_fact_once(
                &TimelineFact::new(idle_key, FactKind::ThreadIdle(stop)),
                None,
            )?;
            record
                .checkpoint
                .progress
                .terminal_cursors
                .insert(terminal.cursor);
        }
        let projected_usage =
            session_usage_value(evidence.usage.clone(), evidence.price_snapshot.as_ref())
                .map_err(StateError::Run)?;
        record.session.usage = projected_usage;
        // A budget-reach transition owns its own aggregate pause once its
        // interval has closed and no call awaits an answer.
        let active_child = record.child_threads.iter().any(|child| {
            matches!(
                child.status,
                SessionThreadStatus::Running | SessionThreadStatus::Rescheduling
            )
        });
        let answerable = evidence.root_pending.is_some() || !evidence.child_pending.is_empty();
        if persisted_status == SessionStatus::Idle && !active_child && !answerable {
            for (generation, usage, budget) in projected_budget_reach.iter().cloned() {
                let at = |role| {
                    FactKey::new(session_id, role, FactProvenance::BudgetReach { generation })
                };
                record.push_fact(
                    at(FactRole::BudgetReachUsage),
                    OutboundKind::SessionUsage {
                        usage,
                        budget: Some(budget),
                    },
                );
                record.push_fact(
                    at(FactRole::BudgetReachStatusIdle),
                    OutboundKind::SessionStatusIdle {
                        stop_reason: StopReason::BudgetReached,
                    },
                );
                record.checkpoint.source.budget_reach_generation = generation;
            }
        }
        self.bracket_aggregate_outside_intervals(record, session_id, persisted, evidence)
    }

    /// The aggregate Running edge and usage/idle pair of every period no
    /// admitted Session interval owns, decided by walking the committed
    /// lifecycle in order. The aggregate opens at the first opening after it
    /// closed (or, for a period first committed at its boundary, at that
    /// boundary's prefix), and closes at every boundary after which neither the
    /// root nor a coordinated child runs and no Session interval holds it. The
    /// pair is keyed by that boundary and carries the usage and
    /// stop reason as of it, so a reader that sees the boundary alone and one
    /// that reads long after it publish the same pair.
    ///
    /// Class A of the timeline's schedule independence: the pair used to be
    /// keyed by the latest terminal a refresh had seen and to carry the usage
    /// and stop reason of that refresh, so a reader that saw two boundaries in
    /// one read published one pair and a reader that saw each published two.
    /// Class C: the opening used to be one per refresh, keyed by the latest
    /// opening of the latest terminal's Run, unbounded by that terminal, so a
    /// reader that saw a pause and its resume in one read opened the pause's
    /// period at the resume and published no opening for the pause's period.
    fn bracket_aggregate_outside_intervals(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        evidence: &CandidateEvidence,
    ) -> Result<(), StateError> {
        let accepted = &evidence.acceptance.accepted;
        let historical_pending = &evidence.historical_pending;
        let known_children = evidence
            .links
            .iter()
            .map(|link| link.thread_id.0.as_str())
            .filter(|thread| {
                evidence.child_dispositions.get(*thread)
                    != Some(&awaken_agent_contract::ThreadDisposition::Archived)
            })
            .collect::<std::collections::HashSet<_>>();
        let hold = awaken_managed_session_events::interval_hold(persisted, accepted);
        let budget_reach = awaken_managed_session_events::BudgetReachAnchor::of(persisted, accepted);
        let mut root_running = false;
        let mut running_children = std::collections::HashSet::<&str>::new();
        let mut paused = std::collections::BTreeMap::<&str, &Pending>::new();
        let mut deferred: Option<(Option<&awaken_agent_contract::agent::run::Id>, StopReason)> =
            None;
        let mut aggregate_open = false;
        let mut openings = Vec::new();
        let mut closes = Vec::new();
        for event in accepted {
            let thread = event.thread_id.0.as_str();
            let is_root = thread == session_id;
            if !is_root && !known_children.contains(thread) {
                continue;
            }
            if awaken_managed_session_events::is_opening(event.kind) {
                if !aggregate_open {
                    openings.push(FactKey::new(
                        session_id,
                        FactRole::AggregateStatusRunning,
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
            if !awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind) {
                continue;
            }
            if is_root && !aggregate_open {
                // A period first committed at its boundary (a fast Run, or a
                // pause ended without resuming) opens at that boundary's
                // prefix.
                openings.push(FactKey::new(
                    session_id,
                    FactRole::AggregateTerminalStatusRunning,
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
            match pending {
                Some(pending) => {
                    paused.insert(thread, pending);
                }
                None => {
                    paused.remove(thread);
                }
            }
            let budget_pause = event.await_reason.as_ref()
                == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
            let idle = !root_running
                && running_children.is_empty()
                && !hold.session_active.contains(&event.cursor);
            if is_root {
                // The root Run's newest reason supersedes its own earlier
                // one; a child reason recorded first, or a budget pause,
                // stands; a budget reason always stands.
                let reason = Self::public_run_stop_reason(
                    record,
                    None,
                    &event.state,
                    pending,
                    event.await_reason.as_ref(),
                )?;
                let replace = match &deferred {
                    None => true,
                    Some((_, StopReason::BudgetReached)) => false,
                    Some((Some(owner), _)) => *owner == &event.run_id,
                    Some((None, _)) => false,
                };
                if replace || matches!(reason, StopReason::BudgetReached) {
                    deferred = Some((Some(&event.run_id), reason));
                }
            } else if budget_pause {
                deferred = Some((None, StopReason::BudgetReached));
            } else if !idle {
                // A child boundary that cannot close the aggregate records
                // its reason for the close a later boundary makes.
                let reason = Self::public_run_stop_reason(
                    record,
                    Some(thread),
                    &event.state,
                    pending,
                    event.await_reason.as_ref(),
                )?;
                deferred.get_or_insert((None, reason));
            }
            if hold.closed_by_interval.contains(&event.cursor) {
                // The admitted interval's own close publishes this pause, at
                // its latest boundary.
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
                match deferred.take() {
                    Some((_, reason)) => reason,
                    None => StopReason::EndTurn,
                }
            } else {
                let mut event_ids = paused
                    .iter()
                    .map(|(thread, pending)| {
                        let owner = (*thread != session_id).then_some(*thread);
                        record
                            .projected_tool_index_for(owner)
                            .latest
                            .get(&pending.tool_use_id)
                            .cloned()
                            .ok_or_else(|| {
                                StateError::Run(RunError::internal(
                                    "an answerable pause has no public Managed tool event",
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                event_ids.sort();
                event_ids.dedup();
                deferred = None;
                StopReason::RequiresAction { event_ids }
            };
            if matches!(stop, StopReason::BudgetReached) || budget_reached {
                // The durable budget-reach transition owns this pause.
                continue;
            }
            closes.push((event, stop));
        }
        for opening in openings {
            Self::append_aggregate_running_once(record, opening);
        }
        for (boundary, stop_reason) in closes {
            let at = |role| {
                FactKey::new(
                    session_id,
                    role,
                    FactProvenance::LifecyclePrefix {
                        cursor: boundary.cursor.0,
                    },
                )
            };
            let usage = session_usage_value(
                evidence.usage_as_of(persisted, session_id, boundary.source_commit_cursor),
                evidence.price_snapshot.as_ref(),
            )
            .map_err(StateError::Run)?;
            let budget = record.session.budget.clone();
            record.append_fact_once(
                at(FactRole::AggregateUsage),
                OutboundKind::SessionUsage { usage, budget },
            );
            record.append_fact_once(
                at(FactRole::AggregateStatusIdle),
                OutboundKind::SessionStatusIdle { stop_reason },
            );
        }
        Ok(())
    }
}
