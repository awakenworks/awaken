// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
//! Out-of-band memory extraction: the process-startup wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! the shared Agent Run substrate, fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a Run, and reads memories back for recall.

use awaken_credential_contract::{
    AttemptCredentialBinding, AttemptCredentialRealization, CredentialRealizationProfile,
    CredentialRealizationReceipt, CredentialRealizationRecordError, CredentialRealizationRecorder,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    EXTRACT_PROMPT, MemoryExtractionController, MemoryExtractionDriver, MemoryExtractionError,
    MemoryExtractionIntent, MemoryExtractionMutation, MemoryExtractionOutcome,
    MemoryExtractionRepository, MemoryExtractorSnapshot, MemoryMutationReceipt, MemoryStoreHandle,
    MemoryTerminalExtractionRequest, WriteMemoryTool, accepts_memory_content, sanitize_stem,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
#[cfg(test)]
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::agent_runner::{AgentRunError, run_configured_agent_with_id};
use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::store::HostCommit;

mod platform;
mod selector;
mod terminal_observer;
pub(crate) use platform::PlatformMemoryHandle;
pub(crate) use selector::AgentSelector;

static EXTRACTION_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

mod captured_writes;
use captured_writes::{CapturedMemoryWrites, writes_from_committed_agent};

/// Host-level extraction/selection capability. It owns the auxiliary-agent
/// machinery but deliberately owns no MemoryStore identity or content handle.
/// A Session must create a [`BoundMemory`] from its resolved resource manifest.
pub struct MemoryRuntime {
    llm: Arc<dyn LlmExecutor>,
    /// The Host's runtime-only inference materialization, when its
    /// composition installed one (`HostDependencies::with_inference_materializer`).
    inference_materializer:
        Option<Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>>,
    /// Fresh sandboxes for extraction/selection Runs, from the process
    /// composition's `HostDependencies::with_auxiliary_sandboxes`.
    provider: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
    background: Arc<BackgroundRuns>,
    claim_owner: String,
    extractions: Arc<dyn MemoryExtractionRepository>,
    /// The Host's process clock extraction claims and receipts are stamped on.
    clock: Arc<dyn awaken_time::WallClock>,
}

/// One Session-scoped MemoryStore binding shared by recall and extraction.
#[derive(Clone)]
pub struct BoundMemory {
    runtime: Arc<MemoryRuntime>,
    /// Physical Session/commit partition that owns extraction execution. A
    /// delegated child's logical Thread remains in its transcript snapshot.
    session_thread_id: String,
    store: Arc<dyn MemoryStoreHandle>,
    platform: Arc<PlatformMemoryHandle>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    workspace_id: String,
    memory_store_id: String,
    memory_config_version: u64,
    recall_enabled: bool,
    extraction_enabled: bool,
    execution: Arc<RwLock<Option<Arc<HostCommit>>>>,
}

/// Adapter from the Memory bounded context's durable claim to the common
/// attempt-credential SPIs. It owns no materialization logic: it only proves
/// the exact extraction claim and stores the common secret-free receipt back in
/// that same aggregate.
struct MemoryExtractionCredentialAuthority {
    repository: Arc<dyn MemoryExtractionRepository>,
    /// The Host's process clock the claim lease is judged on.
    clock: Arc<dyn awaken_time::WallClock>,
    intent_id: String,
    owner: String,
    generation: u64,
    bindings: Vec<AttemptCredentialBinding>,
}

impl MemoryExtractionCredentialAuthority {
    async fn current(&self) -> Result<MemoryExtractionIntent, String> {
        let intent = self
            .repository
            .get_extraction(&self.intent_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Memory extraction credential claim disappeared".to_string())?;
        let now = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if intent.claim_owner.as_deref() != Some(self.owner.as_str())
            || intent.claim_generation != self.generation
            || intent
                .lease_expires_at_unix_ms
                .is_none_or(|expires| expires <= now)
        {
            return Err("Memory extraction credential claim is stale".into());
        }
        Ok(intent)
    }
}

#[async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for MemoryExtractionCredentialAuthority {
    async fn verify_current(&self) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        self.current()
            .await
            .map(|_| ())
            .map_err(|_| awaken_runtime_contract::AttemptOwnershipError::Lost)
    }
}

