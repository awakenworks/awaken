// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Detached event admission, exact-prefix observations and terminal settlement mutate
// @formal reason: the same revision-bound Session root. Replay and ordering remain covered by the
// @formal reason: existing transition checks and tests; external dispatch is not proved by this link.
use super::{
    SessionCleanupError, SessionEventBatch, SessionEventBatchError, SessionEventCommand,
    SessionEventProjectionAnchor, SessionEventsOperations, SessionRevision,
};

impl SessionEventsOperations<'_> {
    /// Append one detached admission at this root's exact successor revision.
    /// Closed retry receipts may still be retained by a terminal root.
    pub fn append_batch(&mut self, batch: SessionEventBatch) -> Result<(), SessionEventBatchError> {
        let state = &mut self.aggregate.state;
        let next = state
            .revision
            .0
            .checked_add(1)
            .map(SessionRevision)
            .ok_or(SessionEventBatchError::ProgressMismatch)?;
        let expected_id =
            awaken_session_event_contract::session_event_batch_id(&state.session_id, next)?;
        if batch.admitted_revision != next
            || batch.batch_id != expected_id
            || batch.wake_activity_epoch.is_some()
            || (state.is_terminal() && !batch.admission_closed)
            || state.event_batches.iter().any(|old| {
                old.batch_id == batch.batch_id
                    || (batch.idempotency_key.is_some()
                        && old.idempotency_key == batch.idempotency_key)
            })
        {
            return Err(SessionEventBatchError::ProgressMismatch);
        }
        if batch.admission_closed && !batch.events.is_empty() {
            return Err(SessionEventBatchError::ProgressMismatch);
        }
        for (ordinal, entry) in batch.events.iter().enumerate() {
            let operation = awaken_session_event_contract::session_event_batch_operation(
                &batch.batch_id,
                ordinal,
            )?;
            if entry.event.operation_id() != operation {
                return Err(SessionEventBatchError::ProgressMismatch);
            }
            match &entry.event {
                SessionEventCommand::UserMessage { run_id, .. }
                    if *run_id
                        != awaken_session_event_contract::session_event_user_run_id(
                            &state.session_id,
                            &operation,
                        ) =>
                {
                    return Err(SessionEventBatchError::ProgressMismatch);
                }
                SessionEventCommand::DefineOutcome { outcome_id, .. }
                    if *outcome_id
                        != awaken_session_event_contract::session_event_outcome_id(
                            &state.session_id,
                            &operation,
                        ) =>
                {
                    return Err(SessionEventBatchError::ProgressMismatch);
                }
                _ => {}
            }
        }
        batch.validate_tool_reply_occupancy(&state.event_batches)?;
        state.event_batches.push(batch);
        Ok(())
    }

    /// Apply a committed-input/processed observation and an optional exact
    /// Interrupt supersession atomically. Rejection preserves the whole root.
    pub fn observe(
        &mut self,
        batch_id: &str,
        operation_id: &str,
        projection_anchor: SessionEventProjectionAnchor,
        processed: bool,
        now: u64,
        superseded: Option<(&str, &str)>,
    ) -> Result<bool, SessionEventBatchError> {
        let mut batches = self.aggregate.state.event_batches.clone();
        let mut changed;
        {
            let batch = batches
                .iter_mut()
                .find(|batch| batch.batch_id == batch_id)
                .ok_or(SessionEventBatchError::ProgressMismatch)?;
            let entry = batch
                .events
                .iter()
                .find(|entry| entry.event.operation_id() == operation_id)
                .ok_or(SessionEventBatchError::ProgressMismatch)?;
            if superseded.is_some()
                && !matches!(&entry.event, SessionEventCommand::Interrupt { .. })
            {
                return Err(SessionEventBatchError::ProgressMismatch);
            }
            changed = if processed {
                batch.mark_processed(operation_id, projection_anchor, now)?
            } else {
                batch.anchor_committed_input(operation_id, projection_anchor)?
            };
        }
        if let Some((failed_batch_id, failed_operation_id)) = superseded {
            let batch = batches
                .iter_mut()
                .find(|batch| batch.batch_id == failed_batch_id)
                .ok_or(SessionEventBatchError::ProgressMismatch)?;
            let ordinal = batch
                .events
                .iter()
                .position(|entry| entry.event.operation_id() == failed_operation_id)
                .ok_or(SessionEventBatchError::ProgressMismatch)?;
            if !matches!(
                &batch.events[ordinal].event,
                SessionEventCommand::ToolReply { .. }
            ) {
                return Err(SessionEventBatchError::ProgressMismatch);
            }
            if !batch.events[ordinal].processed {
                let adjacent_system = batch.events.get(ordinal + 1).and_then(|entry| {
                    matches!(&entry.event, SessionEventCommand::SystemMessage { .. })
                        .then(|| entry.event.operation_id().to_string())
                });
                changed |= batch.mark_processed(failed_operation_id, projection_anchor, now)?;
                if let Some(system_operation_id) = adjacent_system
                    && batch.events.iter().any(|entry| {
                        entry.event.operation_id() == system_operation_id && !entry.processed
                    })
                {
                    changed |=
                        batch.mark_processed(&system_operation_id, projection_anchor, now)?;
                }
            }
        }
        if changed {
            self.aggregate.state.event_batches = batches;
        }
        Ok(changed)
    }

    /// Resolve accepted input from this root's verified frozen cleanup cursor.
    pub fn resolve_terminally(&mut self, now: u64) -> Result<usize, SessionCleanupError> {
        let state = &mut self.aggregate.state;
        if !state.is_terminal() {
            return Ok(0);
        }
        let cleanup = state.verified_terminal_cleanup()?;
        if !cleanup.is_requested() && !cleanup.is_completed() {
            return Ok(0);
        }
        let anchor = cleanup.runtime_commit_cursor().map(|source_commit_cursor| {
            SessionEventProjectionAnchor {
                source_commit_cursor,
            }
        });
        Ok(state
            .event_batches
            .iter_mut()
            .map(|batch| batch.resolve_terminally(anchor, now))
            .sum())
    }
}
