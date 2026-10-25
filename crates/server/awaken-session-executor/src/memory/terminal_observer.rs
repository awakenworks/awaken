// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: the extracted Host adapter binds a frozen Agent publication,
// @formal reason: Session-scoped Memory handle, and committed Thread reader to the existing
// @formal reason: MemoryTerminalObserver and extraction aggregate. Dynamic path: disabled or absent
// @formal reason: plugins produce no observer; missing required bindings refuse, while an exact binding delegates terminal extraction to
// @formal reason: the one durable Memory controller, so this requirement-only classification adds no
// @formal reason: observer state machine or consistency authority and makes no direct formal-proof claim.
//! Host projection from a frozen Memory publication to the shared terminal observer.
//!
//! The Memory bounded context retains the observer and durable extraction state
//! machine. This module only binds the published Agent snapshot, Session-scoped
//! Memory handle, and committed Thread reader onto those existing authorities.

use awaken_runtime_contract::terminal::RunTerminalObserver;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_ext_memory::{
    DEFAULT_MEMORY_INSTRUCTIONS, MEMORY_AGENT_ID, MemoryExtractorSnapshot,
    MemoryTerminalExtraction, MemoryTerminalObserver, default_memory_agent,
};

use super::BoundMemory;
use awaken_session_run_delivery::SessionCommit;

struct BoundMemoryTerminalExtraction {
    memory: BoundMemory,
    extractor: MemoryExtractorSnapshot,
}

#[async_trait]
impl MemoryTerminalExtraction for BoundMemoryTerminalExtraction {
    async fn extract_terminal(
        &self,
        terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
        transcript: TranscriptSnapshot,
    ) -> Result<(), String> {
        self.memory
            .trigger(&terminal.run_id.0, transcript, self.extractor.clone())
            .await
            .map_err(|error| error.to_string())
    }
}

/// Selected terminal Memory roles, with only a weak view of resident Sessions.
pub(crate) struct MemoryTerminalObservers {
    sessions: Weak<crate::installed_session::InstalledSessions>,
    bindings: awaken_session_realization::memory_binding_compiler::MemoryBindingCompiler,
    runtime: Arc<super::MemoryRuntime>,
    local_workspace: String,
    direct_config: Option<serde_json::Value>,
}
impl MemoryTerminalObservers {
    pub(crate) fn new(
        sessions: Weak<crate::installed_session::InstalledSessions>,
        bindings: awaken_session_realization::memory_binding_compiler::MemoryBindingCompiler,
        runtime: Arc<super::MemoryRuntime>,
        local_workspace: String,
        direct_config: Option<serde_json::Value>,
    ) -> Self {
        Self {
            sessions,
            bindings,
            runtime,
            local_workspace,
            direct_config,
        }
    }
    pub(crate) fn for_thread(
        &self,
        thread: &str,
    ) -> Result<Option<BoundMemory>, crate::realization_error::RealizationError> {
        let Some(sessions) = self.sessions.upgrade() else {
            return Ok(None);
        };
        Ok(sessions
            .realizations()
            .read_realization(thread, |r| r.staging().memory.clone())?
            .flatten()
            .map(|binding| self.runtime.bound(binding)))
    }
    pub(crate) async fn for_dispatch(
        &self,
        dispatch: &awaken_dispatch_contract::RunDispatch,
        commit: Arc<awaken_session_run_delivery::SessionCommit>,
    ) -> Result<Option<Arc<dyn RunTerminalObserver>>, crate::SessionExecutionError> {
        let manifest =
            awaken_dispatch_execution::session_envelope::decode_dispatched_resource_manifest(
                dispatch,
            )
            .map_err(crate::SessionExecutionError::internal)?;
        let workspace = dispatch
            .execution_scope
            .as_ref()
            .map_or_else(|| self.local_workspace.as_str(), |scope| scope.0.0.as_str());
        let frozen_publications = awaken_session_run_delivery::RUN_PUBLICATIONS
            .exact_source(
                &dispatch.activation.snapshot,
                &dispatch.agent_publications,
                workspace,
            )
            .map_err(crate::SessionExecutionError::internal)?;
        self.for_published_run(
            &dispatch.session_thread_id().0,
            &dispatch.activation.snapshot,
            dispatch.activation.effective_model_ref(),
            manifest.as_ref(),
            frozen_publications.as_ref(),
            commit,
        )
        .await
    }

