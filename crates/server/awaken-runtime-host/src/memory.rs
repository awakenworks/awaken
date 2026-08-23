//! Out-of-band memory extraction: the process-startup wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! the shared Agent Run substrate, fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a Run, and reads memories back for recall.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    EXTRACT_PROMPT, MEMORY_AGENT_ID, MemoryExtractionController, MemoryExtractionDriver,
    MemoryExtractionError, MemoryExtractionIntent, MemoryExtractionMutation,
    MemoryExtractionRepository, MemoryExtractorSnapshot, MemoryMutationReceipt, MemoryStoreHandle,
    MemoryTerminalExtraction, MemoryTerminalExtractionRequest, MemoryTerminalObserver,
    WriteMemoryTool, accepts_memory_content, sanitize_stem,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::agent_runner::run_configured_agent_with_id;
use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::store::HostCommit;

mod platform;
mod selector;
pub(crate) use platform::PlatformMemoryHandle;
pub(crate) use selector::AgentSelector;

// The config pieces the host wires (registering the default extractor agent).
use awaken_ext_memory::{DEFAULT_MEMORY_INSTRUCTIONS, default_memory_agent};

static EXTRACTION_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct CapturedMemoryWrites {
    writes: Mutex<BTreeMap<String, String>>,
}

impl CapturedMemoryWrites {
    fn take(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut *self.writes.lock().expect("captured Memory writes"))
    }
}

fn writes_from_committed_agent(messages: &[Message]) -> BTreeMap<String, String> {
    let mut writes = BTreeMap::new();
    for block in messages.iter().flat_map(|message| &message.content) {
        let ContentBlock::ToolUse { name, input, .. } = block else {
            continue;
        };
        if name != "write_memory" {
            continue;
        }
        let Some(name) = input.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(content) = input.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if accepts_memory_content(content) {
            writes.insert(format!("/{}.md", sanitize_stem(name)), content.to_string());
        }
    }
    writes
}

#[async_trait]
impl MemoryStoreHandle for CapturedMemoryWrites {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        let path = format!("/{}.md", sanitize_stem(name));
        self.writes
            .lock()
            .map_err(|error| error.to_string())?
            .insert(path.clone(), content.to_string());
        Ok(path)
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        Ok(Vec::new())
    }
}

