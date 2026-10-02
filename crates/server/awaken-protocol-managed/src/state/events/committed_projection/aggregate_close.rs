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
        evidence: &CandidateEvidence,
        root: &RootProjection,
        plan: &awaken_managed_session_events::AggregatePlan,
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
        // Each historical reach is admitted by its own committed close and
        // answerable-wait history. A later Running Session, pending call or
        // child Archive cannot erase a previously issued budget pair.
        for (generation, usage, budget) in projected_budget_reach.iter().cloned() {
            let at =
                |role| FactKey::new(session_id, role, FactProvenance::BudgetReach { generation });
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
                    stop_details: None,
                },
            );
            record.checkpoint.source.budget_reach_generation = generation;
        }
        Self::render_aggregate_fallback(record, persisted, evidence, plan)
    }

    /// Render the sole domain aggregate plan after root and child tool event
    /// identities exist. This adapter never reselects a lifecycle period.
    fn render_aggregate_fallback(
        record: &mut SessionRecord,
        persisted: &awaken_session_state_contract::PersistedSession,
        evidence: &CandidateEvidence,
        plan: &awaken_managed_session_events::AggregatePlan,
    ) -> Result<(), StateError> {
        let session_id = record.session.id.clone();
        for opening in &plan.fallback_openings {
            Self::append_aggregate_running_once(record, opening.clone());
        }
        for close in &plan.fallback_closes {
            let stop_reason = match &close.stop {
                awaken_managed_session_events::AggregateStop::Run { calls_owner, stop } => {
                    let owner = (calls_owner != &session_id).then_some(calls_owner.as_str());
                    timeline_encoding::resolve_stop(record, owner, stop)?
                }
                awaken_managed_session_events::AggregateStop::Calls { thread_calls } => {
                    let mut event_ids = thread_calls
                        .iter()
                        .map(|(thread, call)| {
                            let owner = (thread != &session_id).then_some(thread.as_str());
                            record
                                .projected_tool_index_for(owner)
                                .latest
                                .get(call)
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
                    StopReason::RequiresAction { event_ids }
                }
            };
            let usage = session_usage_value(
                evidence.usage_as_of(persisted, &session_id, close.source_commit_cursor),
                evidence.price_snapshot.as_ref(),
            )
            .map_err(StateError::Run)?;
            let budget = record.session.budget.clone();
            record.append_fact_once(
                close.usage.clone(),
                OutboundKind::SessionUsage { usage, budget },
            );
            record.append_fact_once(
                close.idle.clone(),
                OutboundKind::SessionStatusIdle {
                    stop_reason,
                    stop_details: None,
                },
            );
        }
        Ok(())
    }
}
