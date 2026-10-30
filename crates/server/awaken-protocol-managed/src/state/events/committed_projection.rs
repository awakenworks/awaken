// @formal entrypoint session.runtime_projection_failed_refresh_stutters_warm_state
// @formal entrypoint session.runtime_projection_logical_deletion_requires_positive_exact_scoped_evidence
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! The sole warm/cold projection from committed Session, Thread, Run, and Outcome truth.
//!
//! One candidate is built in stages, each owned by its own module: the
//! committed evidence is read and derived (`candidate_evidence`), its source
//! prefix is fenced again (`source_fence`), a private copy of the record is
//! reduced (`root_projection`, the delegation projector, `aggregate_close`,
//! the interval and terminal projections), and the result is put in
//! canonical order (`canonical_order`) before the CAS publisher sees it.

use super::*;

mod activation_failure;
mod aggregate_close;
mod candidate_evidence;
mod canonical_order;
mod historical_pending;
mod interval_aggregate_projection;
#[cfg(test)]
mod oracle_tests;
mod publication;
mod refresh_entrypoint;
pub(crate) use refresh_entrypoint::ProjectionObservationDecision;
mod root_projection;
mod run_observation_projection;
pub(in crate::state::events) mod source_fence;
mod usage_projection;
use crate::state::session_record::ProjectionDeliveryMemo;
use awaken_managed_session_events::{
    BudgetReachAnchor, OrderEvidence, OrderIndex, ParentTerminalEvidence,
};
use awaken_session_application::SessionRecoveryPurpose;
use candidate_evidence::CandidateEvidence;
use canonical_order::CanonicalizationEvidence;
pub(super) use root_projection::transcript_fact_key;

/// One clean reduction base. Only `memo` came from a prior warm publication;
/// `owner_scope` and `record` derive from the current durable Session root.
/// The record starts with
/// no committed Events, children, consumption marks or lifecycle checkpoint.
/// Consumer: the sole committed-prefix candidate builder below.
struct CommittedPrefixBase {
    owner_scope: String,
    record: SessionRecord,
    memo: ProjectionDeliveryMemo,
}

/// One update wire phase on the already-fenced, same immutable Root candidate.
/// It lends exact payloads to the original encoder and never publishes the cache.
fn project_root_fields_and_updates(
    record: &mut SessionRecord,
    persisted: &PersistedSession,
) -> Result<(), StateError> {
    ManagedState::apply_persisted_session(record, persisted)?;
    for (fact, update) in awaken_managed_session_events::derive_session_update_events(persisted)
        .map_err(projection_error)?
    {
        let kind = super::timeline_encoding::encode_fact(
            record,
            &fact,
            None,
            Some((persisted.session_id.as_str(), update)),
        )?;
        record.append_fact_once(fact.key, kind);
    }
    Ok(())
}

impl ManagedState {
    fn committed_prefix_base(
        &self,
        session_id: &str,
        owner_scope: &str,
        persisted: &awaken_session_contract::PersistedSession,
        memo: ProjectionDeliveryMemo,
    ) -> Result<CommittedPrefixBase, StateError> {
        if persisted.session_id != session_id {
            return Err(StateError::Run(RunError::internal(
                "committed projection root belongs to another Session",
            )));
        }
        let agent_id = persisted
            .agent_id()
            .map(str::to_string)
            .ok_or(StateError::NotFound)?;
        let session = self.rehydrated_session_for(
            session_id,
            owner_scope,
            persisted.clone(),
            SessionRecoveryPurpose::CollectionRead,
        )?;
        let mut record = SessionRecord::new(
            owner_scope.to_owned(),
            agent_id,
            session,
            persisted.revision,
            persisted.resources.clone(),
            RootTimeEvidence::from(persisted),
            Vec::new(),
        );
        record.cache_revision = memo.cache_revision;
        Ok(CommittedPrefixBase {
            owner_scope: owner_scope.to_string(),
            record,
            memo,
        })
    }