#[async_trait]
impl CredentialRealizationRecorder for MemoryExtractionCredentialAuthority {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), CredentialRealizationRecordError> {
        awaken_credential_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
        // The extraction intent is SDK state; it carries the receipt as an
        // opaque token fenced by claim epoch and candidate (ADR-0086 §7).
        let receipt = serde_json::to_value(&receipt)
            .and_then(awaken_ext_memory::CredentialRealizationToken::new)
            .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
        for _ in 0..4 {
            let mut intent = self
                .current()
                .await
                .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
            let expected_revision = intent.revision;
            intent
                .record_credential_realization(
                    &self.owner,
                    self.generation,
                    awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                    receipt.clone(),
                )
                .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
            if intent.revision == expected_revision {
                // The aggregate already contains this exact receipt. Its
                // idempotent command deliberately does not bump revision, so
                // issuing a CAS that requires expected+1 would manufacture a
                // conflict on the second model step.
                return Ok(());
            }
            match self
                .repository
                .compare_and_swap_extraction(expected_revision, intent)
                .await
            {
                Ok(()) => return Ok(()),
                Err(MemoryExtractionError::RevisionConflict(_)) => continue,
                Err(error) => {
                    return Err(CredentialRealizationRecordError(error.to_string()));
                }
            }
        }
        Err(CredentialRealizationRecordError(
            "Memory extraction credential receipt CAS remained contended".into(),
        ))
    }
}

impl MemoryRuntime {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
        background: Arc<BackgroundRuns>,
        extractions: Arc<dyn MemoryExtractionRepository>,
        clock: Arc<dyn awaken_time::WallClock>,
        inference_materializer: Option<
            Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>,
        >,
    ) -> Self {
        Self {
            clock,
            llm,
            inference_materializer,
            provider,
            background,
            claim_owner: format!(
                "memory-extractor:{}:{}",
                std::process::id(),
                EXTRACTION_OWNER_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            extractions,
        }
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        session_thread_id: impl Into<String>,
        workspace_id: impl Into<String>,
        platform: Arc<PlatformMemoryHandle>,
        resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
        config: &awaken_resource_contract::MemoryStoreConfigVersion,
        writable: bool,
    ) -> BoundMemory {
        BoundMemory {
            runtime: self.clone(),
            session_thread_id: session_thread_id.into(),
            store: platform.clone(),
            platform,
            resource_validator,
            workspace_id: workspace_id.into(),
            memory_store_id: config.memory_store_id.to_string(),
            memory_config_version: config.version.0,
            // The binding grants data-plane availability only. Whether the main
            // Agent activates recall/extraction and with which bounds is owned by
            // its `memory` plugin configuration, never by a Store policy copy.
            recall_enabled: true,
            extraction_enabled: writable,
            execution: Arc::new(RwLock::new(None)),
        }
    }

    /// Await every in-flight extraction started by any bound Session.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }

    pub(crate) fn background(&self) -> Arc<BackgroundRuns> {
        self.background.clone()
    }

    fn auxiliary_sandbox_provider(
        &self,
    ) -> Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider> {
        self.provider.clone()
    }

    fn materialize_extractor(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<(Arc<dyn LlmExecutor>, RuntimeRunContext), String> {
        let snapshot = &intent.extractor;
        if let Some(materializer) = self.inference_materializer.as_ref() {
            let holder = CredentialRealizationProfile::self_hosted_native().inference_holder;
            let model = &snapshot.agent.resolved_spec.model_binding;
            let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
                &[model],
                Some(&holder),
                &materializer.credential_realization_capabilities(),
                intent.claim_generation,
                awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
            )
            .map_err(|error| error.to_string())?;
            let owner = intent
                .claim_owner
                .clone()
                .ok_or_else(|| "Memory extractor has no durable claim owner".to_string())?;
            let authority = Arc::new(MemoryExtractionCredentialAuthority {
                repository: self.extraction_repository(),
                clock: self.clock.clone(),
                intent_id: intent.intent_id.clone(),
                owner,
                generation: intent.claim_generation,
                bindings: bindings.clone(),
            });
            let context = RuntimeRunContext::new()
                .with_ownership(authority.clone())
                .with_credential_realization(AttemptCredentialRealization::new(
                    bindings, authority,
                ));
            return materializer
                .materialize_pinned(model, &context)
                .map(|executor| (executor, context))
                .ok_or_else(|| {
                    format!(
                        "published model candidate `{}` is unavailable",
                        model.binding().identity().1
                    )
                });
        }
        matches!(
            snapshot.agent.resolved_spec.model_binding.provisioning(),
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        )
        .then(|| (self.llm.clone(), RuntimeRunContext::new()))
        .ok_or_else(|| {
            "published model candidate requires an installed credential materializer".into()
        })
    }

    pub(crate) fn extraction_repository(&self) -> Arc<dyn MemoryExtractionRepository> {
        self.extractions.clone()
    }

    fn extraction_controller(&self) -> MemoryExtractionController {
        MemoryExtractionController::new(
            self.extraction_repository(),
            self.claim_owner.clone(),
            self.clock.clone(),
        )
    }
}

