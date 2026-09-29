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
mod permission_harvest;
mod publication;
mod refresh_entrypoint;
pub(crate) use refresh_entrypoint::ProjectionObservationDecision;
mod root_projection;
mod run_observation_projection;
mod source_fence;
mod usage_projection;
use candidate_evidence::CandidateEvidence;
use canonical_order::*;
pub(super) use root_projection::transcript_fact_key;

impl ManagedState {
    async fn build_committed_projection_candidate(
        &self,
        session_id: &str,
    ) -> Result<
        publication::ProjectionRefreshBuild<publication::ProjectionRefreshCandidate>,
        StateError,
    > {
        let started = awaken_lease_contract::mono::Stopwatch::start();
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Include that root in the
        // same private candidate as Runtime facts so GET cannot publish a stale
        // preparing/idle status or a torn cross-source projection.
        let persisted = self
            .roles
            .roots
            .session(&awaken_session_types::SessionId::from(session_id))
            .await
            .map_err(StateError::from)?;
        // Snapshot the complete warm projection and its process-local CAS token
        // before asynchronous source reads. Every path below builds privately
        // and publishes through the same compare-and-swap owner.
        let base_record = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or(StateError::NotFound)?;
        self.build_committed_projection_candidate_over(session_id, persisted, base_record, started)
            .await
    }

    /// Reduce one committed prefix onto `base_record`. The warm refresh
    /// passes this replica's published record; the warm/cold oracle passes a
    /// freshly rehydrated one, so both run the one reducer.
    async fn build_committed_projection_candidate_over(
        &self,
        session_id: &str,
        persisted: awaken_session_state_contract::PersistedSession,
        base_record: SessionRecord,
        started: awaken_lease_contract::mono::Stopwatch,
    ) -> Result<
        publication::ProjectionRefreshBuild<publication::ProjectionRefreshCandidate>,
        StateError,
    > {
        let persisted_status = Self::wire_session_status(&persisted);
        let base_cache_revision = base_record.cache_revision;
        let evidence = CandidateEvidence::read(
            self,
            session_id,
            &persisted,
            base_record.checkpoint.source.lifecycle_cursor,
            started,
        )
        .await?;
        let selection_done = started.elapsed();
        if self
            .candidate_source_moved(session_id, &persisted, &evidence)
            .await?
        {
            return Ok(publication::ProjectionRefreshBuild::Retry);
        }
        let validation_done = started.elapsed();

        // Build the complete disposable projection on a private result snapshot. A
        // transcript can be observed between its ToolUse and ToolResult commits,
        // and any later validation/canonicalization error must not retain the
        // earlier mutations (message-consumption marks, cursors, or Events) in
        // this process. Durable Session/Runtime facts remain the sole recovery
        // authority; only a fully valid projection replaces the warm cache.
        let mut staged_record = base_record;
        Self::apply_persisted_session(&mut staged_record, &persisted);
        let record = &mut staged_record;
        let root = self.project_root_prefix(record, session_id, &persisted, &evidence)?;
        let delegation = evidence.delegation();
        self.append_delegation_projections(record, delegation)?;
        self.close_aggregate(
            record,
            session_id,
            &persisted,
            persisted_status,
            &evidence,
            &root,
        )?;
        // Parent terminality is a durable Session fact and absorbs every
        // derived child lifecycle. Keeping this at the end of the sole warm/cold
        // projector preserves child output/lifecycle ordering, closes each child
        // stream, then closes only the aggregate primary stream.
        self.append_activation_failure_error_projection(
            record,
            &persisted,
            &evidence.acceptance.accepted,
        )?;
        if persisted.execution
            == awaken_session_state_contract::SessionExecutionState::ActivationFailed
            || persisted.terminal_cleanup.runtime_commit_cursor().is_some()
        {
            self.append_parent_terminal_projection(record)?;
        }
        Self::replace_interval_aggregate_projections(
            record,
            &persisted,
            &evidence.acceptance.accepted,
            &evidence.historical_pending,
            evidence.price_snapshot.as_ref(),
        )?;
        let reduction_done = started.elapsed();
        Self::canonicalize_committed_events(
            record,
            CanonicalizationEvidence {
                persisted: &persisted,
                lifecycle_events: &evidence.acceptance.accepted,
                root_snapshot: evidence.root.as_ref(),
                outcome_projections: &evidence.outcomes,
                delegation,
                root_pending: evidence.root_pending.as_ref(),
                child_pending: &evidence.child_pending,
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
                .map_err(Self::map_application_mutation_error)?;
            let expected_owner = self.owner_scope(session_id).ok_or(StateError::NotFound)?;
            if !refresh_entrypoint::logical_deletion_is_proven(
                true,
                persisted.session_id == session_id,
                owner == expected_owner,
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
        let publication_started = awaken_lease_contract::mono::Stopwatch::start();
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

// Test-only re-exports stay after every production item: they name the
// kernels the Session event tests pin, under the paths those tests use.
#[cfg(test)]
pub(super) use awaken_managed_session_events::{
    budget_reach_close_cursor, budget_reach_projection_close_cursor,
    message_projection_ordinal_bound,
};
#[cfg(test)]
pub(super) use source_fence::projection_validation_needs_trailing_root;