    async fn build_committed_projection_candidate(
        &self,
        session_id: &str,
    ) -> Result<
        publication::ProjectionRefreshBuild<publication::ProjectionRefreshCandidate>,
        StateError,
    > {
        let started = awaken_time::mono::Stopwatch::start();
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Include that root in the
        // same private candidate as Runtime facts so GET cannot publish a stale
        // preparing/idle status or a torn cross-source projection.
        let recovered = self.read_persisted_session_projection(session_id).await?;
        let persisted = recovered.session;
        // Only the process-local CAS token and transient delivery overlay
        // survive from the prior warm record. All timeline decisions are
        // rebuilt from this durable root and the fenced Thread evidence.
        let memo = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or(StateError::NotFound)?
            .into_delivery_memo();
        let base =
            self.committed_prefix_base(session_id, &recovered.owner_scope, &persisted, memo)?;
        self.build_committed_projection_candidate_over(session_id, persisted, base, started)
            .await
    }

    /// Reduce one committed prefix onto a clean root-derived base. Warm and
    /// cold callers enter this same reducer; no prior durable projection is
    /// an input to its fact selection.
    async fn build_committed_projection_candidate_over(
        &self,
        session_id: &str,
        persisted: awaken_session_contract::PersistedSession,
        base: CommittedPrefixBase,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<
        publication::ProjectionRefreshBuild<publication::ProjectionRefreshCandidate>,
        StateError,
    > {
        let CommittedPrefixBase {
            owner_scope,
            record: mut staged_record,
            memo,
        } = base;
        let base_cache_revision = staged_record.cache_revision;
        let evidence = CandidateEvidence::read(self, session_id, &persisted, started).await?;
        let selection_done = started.elapsed();
        if self
            .candidate_source_moved(session_id, &persisted, &evidence)
            .await?
        {
            return Ok(publication::ProjectionRefreshBuild::Retry);
        }
        let activation_failure = awaken_managed_session_events::activation_failure_projection(
            &persisted,
            &evidence.acceptance.accepted,
        )
        .map_err(projection_error)?;
        if matches!(
            &activation_failure,
            awaken_managed_session_events::ActivationFailureProjection::AwaitingSourceRun
        ) {
            // The Session failure precedes this claimed Run's terminal commit.
            // Reuse the existing bounded source retry; publishing either the
            // broader error or parent terminal now would withdraw or reorder
            // an issued Event when the Run's exact failure arrives.
            return Ok(publication::ProjectionRefreshBuild::Retry);
        }
        let validation_done = started.elapsed();
        // One committed ordering index serves both first-listable child
        // activity selection and the final Event sort. Neither stage may
        // rediscover a different cursor from mutable published Thread status.
        let outcomes = evidence
            .outcomes
            .iter()
            .map(awaken_managed_session_events::CommittedOutcome::as_anchored)
            .collect::<Vec<_>>();
        let budget_reach = BudgetReachAnchor::of(
            &persisted,
            &evidence.acceptance.accepted,
            &evidence.historical_pending,
        );
        let order_index = OrderIndex::new(OrderEvidence {
            updates: awaken_session_contract::SessionUpdateHistory::for_session(&persisted),
            session_id,
            batches: &persisted.event_batches,
            closed_intervals: &persisted.closed_runtime_intervals,
            budget_reach: &budget_reach,
            parent_terminal: ParentTerminalEvidence::of(&persisted),
            lifecycle: &evidence.acceptance.accepted,
            root: evidence.root.as_ref(),
            children: &evidence.children,
            links: &evidence.links,
            outcomes: &outcomes,
            historical_pending: &evidence.historical_pending,
        });
        // One domain plan selects both interval-owned and fallback aggregate
        // facts. The adapter only renders it after root/child tool identities
        // exist; it cannot run a second lifecycle selection walk.
        let (aggregate_plan, coordination) = evidence.derive_plans(&persisted, &order_index)?;

        // Build the complete disposable projection on a private result snapshot. A
        // transcript can be observed between its ToolUse and ToolResult commits,
        // and any later validation/canonicalization error must not retain the
        // earlier mutations (message-consumption marks, cursors, or Events) in
        // this process. Durable Session/Runtime facts remain the sole recovery
        // authority; only a fully valid projection replaces the warm cache.
        let record = &mut staged_record;
        project_root_fields_and_updates(record, &persisted)?;
        let root =
            self.project_root_prefix(record, session_id, &persisted, &evidence, &order_index)?;
        let delegation = evidence.delegation(&order_index);
        let mut child_threads =
            self.append_delegation_projections(record, delegation, &coordination)?;
        self.close_aggregate(
            record,
            &persisted,
            &evidence,
            &root,
            &aggregate_plan,
            &order_index,
        )?;
        // Parent terminality is a durable Session fact and absorbs every
        // derived child lifecycle. Keeping this at the end of the sole warm/cold
        // projector preserves child output/lifecycle ordering, closes each child
        // stream, then closes only the aggregate primary stream.
        self.append_activation_failure_error_projection(record, &activation_failure)?;
        if matches!(
            &activation_failure,
            awaken_managed_session_events::ActivationFailureProjection::Ready { .. }
        ) || persisted.terminal_cleanup.runtime_commit_cursor().is_some()
        {
            self.append_parent_terminal_projection(record, &mut child_threads)?;
        }
        Self::append_interval_aggregate_projections(
            record,
            &aggregate_plan,
            evidence.price_snapshot.as_ref(),
        )?;
        let reduction_done = started.elapsed();
        // Attach local previews only after every durable fact has been
        // selected. The canonicalizer reattaches them by their stable fact id,
        // never by a warm event position or as a projection input.
        record.events.extend(memo.transient_events);
        record.event_thread_owners.extend(memo.transient_owners);
        Self::canonicalize_committed_events(
            record,
            CanonicalizationEvidence {
                outcome_projections: &evidence.outcomes,
                index: &order_index,
                coordination: &coordination,
            },
        )?;
        if persisted.is_hidden() && persisted.is_terminal() {
            // Logical Delete is an absorbing Session-root fact, independent
            // of Runtime commit cursors. Emit it after the canonical retained
            // history, using the same renderer as the deletion producer and
            // scoped tombstone observer. A retained root must not close SSE as
            // mere Terminated before its actual Delete fact is delivered.
            let owner = self
                .roles
                .roots
                .owner(&awaken_session_types::SessionId::from(session_id))
                .await
                .map_err(StateError::from)?;
            if !refresh_entrypoint::logical_deletion_is_proven(
                true,
                persisted.session_id == session_id,
                owner == owner_scope,
                persisted.is_hidden(),
                persisted.is_terminal(),
                false,
            ) {
                return Err(StateError::NotFound);
            }
            record.append_event_once(session_deletion_event(&owner, session_id));
        }
        let canonicalization_done = started.elapsed();
        let timings = evidence.timings;
        tracing::debug!(
            session_id,
            outcomes_ms = timings.outcomes.as_millis() as u64,
            snapshots_ms = (timings.snapshots - timings.outcomes).as_millis() as u64,
            lifecycle_ms = (timings.lifecycle - timings.snapshots).as_millis() as u64,
            usage_ms = (evidence.usage_done - timings.lifecycle).as_millis() as u64,
            selection_ms = (selection_done - evidence.usage_done).as_millis() as u64,
            validation_ms = (validation_done - selection_done).as_millis() as u64,
            reduction_ms = (reduction_done - validation_done).as_millis() as u64,
            canonicalization_ms = (canonicalization_done - reduction_done).as_millis() as u64,
            elapsed_ms = canonicalization_done.as_millis() as u64,
            "Managed committed projection candidate completed"
        );
        Ok(publication::ProjectionRefreshBuild::Complete(
            publication::ProjectionRefreshCandidate::new(
                base_cache_revision,
                staged_record,
                persisted.revision,
                evidence.root.as_ref(),
                &evidence.children,
            ),
        ))
    }

    async fn refresh_committed_projection_once(
        &self,
        session_id: &str,
    ) -> Result<bool, StateError> {
        let build = self.build_committed_projection_candidate(session_id).await;
        let publication_started = awaken_time::mono::Stopwatch::start();
        let result = self.finish_projection_refresh(session_id, build);
        tracing::debug!(
            session_id,
            publication_ms = publication_started.elapsed().as_millis() as u64,
            "Managed committed projection publication completed"
        );
        #[cfg(test)]
        if matches!(result, Ok(true)) {
            Box::pin(self.check_warm_equals_cold_rebuild(session_id)).await;
        }
        result
    }
}