impl BoundMemory {
    #[cfg(test)]
    pub(crate) fn memory_store_id(&self) -> &str {
        &self.memory_store_id
    }

    pub(crate) fn recall_enabled(&self) -> bool {
        self.recall_enabled
    }

    pub(crate) fn extraction_enabled(&self) -> bool {
        self.extraction_enabled
    }

    fn bind_execution(&self, commit: Arc<HostCommit>) {
        *self
            .execution
            .write()
            .expect("Memory execution context lock poisoned") = Some(commit);
    }

    async fn bind_recovery(&self, commit: Arc<HostCommit>) -> Result<bool, MemoryExtractionError> {
        self.bind_execution(commit);
        self.reconcile().await
    }

    fn matches_intent(&self, intent: &MemoryExtractionIntent) -> bool {
        intent.session_id == self.session_thread_id
            && intent.workspace_id == self.workspace_id
            && intent.memory_store_id == self.memory_store_id
            && intent.memory_config_version == self.memory_config_version
    }

    async fn validate_live_resource(&self) -> Result<(), String> {
        let Some(validator) = self.resource_validator.as_ref() else {
            return Ok(());
        };
        validator
            .verify_memory_binding(
                &self.workspace_id,
                &self.memory_store_id,
                awaken_resource_contract::ConfigVersion(self.memory_config_version),
            )
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn list_bound_memories(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
        self.validate_live_resource().await?;
        self.platform.list(prefix).await
    }

    pub(crate) async fn read_bound_memory(
        &self,
        path: &str,
    ) -> Result<Option<awaken_resource_contract::Memory>, String> {
        self.validate_live_resource().await?;
        self.platform.read(path).await
    }

    pub(crate) async fn write_bound_memory(
        &self,
        path: &str,
        content: &str,
        expected_generation: Option<&awaken_resource_contract::MemoryGeneration>,
    ) -> Result<awaken_resource_contract::Memory, String> {
        self.validate_live_resource().await?;
        self.platform
            .write_exact(path, content, expected_generation)
            .await
    }

    pub(crate) async fn delete_bound_memory(
        &self,
        path: &str,
        expected_generation: &awaken_resource_contract::MemoryGeneration,
    ) -> Result<bool, String> {
        self.validate_live_resource().await?;
        self.platform.delete_exact(path, expected_generation).await
    }

    /// Durably enqueue one extraction keyed by the terminal commit, then drive it
    /// asynchronously. Returning from this method means the intent is persistent,
    /// not that extraction has completed.
    pub async fn trigger(
        &self,
        terminal_commit_id: &str,
        committed: TranscriptSnapshot,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<(), MemoryExtractionError> {
        self.runtime
            .extraction_controller()
            .enqueue_terminal(MemoryTerminalExtractionRequest {
                workspace_id: self.workspace_id.clone(),
                session_id: self.session_thread_id.clone(),
                terminal_run_id: terminal_commit_id.to_string(),
                memory_store_id: self.memory_store_id.clone(),
                memory_config_version: self.memory_config_version,
                committed_transcript: committed,
                extractor,
            })
            .await?;
        self.reconcile().await?;
        Ok(())
    }

    /// RunResume every non-terminal intent for this exact physical Session and
    /// frozen resource binding. Invoked after enqueue and after parent Session
    /// rehydration, so a process crash cannot lose a delegated child's remaining
    /// extraction/store/receipt work.
    pub async fn reconcile(&self) -> Result<bool, MemoryExtractionError> {
        if self
            .execution
            .read()
            .expect("Memory execution context lock poisoned")
            .is_none()
        {
            return Ok(false);
        }
        let controller = self.runtime.extraction_controller();
        if !controller.has_recoverable(self).await? {
            return Ok(false);
        }
        let bound = self.clone();
        self.runtime
            .background
            .spawn(
                BackgroundWorkClass::ExternalDurable {
                    durable_intent_id: format!("memory-extraction:{}", self.session_thread_id),
                },
                async move {
                    bound.drive_recoverable().await;
                },
            )
            .await;
        Ok(true)
    }

    async fn drive_recoverable(&self) {
        self.runtime
            .extraction_controller()
            .drive_recoverable(self)
            .await;
    }

    async fn extract_mutations(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        let capture = Arc::new(CapturedMemoryWrites::default());
        let mut seed = intent.transcript.clone();
        seed.push(Message {
            id: MessageId(format!("{}-mem-prompt", intent.terminal_commit_id)),
            role: Role::User,
            content: vec![ContentBlock::text(
                intent
                    .extractor
                    .extraction_prompt
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(EXTRACT_PROMPT),
            )],
        });
        let catalog = AgentCatalog::new().with_agent(intent.extractor.agent.clone());
        let (executor, context) = match self.runtime.materialize_extractor(intent) {
            Ok(materialized) => materialized,
            Err(reason) => return MemoryExtractionOutcome::Retryable { reason },
        };
        let tool = erase(WriteMemoryTool::from_handle(capture.clone()));
        let commit = match self
            .execution
            .read()
            .expect("Memory execution context lock poisoned")
            .clone()
        {
            Some(commit) => commit,
            None => {
                return MemoryExtractionOutcome::Retryable {
                    reason: "Memory extraction is waiting for the Session commit/history binding"
                        .to_string(),
                };
            }
        };
        let context = context
            .with_commit(commit.clone())
            .with_reader(commit.clone());
        let auxiliary_thread_id = intent.auxiliary_thread_id();
        let auxiliary_sandboxes = self.runtime.auxiliary_sandbox_provider();
        if let Err(error) = run_configured_agent_with_id(
            &catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(auxiliary_sandboxes.as_ref()),
            executor,
            &intent.extractor.agent.root_agent_id.0,
            &auxiliary_thread_id,
            RunId(intent.auxiliary_run_id()),
            seed,
            vec![tool],
            context,
        )
        .await
        {
            return classify_extraction_run_error(
                intent,
                commit.run_state(&RunId(intent.auxiliary_run_id())),
                error,
            );
        }
        let mut writes = capture.take();
        if writes.is_empty() {
            writes = writes_from_committed_agent(
                &commit.committed_messages(&ThreadId(auxiliary_thread_id)),
            );
        }
        match self.platform.plan_mutations(writes).await {
            Ok(mutations) => MemoryExtractionOutcome::Mutations(mutations),
            Err(reason) => MemoryExtractionOutcome::Retryable { reason },
        }
    }

    /// A live-validating Memory handle shared with the recall plugin. Returning
    /// the underlying data-plane handle would let an already-bound Session bypass
    /// a later suspend/archive transition.
    pub fn store(&self) -> Arc<dyn MemoryStoreHandle> {
        Arc::new(self.clone())
    }
}

#[async_trait]
impl MemoryExtractionDriver for BoundMemory {
    fn accepts(&self, intent: &MemoryExtractionIntent) -> bool {
        self.matches_intent(intent)
    }

    async fn validate_binding(&self, _intent: &MemoryExtractionIntent) -> Result<(), String> {
        self.validate_live_resource()
            .await
            .map_err(|_| "MemoryStore is missing, suspended, archived, or deleted".to_string())
    }

    async fn extract(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        self.extract_mutations(intent).await
    }

    async fn apply(
        &self,
        _intent: &MemoryExtractionIntent,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        self.platform.apply_mutation(mutation).await
    }
}

fn classify_extraction_run_error(
    intent: &MemoryExtractionIntent,
    observed_state: Option<RunState>,
    error: AgentRunError,
) -> MemoryExtractionOutcome {
    let reason = error.to_string();
    match error {
        AgentRunError::Configuration(_) => MemoryExtractionOutcome::PermanentFailure { reason },
        AgentRunError::Provisioning(_) => MemoryExtractionOutcome::Retryable { reason },
        AgentRunError::Runtime(_) => match observed_state {
            Some(RunState::Ended(EndCause::Indeterminate)) | None | Some(RunState::Running) => {
                MemoryExtractionOutcome::ExecutionIndeterminate {
                    auxiliary_run_id: intent.auxiliary_run_id(),
                    reason,
                }
            }
            Some(RunState::Ended(_)) | Some(RunState::Awaiting) => {
                MemoryExtractionOutcome::PermanentFailure { reason }
            }
        },
    }
}

#[async_trait]
impl MemoryStoreHandle for BoundMemory {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        self.validate_live_resource().await?;
        self.store.write(name, content).await
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        self.validate_live_resource().await?;
        self.store.entries().await
    }
}

impl crate::host::SharedHost {
    /// The Memory content data-plane port used by an outer process startup to
    /// construct a worker-side mounter. It carries no principal or policy state.
    pub fn memory_repository(&self) -> Arc<dyn awaken_resource_contract::MemoryRepository> {
        self.resources.memory_stores.fs_handle()
    }