    pub(crate) async fn for_published_run(
        &self,
        session_thread: &str,
        snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
        effective_model_ref: &str,
        dispatched_resources: Option<&crate::session_resource::SessionResourceManifest>,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<SessionCommit>,
    ) -> Result<Option<Arc<dyn RunTerminalObserver>>, crate::SessionExecutionError> {
        let Some((config, model)) =
            awaken_ext_memory::published_extraction_configuration(snapshot, effective_model_ref)
                .map_err(crate::SessionExecutionError::internal)?
        else {
            return Ok(None);
        };
        if !config.extraction_enabled {
            return Ok(None);
        }
        let memory = if let Some(manifest) = dispatched_resources {
            let binding_id = config.binding_id.as_deref().ok_or_else(|| {
                crate::SessionExecutionError::internal(
                    "the frozen Memory plugin requires an explicit `memory.binding_id`",
                )
            })?;
            let binding = self
                .bindings
                .compile_frozen(session_thread, manifest, binding_id)
                .await
                .map_err(|error| crate::SessionExecutionError::internal(error.to_string()))?;
            let memory = self.runtime.bound(binding);
            if !memory.binding().extraction_enabled {
                return Ok(None);
            }
            memory
                .bind_recovery(commit.clone())
                .await
                .map_err(|error| crate::SessionExecutionError::internal(error.to_string()))?;
            memory
        } else {
            let memory = self.for_thread(session_thread)?.ok_or_else(|| crate::SessionExecutionError::internal(
                "Memory settlement without a frozen manifest requires its original resident Session binding",
            ))?;
            // Resident Session construction has already attached `commit` to
            // every writable frozen binding through
            // `bind_thread_memory_recovery`, including this selected one.
            memory
        };
        if !memory.binding().extraction_enabled {
            return Ok(None);
        }
        Self::configured_memory_terminal_observer(
            config,
            model,
            memory,
            frozen_publications,
            commit,
        )
        .await
    }

    /// Build the direct compatibility observer without manufacturing a Session
    /// snapshot or a durable Environment handle. A published Agent remains the
    /// exact configuration authority; an unpinned direct Session uses the same
    /// host Memory config and host-executor model binding that `server_config`
    /// projects later during ordinary context construction.
    pub(crate) async fn for_direct_run(
        &self,
        session_thread: &str,
        published: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        effective_model_ref: &str,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<SessionCommit>,
    ) -> Result<Option<Arc<dyn RunTerminalObserver>>, crate::SessionExecutionError> {
        let Some(memory) = self.for_thread(session_thread)? else {
            return Ok(None);
        };
        let (config, model) = match published {
            Some(snapshot) => {
                let Some(configuration) = awaken_ext_memory::published_extraction_configuration(
                    snapshot,
                    effective_model_ref,
                )
                .map_err(crate::SessionExecutionError::internal)?
                else {
                    return Ok(None);
                };
                configuration
            }
            None => awaken_ext_memory::direct_extraction_configuration(
                self.direct_config.as_ref(),
                effective_model_ref,
            )
            .map_err(crate::SessionExecutionError::internal)?,
        };
        if !config.extraction_enabled || !memory.binding().extraction_enabled {
            return Ok(None);
        }
        memory
            .bind_recovery(commit.clone())
            .await
            .map_err(|error| crate::SessionExecutionError::internal(error.to_string()))?;
        Self::configured_memory_terminal_observer(
            config,
            model,
            memory,
            frozen_publications,
            commit,
        )
        .await
    }

    async fn configured_memory_terminal_observer(
        config: awaken_ext_memory::MemoryConfig,
        model: awaken_runtime_contract::resolved::ResolvedModelCandidate,
        memory: BoundMemory,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<SessionCommit>,
    ) -> Result<Option<Arc<dyn RunTerminalObserver>>, crate::SessionExecutionError> {
        let agent_id = config.agent_id.as_deref().unwrap_or(MEMORY_AGENT_ID);
        let agent = awaken_session_run_delivery::resolve_auxiliary_snapshot(
            Some(frozen_publications),
            &memory.binding().workspace_id,
            agent_id,
            default_memory_agent(model, DEFAULT_MEMORY_INSTRUCTIONS),
            config.instructions.as_deref(),
        )
        .map_err(crate::SessionExecutionError::internal)?;
        let extraction = Arc::new(BoundMemoryTerminalExtraction {
            memory,
            extractor: MemoryExtractorSnapshot {
                agent,
                extraction_prompt: config.extraction_prompt,
            },
        });
        let reader: Arc<dyn CommittedThreadView> = commit;
        Ok(Some(Arc::new(MemoryTerminalObserver::new(
            reader, extraction,
        ))))
    }
}
