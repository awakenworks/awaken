// @formal surface obligation=memory_extraction.unknown_execution_never_reenters_retry
// @formal reason: The existing extraction controller owns claims and recovery. This driver reads
// @formal reason: one committed authority and dispatches only the original stable auxiliary Run;
// @formal reason: legacy/unknown work cannot relocate or create a replacement Run.
//! Memory extraction as ordinary Worker Runs, over the existing delivery owners.

mod settlement;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_dispatch_contract::{
    RunDispatch, SessionRunAdmissionObservation, SessionRunAdmissionReads as _,
};
use awaken_ext_memory::{
    MemoryExtractionCommitScope, MemoryExtractionDriver, MemoryExtractionError,
    MemoryExtractionIntent, MemoryExtractionMutation, MemoryExtractionOutcome,
    MemoryMutationReceipt,
};
use awaken_runtime_contract::RunActivation;
use awaken_session_types::{RunError, RunErrorKind};

use super::dispatched_runs::DispatchedRuns;
use crate::{
    CommittedReads, CompletionRegistry, MemoryBinding, MemoryProposalError, SessionProjections,
};

pub(super) fn settlement_observer(
    ports: &super::WorkerPlacement,
) -> Arc<dyn awaken_dispatch_contract::DispatchSettlementObserver> {
    Arc::new(settlement::DispatchedMemorySettlement::new(ports))
}

pub(super) async fn recover_bindings(
    ports: &super::WorkerPlacement,
    session: &str,
    manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
) -> Result<(), RunError> {
    for input in manifest.resources.inputs() {
        let Some((_, binding)) = ports
            .memory_bindings
            .compile(session, &manifest.workspace_id, input, None)
            .await?
        else {
            continue;
        };
        if !binding.extraction_enabled {
            continue;
        }
        let driver = Arc::new(DispatchedMemory::new(
            binding,
            ports.reads.clone(),
            ports.projections.clone(),
            ports.owner.clone(),
            ports.completion.clone(),
        ));
        ports
            .memory_extractions
            .recover(session, driver)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
    }
    Ok(())
}

pub(super) struct DispatchedMemory {
    binding: Arc<MemoryBinding>,
    reads: Arc<CommittedReads>,
    projections: Arc<SessionProjections>,
    owner: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
    completion: Arc<CompletionRegistry>,
}

impl DispatchedMemory {
    pub(super) fn new(
        binding: Arc<MemoryBinding>,
        reads: Arc<CommittedReads>,
        projections: Arc<SessionProjections>,
        owner: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
        completion: Arc<CompletionRegistry>,
    ) -> Self {
        Self {
            binding,
            reads,
            projections,
            owner,
            completion,
        }
    }
    fn unknown(
        intent: &MemoryExtractionIntent,
        reason: impl Into<String>,
    ) -> MemoryExtractionOutcome {
        MemoryExtractionOutcome::ExecutionIndeterminate {
            auxiliary_run_id: intent.auxiliary_run_id(),
            reason: reason.into(),
        }
    }

    async fn plan(
        &self,
        intent: &MemoryExtractionIntent,
        snapshot: &RunRecoverySnapshot,
    ) -> MemoryExtractionOutcome {
        let run = RunId(intent.auxiliary_run_id());
        let record = match snapshot.run_record(&run) {
            Ok(record) => record,
            Err(_) => {
                return MemoryProposalError::RunIdentityMismatch.into_extraction_outcome(intent);
            }
        };
        let writes = match crate::committed_memory_writes(intent, record, &snapshot.messages) {
            Ok(writes) => writes,
            Err(error) => return error.into_extraction_outcome(intent),
        };
        match self.binding.platform().plan_mutations(writes).await {
            Ok(mutations) => MemoryExtractionOutcome::Mutations(mutations),
            Err(reason) => MemoryExtractionOutcome::Retryable { reason },
        }
    }

    fn original_request_matches(
        intent: &MemoryExtractionIntent,
        expected: RunActivation,
        request: &RunDispatch,
    ) -> bool {
        let expected = awaken_dispatch_execution::placement::bind_root_tool_configuration_format(
            expected, false,
        );
        request.activation == expected
            && request.identity_scope
                == awaken_dispatch_contract::DispatchIdentityScope::FullDispatch
            && !request.requires_session_work_authority()
            && request.session_thread_id() == &ThreadId(intent.auxiliary_commit_thread_id())
            && request
                .execution_scope
                .as_ref()
                .map(|scope| scope.0.as_str())
                == Some(intent.workspace_id.as_str())
            && request.placement.location
                == awaken_worker_contract::ExecutionLocation::RemoteRequired
    }

