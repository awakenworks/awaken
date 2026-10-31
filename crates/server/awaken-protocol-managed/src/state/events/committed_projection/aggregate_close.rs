// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Aggregate close of one projection candidate: the primary Thread's terminal
//! idles, the budget-reach pause, and the aggregate Running edge and
//! usage/idle pair of every period that no admitted interval owns.
//!
//! Consumer: the candidate builder, after the root and delegation projectors.

use super::candidate_evidence::CandidateEvidence;
use super::root_projection::RootProjection;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

impl ManagedState {
    pub(super) fn close_aggregate(
        &self,
        record: &mut SessionRecord,
        persisted: &awaken_session_contract::PersistedSession,
        evidence: &CandidateEvidence,
        root: &RootProjection,
        plan: &awaken_managed_session_events::AggregatePlan,
        index: &OrderIndex<'_>,
    ) -> Result<(), StateError> {
        let session_id = record.session.id.clone();
        let RootProjection {
            projected_budget_reach,
        } = root;
        let historical_pending = &evidence.historical_pending;
        // Every root Run boundary publishes its Thread idle (and fault) once,
        // from its own committed lifecycle and audit.
        for terminal in awaken_managed_session_events::root_run_boundaries(
            &session_id,
            &evidence.acceptance.accepted,
        ) {
            let idle_key =
                awaken_managed_session_events::root_terminal_idle_key(&record.session.id, terminal);
            let idle_id = record.fact_id(&idle_key);
            if record.has_event_id(&idle_id) {
                continue;
            }
            let facts = awaken_managed_session_events::root_terminal_facts(
                &record.session.id,
                terminal,
                evidence.root.as_ref(),
                historical_pending,
                index,
            )
            .map_err(projection_error)?;
            if let Some(error) = &facts.error {
                record.publish_fact_once(error, None)?;
            }
            record.publish_fact_once(&facts.idle, None)?;
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
        for reach in projected_budget_reach {
            for fact in reach.facts(&session_id) {
                record.publish_fact(&fact, None)?;
            }
            record.checkpoint.source.budget_reach_generation = reach.generation;
        }
        Self::render_aggregate_fallback(record, persisted, evidence, plan)
    }

    /// Render the sole domain aggregate plan after root and child tool event
    /// identities exist. This adapter never reselects a lifecycle period.
    fn render_aggregate_fallback(
        record: &mut SessionRecord,
        persisted: &awaken_session_contract::PersistedSession,
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
                        .map(|(thread, source)| {
                            let owner = (thread != &session_id).then_some(thread.as_str());
                            record.projected_tool_index_for(owner).event_id_for_source(
                                &ManagedState::public_tool_thread_id(&session_id, owner),
                                &source.call_id,
                                Some((source.source_id.as_str(), source.call_id.as_str())),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    event_ids.sort();
                    event_ids.dedup();
                    StopReason::RequiresAction { event_ids }
                }
            };
            let usage = session_usage_value(
                awaken_managed_session_events::session_usage_as_of(
                    &persisted.closed_runtime_intervals,
                    &session_id,
                    evidence.root.as_ref(),
                    &evidence.children,
                    close.source_commit_cursor,
                )
                .map_err(projection_error)?,
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