    /// The Workspace-scoped Skill aggregate port shared by HTTP authoring and
    /// runtime activation. `None` means this host has no durable Skill plane.
    #[cfg(any(test, feature = "test-support"))]
    pub fn skill_store(&self) -> Option<Arc<dyn awaken_resource_contract::SkillStore>> {
        self.tools.skills.store_handle()
    }

    pub(crate) fn memory_mounter(
        &self,
    ) -> Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>> {
        self.env.memory_mounter.clone()
    }

    pub(crate) fn register_thread_memory(&self, thread: &str, memory: Option<Arc<BoundMemory>>) {
        self.session_slots.update(thread, |slot| {
            slot.memory_bindings.clear();
            slot.memory = memory;
        });
    }

    /// Replace the complete set of standard mounted MemoryStore bindings. No
    /// automatic recall/extraction binding is inferred from collection order.
    #[cfg(test)]
    pub(crate) fn register_thread_memory_bindings(
        &self,
        thread: &str,
        bindings: std::collections::HashMap<String, Arc<BoundMemory>>,
    ) {
        self.session_slots.update(thread, |slot| {
            slot.memory_bindings = bindings;
            slot.memory = None;
        });
    }

    /// Select the exact Awaken extension binding authored by the published
    /// Agent. A missing binding clears any prior selection before failing, so a
    /// rebuilt context cannot retain stale automatic-memory authority.
    pub(crate) fn select_thread_memory_binding(
        &self,
        thread: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<Arc<BoundMemory>>, String> {
        let selected = self.session_slots.update(thread, |slot| {
            let selected = binding_id.and_then(|id| slot.memory_bindings.get(id).cloned());
            slot.memory = selected.clone();
            selected
        });
        match (binding_id, selected) {
            (_, Some(selected)) => Ok(Some(selected)),
            (None, None) => Ok(None),
            (Some(binding_id), None) => Err(format!(
                "automatic memory binding `{binding_id}` is not mounted for this Session"
            )),
        }
    }

    pub(crate) fn memory_for_thread(&self, thread: &str) -> Option<Arc<BoundMemory>> {
        self.session_slots
            .read(thread, |slot| slot.memory.clone())
            .flatten()
    }

    /// Attach the one physical Session commit authority to every writable
    /// MemoryStore binding restored from its frozen manifest, then resume any
    /// matching durable intent. Recovery is resource-binding-owned rather than
    /// dependent on whether the parent Agent selects the Memory plugin: a child
    /// may have selected extraction even when its parent did not.
    pub(crate) async fn bind_thread_memory_recovery(
        &self,
        session_thread: &str,
        commit: Arc<HostCommit>,
    ) -> Result<usize, crate::HostError> {
        // A database-less execution Worker owns only the frozen Memory data-plane
        // binding. The Coordinator's guarded settlement observer is the sole
        // owner of remote terminal observation and its durable extraction outbox;
        // probing the Worker's fail-closed repository here would duplicate that
        // authority and prevent an otherwise valid claimed Runtime from starting.
        if !self.authority.observes_terminal_memory_locally() {
            return Ok(0);
        }
        let bindings = self
            .session_slots
            .read(session_thread, |slot| {
                let mut bindings: Vec<Arc<BoundMemory>> = Vec::new();
                for memory in slot
                    .memory_bindings
                    .values()
                    .cloned()
                    .chain(slot.memory.iter().cloned())
                {
                    if !bindings
                        .iter()
                        .any(|existing| Arc::ptr_eq(existing, &memory))
                    {
                        bindings.push(memory);
                    }
                }
                bindings
            })
            .unwrap_or_default();
        let mut scheduled = 0;
        for memory in bindings {
            if memory.extraction_enabled()
                && memory
                    .bind_recovery(commit.clone())
                    .await
                    .map_err(|error| {
                        crate::HostError::internal(format!(
                            "recover Memory extraction for physical Session `{session_thread}`: {error}"
                        ))
                    })?
            {
                scheduled += 1;
            }
        }
        Ok(scheduled)
    }

    pub(crate) fn platform_memory_handle(
        &self,
        store_id: String,
        writable: bool,
    ) -> Arc<PlatformMemoryHandle> {
        Arc::new(PlatformMemoryHandle::new(
            self.resources.memory_stores.fs_handle(),
            store_id,
            writable,
        ))
    }

    /// Install one already-resolved MemoryStore binding for a thread. This is an
    /// ACL/startup seam for embedders: ownership and authorization must have
    /// completed before calling it; the Runtime Host receives only the opaque store
    /// id, pinned config, and maximum access.
    pub fn bind_resolved_memory(
        &self,
        thread: &str,
        workspace_id: &str,
        config: &awaken_resource_contract::MemoryStoreConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        resource_validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) {
        let writable = access == awaken_resource_contract::ResourceAccess::ReadWrite;
        let handle = self.platform_memory_handle(config.memory_store_id.to_string(), writable);
        let bound = self.resources.memory.bind(
            thread,
            workspace_id,
            handle,
            Some(resource_validator),
            config,
            writable,
        );
        self.register_thread_memory(thread, Some(Arc::new(bound)));
    }
}

#[cfg(test)]
mod tests;