    async fn extract_run(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        if !self.binding.matches_intent(intent) {
            return MemoryExtractionOutcome::PermanentFailure {
                reason: "Memory extraction belongs to another frozen binding".into(),
            };
        }
        let run = RunId(intent.auxiliary_run_id());
        let thread = ThreadId(intent.auxiliary_thread_id());
        let commit = match self
            .reads
            .commit_for_read(&intent.auxiliary_commit_thread_id())
            .await
        {
            Ok(commit) => commit,
            Err(error) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                };
            }
        };
        let Some(authority) = commit.committed_authority() else {
            return MemoryExtractionOutcome::PermanentFailure {
                reason: "Memory extraction requires committed Coordinator authority".into(),
            };
        };
        let snapshot = match authority.recovery_snapshot(&thread, &run).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                };
            }
        };
        if snapshot.thread_id != thread || snapshot.claimed_run_id != run {
            return MemoryProposalError::RunIdentityMismatch.into_extraction_outcome(intent);
        }
        let record = match snapshot.run_record(&run) {
            Ok(record) => record,
            Err(_) => {
                return MemoryProposalError::RunIdentityMismatch.into_extraction_outcome(intent);
            }
        };
        if record.is_some_and(|record| record.state != RunState::Running) {
            return self.plan(intent, &snapshot).await;
        }
        if intent.execution().commit_scope != MemoryExtractionCommitScope::AuxiliaryThread {
            return Self::unknown(
                intent,
                "legacy extraction retains its original Session commit partition; no committed result authorizes relocation",
            );
        }
        if let Err(error) = self
            .projections
            .register_thread_workspace(&thread.0, &intent.workspace_id)
        {
            return MemoryExtractionOutcome::Retryable {
                reason: error.to_string(),
            };
        }
        match self.projections.projection(&thread.0) {
            Ok(None) => {}
            Ok(Some(_)) => {
                return MemoryExtractionOutcome::PermanentFailure {
                    reason: "auxiliary extraction Thread already belongs to a Session projection"
                        .into(),
                };
            }
            Err(error) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                };
            }
        }
        let mut activation = RunActivation::new(
            run.clone(),
            thread.clone(),
            intent.extractor.agent.clone(),
            crate::memory_extraction_input(intent),
        );
        activation.data_subject_id = intent.execution().data_subject_id.clone();
        let observation = match self.owner.observe_session_run_admission(&run).await {
            Ok(observation) => observation,
            Err(error) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                };
            }
        };
        let runs = DispatchedRuns::new(
            self.projections.as_ref(),
            self.owner.as_ref(),
            &self.completion,
            thread.clone(),
            commit,
            false,
        );
        let result = match observation {
            SessionRunAdmissionObservation::Live(request) => {
                if !Self::original_request_matches(intent, activation, &request) {
                    return MemoryExtractionOutcome::PermanentFailure {
                        reason:
                            "extractor Run id already has another frozen request or commit scope"
                                .into(),
                    };
                }
                runs.deliver(*request).await
            }
            SessionRunAdmissionObservation::Completed { .. } => {
                return Self::unknown(
                    intent,
                    "dispatch completion has no authoritative extractor result",
                );
            }
            SessionRunAdmissionObservation::Missing if record.is_some() => {
                return Self::unknown(intent, "started extraction has no original dispatch row");
            }
            SessionRunAdmissionObservation::Missing => runs.dispatch(activation).await,
        };
        let snapshot = match authority.recovery_snapshot(&thread, &run).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Self::unknown(
                    intent,
                    format!("extractor delivery requires authoritative observation: {error}"),
                );
            }
        };
        if snapshot.thread_id != thread || snapshot.claimed_run_id != run {
            return MemoryProposalError::RunIdentityMismatch.into_extraction_outcome(intent);
        }
        match snapshot.run_record(&run) {
            Ok(Some(_)) => self.plan(intent, &snapshot).await,
            Err(_) => MemoryProposalError::RunIdentityMismatch.into_extraction_outcome(intent),
            Ok(None) => match result {
                Err(RunError {
                    kind: RunErrorKind::BadRequest,
                    message,
                    ..
                }) => MemoryExtractionOutcome::PermanentFailure { reason: message },
                Err(error) => MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                },
                Ok(_) => Self::unknown(
                    intent,
                    "settled extraction has no authoritative committed Run",
                ),
            },
        }
    }
}

#[async_trait::async_trait]
impl MemoryExtractionDriver for DispatchedMemory {
    fn accepts(&self, intent: &MemoryExtractionIntent) -> bool {
        self.binding.matches_intent(intent)
    }

    fn extraction_commit_scope(&self) -> MemoryExtractionCommitScope {
        MemoryExtractionCommitScope::AuxiliaryThread
    }

    async fn validate_binding(
        &self,
        _intent: &MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        self.binding.validate_extraction_binding().await
    }

    async fn extract(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        self.extract_run(intent).await
    }

    async fn apply(
        &self,
        _intent: &MemoryExtractionIntent,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        self.binding.platform().apply_mutation(mutation).await
    }
}
