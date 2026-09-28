// @formal entrypoint checkpoint.recovery_never_precedes_commit
// @formal evidence model_operation.ordinal_never_wraps_or_reuses_a_coordinate
// @formal evidence model_operation.reentry_requires_positive_execution_evidence
//! Model invocation evidence uses the same incremental commit owner as tools.
//! There is no receipt store, recovery scheduler, or new execution authority.

use super::*;
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelOperation, ModelOperationContext, ModelOperationPhase,
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
    let previous_unknown =
        !ModelOperation::permits_reentry(previous.as_ref(), recovering_checkpoint);
    if previous_unknown {
        context.physical_quiescence.mark_model_uncertain(
            previous
                .as_ref()
                .map_or(Default::default(), |operation| operation.effect_scope),
        );
    }
    if !context.physical_quiescence.permits_execution() || previous_unknown {
        return Err(awaken_runtime_contract::llm::Error::Provider(
            "attempt participants or previous model invocation lack termination evidence".into(),
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
/// Return the rebuilt state even on an observation error: control commands may
/// still seal logical tool state, but the uncertainty latch forbids physical ACK.
/// Owning the input here prevents callers pairing evidence with another view
/// and avoids retaining a duplicate full state log throughout model execution.
pub(super) async fn recover_checkpoint(
    committed_state: Vec<awaken_agent_contract::agent::state::Command>,
    context: &RuntimeRunContext,
    run_id: &RunId,
    thread_id: &ThreadId,
) -> (Store, Result<RecoveryObservation>) {
    // Inspect the SAME frozen prefix used to rebuild this execution view, never
    // context.reader again (resume may have a different authoritative reader).
    // A later completion cannot erase an older Unknown. A late receipt for an
    // older ordinal may resolve that invocation, but cannot rewind allocation.
    let recorded = ActiveModelOperation::recorded(&committed_state, run_id).map_err(|error| {
        context.physical_quiescence.mark_uncertain();
        Error::Commit(format!("model operation evidence unavailable: {error}"))
    });
    let mut store = store_from_run_commands(committed_state, run_id);
    let observation = match recorded {
        Ok(recorded) => {
            for operation in recorded
                .iter()
                .filter(|operation| !operation.execution.permits_reentry())
            {
                context
                    .physical_quiescence
                    .mark_model_uncertain(operation.effect_scope);
            }
            if let Some(latest) = recorded.last() {
                store.apply(&ActiveModelOperation::write(&Some(latest.clone())));
            }
            observe_checkpoint(&store, context, run_id, thread_id).await
        }
        Err(error) => Err(error),
    };
    (store, observation)
}

async fn observe_checkpoint(
    store: &Store,
    context: &RuntimeRunContext,
    run_id: &RunId,
    thread_id: &ThreadId,
) -> Result<RecoveryObservation> {
    let uncertain = || {
        if context.physical_quiescence.permits_execution() {
            context.physical_quiescence.mark_uncertain();
        }
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
        // No checkpoint store is wired at all: this deployment never flushes a
        // partial, so there is nothing to salvage and nothing was missed.
        None => None,
        Some(checkpoints) => {
            let read = async {
                tokio::time::timeout(CHECKPOINT_OBSERVATION_TIMEOUT, checkpoints.get(&run_id.0))
                    .await
                    .unwrap_or_else(|_| {
                        // The deadline expired with the read still outstanding.
                        // We learned nothing; we did not learn there is nothing.
                        Observed::Unknown(UnknownCause::Interrupted(
                            "checkpoint observation deadline exceeded".into(),
                        ))
                    })
            };
            // Every arm is written out: the only path that proceeds to fresh or
            // resumed execution is one where the store actually answered.
            match inference::model_read(context.cancellation.as_ref(), read).await {
                Some(Observed::Present(checkpoint))
                    if checkpoint.run_id == run_id.0 && checkpoint.thread_id == thread_id.0 =>
                {
                    Some(checkpoint)
                }
                Some(Observed::Absent) => None,
                Some(Observed::Present(_)) => {
                    tracing::warn!(run_id = %run_id.0, "stream checkpoint identity mismatch");
                    return Ok(RecoveryObservation::End(uncertain()));
                }
                Some(Observed::Unknown(cause)) => {
                    tracing::warn!(run_id = %run_id.0, error = %cause, "failed to load stream checkpoint");
                    return Ok(RecoveryObservation::End(uncertain()));
                }
                // Cancelled mid-read: the observation never completed.
                None => return Ok(RecoveryObservation::End(uncertain())),
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
        effect_scope: awaken_runtime_contract::model_operation::ModelEffectScope,
    ) -> awaken_runtime_contract::llm::Result<Option<ModelOperationContext>> {
        let previous = self.verify_reentry(false)?;
        let mut next = ModelOperation::next(previous.as_ref(), binding.clone())
            .ok_or_else(|| self.reject("model operation ordinal exhausted"))?;
        next.effect_scope = effect_scope;
        // An ephemeral direct Runtime can still infer, but its in-memory
        // ordinal cannot masquerade as a committed, reconstructible identity.
        let identity = self
            .context
            .commit
            .as_ref()
            .map(|_| ModelOperationContext::from_operation(self.thread_id, self.run_id, &next));
        self.persist(next).await?;
        Ok(identity)
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
