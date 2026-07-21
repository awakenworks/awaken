//! Out-of-band memory extraction: the composition-root wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! the shared Agent Run substrate, fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a turn, and reads memories back for recall.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::{AgentRunArgs, erase, invoke_agent_tool};
use awaken_ext_memory::{
    DEFAULT_SELECTOR_INSTRUCTIONS, EXTRACT_PROMPT, MEMORY_AGENT_ID, MemoryStoreHandle,
    RecallBounds, RecallSelector, SELECTOR_AGENT_ID, WriteMemoryTool, default_selector_agent,
    parse_indices, sanitize_stem, select_input,
};
use awaken_protocol_managed::{
    MemoryExtractionError, MemoryExtractionIntent, MemoryExtractionMutation,
    MemoryExtractionReceipt, MemoryExtractionRepository, MemoryExtractionStatus,
    MemoryExtractorSnapshot, MemoryMutationReceipt,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::agent_runner::run_configured_agent;
use crate::background::BackgroundRuns;
use crate::judge::HostAgentTool;

// The config pieces the host wires (registering the default extractor agent).
pub use awaken_ext_memory::{DEFAULT_MEMORY_INSTRUCTIONS, default_memory_agent};

const EXTRACTION_LEASE_MS: u64 = 3_000;
const EXTRACTION_HEARTBEAT_MS: u64 = 1_000;
static EXTRACTION_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

/// A [`RecallSelector`] backed by the `memory-selector` sub-agent: a single-step,
/// tool-free, plugin-free run driven through the shared aux-run port
/// through the same ordinary Agent-backed tool used by the judge and compactor.
/// Its configuration activates no plugins, so memory
/// recall cannot recursively invoke itself; the port keeps its usage outside the
/// user session's accounting projection (housekeeping, not delegated work).
pub(crate) struct AgentSelector {
    agent_tool: Arc<dyn RawTool>,
}

impl AgentSelector {
    pub(crate) fn new(llm: Arc<dyn LlmExecutor>, model_ref: &str) -> Self {
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_selector_agent(
            model_ref,
            DEFAULT_SELECTOR_INSTRUCTIONS,
        )));
        let base = std::env::temp_dir()
            .join("awaken-server")
            .join(format!("{}-mem-select", std::process::id()));
        Self {
            agent_tool: Arc::new(HostAgentTool {
                llm,
                provider: LocalProvider::new(base),
                catalog,
                seq: AtomicU64::new(0),
            }),
        }
    }
}

#[async_trait]
impl RecallSelector for AgentSelector {
    async fn select(&self, query: &str, manifest: &[(usize, String)], max: usize) -> Vec<usize> {
        let input = select_input(query, manifest, max);
        // Fire the selector through the shared port; a runner error degrades to
        // "select nothing" (recall falls back to no memories rather than failing the
        // turn). The port surfaces only the reply text, its usage stays isolated.
        let reply = invoke_agent_tool(
            self.agent_tool.as_ref(),
            "memory-selector-agent-run",
            AgentRunArgs {
                agent_id: SELECTOR_AGENT_ID.to_string(),
                seed: vec![Message {
                    id: MessageId("mem-select".into()),
                    role: Role::User,
                    content: vec![ContentBlock::text(input)],
                }],
            },
            None,
        )
        .await
        .ok()
        .filter(|output| !output.is_error)
        .map(|output| output.content)
        .unwrap_or_default();
        parse_indices(&reply, manifest.len(), max)
    }
}

/// Message-id prefix for injected recall blocks. Shared so the host stamps it and
/// extraction filters it (recall is context, not a conversation fact).
pub const RECALL_MSG_PREFIX: &str = "mem-recall-";

/// Runtime adapter over one already-authorized platform MemoryStore. It carries
/// only data-plane identity and maximum access; authorization policy remains at
/// the edge that constructs it.
pub(crate) struct PlatformMemoryHandle {
    fs: Arc<dyn awaken_memory_store::MemoryRepository>,
    store_id: String,
    writable: bool,
}

