//! Atomic outbox relay operations for the in-memory Dispatch aggregate.
//!
//! The parent module still owns the one mutex-backed state and validation
//! helpers. This module is only the `Outbox` port projection over that authority.

use super::*;

#[async_trait]
impl Outbox for MemoryDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        let input = normalize_pending_millis(input);
        if let Some(existing) = state
            .outbox
            .iter()
            .find(|i| i.message_id == input.message_id)
        {
            return if existing == &input {
                Ok(false)
            } else {
                Err(DispatchError::Rejected(format!(
                    "idempotency key `{}` was reused with another outbox payload",
                    input.message_id
                )))
            };
        }
        state.outbox.push(input);
        Ok(true)
    }

    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: u64,
    ) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        let input = normalize_pending_millis(input);
        let row = state.rows.get(&input.run_id).ok_or_else(|| {
            DispatchError::Rejected(format!(
                "Session resume Run `{}` was not found",
                input.run_id.0
            ))
        })?;
        validate_session_resume_target(
            &row.request,
            &input,
            session_thread_id,
            prior_session_activity_epoch,
            session_activity_epoch,
        )?;
        let current_epoch = row.request.session_activity_epoch;
        let accepts_new_resume = !row.cancellation_requested
            && matches!(
                (row.state, row.lease.is_some()),
                (DispatchState::Pending | DispatchState::Awaiting, false)
                    | (DispatchState::Leased, true)
            );
        let exact = validate_session_resume_evidence(
            &input,
            state
                .outbox
                .iter()
                .chain(state.pending.iter().map(|pending| &pending.input)),
        )?;
        if !validate_session_resume_activity_transition(
            current_epoch,
            prior_session_activity_epoch,
            session_activity_epoch,
            exact,
        )? {
            return Ok(false);
        }
        if !accepts_new_resume {
            return Err(DispatchError::Rejected(
                "Session resume requires a Pending, Awaiting, or currently Leased dispatch"
                    .to_string(),
            ));
        }
        state
            .rows
            .get_mut(&input.run_id)
            .expect("validated dispatch row remains under the transaction mutex")
            .request
            .session_activity_epoch = Some(session_activity_epoch);
        state.outbox.push(input);
        Ok(true)
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        // Validate the whole in-memory transaction before moving anything: an
        // outbox retry may match an existing pending row exactly, but the same
        // id with another payload is corruption and must leave the outbox intact.
        for input in &state.outbox {
            if let Some(existing) = state
                .pending
                .iter()
                .find(|pending| pending.input.message_id == input.message_id)
                && existing.input != *input
            {
                return Err(DispatchError::Rejected(format!(
                    "idempotency key `{}` was reused with another relayed payload",
                    input.message_id
                )));
            }
        }
        let staged = std::mem::take(&mut state.outbox);
        let relayed = staged.len();
        for input in staged {
            // Idempotent target append: skip a message already pending.
            if !state
                .pending
                .iter()
                .any(|p| p.input.message_id == input.message_id)
            {
                append_pending(&mut state, input)?;
            }
        }
        Ok(relayed)
    }

    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        let message_id = input.message_id.clone();
        // Canonical Run identity is checked first so a conflicting retry cannot
        // consume or alter the independently durable report.
        let replay = known_run_identity(&state, &request)?;
        let position = state
            .outbox
            .iter()
            .position(|candidate| candidate.message_id == message_id);
        let existing = position
            .map(|position| state.outbox[position].clone())
            .or_else(|| {
                state
                    .pending
                    .iter()
                    .find(|pending| pending.input.message_id == message_id)
                    .map(|pending| pending.input.clone())
            });
        if existing.as_ref().is_some_and(|existing| existing != &input) {
            return Err(DispatchError::Rejected(format!(
                "idempotency key `{message_id}` was reused with another continuation payload"
            )));
        }
        validate_outbox_continuation(&input, &request, &admission)?;
        if replay {
            if let Some(position) = position {
                state.outbox.remove(position);
            }
            return Ok(());
        }

        // The in-memory mutex is the transaction boundary. Validate pending
        // idempotency before enqueue so every remaining step is infallible and
        // an error cannot expose a half-applied state.
        if let Some(existing) = state
            .pending
            .iter()
            .find(|pending| pending.input.message_id == input.message_id)
            && existing.input != input
        {
            return Err(DispatchError::Rejected(format!(
                "idempotency key `{}` was reused with another pending-input payload",
                input.message_id
            )));
        }
        if let ContinuationAdmission::SessionChild(policy) = &admission {
            let parent = session_child_parent(&request)?.clone();
            ensure_session_child_capacity(
                &request,
                policy,
                known_session_child_threads(&state, &parent),
            )?;
        }
        enqueue_with_local(&mut state, request, SubmitOptions::default())?;
        append_pending(&mut state, input)?;
        if let Some(position) = position {
            state.outbox.remove(position);
        }
        Ok(())
    }
}
