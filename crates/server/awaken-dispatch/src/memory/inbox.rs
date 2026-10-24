// @formal entrypoint ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
//! In-memory Inbox projection over the canonical dispatch state.

use super::*;
use crate::inbox_mutation::{
    PendingInputMutation, PendingInputMutationPlan, plan_pending_input_mutation,
};

#[async_trait]
impl Inbox for MemoryDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        append_pending(&mut state, input)
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let pos = state
            .pending
            .iter()
            .position(|p| p.input.message_id == message_id);
        match plan_pending_input_mutation(
            pos.map(|position| state.pending[position].revision),
            expected_revision,
            PendingInputMutation::Retract,
            u64::MAX,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => Ok(outcome),
            PendingInputMutationPlan::Retract => {
                state
                    .pending
                    .remove(pos.expect("retract plan proves a present row"));
                Ok(CasOutcome::Applied)
            }
            PendingInputMutationPlan::Edit { .. } => unreachable!("retract cannot plan edit"),
        }
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let row = state
            .pending
            .iter_mut()
            .find(|p| p.input.message_id == message_id);
        match plan_pending_input_mutation(
            row.as_ref().map(|row| row.revision),
            expected_revision,
            PendingInputMutation::Edit,
            u64::MAX,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => Ok(outcome),
            PendingInputMutationPlan::Edit { next_revision } => {
                let row = row.expect("edit plan proves a present row");
                row.input.result = result;
                row.revision = next_revision;
                Ok(CasOutcome::Applied)
            }
            PendingInputMutationPlan::Retract => unreachable!("edit cannot plan retract"),
        }
    }
}

#[async_trait]
impl awaken_dispatch_contract::InboxRead for MemoryDispatchStore {
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
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    use awaken_dispatch_contract::InboxRead;

    #[tokio::test]
    async fn exhausted_pending_edit_preserves_result_revision_and_retractability() {
        // Causes: I1 exact pending row at u64::MAX; I2 stale expected revision;
        // I3 an edit requests new bytes; I4 exact retract. Effects: R1 I1+I3
        // rejects without changing bytes/revision (and never poisons the lock);
        // R2 I2=>Mismatch; R3 I1+I4 removes the row without incrementing it.
        let store = MemoryDispatchStore::new(Arc::new(awaken_lease_testkit::ManualClock::new(0)));
        let thread = ThreadId("revision-limit-thread".into());
        store
            .append(PendingInput {
                message_id: "revision-limit".into(),
                run_id: RunId("revision-limit-run".into()),
                thread_id: thread.clone(),
                correlation_id: "revision-limit-call".into(),
                available_at_ms: None,
                context_messages: Vec::new(),
                result: ResumeResult::Input("old".into()),
            })
            .await
            .unwrap();
        lock(&store.state).unwrap().pending[0].revision = u64::MAX;
        let before = store.list(&thread).await.unwrap();
        assert!(
            store
                .edit(
                    "revision-limit",
                    u64::MAX,
                    ResumeResult::Input("new".into())
                )
                .await
                .is_err(),
            "R1"
        );
        assert_eq!(
            store.list(&thread).await.unwrap(),
            before,
            "R1 exact unchanged frame"
        );
        assert_eq!(
            store
                .edit("revision-limit", 1, ResumeResult::Input("new".into()))
                .await
                .unwrap(),
            CasOutcome::RevisionMismatch,
            "R2"
        );
        assert_eq!(
            store.retract("revision-limit", u64::MAX).await.unwrap(),
            CasOutcome::Applied,
            "R3"
        );
        assert!(
            store.list(&thread).await.unwrap().is_empty(),
            "R3 no stranded row"
        );
    }
}