impl PlatformMemoryHandle {
    pub(crate) fn new(
        fs: Arc<dyn awaken_memory_store::MemoryRepository>,
        store_id: String,
        writable: bool,
    ) -> Self {
        Self {
            fs,
            store_id,
            writable,
        }
    }

    async fn plan_mutations(
        &self,
        writes: BTreeMap<String, String>,
    ) -> Result<Vec<MemoryExtractionMutation>, String> {
        let mut mutations = Vec::with_capacity(writes.len());
        for (path, content) in writes {
            let current = self
                .fs
                .get_by_path(&self.store_id, &path)
                .await
                .map_err(|error| error.to_string())?;
            mutations.push(MemoryExtractionMutation {
                path,
                target_sha256: awaken_memory_store::sha256_hex(&content),
                content,
                observed_sha256: current.map(|memory| memory.content_sha256),
            });
        }
        Ok(mutations)
    }

    async fn apply_mutation(
        &self,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        let current = self
            .fs
            .get_by_path(&self.store_id, &mutation.path)
            .await
            .map_err(|error| error.to_string())?;
        if current
            .as_ref()
            .is_some_and(|memory| memory.content_sha256 == mutation.target_sha256)
        {
            return Ok(MemoryMutationReceipt {
                path: mutation.path.clone(),
                target_sha256: mutation.target_sha256.clone(),
                already_applied: true,
            });
        }
        match (current, mutation.observed_sha256.as_deref()) {
            (None, None) => {
                self.fs
                    .create(&self.store_id, &mutation.path, &mutation.content)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            (Some(current), Some(expected)) if current.content_sha256 == expected => {
                self.fs
                    .update(&self.store_id, &current.id, &mutation.content, expected)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            _ => {
                return Err(format!(
                    "memory `{}` changed after extraction planning",
                    mutation.path
                ));
            }
        }
        Ok(MemoryMutationReceipt {
            path: mutation.path.clone(),
            target_sha256: mutation.target_sha256.clone(),
            already_applied: false,
        })
    }
}

#[derive(Default)]
struct CapturedMemoryWrites {
    writes: Mutex<BTreeMap<String, String>>,
}

impl CapturedMemoryWrites {
    fn take(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut *self.writes.lock().expect("captured Memory writes"))
    }
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

#[async_trait]
impl MemoryStoreHandle for PlatformMemoryHandle {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        let path = format!("/{}.md", sanitize_stem(name));
        match self
            .fs
            .get_by_path(&self.store_id, &path)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(current) => self
                .fs
                .update(
                    &self.store_id,
                    &current.id,
                    content,
                    &current.content_sha256,
                )
                .await
                .map_err(|error| error.to_string())?,
            None => self
                .fs
                .create(&self.store_id, &path, content)
                .await
                .map_err(|error| error.to_string())?,
        };
        Ok(path)
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        let mut entries = Vec::new();
        for item in self
            .fs
            .list(&self.store_id, "/")
            .await
            .map_err(|error| error.to_string())?
        {
            let Some(memory) = self
                .fs
                .get_by_path(&self.store_id, &item.path)
                .await
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let Some(content) = memory.content.filter(|content| !content.trim().is_empty()) else {
                continue;
            };
            let nanos = u64::try_from(memory.updated_unix_nanos).unwrap_or(u64::MAX);
            entries.push(awaken_ext_memory::Entry {
                path: std::path::PathBuf::from(memory.path),
                content: content.trim().to_string(),
                modified: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos),
            });
        }
        entries.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(entries)
    }
}

/// Host-level extraction/selection capability. It owns the auxiliary-agent
/// machinery but deliberately owns no MemoryStore identity or content handle.
/// A Session must create a [`BoundMemory`] from its resolved resource manifest.
pub struct MemoryRuntime {
    llm: Arc<dyn LlmExecutor>,
    inference_materializer:
        RwLock<Option<Arc<dyn crate::inference_routing::InferenceExecutorMaterializer>>>,
    provider: Arc<LocalProvider>,
    catalog: Arc<AgentCatalog>,
    background: Arc<BackgroundRuns>,
    claim_owner: String,
    extractions: RwLock<Arc<dyn MemoryExtractionRepository>>,
}

