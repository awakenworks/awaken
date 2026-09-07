//! Model invocation evidence uses the same incremental commit owner as tools.
//! There is no receipt store, recovery scheduler, or new execution authority.

use super::*;
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelOperation, ModelOperationPhase,
};
use awaken_runtime_contract::resolved::ModelBinding;

// Observation budget only: independent of Run execution, lease and provider
// deadlines. Expiry may discard this read, never a durable write or stop proof.
const CHECKPOINT_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The one predecessor decision, shared by initial recovery and every subsequent
/// invocation. Malformed evidence vetoes physical return just like Unknown.
fn verify_reentry(
    store: &Store,
    context: &RuntimeRunContext,
    recovering_checkpoint: bool,
) -> awaken_runtime_contract::llm::Result<Option<ModelOperation>> {
    let previous = ActiveModelOperation::load(store).map_err(|error| {
        context.physical_quiescence.mark_uncertain();
        awaken_runtime_contract::llm::Error::Unauthorized(format!(
            "model operation evidence unavailable: {error}"
        ))
    })?;
    if !ModelOperation::permits_reentry(previous.as_ref(), recovering_checkpoint) {
        context.physical_quiescence.mark_uncertain();
        return Err(awaken_runtime_contract::llm::Error::Provider(
            "previous model invocation has no termination evidence".into(),
        ));
    }
    Ok(previous)
}

/// A transient observation, not another persistent recovery state machine.
pub(super) enum RecoveryObservation {
    Ready(Option<StreamCheckpoint>),
    End(EndCause),
}

/// Observe recovery once before initial/resumed effects, not after permission
/// approval or delegation. Pass the validated partial into the Native loop;
/// model-free handoffs share this gate without needing a model executor.
pub(super) async fn recover_checkpoint(
    store: &Store,
    context: &RuntimeRunContext,
    run_id: &RunId,
    thread_id: &ThreadId,
) -> Result<RecoveryObservation> {
    let uncertain = || {
        context.physical_quiescence.mark_uncertain();
        if context.is_cancelled() {
            EndCause::Cancelled
        } else {
            EndCause::Indeterminate
        }
    };
    let previous = match verify_reentry(store, context, false) {
        Ok(previous) => previous,
        Err(awaken_runtime_contract::llm::Error::Unauthorized(detail)) => {
            return Err(Error::Commit(detail));
        }
        Err(_) => return Ok(RecoveryObservation::End(uncertain())),
    };
    let checkpoint = match context.stream_checkpoint.as_ref() {
        None => None,
        Some(checkpoints) => {
            let read = async {
                tokio::time::timeout(CHECKPOINT_OBSERVATION_TIMEOUT, checkpoints.get(&run_id.0))
                    .await
                    .unwrap_or_else(|_| {
                        Err(awaken_agent_contract::stream::checkpoint::StreamCheckpointError::Storage(
                            "checkpoint observation deadline exceeded".into(),
                        ))
                    })
            };
            match inference::model_read(context.cancellation.as_ref(), read).await {
                Some(Ok(checkpoint))
                    if checkpoint.as_ref().is_none_or(|checkpoint| {
                        checkpoint.run_id == run_id.0 && checkpoint.thread_id == thread_id.0
                    }) =>
                {
                    checkpoint
                }
                observation => {
                    match observation {
                        Some(Err(error)) => {
                            tracing::warn!(run_id = %run_id.0, %error, "failed to load stream checkpoint")
                        }
                        Some(Ok(_)) => {
                            tracing::warn!(run_id = %run_id.0, "stream checkpoint identity mismatch")
                        }
                        None => {}
                    }
                    return Ok(RecoveryObservation::End(uncertain()));
                }
            }
        }
    };
    if !ModelOperation::permits_reentry(previous.as_ref(), checkpoint.is_some()) {
        return Ok(RecoveryObservation::End(uncertain()));
    }
    if context.is_cancelled() {
        return Ok(RecoveryObservation::End(EndCause::Cancelled));
    }
    Ok(RecoveryObservation::Ready(checkpoint))
}

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
        verify_reentry(self.store, self.context, recovering_checkpoint).inspect_err(|error| {
            if matches!(error, awaken_runtime_contract::llm::Error::Unauthorized(_)) {
                self.failed = true;
            }
        })
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
