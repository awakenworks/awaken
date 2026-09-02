//! Host projection from a frozen Memory publication to the shared terminal observer.
//!
//! The Memory bounded context retains the observer and durable extraction state
//! machine. This module only binds the published Agent snapshot, Session-scoped
//! Memory handle, and committed Thread reader onto those existing authorities.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_ext_memory::{
    DEFAULT_MEMORY_INSTRUCTIONS, MEMORY_AGENT_ID, MemoryExtractorSnapshot,
    MemoryTerminalExtraction, MemoryTerminalObserver, default_memory_agent,
};

use super::BoundMemory;
use crate::store::HostCommit;

struct BoundMemoryTerminalExtraction {
    memory: Arc<BoundMemory>,
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

impl crate::host::SharedHost {
    fn published_memory_terminal_configuration(
        snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
        effective_model_ref: &str,
    ) -> Result<
        Option<(
            awaken_ext_memory::MemoryConfig,
            awaken_runtime_contract::resolved::ResolvedModelCandidate,
        )>,
        crate::HostError,
    > {
        if !snapshot
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_memory::MEMORY_PLUGIN_ID)
        {
            return Ok(None);
        }
        let config = awaken_ext_memory::MemoryConfig::from_value(
            snapshot
                .resolved_spec
                .plugin_config
                .get(awaken_ext_memory::MEMORY_PLUGIN_ID),
        )
        .map_err(|error| {
            crate::HostError::internal(format!(
                "invalid frozen Memory plugin configuration: {error}"
            ))
        })?;
        let model = snapshot
            .resolved_spec
            .candidate_for_model(effective_model_ref)
            .cloned()
            .ok_or_else(|| {
                crate::HostError::internal(format!(
                    "memory extraction model `{effective_model_ref}` is absent from frozen snapshot `{}`",
                    snapshot.id.0
                ))
            })?;
        Ok(Some((config, model)))
    }

    pub(crate) async fn memory_terminal_observer(
        &self,
        session_thread: &str,
        snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
        effective_model_ref: &str,
        dispatched_resources: Option<&awaken_session_contract::SessionResourceManifest>,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let Some((config, model)) =
            Self::published_memory_terminal_configuration(snapshot, effective_model_ref)?
        else {
            return Ok(None);
        };
        if !config.extraction_enabled {
            return Ok(None);
        }
        let memory = if let Some(manifest) = dispatched_resources {
            let binding_id = config.binding_id.as_deref().ok_or_else(|| {
                crate::HostError::internal(
                    "the frozen Memory plugin requires an explicit `memory.binding_id`",
                )
            })?;
            let memory = self
                .compile_dispatched_memory_binding(session_thread, manifest, binding_id)
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
            if !memory.extraction_enabled() {
                return Ok(None);
            }
            memory
                .bind_recovery(commit.clone())
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
            memory
        } else {
            let Some(memory) = self.memory_for_thread(session_thread) else {
                return Ok(None);
            };
            // Resident Session construction has already attached `commit` to
            // every writable frozen binding through
            // `bind_thread_memory_recovery`, including this selected one.
            memory
        };
        if !memory.extraction_enabled() {
            return Ok(None);
        }
        self.configured_memory_terminal_observer(config, model, memory, frozen_publications, commit)
            .await
    }

    /// Build the direct compatibility observer without manufacturing a Session
    /// snapshot or a durable Environment handle. A published Agent remains the
    /// exact configuration authority; an unpinned direct Session uses the same
    /// host Memory config and host-executor model binding that `server_config`
    /// projects later during ordinary context construction.
    pub(crate) async fn direct_memory_terminal_observer(
        &self,
        session_thread: &str,
        published: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        effective_model_ref: &str,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let Some(memory) = self.memory_for_thread(session_thread) else {
            return Ok(None);
        };
        let (config, model) = match published {
            Some(snapshot) => {
                let Some(configuration) =
                    Self::published_memory_terminal_configuration(snapshot, effective_model_ref)?
                else {
                    return Ok(None);
                };
                configuration
            }
            None => {
                let config = awaken_ext_memory::MemoryConfig::from_value(
                    self.plugin_config.get(awaken_ext_memory::MEMORY_PLUGIN_ID),
                )
                .map_err(|error| {
                    crate::HostError::internal(format!(
                        "invalid direct Memory plugin configuration: {error}"
                    ))
                })?;
                let model = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        "default",
                        effective_model_ref,
                        "default",
                    ),
                );
                (config, model)
            }
        };
        if !config.extraction_enabled || !memory.extraction_enabled() {
            return Ok(None);
        }
        memory
            .bind_recovery(commit.clone())
            .await
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        self.configured_memory_terminal_observer(config, model, memory, frozen_publications, commit)
            .await
    }

    async fn configured_memory_terminal_observer(
        &self,
        config: awaken_ext_memory::MemoryConfig,
        model: awaken_runtime_contract::resolved::ResolvedModelCandidate,
        memory: Arc<BoundMemory>,
        frozen_publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
        commit: Arc<HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let agent_id = config.agent_id.as_deref().unwrap_or(MEMORY_AGENT_ID);
        let agent = crate::agent_catalog::resolve_auxiliary_snapshot(
            Some(frozen_publications),
            &memory.workspace_id,
            agent_id,
            default_memory_agent(model, DEFAULT_MEMORY_INSTRUCTIONS),
            config.instructions.as_deref(),
        )
        .map_err(crate::HostError::internal)?;
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