/// One Session-scoped MemoryStore binding shared by recall and extraction.
#[derive(Clone)]
pub struct BoundMemory {
    runtime: Arc<MemoryRuntime>,
    store: Arc<dyn MemoryStoreHandle>,
    platform: Arc<PlatformMemoryHandle>,
    resource_configs: Arc<dyn awaken_protocol_managed::resource_plane::ResourceConfigSource>,
    workspace_id: String,
    memory_store_id: String,
    memory_config_version: u64,
    bounds: RecallBounds,
    recall_enabled: bool,
    extraction_enabled: bool,
}

impl MemoryRuntime {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<LocalProvider>,
        catalog: Arc<AgentCatalog>,
        background: Arc<BackgroundRuns>,
        extractions: Arc<dyn MemoryExtractionRepository>,
    ) -> Self {
        Self {
            llm,
            inference_materializer: RwLock::new(None),
            provider,
            catalog,
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
        workspace_id: impl Into<String>,
        platform: Arc<PlatformMemoryHandle>,
        resource_configs: Arc<dyn awaken_protocol_managed::resource_plane::ResourceConfigSource>,
        config: &awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion,
        writable: bool,
    ) -> BoundMemory {
        let bounds = RecallBounds {
            max_entries: usize::try_from(config.recall_policy.max_results)
                .unwrap_or(usize::MAX)
                .max(1),
            ..RecallBounds::default()
        };
        BoundMemory {
            runtime: self.clone(),
            store: platform.clone(),
            platform,
            resource_configs,
            workspace_id: workspace_id.into(),
            memory_store_id: config.memory_store_id.clone(),
            memory_config_version: config.version.0,
            bounds,
            recall_enabled: config.recall_policy.enabled,
            extraction_enabled: writable && config.extraction_policy.enabled,
        }
    }

    /// Await every in-flight extraction started by any bound Session.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }

    pub fn set_extraction_repository(&self, repository: Arc<dyn MemoryExtractionRepository>) {
        *self
            .extractions
            .write()
            .expect("Memory extraction repository lock poisoned") = repository;
    }

    pub(crate) fn set_inference_materializer(
        &self,
        materializer: Arc<dyn crate::inference_routing::InferenceExecutorMaterializer>,
    ) {
        *self
            .inference_materializer
            .write()
            .expect("Memory inference materializer lock poisoned") = Some(materializer);
    }

    fn materialize_extractor(
        &self,
        snapshot: &MemoryExtractorSnapshot,
    ) -> Result<Arc<dyn LlmExecutor>, String> {
        if let Some(materializer) = self
            .inference_materializer
            .read()
            .expect("Memory inference materializer lock poisoned")
            .as_ref()
        {
            return materializer
                .materialize_pinned(&snapshot.model_ref, &snapshot.inference_access)
                .ok_or_else(|| {
                    format!(
                        "pinned inference access for model `{}` is unavailable",
                        snapshot.model_ref
                    )
                });
        }
        snapshot
            .inference_access
            .is_host_executor_for(&snapshot.model_ref)
            .then(|| self.llm.clone())
            .ok_or_else(|| {
                "pinned inference access requires an installed credential materializer".into()
            })
    }

    pub(crate) fn extraction_repository(&self) -> Arc<dyn MemoryExtractionRepository> {
        self.extractions
            .read()
            .expect("Memory extraction repository lock poisoned")
            .clone()
    }
}

impl BoundMemory {
    pub(crate) fn recall_enabled(&self) -> bool {
        self.recall_enabled
    }

    pub(crate) fn extraction_enabled(&self) -> bool {
        self.extraction_enabled
    }

    fn matches_intent(&self, intent: &MemoryExtractionIntent) -> bool {
        intent.workspace_id == self.workspace_id
            && intent.memory_store_id == self.memory_store_id
            && intent.memory_config_version == self.memory_config_version
    }

