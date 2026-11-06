// @formal surface obligation=memory_extraction.unknown_execution_never_reenters_retry
// @formal reason: Coordinator settlement freezes the original request's extraction intent before
// @formal reason: returning. The existing controller owns claims and recovery; ordinary Run delivery
// @formal reason: owns execution, and only a committed authority can supply the captured prefix.
//! Memory terminal observation for Sessions executed by registered Workers.

use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_dispatch_contract::{DispatchSettlementError, DispatchSettlementObserver, RunDispatch};
use awaken_ext_memory::{
    MemoryExtractionCommitScope, MemoryExtractionDriver, MemoryExtractionExecution,
    MemoryExtractorSnapshot, MemoryTerminalExtraction, MemoryTerminalExtractionRequest,
    MemoryTerminalObserver,
};
use awaken_runtime_contract::terminal::{CommittedTerminalRun, RunTerminalObserver};
use awaken_worker_contract::RunClaim;

use super::super::WorkerPlacement;
use super::DispatchedMemory;
use crate::{
    CommittedReads, CompletionRegistry, MemoryBinding, MemoryBindingCompiler, MemoryExtractions,
    SessionCommit, SessionProjections,
};

pub(super) struct DispatchedMemorySettlement {
    bindings: MemoryBindingCompiler,
    extractions: Arc<MemoryExtractions>,
    reads: Arc<CommittedReads>,
    projections: Arc<SessionProjections>,
    owner: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
    completion: Arc<CompletionRegistry>,
}

impl DispatchedMemorySettlement {
    pub(super) fn new(ports: &WorkerPlacement) -> Self {
        Self {
            bindings: ports.memory_bindings.clone(),
            extractions: ports.memory_extractions.clone(),
            reads: ports.reads.clone(),
            projections: ports.projections.clone(),
            owner: ports.owner.clone(),
            completion: ports.completion.clone(),
        }
    }

    async fn observer(
        &self,
        dispatch: &RunDispatch,
        state: &RunState,
        commit: Arc<SessionCommit>,
    ) -> Result<Option<Arc<dyn RunTerminalObserver>>, DispatchSettlementError> {
        // The existing descriptor namespace marks a proposal-producing Run,
        // which cannot recursively create another extraction for its output.
        if crate::MemoryWriteProposals::descriptor(&dispatch.activation.snapshot).is_some() {
            return Ok(None);
        }
        let Some((config, model)) = awaken_ext_memory::published_extraction_configuration(
            &dispatch.activation.snapshot,
            dispatch.activation.effective_model_ref(),
        )
        .map_err(DispatchSettlementError)?
        else {
            return Ok(None);
        };
        if !config.extraction_enabled {
            return Ok(None);
        }
        let manifest =
            awaken_dispatch_execution::session_envelope::decode_dispatched_resource_manifest(
                dispatch,
            )
            .map_err(DispatchSettlementError)?
            .ok_or_else(|| {
                DispatchSettlementError(
                    "Worker Memory settlement requires its original frozen resource manifest"
                        .into(),
                )
            })?;
        let binding_id = config.binding_id.as_deref().ok_or_else(|| {
            DispatchSettlementError(
                "the frozen Memory plugin requires an explicit `memory.binding_id`".into(),
            )
        })?;
        let binding = self
            .bindings
            .compile_frozen(&dispatch.session_thread_id().0, &manifest, binding_id)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))?;
        if !binding.extraction_enabled {
            return Ok(None);
        }
        let publications = crate::RUN_PUBLICATIONS
            .exact_source(
                &dispatch.activation.snapshot,
                &dispatch.agent_publications,
                &manifest.workspace_id,
            )
            .map_err(DispatchSettlementError)?;
        let extractor = crate::memory_extractor_snapshot(
            &manifest.workspace_id,
            config,
            model,
            publications.as_ref(),
        )
        .map_err(DispatchSettlementError)?;
        let authority = commit.committed_authority().ok_or_else(|| {
            DispatchSettlementError(
                "Worker Memory settlement requires committed Coordinator authority".into(),
            )
        })?;
        let prefix = authority
            .recovery_snapshot(dispatch.thread_id(), dispatch.run_id())
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))?;
        if &prefix.thread_id != dispatch.thread_id() || &prefix.claimed_run_id != dispatch.run_id()
        {
            return Err(DispatchSettlementError(
                "Memory terminal prefix does not match its original Run and Thread".into(),
            ));
        }
        let record = prefix
            .run_record(dispatch.run_id())
            .map_err(|error| DispatchSettlementError(error.to_string()))?;
        if record.is_none_or(|record| &record.state != state) {
            return Err(DispatchSettlementError(
                "Memory terminal prefix does not contain the guarded committed state".into(),
            ));
        }
        let driver: Arc<dyn MemoryExtractionDriver> = Arc::new(DispatchedMemory::new(
            binding.clone(),
            self.reads.clone(),
            self.projections.clone(),
            self.owner.clone(),
            self.completion.clone(),
        ));
        let extraction = Arc::new(DispatchedMemoryTerminal {
            binding,
            extractions: self.extractions.clone(),
            driver,
            extractor,
            execution: MemoryExtractionExecution {
                commit_scope: MemoryExtractionCommitScope::AuxiliaryThread,
                data_subject_id: dispatch.activation.data_subject_id.clone(),
            },
        });
        let reader: Arc<dyn CommittedThreadView> = Arc::new(prefix);
        Ok(Some(Arc::new(MemoryTerminalObserver::new(
            reader, extraction,
        ))))
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for DispatchedMemorySettlement {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        crate::observe_dispatched_memory_terminal(
            &self.reads,
            dispatch,
            claim,
            committed_state,
            |commit| self.observer(dispatch, committed_state, commit),
        )
        .await
    }
}

struct DispatchedMemoryTerminal {
    binding: Arc<MemoryBinding>,
    extractions: Arc<MemoryExtractions>,
    driver: Arc<dyn MemoryExtractionDriver>,
    extractor: MemoryExtractorSnapshot,
    execution: MemoryExtractionExecution,
}

#[async_trait::async_trait]
impl MemoryTerminalExtraction for DispatchedMemoryTerminal {
    async fn extract_terminal(
        &self,
        terminal: &CommittedTerminalRun,
        transcript: TranscriptSnapshot,
    ) -> Result<(), String> {
        self.extractions
            .controller()
            .enqueue_terminal(MemoryTerminalExtractionRequest {
                workspace_id: self.binding.workspace_id.clone(),
                session_id: self.binding.session_thread_id.clone(),
                terminal_run_id: terminal.run_id.0.clone(),
                memory_store_id: self.binding.memory_store_id.clone(),
                memory_config_version: self.binding.memory_config_version,
                committed_transcript: transcript,
                extractor: self.extractor.clone(),
                execution: self.execution.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        // Registration returns before a Worker is available. Waiting for the
        // extractor here would deadlock a single Worker still settling its parent.
        self.extractions
            .recover(&self.binding.session_thread_id, self.driver.clone())
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}
