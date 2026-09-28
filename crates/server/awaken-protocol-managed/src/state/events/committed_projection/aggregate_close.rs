// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Aggregate close of one projection candidate: the deferred stop reason, the
//! primary Thread's terminal idles, the aggregate usage and the one aggregate
//! idle (or budget-reach pause) per terminal prefix.
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
        root: &RootProjection<'_>,
    ) -> Result<(), StateError> {
        let RootProjection {
            root_terminal,
            projected_root_terminal,
            projected_budget_reach,
        } = root;
        let (root_terminal, projected_root_terminal) = (*root_terminal, *projected_root_terminal);
        let historical_pending = &evidence.historical_pending;
        let root_pending = evidence.root_pending.as_ref();
        let child_pending = &evidence.child_pending;
        let known_children = evidence
            .links
            .iter()
            .map(|link| link.thread_id.0.as_str())
            .collect::<std::collections::HashSet<_>>();
        let child_terminal = evidence.new_lifecycle.iter().rev().find(|event| {
            known_children.contains(event.thread_id.0.as_str())
                && evidence.child_dispositions.get(event.thread_id.0.as_str())
                    != Some(&awaken_agent_contract::ThreadDisposition::Archived)
                && awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind)
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
        if let Some(terminal) = projected_root_terminal {
            let thread_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                historical_pending.get(&terminal.cursor).or(root_pending),
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
        // All root terminals use this one reducer. A current-ticket-only fast
        // path would freeze an empty approval payload when the latest terminal
        // is historical and its reply has already resumed the Run.
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
                historical_pending
                    .get(&terminal.cursor)
                    .or(root_pending.filter(|_| root_terminal == Some(terminal))),
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
        for (thread_id, pending) in child_pending {
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
            && (projected_root_terminal.is_some()
                || child_terminal.is_some()
                || record.deferred_session_stop_reason.is_some()
                || (!projected_budget_reach.is_empty() && pending_event_ids.is_empty()))
        {
            if pending_event_ids.is_empty() && !projected_budget_reach.is_empty() {
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
                // The durable BudgetReachTransition is the sole aggregate
                // budget-pause authority. A lifecycle Awaiting fact may become
                // visible one refresh before that transition, or after its
                // generation was already projected; publishing it here would
                // create a second `session.status_idle:budget_reached` for one
                // cap transition. Thread lifecycle remains projected above.
                if !matches!(stop_reason, StopReason::BudgetReached) {
                    // Only a terminal lifecycle prefix not yet represented by an
                    // aggregate idle is replayable here. The shared terminal-cursor
                    // set covers root and children.
                    let aggregate_terminal_cursor = record
                        .checkpoint
                        .progress
                        .terminal_cursors
                        .iter()
                        .max()
                        .copied()
                        .ok_or_else(|| {
                            StateError::Run(RunError::internal(
                                "idle aggregate has no committed terminal lifecycle boundary",
                            ))
                        })?;
                    let at = |role| {
                        FactKey::new(
                            session_id,
                            role,
                            FactProvenance::LifecyclePrefix {
                                cursor: aggregate_terminal_cursor.0,
                            },
                        )
                    };
                    let budget = record.session.budget.clone();
                    record.push_fact(
                        at(FactRole::AggregateUsage),
                        OutboundKind::SessionUsage {
                            usage: projected_usage,
                            budget,
                        },
                    );
                    record.push_fact(
                        at(FactRole::AggregateStatusIdle),
                        OutboundKind::SessionStatusIdle { stop_reason },
                    );
                }
            }
            record.deferred_session_stop_reason = None;
        }
        Ok(())
    }
}