    fn validate_live_resource(&self) -> Result<(), String> {
        self.resource_configs
            .resolve_memory_store(&self.workspace_id, &self.memory_store_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Durably enqueue one extraction keyed by the terminal commit, then drive it
    /// asynchronously. Returning from this method means the intent is persistent,
    /// not that extraction has completed.
    pub async fn trigger(
        &self,
        thread: &str,
        terminal_commit_id: &str,
        committed: Vec<Message>,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<(), MemoryExtractionError> {
        if self.runtime.catalog.resolve(MEMORY_AGENT_ID).is_none() {
            return Ok(());
        }
        // Drop recalled-memory messages from the seed: they are injected context,
        // not new conversation facts. Without this the extractor re-saves what it
        // just recalled (a cross-thread self-copy loop).
        let seed: Vec<Message> = committed
            .into_iter()
            .filter(|m| !m.id.0.starts_with(RECALL_MSG_PREFIX))
            .collect();
        let idempotency_key = format!("{thread}:{terminal_commit_id}");
        let intent_id = format!("memory-extraction:{idempotency_key}");
        let repository = self.runtime.extraction_repository();
        if let Some(existing) = repository.get_extraction(&intent_id).await? {
            if existing.workspace_id != self.workspace_id
                || existing.session_id != thread
                || existing.terminal_commit_id != terminal_commit_id
                || existing.memory_store_id != self.memory_store_id
                || existing.memory_config_version != self.memory_config_version
            {
                return Err(MemoryExtractionError::IdempotencyConflict(idempotency_key));
            }
            self.reconcile(thread).await;
            return Ok(());
        }
        let intent = MemoryExtractionIntent::new(
            intent_id,
            idempotency_key,
            self.workspace_id.clone(),
            thread,
            terminal_commit_id,
            self.memory_store_id.clone(),
            self.memory_config_version,
            seed,
            extractor,
        )?;
        repository.put_extraction_if_absent(intent).await?;
        self.reconcile(thread).await;
        Ok(())
    }

    /// Resume every non-terminal intent for this exact frozen binding. Invoked
    /// after enqueue and after Session rehydration, so a process crash cannot lose
    /// the remaining extraction/store/receipt work.
    pub async fn reconcile(&self, thread: &str) {
        let bound = self.clone();
        let thread = thread.to_string();
        self.runtime
            .background
            .spawn(async move {
                bound.drive_recoverable(&thread).await;
            })
            .await;
    }

    async fn drive_recoverable(&self, thread: &str) {
        const MAX_ATTEMPTS: u32 = 5;
        let repository = self.runtime.extraction_repository();
        let owner = self.runtime.claim_owner.as_str();
        loop {
            let Ok(candidates) = repository.recoverable_extractions(64).await else {
                return;
            };
            let Some(mut intent) = candidates
                .into_iter()
                .find(|intent| intent.session_id == thread && self.matches_intent(intent))
            else {
                return;
            };
            let now = unix_ms();
            let expected_revision = intent.revision;
            let generation = match intent.claim(owner, now, EXTRACTION_LEASE_MS) {
                Ok(generation) => generation,
                Err(MemoryExtractionError::LeaseHeld {
                    lease_expires_at_unix_ms,
                }) => {
                    tokio::time::sleep(Duration::from_millis(
                        lease_expires_at_unix_ms
                            .saturating_sub(now)
                            .saturating_add(1),
                    ))
                    .await;
                    continue;
                }
                Err(_) => return,
            };
            if repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .is_err()
            {
                continue;
            }

            let result = self
                .advance_claimed(&repository, &mut intent, generation)
                .await;
            if let Err((error, terminal)) = result {
                let Ok(Some(current)) = repository.get_extraction(&intent.intent_id).await else {
                    return;
                };
                if current.revision != intent.revision
                    || current.claim_owner.as_deref() != Some(owner)
                    || current.claim_generation != generation
                {
                    continue;
                }
                let now = unix_ms();
                let expected_revision = intent.revision;
                let transition = if terminal || intent.attempts >= MAX_ATTEMPTS {
                    intent.terminal_fail(owner, generation, now, error)
                } else {
                    intent.retry(owner, generation, now, error)
                };
                if transition.is_ok() {
                    let _ = repository
                        .compare_and_swap_extraction(expected_revision, intent.clone())
                        .await;
                }
                if !terminal && intent.attempts < MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(
                        25 * u64::from(intent.attempts.max(1)),
                    ))
                    .await;
                    continue;
                }
            }
        }
    }

    async fn advance_claimed(
        &self,
        repository: &Arc<dyn MemoryExtractionRepository>,
        intent: &mut MemoryExtractionIntent,
        generation: u64,
    ) -> Result<(), (String, bool)> {
        if self.validate_live_resource().is_err() {
            return Err((
                "MemoryStore is missing, suspended, archived, or deleted".into(),
                true,
            ));
        }
        if intent.status == MemoryExtractionStatus::Claimed {
            let extraction_input = intent.clone();
            let extraction = self.extract_mutations(&extraction_input);
            tokio::pin!(extraction);
            let mutations = loop {
                tokio::select! {
                    result = &mut extraction => break result.map_err(|error| (error, false))?,
                    () = tokio::time::sleep(Duration::from_millis(EXTRACTION_HEARTBEAT_MS)) => {
                        let expected_revision = intent.revision;
                        intent
                            .renew_claim(
                                &self.runtime.claim_owner,
                                generation,
                                unix_ms(),
                                EXTRACTION_LEASE_MS,
                            )
                            .map_err(|error| (error.to_string(), false))?;
                        repository
                            .compare_and_swap_extraction(expected_revision, intent.clone())
                            .await
                            .map_err(|error| (error.to_string(), false))?;
                    }
                }
            };
            let expected_revision = intent.revision;
            intent
                .mark_extracted(&self.runtime.claim_owner, generation, unix_ms(), mutations)
                .map_err(|error| (error.to_string(), false))?;
            repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Extracted {
            self.validate_live_resource()
                .map_err(|error| (error, true))?;
            let mut receipts = Vec::with_capacity(intent.mutations.len());
            for mutation in &intent.mutations {
                receipts.push(
                    self.platform
                        .apply_mutation(mutation)
                        .await
                        .map_err(|error| (error, false))?,
                );
            }
            let expected_revision = intent.revision;
            intent
                .mark_stored(
                    &self.runtime.claim_owner,
                    generation,
                    unix_ms(),
                    MemoryExtractionReceipt {
                        stored_at_unix_ms: unix_ms(),
                        mutations: receipts,
                    },
                )
                .map_err(|error| (error.to_string(), false))?;
            repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Stored {
            let expected_revision = intent.revision;
            intent
                .complete(&self.runtime.claim_owner, generation, unix_ms())
                .map_err(|error| (error.to_string(), false))?;
            repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        Ok(())
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
        let instructions = intent
            .extractor
            .instructions
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_MEMORY_INSTRUCTIONS);
        let catalog = AgentCatalog::new().with_agent(default_memory_agent(
            &intent.extractor.model_ref,
            instructions,
        ));
        let executor = self.runtime.materialize_extractor(&intent.extractor)?;
        let tool = erase(WriteMemoryTool::from_handle(capture.clone()));
        run_configured_agent(
            &catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(&self.runtime.provider),
            executor,
            &intent.extractor.agent_id,
            &format!("{}::mem", intent.session_id),
            seed,
            vec![tool],
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .map_err(|error| error.to_string())?;
        self.platform.plan_mutations(capture.take()).await
    }

    /// The memory store (shared with the recall plugin, which reads it at
    /// `BeforeInference`).
    pub fn store(&self) -> Arc<dyn MemoryStoreHandle> {
        self.store.clone()
    }

    /// The recall bounds (shared with the recall plugin).
    pub fn bounds(&self) -> RecallBounds {
        self.bounds.clone()
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl crate::host::SharedHost {
    /// The Memory content data-plane port used by an outer composition root to
    /// construct a worker-side mounter. It carries no principal or policy state.
    pub fn memory_repository(&self) -> Arc<dyn awaken_memory_store::MemoryRepository> {
        self.memory_stores.fs_handle()
    }

    /// Install the worker adapter behind the neutral provisioning port. This is
    /// intentionally separate from [`SharedHost::new`](crate::SharedHost::new):
    /// runtime-host must not depend on a FUSE/copy implementation crate.
    pub fn install_memory_mounter(
        &self,
        mounter: Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
        self.provider.install_memory_mounter(mounter.clone());
        *self
            .memory_mounter
            .write()
            .expect("memory mounter lock poisoned") = Some(mounter);
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
        self.thread_memory
            .lock()
            .expect("thread memory mutex poisoned")
            .insert(thread.to_string(), memory);
    }

    pub(crate) fn memory_for_thread(&self, thread: &str) -> Option<Arc<BoundMemory>> {
        self.thread_memory
            .lock()
            .expect("thread memory mutex poisoned")
            .get(thread)
            .cloned()
            .flatten()
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
    /// ACL/composition seam for embedders: ownership and authorization must have
    /// completed before calling it; the Runtime Host receives only the opaque store
    /// id, pinned config, and maximum access.
    pub fn bind_resolved_memory(
        &self,
        thread: &str,
        workspace_id: &str,
        config: &awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion,
        access: awaken_protocol_managed::resource_plane::ResourceAccess,
        resource_configs: Arc<dyn awaken_protocol_managed::resource_plane::ResourceConfigSource>,
    ) {
        let writable = access == awaken_protocol_managed::resource_plane::ResourceAccess::ReadWrite;
        let handle = self.platform_memory_handle(config.memory_store_id.clone(), writable);
        let bound = self
            .memory
            .bind(workspace_id, handle, resource_configs, config, writable);
        self.register_thread_memory(thread, Some(Arc::new(bound)));
    }

    /// Replace the extraction work repository assembled by the default local
    /// host. Cloud composition roots install the same Postgres repository used by
    /// their Session application plane; resource stores remain unaware of it.
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
    use async_trait::async_trait;
    use awaken_memory_store::MemoryRepository as _;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult, ToolCall,
    };

    /// A stub extractor model: first turn emits a write_memory call; once it sees
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

    fn user(text: &str) -> Message {
        Message {
            id: MessageId("u1".into()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    fn extractor(
        instructions: Option<&str>,
        extraction_prompt: Option<&str>,
    ) -> MemoryExtractorSnapshot {
        MemoryExtractorSnapshot {
            agent_id: MEMORY_AGENT_ID.into(),
            model_ref: "stub".into(),
            inference_access: awaken_runtime_contract::InferenceAccess::host_executor("stub"),
            instructions: instructions.map(str::to_string),
            extraction_prompt: extraction_prompt.map(str::to_string),
        }
    }

    fn bound_test_memory(
        llm: Arc<dyn LlmExecutor>,
        sandbox_base: &std::path::Path,
        _memory_root: &std::path::Path,
    ) -> (
        Arc<MemoryRuntime>,
        BoundMemory,
        Arc<awaken_memory_store::VolatileMemoryRepository>,
        Arc<awaken_session_store::InMemorySessionRepository>,
    ) {
        let catalog = Arc::new(
            AgentCatalog::new()
                .with_agent(default_memory_agent("stub", DEFAULT_MEMORY_INSTRUCTIONS)),
        );
        let extractions = Arc::new(awaken_session_store::InMemorySessionRepository::default());
        let runtime = Arc::new(MemoryRuntime::new(
            llm,
            Arc::new(LocalProvider::new(sandbox_base)),
            catalog,
            Arc::new(BackgroundRuns::new()),
            extractions.clone(),
        ));
        let config = awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion {
            memory_store_id: "test-store".into(),
            version: awaken_protocol_managed::resource_plane::ConfigVersion::INITIAL,
            recall_policy: Default::default(),
            extraction_policy: Default::default(),
            retention_policy: Default::default(),
        };
        let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
        let platform = Arc::new(PlatformMemoryHandle::new(
            repository.clone(),
            "test-store".into(),
            true,
        ));
        let bound = runtime.bind(
            "ws-test",
            platform,
            Arc::new(TestResourceConfigs),
            &config,
            true,
        );
        (runtime, bound, repository, extractions)
    }

    struct TestResourceConfigs;

    impl awaken_protocol_managed::resource_plane::ResourceConfigSource for TestResourceConfigs {
        fn resolve_memory_store(
            &self,
            workspace_id: &str,
            id: &str,
        ) -> Result<
            awaken_protocol_managed::resource_plane::ResolvedMemoryStoreConfig,
            awaken_protocol_managed::resource_plane::ResourceCatalogError,
        > {
            use awaken_protocol_managed::resource_plane::{
                ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
                ResolvedMemoryStoreConfig, ResourceState,
            };
            Ok(ResolvedMemoryStoreConfig {
                definition: MemoryStoreDefinition {
                    id: id.into(),
                    workspace_id: workspace_id.into(),
                    name: id.into(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                },
                config: MemoryStoreConfigVersion {
                    memory_store_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    recall_policy: Default::default(),
                    extraction_policy: Default::default(),
                    retention_policy: Default::default(),
                },
            })
        }

        fn resolve_repository(
            &self,
            _workspace_id: &str,
            id: &str,
        ) -> Result<
            awaken_protocol_managed::resource_plane::ResolvedRepositoryConfig,
            awaken_protocol_managed::resource_plane::ResourceCatalogError,
        > {
            Err(awaken_protocol_managed::resource_plane::ResourceCatalogError::NotFound(id.into()))
        }
    }

    /// A model that replies with fixed selection indices, standing in for the
    /// `memory-selector` sub-agent.
    struct IndexModel;

    #[async_trait]
    impl LlmExecutor for IndexModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("relevant: [1], [2]"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn agent_selector_runs_the_subagent_and_parses_its_reply() {
        let selector = AgentSelector::new(Arc::new(IndexModel), "stub");
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

        let (runtime, extraction, repository, _extractions) =
            bound_test_memory(Arc::new(ExtractorModel), &sandbox_base, &mem_root);

        extraction
            .trigger(
                "thread-1",
                "terminal-1",
                vec![user("I really like rust")],
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
        let block = awaken_ext_memory::recall::render(&entries, &extraction.bounds())
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
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sandbox_base = std::env::temp_dir().join(format!("awaken-mem2-sbx-{stamp}"));
        let mem_root = std::env::temp_dir().join(format!("awaken-mem2-root-{stamp}"));
        let (runtime, extraction, repository, _extractions) =
            bound_test_memory(Arc::new(SeedEchoModel), &sandbox_base, &mem_root);

        // A committed history with a recalled-memory system message + a real turn.
        let recall = Message::text(
            MessageId(format!("{RECALL_MSG_PREFIX}1")),
            Role::System,
            "RECALLED SECRET",
        );
        extraction
            .trigger(
                "t",
                "terminal-1",
                vec![recall, user("please note this")],
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
        assert!(seen.contains("please note this"), "real turn seen: {seen}");
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
        let (runtime, extraction, repository, _extractions) =
            bound_test_memory(Arc::new(SeedEchoModel), &sandbox_base, &mem_root);

        extraction
            .trigger(
                "t-custom",
                "terminal-1",
                vec![user("remember this")],
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
        let (runtime, extraction, repository, extractions) =
            bound_test_memory(Arc::new(ExtractorModel), &sandbox_base, &mem_root);

        for _ in 0..2 {
            extraction
                .trigger(
                    "thread-redelivery",
                    "terminal-7",
                    vec![user("I really like rust")],
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
        let (runtime, extraction, repository, extractions) =
            bound_test_memory(Arc::new(ExtractorModel), &sandbox_base, &mem_root);
        let content = "customer maintenance is Sunday";
        let target = awaken_memory_store::sha256_hex(content);
        let mut intent = MemoryExtractionIntent::new(
            "memory-extraction:thread-crash:terminal-8",
            "thread-crash:terminal-8",
            "ws-test",
            "thread-crash",
            "terminal-8",
            "test-store",
            1,
            vec![user("remember the maintenance window")],
            MemoryExtractorSnapshot {
                agent_id: MEMORY_AGENT_ID.into(),
                model_ref: "stub".into(),
                inference_access: awaken_runtime_contract::InferenceAccess::host_executor("stub"),
                instructions: None,
                extraction_prompt: None,
            },
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

        extraction.reconcile("thread-crash").await;
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
}
