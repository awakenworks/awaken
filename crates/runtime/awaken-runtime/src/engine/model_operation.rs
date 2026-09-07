//! Model invocation evidence uses the same incremental commit owner as tools.
//! There is no receipt store, recovery scheduler, or new execution authority.

use super::*;
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelOperation, ModelOperationPhase,
};
use awaken_runtime_contract::resolved::ModelBinding;

pub(super) struct ModelOperationJournal<'a> {
    pub ledger: &'a mut StepLedger,
    pub store: &'a mut Store,
    pub context: &'a RuntimeRunContext,
    pub thread_id: &'a ThreadId,
    pub run_id: &'a RunId,
    failed: bool,
}

impl<'a> ModelOperationJournal<'a> {
    pub fn new(
        ledger: &'a mut StepLedger,
        store: &'a mut Store,
        context: &'a RuntimeRunContext,
        thread_id: &'a ThreadId,
        run_id: &'a RunId,
    ) -> Self {
        Self {
            ledger,
            store,
            context,
            thread_id,
            run_id,
            failed: false,
        }
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn verify_reentry(
        &mut self,
        recovering_checkpoint: bool,
    ) -> awaken_runtime_contract::llm::Result<Option<ModelOperation>> {
        let previous =
            ActiveModelOperation::load(self.store).map_err(|error| self.reject(error))?;
        if !ModelOperation::permits_reentry(previous.as_ref(), recovering_checkpoint) {
            self.context.physical_quiescence.mark_uncertain();
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "previous model invocation has no termination evidence".into(),
            ));
        }
        Ok(previous)
    }

    pub async fn enter(
        &mut self,
        binding: &ModelBinding,
    ) -> awaken_runtime_contract::llm::Result<()> {
        let previous = self.verify_reentry(false)?;
        let next = ModelOperation::next(previous.as_ref(), binding.clone())
            .ok_or_else(|| self.reject("model operation ordinal exhausted"))?;
        self.persist(next).await
    }

    pub async fn returned(
        &mut self,
        timed_out: bool,
        execution: awaken_runtime_contract::model_operation::ModelExecutionEvidence,
    ) -> awaken_runtime_contract::llm::Result<()> {
        let mut operation = ActiveModelOperation::load(self.store)
            .map_err(|error| self.reject(error))?
            .ok_or_else(|| self.reject("model operation entry is missing"))?;
        operation.phase = if timed_out {
            ModelOperationPhase::TimedOut
        } else {
            ModelOperationPhase::Returned
        };
        operation.execution = execution;
        self.persist(operation).await
    }

    async fn persist(
        &mut self,
        operation: ModelOperation,
    ) -> awaken_runtime_contract::llm::Result<()> {
        let command = ActiveModelOperation::write(&Some(operation));
        self.ledger.staged_state.push(command.clone());
        if validate_batch(&self.ledger.staged_state).is_err() {
            self.ledger.rollback_state();
            return Err(self.reject("staged state conflicts before model invocation"));
        }
        if self.context.commit.is_some()
            && let Err(error) = self
                .ledger
                .commit_delta(self.context, self.thread_id, self.run_id)
                .await
        {
            // An unacknowledged write may already be durable. Do not advance
            // local truth, retry inference, or let the failed draft ride a later
            // terminal commit. The exact persisted prefix remains authoritative.
            self.ledger.rollback_state();
            return Err(self.reject(error));
        }
        self.store.apply(&command);
        Ok(())
    }

    fn reject(&mut self, error: impl std::fmt::Display) -> awaken_runtime_contract::llm::Error {
        self.failed = true;
        awaken_runtime_contract::llm::Error::Unauthorized(format!(
            "model operation evidence unavailable: {error}"
        ))
    }
}