/// Host-level extraction/selection capability. It owns the auxiliary-agent
/// machinery but deliberately owns no MemoryStore identity or content handle.
/// A Session must create a [`BoundMemory`] from its resolved resource manifest.
pub struct MemoryRuntime {
    llm: Arc<dyn LlmExecutor>,
    inference_materializer:
        RwLock<Option<Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>>>,
    provider: Arc<LocalProvider>,
    background: Arc<BackgroundRuns>,
    claim_owner: String,
    extractions: RwLock<Arc<dyn MemoryExtractionRepository>>,
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

struct BoundMemoryTerminalExtraction {
    memory: Arc<BoundMemory>,
    extractor: MemoryExtractorSnapshot,
}

/// Adapter from the Memory bounded context's durable claim to the common
/// attempt-credential SPIs. It owns no materialization logic: it only proves
/// the exact extraction claim and stores the common secret-free receipt back in
/// that same aggregate.
struct MemoryExtractionCredentialAuthority {
    repository: Arc<dyn MemoryExtractionRepository>,
    intent_id: String,
    owner: String,
    generation: u64,
    bindings: Vec<awaken_runtime_contract::AttemptCredentialBinding>,
}

impl MemoryExtractionCredentialAuthority {
    async fn current(&self) -> Result<MemoryExtractionIntent, String> {
        let intent = self
            .repository
            .get_extraction(&self.intent_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Memory extraction credential claim disappeared".to_string())?;
        let now = memory_unix_ms();
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
impl awaken_runtime_contract::CredentialRealizationRecorder
    for MemoryExtractionCredentialAuthority
{
    async fn record(
        &self,
        receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
        awaken_runtime_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| {
                awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
            })?;
        for _ in 0..4 {
            let mut intent = self.current().await.map_err(|error| {
                awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
            })?;
            let expected_revision = intent.revision;
            intent
                .record_credential_realization(
                    &self.owner,
                    self.generation,
                    memory_unix_ms(),
                    receipt.clone(),
                )
                .map_err(|error| {
                    awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
                })?;
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
                    return Err(awaken_runtime_contract::CredentialRealizationRecordError(
                        error.to_string(),
                    ));
                }
            }
        }
        Err(awaken_runtime_contract::CredentialRealizationRecordError(
            "Memory extraction credential receipt CAS remained contended".into(),
        ))
    }
}

fn memory_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
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

impl MemoryRuntime {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<LocalProvider>,
        background: Arc<BackgroundRuns>,
        extractions: Arc<dyn MemoryExtractionRepository>,
    ) -> Self {
        Self {
            llm,
            inference_materializer: RwLock::new(None),
            provider,
            background,
            claim_owner: format!(
                "memory-extractor:{}:{}",
                std::process::id(),
                EXTRACTION_OWNER_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            extractions: RwLock::new(extractions),
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_extraction_repository(&self, repository: Arc<dyn MemoryExtractionRepository>) {
        *self
            .extractions
            .write()
            .expect("Memory extraction repository lock poisoned") = repository;
    }

    pub(crate) fn set_inference_materializer(
        &self,
        materializer: Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>,
    ) {
        *self
            .inference_materializer
            .write()
            .expect("Memory inference materializer lock poisoned") = Some(materializer);
    }

    fn materialize_extractor(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<(Arc<dyn LlmExecutor>, RuntimeRunContext), String> {
        let snapshot = &intent.extractor;
        if let Some(materializer) = self
            .inference_materializer
            .read()
            .expect("Memory inference materializer lock poisoned")
            .as_ref()
        {
            let holder =
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                    .inference_holder;
            let model = &snapshot.agent.resolved_spec.model_binding;
            let bindings = awaken_runtime_contract::compile_candidate_credential_bindings(
                &[model],
                Some(&holder),
                &materializer.credential_realization_capabilities(),
                intent.claim_generation,
                memory_unix_ms(),
            )
            .map_err(|error| error.to_string())?;
            let owner = intent
                .claim_owner
                .clone()
                .ok_or_else(|| "Memory extractor has no durable claim owner".to_string())?;
            let authority = Arc::new(MemoryExtractionCredentialAuthority {
                repository: self.extraction_repository(),
                intent_id: intent.intent_id.clone(),
                owner,
                generation: intent.claim_generation,
                bindings: bindings.clone(),
            });
            let context = RuntimeRunContext::new()
                .with_ownership(authority.clone())
                .with_credential_realization(
                    awaken_runtime_contract::AttemptCredentialRealization::new(bindings, authority),
                );
            return materializer
                .materialize_pinned(model, &context)
                .map(|executor| (executor, context))
                .ok_or_else(|| {
                    format!(
                        "published model candidate `{}` is unavailable",
                        model.binding().model_ref
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
        self.extractions
            .read()
            .expect("Memory extraction repository lock poisoned")
            .clone()
    }

    fn extraction_controller(&self) -> MemoryExtractionController {
        MemoryExtractionController::new(self.extraction_repository(), self.claim_owner.clone())
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

    fn validate_live_resource(&self) -> Result<(), String> {
        self.resource_validator
            .as_ref()
            .map_or(Ok(()), |validator| {
                validator
                    .verify_memory_binding(
                        &self.workspace_id,
                        &self.memory_store_id,
                        awaken_resource_contract::ConfigVersion(self.memory_config_version),
                    )
                    .map_err(|error| error.to_string())
            })
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

    async fn extract_mutations(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<Vec<MemoryExtractionMutation>, String> {
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
        let (executor, context) = self.runtime.materialize_extractor(intent)?;
        let tool = erase(WriteMemoryTool::from_handle(capture.clone()));
        let commit = self
            .execution
            .read()
            .expect("Memory execution context lock poisoned")
            .clone()
            .ok_or_else(|| {
                "Memory extraction is waiting for the Session commit/history binding".to_string()
            })?;
        let context = context
            .with_commit(commit.clone())
            .with_reader(commit.clone());
        let auxiliary_thread_id = intent.auxiliary_thread_id();
        run_configured_agent_with_id(
            &catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(&self.runtime.provider),
            executor,
            &intent.extractor.agent.root_agent_id.0,
            &auxiliary_thread_id,
            RunId(intent.auxiliary_run_id()),
            seed,
            vec![tool],
            context,
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut writes = capture.take();
        if writes.is_empty() {
            writes = writes_from_committed_agent(
                &commit.committed_messages(&ThreadId(auxiliary_thread_id)),
            );
        }
        self.platform.plan_mutations(writes).await
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
            .map_err(|_| "MemoryStore is missing, suspended, archived, or deleted".to_string())
    }

    async fn extract(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<Vec<MemoryExtractionMutation>, String> {
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

#[async_trait]
impl MemoryStoreHandle for BoundMemory {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        self.validate_live_resource()?;
        self.store.write(name, content).await
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        self.validate_live_resource()?;
        self.store.entries().await
    }
}

impl crate::host::SharedHost {
    /// The Memory content data-plane port used by an outer process startup to
    /// construct a worker-side mounter. It carries no principal or policy state.
    pub fn memory_repository(&self) -> Arc<dyn awaken_resource_contract::MemoryRepository> {
        self.memory_stores.fs_handle()
    }

    /// The Workspace-scoped Skill aggregate port shared by HTTP authoring and
    /// runtime activation. `None` means this host has no durable Skill plane.
    #[cfg(any(test, feature = "test-support"))]
    pub fn skill_store(&self) -> Option<Arc<dyn awaken_resource_contract::SkillStore>> {
        self.skills.store_handle()
    }

    /// Install the worker adapter behind the neutral provisioning port. This is
    /// intentionally separate from [`SharedHost::new`](crate::SharedHost::new):
    /// runtime-host must not depend on a FUSE/copy implementation crate.
    pub fn install_memory_mounter(
        &self,
        mounter: Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
        self.provider.install_memory_mounter(mounter.clone());
        self.session_provider
            .install_memory_mounter(mounter.clone());
        if let Some(provider) = &self.backend_owned_session_provider {
            provider.install_memory_mounter(mounter.clone());
        }
        *self
            .memory_mounter
            .write()
            .expect("memory mounter lock poisoned") = Some(mounter);
    }

    /// Whether an outer process startup already selected the Memory mount
    /// adapter. Default platform wiring must preserve an explicit selection.
    #[must_use]
    pub fn has_memory_mounter(&self) -> bool {
        self.memory_mounter
            .read()
            .expect("memory mounter lock poisoned")
            .is_some()
    }

    pub(crate) fn memory_mounter(
        &self,
    ) -> Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>> {
        self.memory_mounter
            .read()
            .expect("memory mounter lock poisoned")
            .clone()
    }

    pub(crate) fn register_thread_memory(&self, thread: &str, memory: Option<Arc<BoundMemory>>) {
        self.session_slots.update(thread, |slot| {
            slot.memory_bindings.clear();
            slot.memory = memory;
        });
    }

    /// Replace the complete set of standard mounted MemoryStore bindings. No
    /// automatic recall/extraction binding is inferred from collection order.
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
        if self.upstream.is_some() {
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

    pub(crate) fn platform_memory_handle(
        &self,
        store_id: String,
        writable: bool,
    ) -> Arc<PlatformMemoryHandle> {
        Arc::new(PlatformMemoryHandle::new(
            self.memory_stores.fs_handle(),
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
        let bound = self.memory.bind(
            thread,
            workspace_id,
            handle,
            Some(resource_validator),
            config,
            writable,
        );
        self.register_thread_memory(thread, Some(Arc::new(bound)));
    }

    /// Test-support override for fixtures built through the volatile constructor.
    /// Product startup injects this authority during SharedHost construction.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_memory_extraction_repository(
        &self,
        repository: Arc<dyn MemoryExtractionRepository>,
    ) {
        self.memory.set_extraction_repository(repository);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedHost;
    use async_trait::async_trait;
    use awaken_ext_memory::{MemoryExtractionStatus, RecallSelector as _};
    use awaken_memory_store::MemoryRepository as _;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult, ToolCall,
    };

    /// A stub extractor model: first Step emits a write_memory call; once it sees
    /// the tool result, it replies done.
    struct ExtractorModel;

    #[async_trait]
    impl LlmExecutor for ExtractorModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let saw_tool_result = request.messages.iter().any(|m| m.role == Role::Tool);
            let output = if saw_tool_result {
                AssistantOutput::text("saved 1 memory")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "user prefs",
                        "content": "user likes rust",
                    }),
                }])
            };
            Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct FailFirstRecoverableRead {
        inner: Arc<dyn MemoryExtractionRepository>,
        remaining: std::sync::atomic::AtomicUsize,
    }

    impl FailFirstRecoverableRead {
        fn new(inner: Arc<dyn MemoryExtractionRepository>) -> Self {
            Self {
                inner,
                remaining: std::sync::atomic::AtomicUsize::new(1),
            }
        }
    }

    #[async_trait]
    impl MemoryExtractionRepository for FailFirstRecoverableRead {
        async fn put_extraction_if_absent(
            &self,
            intent: MemoryExtractionIntent,
        ) -> Result<awaken_ext_memory::PutMemoryExtractionOutcome, MemoryExtractionError> {
            self.inner.put_extraction_if_absent(intent).await
        }

        async fn get_extraction(
            &self,
            intent_id: &str,
        ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
            self.inner.get_extraction(intent_id).await
        }

        async fn extraction_cursor(&self, thread_id: &str) -> Result<usize, MemoryExtractionError> {
            self.inner.extraction_cursor(thread_id).await
        }

        async fn recoverable_extractions(
            &self,
            limit: usize,
        ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
            if self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(MemoryExtractionError::Storage(
                    "transient recovery read".into(),
                ));
            }
            self.inner.recoverable_extractions(limit).await
        }

        async fn compare_and_swap_extraction(
            &self,
            expected_revision: u64,
            intent: MemoryExtractionIntent,
        ) -> Result<(), MemoryExtractionError> {
            self.inner
                .compare_and_swap_extraction(expected_revision, intent)
                .await
        }
    }

    fn user(text: &str) -> Message {
        Message {
            id: MessageId("u1".into()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    fn snapshot(thread: &str, messages: Vec<Message>) -> TranscriptSnapshot {
        TranscriptSnapshot::new(
            awaken_agent_contract::agent::thread::Id(thread.to_string()),
            awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
            messages,
        )
    }

    #[test]
    fn committed_agent_tool_calls_rebuild_only_policy_accepted_mutations() {
        let messages = vec![Message::new(
            MessageId("assistant".into()),
            Role::Assistant,
            vec![
                ContentBlock::tool_use(
                    "valid",
                    "write_memory",
                    serde_json::json!({
                        "name": "User Pref",
                        "content": "The user prefers concise reviews."
                    }),
                ),
                ContentBlock::tool_use(
                    "rejected",
                    "write_memory",
                    serde_json::json!({
                        "name": "fix",
                        "content": "Fixed the bug by patching queue.rs."
                    }),
                ),
                ContentBlock::tool_use(
                    "other",
                    "unrelated",
                    serde_json::json!({"name": "ignored", "content": "ignored"}),
                ),
            ],
        )];

        assert_eq!(
            writes_from_committed_agent(&messages),
            BTreeMap::from([(
                "/User-Pref.md".to_string(),
                "The user prefers concise reviews.".to_string()
            )])
        );
    }

    fn extractor(
        instructions: Option<&str>,
        extraction_prompt: Option<&str>,
    ) -> MemoryExtractorSnapshot {
        let mut extractor =
            MemoryExtractorSnapshot::host_executor(MEMORY_AGENT_ID, "host", "stub", "host");
        if let Some(instructions) = instructions {
            extractor.agent.resolved_spec.instructions = instructions.to_string();
            extractor.agent.recompute_fingerprint().unwrap();
        }
        extractor.extraction_prompt = extraction_prompt.map(str::to_string);
        extractor
    }

    fn bound_test_memory(
        llm: Arc<dyn LlmExecutor>,
        session_id: &str,
        sandbox_base: &std::path::Path,
        _memory_root: &std::path::Path,
    ) -> (
        Arc<MemoryRuntime>,
        BoundMemory,
        Arc<awaken_memory_store::VolatileMemoryRepository>,
        Arc<awaken_session_store::SqliteManagedSessionRepository>,
    ) {
        let extractions = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral Memory extraction repository"),
        );
        let runtime = Arc::new(MemoryRuntime::new(
            llm,
            Arc::new(LocalProvider::new(sandbox_base)),
            Arc::new(BackgroundRuns::new()),
            extractions.clone(),
        ));
        let config = awaken_resource_contract::MemoryStoreConfigVersion {
            memory_store_id: "test-store".into(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
        let platform = Arc::new(PlatformMemoryHandle::new(
            repository.clone(),
            "test-store".into(),
            true,
        ));
        let bound = runtime.bind(
            session_id,
            "ws-test",
            platform,
            Some(crate::host::test_resource_validator()),
            &config,
            true,
        );
        bound.bind_execution(Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(
                awaken_store_inmem::MemoryCommitCoordinator::new(),
            ),
        ))));
        (runtime, bound, repository, extractions)
    }

    struct RecallSnapshotRepository {
        heads: Vec<awaken_memory_store::Memory>,
    }

    #[async_trait]
    impl awaken_memory_store::MemoryRepository for RecallSnapshotRepository {
        async fn snapshot_heads(
            &self,
            _store: &str,
        ) -> Result<Vec<awaken_memory_store::Memory>, awaken_memory_store::MemErr> {
            Ok(self.heads.clone())
        }

        async fn list(
            &self,
            _store: &str,
            _prefix: &str,
        ) -> Result<Vec<awaken_memory_store::MemoryEntry>, awaken_memory_store::MemErr> {
            panic!("Recall must not emulate an atomic snapshot with list")
        }

        async fn get_by_path(
            &self,
            _store: &str,
            _path: &str,
        ) -> Result<Option<awaken_memory_store::Memory>, awaken_memory_store::MemErr> {
            panic!("Recall must not emulate an atomic snapshot with per-path reads")
        }

        async fn create(
            &self,
            _store: &str,
            _path: &str,
            _content: &str,
        ) -> Result<awaken_memory_store::Memory, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no writes")
        }

        async fn update_head(
            &self,
            _store: &str,
            _id: &str,
            _content: &str,
            _base_sha: &str,
            _target_path: Option<&str>,
        ) -> Result<awaken_memory_store::Memory, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no writes")
        }

        async fn rename(
            &self,
            _store: &str,
            _from: &str,
            _to: &str,
        ) -> Result<awaken_memory_store::Memory, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no writes")
        }

        async fn delete_by_path(
            &self,
            _store: &str,
            _path: &str,
        ) -> Result<(), awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no writes")
        }

        async fn delete_if_match(
            &self,
            _store: &str,
            _path: &str,
            _base_id: &str,
            _base_sha: &str,
        ) -> Result<bool, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no writes")
        }

        async fn list_versions(
            &self,
            _store: &str,
        ) -> Result<Vec<awaken_memory_store::MemoryVersion>, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no history reads")
        }

        async fn redact_version(
            &self,
            _store: &str,
            _version_id: &str,
        ) -> Result<Option<awaken_memory_store::MemoryVersion>, awaken_memory_store::MemErr>
        {
            unreachable!("Recall snapshot test performs no history writes")
        }

        async fn purge_store(
            &self,
            _store: &str,
        ) -> Result<awaken_memory_store::MemoryPurgeSummary, awaken_memory_store::MemErr> {
            unreachable!("Recall snapshot test performs no lifecycle writes")
        }
    }

    /// A model that replies with fixed selection indices, standing in for the
    /// `memory-selector` sub-agent.
    struct IndexModel;

    #[async_trait]
    impl LlmExecutor for IndexModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("[1], [2]"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn agent_selector_runs_the_subagent_and_parses_its_reply() {
        // Cause/effect graph and decision-table rule S1:
        // C1 selector snapshot valid + C2 explicit commit/history authority present
        // -> E1 auxiliary Run commits through that authority + E2 parsed selection
        // is returned. The forbidden complement (!C2 -> process-local fallback) is
        // unrepresentable because `AgentSelector::new` requires the authority.
        let commit = Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new()),
        )));
        let selector = AgentSelector::new(
            Arc::new(IndexModel),
            awaken_ext_memory::default_selector_agent(
                "stub",
                awaken_ext_memory::DEFAULT_SELECTOR_INSTRUCTIONS,
            ),
            commit,
        );
        let manifest = vec![
            (0usize, "alpha".to_string()),
            (1, "beta".to_string()),
            (2, "gamma".to_string()),
        ];
        let picked = selector.select("which?", &manifest, 5).await;
        assert_eq!(picked, vec![1, 2]);
    }

    #[tokio::test]
    async fn extraction_writes_a_memory_then_recall_reads_it_back() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem-root-{stamp}"));

        let (runtime, extraction, repository, _extractions) = bound_test_memory(
            Arc::new(ExtractorModel),
            "thread-1",
            &sandbox_base,
            &mem_root,
        );

        assert!(
            !extraction.reconcile().await.unwrap(),
            "an empty recovery scan must not create a background run"
        );
        extraction
            .trigger(
                "terminal-1",
                snapshot("thread-1", vec![user("I really like rust")]),
                extractor(None, None),
            )
            .await
            .unwrap();
        assert!(runtime.drain(Duration::from_secs(10)).await);

        assert_eq!(
            repository
                .get_by_path("test-store", "/user-prefs.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("user likes rust")
        );
        // The read side surfaces it through bounded recall.
        let entries = extraction.store().entries().await.unwrap();
        let block = awaken_ext_memory::recall::render(
            &entries,
            &awaken_ext_memory::RecallBounds::default(),
        )
        .expect("recall block");
        assert!(block.contains("user likes rust"), "got: {block}");
    }

    #[tokio::test]
    async fn platform_handle_enforces_read_only_at_the_data_plane_boundary() {
        let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
        fs.create("store-a", "/existing.md", "safe").await.unwrap();
        let read_only = PlatformMemoryHandle::new(fs.clone(), "store-a".into(), false);

        let error = read_only
            .write("new", "must not persist")
            .await
            .unwrap_err();
        assert!(error.contains("read-only"));
        assert!(
            fs.get_by_path("store-a", "/new.md")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(read_only.entries().await.unwrap().len(), 1);
    }

    /// Cause/effect decision table for Recall reads:
    /// | Rule | atomic snapshot | legacy list/get | Effect |
    /// |---|---|---|---|
    /// | R1 | one non-empty head | panic if called | return one trimmed Recall entry |
    /// | R2 | one blank head | panic if called | omit blank content |
    #[tokio::test]
    async fn recall_entries_use_only_the_atomic_memory_snapshot() {
        let memory = |id: &str, path: &str, content: &str| awaken_memory_store::Memory {
            id: id.into(),
            path: path.into(),
            content_sha256: awaken_memory_store::sha256_hex(content),
            content_size: content.len() as u64,
            version: 1,
            created_unix_nanos: 1,
            updated_unix_nanos: 2,
            content: Some(content.into()),
        };
        let handle = PlatformMemoryHandle::new(
            Arc::new(RecallSnapshotRepository {
                heads: vec![
                    memory("memory-a", "/a.md", "  remembered  "),
                    memory("memory-blank", "/blank.md", "  "),
                ],
            }),
            "store-a".into(),
            false,
        );

        let entries = handle.entries().await.expect("R1/R2");
        assert_eq!(entries.len(), 1, "R1/R2");
        assert_eq!(entries[0].path, std::path::PathBuf::from("/a.md"), "R1");
        assert_eq!(entries[0].content, "remembered", "R1");
    }

    /// An extractor that saves whatever non-prompt text it was seeded with, so the
    /// test can see what reached it.
    struct SeedEchoModel;

    #[async_trait]
    impl LlmExecutor for SeedEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let saved = request.messages.iter().any(|m| {
                m.role == Role::Tool
                    && m.content.iter().any(|b| match b {
                        awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                            content,
                            ..
                        } => crate::config::block_text(content).contains("saved memory"),
                        _ => false,
                    })
            });
            if saved {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("done"),
                    usage: None,
                    stop_reason: None,
                });
            }
            let seen: Vec<String> = request
                .messages
                .iter()
                .filter(|m| m.role == Role::User || m.role == Role::System)
                .map(|m| crate::config::block_text(&m.content))
                .filter(|t| !t.contains("Extract durable memories"))
                .collect();
            Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({ "name": "seen", "content": seen.join("|") }),
                }]),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn extraction_seed_excludes_recalled_memory_messages() {
        // Test design. Causes: C1 committed history contains a recalled-memory
        // System message plus real Run messages; C2 extraction builds its seed.
        // Effects: E1 C2 excludes the recalled payload; E2 genuine conversation
        // remains eligible. Constraint/Invariant: recalled context is read-only
        // input, never new memory evidence. Decision rule: seed both classes and
        // assert only genuine history reaches extraction.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem2-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem2-root-{stamp}"));
        let (runtime, extraction, repository, _extractions) =
            bound_test_memory(Arc::new(SeedEchoModel), "t", &sandbox_base, &mem_root);

        // A committed history with a recalled-memory system message + a real Run.
        let recall = Message::text(
            MessageId(format!("{}-1", awaken_ext_memory::RECALL_MESSAGE_ID_PREFIX)),
            Role::System,
            "RECALLED SECRET",
        );
        extraction
            .trigger(
                "terminal-1",
                snapshot("t", vec![recall, user("please note this")]),
                extractor(None, None),
            )
            .await
            .unwrap();
        assert!(runtime.drain(Duration::from_secs(10)).await);

        let seen = repository
            .get_by_path("test-store", "/seen.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .unwrap();
        assert!(seen.contains("please note this"), "real Run seen: {seen}");
        assert!(
            !seen.contains("RECALLED SECRET"),
            "recalled content must not reach the extractor: {seen}"
        );
    }

    #[tokio::test]
    async fn extraction_uses_the_per_agent_memory_prompts() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem3-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem3-root-{stamp}"));
        let (runtime, extraction, repository, _extractions) = bound_test_memory(
            Arc::new(SeedEchoModel),
            "t-custom",
            &sandbox_base,
            &mem_root,
        );

        extraction
            .trigger(
                "terminal-1",
                snapshot("t-custom", vec![user("remember this")]),
                extractor(Some("CUSTOM MEMORY SYSTEM"), Some("CUSTOM EXTRACTION TASK")),
            )
            .await
            .unwrap();
        assert!(runtime.drain(Duration::from_secs(10)).await);

        let seen = repository
            .get_by_path("test-store", "/seen.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .unwrap();
        assert!(
            seen.contains("CUSTOM MEMORY SYSTEM"),
            "custom instructions reached extractor: {seen}"
        );
        assert!(
            seen.contains("CUSTOM EXTRACTION TASK"),
            "custom task reached extractor: {seen}"
        );
    }

    #[tokio::test]
    async fn terminal_redelivery_creates_one_logical_extraction_and_one_memory_version() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem4-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem4-root-{stamp}"));
        let (runtime, extraction, repository, extractions) = bound_test_memory(
            Arc::new(ExtractorModel),
            "thread-redelivery",
            &sandbox_base,
            &mem_root,
        );

        for _ in 0..2 {
            extraction
                .trigger(
                    "terminal-7",
                    snapshot("thread-redelivery", vec![user("I really like rust")]),
                    extractor(None, None),
                )
                .await
                .unwrap();
        }
        assert!(runtime.drain(Duration::from_secs(10)).await);

        let intent = extractions
            .get_extraction("memory-extraction:thread-redelivery:terminal-7")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(intent.status, MemoryExtractionStatus::Completed);
        assert_eq!(intent.attempts, 1);
        assert_eq!(
            repository.list_versions("test-store").await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn recovery_after_store_before_receipt_does_not_duplicate_a_memory_version() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem5-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem5-root-{stamp}"));
        let (runtime, extraction, repository, extractions) = bound_test_memory(
            Arc::new(ExtractorModel),
            "thread-crash",
            &sandbox_base,
            &mem_root,
        );
        let content = "customer maintenance is Sunday";
        let target = awaken_memory_store::sha256_hex(content);
        let mut intent = MemoryExtractionIntent::new_range(
            "memory-extraction:thread-crash:terminal-8",
            "thread-crash:terminal-8",
            "ws-test",
            "thread-crash",
            "terminal-8",
            "test-store",
            1,
            0,
            1,
            vec![user("remember the maintenance window")],
            MemoryExtractorSnapshot::host_executor(MEMORY_AGENT_ID, "host", "stub", "host"),
        )
        .unwrap();
        let generation = intent.claim("crashed-worker", 0, 1).unwrap();
        intent
            .mark_extracted(
                "crashed-worker",
                generation,
                0,
                vec![MemoryExtractionMutation {
                    path: "/maintenance.md".into(),
                    content: content.into(),
                    observed_sha256: None,
                    target_sha256: target.clone(),
                }],
            )
            .unwrap();
        extractions.put_extraction_if_absent(intent).await.unwrap();
        // The data-plane commit happened, then the process died before the intent
        // advanced to Stored. Recovery must recognize the target hash as applied.
        repository
            .create("test-store", "/maintenance.md", content)
            .await
            .unwrap();

        extraction.reconcile().await.unwrap();
        assert!(runtime.drain(Duration::from_secs(10)).await);
        let recovered = extractions
            .get_extraction("memory-extraction:thread-crash:terminal-8")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, MemoryExtractionStatus::Completed);
        assert!(recovered.receipt.unwrap().mutations[0].already_applied);
        assert_eq!(
            repository.list_versions("test-store").await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn parent_rehydration_recovers_a_child_intent_after_enqueue_crash() {
        // Cause/effect graph: C1 the terminal transcript belongs to a logical
        // child; C2 its extraction binding and commit authority belong to the
        // parent physical Session; C3 the first process dies after durable
        // enqueue but before any claim; C4 the parent binding is rehydrated from
        // the same Workspace/Store/config; C5 the selected direct binding is/is
        // not already in the binding map and one Arc may appear under two binding
        // ids; C6 a read-only sibling binding exists; C7 the first recovery read
        // fails transiently. Effects: E1 the intent
        // persists the parent Session and logical child without a third identity;
        // E2 no work starts without a commit binding; E3 parent rehydration
        // accepts and completes the child intent independently of plugin
        // selection; E4 the stable auxiliary Run commits through the parent
        // authority; E5 one governed Memory version is written; E6 identical
        // Arcs are driven once, the direct-only writable binding receives the
        // commit authority, and the read-only binding does not drive recovery;
        // E7 the first rehydrate fails closed and the same recovery entry
        // converges on its next resident/cold wake.
        // Constraints: no child Session slot or physical commit partition exists;
        // recovery reuses the ordinary BoundMemory/controller/repository path.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5       | C6 | C7        | effects          |
        // | R1   | T  | T  | T  | F  | -        | -  | F         | E1,E2           |
        // | R2   | T  | T  | T  | T  | duplicate| T  | F         | E1,E3,E4,E5,E6 |
        // | R3   | T  | T  | T  | T  | direct   | T  | F         | E3,E6           |
        // | R4   | T  | T  | T  | T  | either   | T  | once      | E3,E6,E7        |
        let storage = tempfile::tempdir().expect("Memory recovery tempdir");
        let extraction_db = storage.path().join("sessions.db");
        let extraction_db = extraction_db.to_string_lossy().to_string();
        let parent = "memory-parent-session";
        let child = "memory-logical-child";
        let terminal = "memory-child-terminal";
        let intent_id = format!("memory-extraction:{child}:{terminal}");
        let config = awaken_resource_contract::MemoryStoreConfigVersion {
            memory_store_id: "test-store".into(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        let content = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
        let platform = Arc::new(PlatformMemoryHandle::new(
            content.clone(),
            "test-store".into(),
            true,
        ));

        let first_repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&extraction_db)
                .expect("R1 open extraction repository"),
        );
        let first_runtime = Arc::new(MemoryRuntime::new(
            Arc::new(ExtractorModel),
            Arc::new(LocalProvider::new(storage.path().join("first-sandbox"))),
            Arc::new(BackgroundRuns::new()),
            first_repository.clone(),
        ));
        let child_binding = first_runtime.bind(
            parent,
            "ws-test",
            platform.clone(),
            Some(crate::host::test_resource_validator()),
            &config,
            true,
        );
        child_binding
            .trigger(
                terminal,
                snapshot(child, vec![user("remember child recovery")]),
                extractor(None, None),
            )
            .await
            .expect("R1 durable enqueue");
        let pending = first_repository
            .get_extraction(&intent_id)
            .await
            .expect("R1 query pending intent")
            .expect("R1 pending intent exists");
        assert_eq!(pending.session_id, parent, "R1/E1 physical Session");
        assert_eq!(pending.logical_thread_id(), child, "R1/E1 logical child");
        assert_eq!(pending.status, MemoryExtractionStatus::Pending, "R1/E2");
        assert!(first_runtime.drain(Duration::from_secs(1)).await, "R1/E2");
        drop(child_binding);
        drop(first_runtime);
        drop(first_repository);

        let recovered_repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&extraction_db)
                .expect("R2 reopen extraction repository"),
        );
        let recovered_host = Arc::new(SharedHost::new(Arc::new(ExtractorModel), "stub"));
        recovered_host.install_memory_extraction_repository(Arc::new(
            FailFirstRecoverableRead::new(recovered_repository.clone()),
        ));
        let parent_commit = Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new()),
        )));
        let parent_binding = Arc::new(recovered_host.memory.bind(
            parent,
            "ws-test",
            platform.clone(),
            Some(crate::host::test_resource_validator()),
            &config,
            true,
        ));
        let read_only_binding = Arc::new(recovered_host.memory.bind(
            parent,
            "ws-test",
            platform.clone(),
            Some(crate::host::test_resource_validator()),
            &config,
            false,
        ));
        let direct_config = awaken_resource_contract::MemoryStoreConfigVersion {
            memory_store_id: "direct-store".into(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        let direct_binding = Arc::new(recovered_host.memory.bind(
            parent,
            "ws-test",
            Arc::new(PlatformMemoryHandle::new(
                content.clone(),
                "direct-store".into(),
                true,
            )),
            Some(crate::host::test_resource_validator()),
            &direct_config,
            true,
        ));
        recovered_host.register_thread_memory_bindings(
            parent,
            std::collections::HashMap::from([
                ("child-memory".into(), parent_binding.clone()),
                ("duplicate-alias".into(), parent_binding.clone()),
                ("read-only".into(), read_only_binding.clone()),
            ]),
        );
        recovered_host
            .session_slots
            .update(parent, |slot| slot.memory = Some(direct_binding.clone()));
        let first_recovery = recovered_host
            .bind_thread_memory_recovery(parent, parent_commit.clone())
            .await;
        assert!(
            first_recovery
                .unwrap_err()
                .to_string()
                .contains("transient recovery read"),
            "R4/E7 first wake exposes the repository failure"
        );
        assert_eq!(
            recovered_host
                .bind_thread_memory_recovery(parent, parent_commit.clone())
                .await
                .unwrap(),
            1,
            "R2/R3/R4 E3,E6,E7 next wake schedules one matching writable binding"
        );
        assert!(
            recovered_host.drain_memory(Duration::from_secs(10)).await,
            "R2/E3 recovery drained"
        );
        assert!(
            parent_binding.execution.read().unwrap().is_some(),
            "R2/E6 mapped writable binding receives commit"
        );
        assert!(
            direct_binding.execution.read().unwrap().is_some(),
            "R3/E6 direct-only selected binding receives commit"
        );
        assert!(
            read_only_binding.execution.read().unwrap().is_none(),
            "R2/R3 E6 read-only binding never drives extraction"
        );

        let completed = recovered_repository
            .get_extraction(&intent_id)
            .await
            .expect("R2 query recovered intent")
            .expect("R2 recovered intent exists");
        assert_eq!(completed.status, MemoryExtractionStatus::Completed, "R2/E3");
        assert_eq!(completed.session_id, parent, "R2/E1");
        assert_eq!(completed.logical_thread_id(), child, "R2/E1");
        let auxiliary_thread = ThreadId(completed.auxiliary_thread_id());
        assert!(
            parent_commit.latest_run(&auxiliary_thread).is_some(),
            "R2/E4 auxiliary Run uses parent commit authority"
        );
        assert_eq!(
            content.list_versions("test-store").await.unwrap().len(),
            1,
            "R2/E5"
        );
    }

    #[tokio::test]
    async fn database_less_worker_does_not_claim_memory_extraction_recovery() {
        // Cause/effect graph: C1 the Host owns local durable extraction authority
        // or is a database-less upstream Worker; C2 a frozen Memory binding is
        // writable or read-only; C3 the extraction repository is available or is
        // the Worker's fail-closed port; C4 a matching durable intent exists or
        // does not; C5 no automatic Memory binding is selected. Effects: E1 a
        // local writable owner schedules matching work;
        // E2 a local repository failure remains visible; E3 a read-only binding
        // never receives recovery execution; E4 an upstream Worker neither probes
        // the outbox nor binds a commit and returns normally; E5 direct Worker
        // authority access still fails closed. Constraints: the Coordinator's
        // guarded settlement observer remains the sole remote terminal/extraction
        // owner; no Worker repository, role flag, or second reconciler is added.
        //
        // | Rule | role     | binding/use          | repository  | intent | C5 | effects |
        // | R1   | local    | RW                   | available   | yes    | any | E1      |
        // | R2   | local    | RW                   | fails       | any    | any | E2      |
        // | R3   | local    | RO                   | any         | any    | any | E3      |
        // | R4   | upstream | RW                   | unavailable | any    | yes | E4      |
        // | R5   | upstream | direct authority call| unavailable | any   | any | E5      |
        // R1-R3 are exercised by
        // `parent_rehydration_recovers_a_child_intent_after_enqueue_crash`;
        // R5 is owned by `every_worker_extraction_authority_operation_fails_closed`.
        let session = "database-less-worker-memory-session";
        let content = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
        let host = Arc::new(
            SharedHost::new_worker_with_deployment(
                Arc::new(ExtractorModel),
                "stub",
                Arc::new(awaken_resource_contract::UnavailableFileContentSource),
                content.clone(),
                crate::DeploymentConfig::ephemeral(),
            )
            .with_worker_upstream(
                awaken_worker_transport_security::WorkerUpstream::new("http://coordinator.invalid"),
            ),
        );
        let config = awaken_resource_contract::MemoryStoreConfigVersion {
            memory_store_id: "worker-memory-store".into(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        let binding = Arc::new(host.memory.bind(
            session,
            "worker-memory-workspace",
            Arc::new(PlatformMemoryHandle::new(
                content,
                config.memory_store_id.to_string(),
                true,
            )),
            Some(crate::host::test_resource_validator()),
            &config,
            true,
        ));
        host.register_thread_memory_bindings(
            session,
            std::collections::HashMap::from([("memory".into(), binding.clone())]),
        );
        assert!(
            host.memory_for_thread(session).is_none(),
            "R4/C5 no automatic Memory binding is selected"
        );
        let commit = Arc::new(host.build_commit(session).await.expect("R4 Worker commit"));

        assert_eq!(
            host.bind_thread_memory_recovery(session, commit)
                .await
                .expect("R4 skips Coordinator-owned extraction recovery"),
            0,
            "R4/E4 no Worker recovery is scheduled"
        );
        assert!(
            binding.execution.read().unwrap().is_none(),
            "R4/E4 the Worker does not bind Coordinator extraction execution"
        );
    }
}
