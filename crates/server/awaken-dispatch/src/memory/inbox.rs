//! In-memory Inbox projection over the canonical dispatch state.

use super::*;

#[async_trait]
impl Inbox for MemoryDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        append_pending(&mut state, input)
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .pending
            .iter()
            .filter(|p| &p.input.thread_id == thread_id)
            .map(|p| PendingRecord {
                input: p.input.clone(),
                revision: p.revision,
            })
            .collect())
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(pos) = state
            .pending
            .iter()
            .position(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if state.pending[pos].revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        state.pending.remove(pos);
        Ok(CasOutcome::Applied)
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(row) = state
            .pending
            .iter_mut()
            .find(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if row.revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        row.input.result = result;
        row.revision += 1;
        Ok(CasOutcome::Applied)
    }
}
