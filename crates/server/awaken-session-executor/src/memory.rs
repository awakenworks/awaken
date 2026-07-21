//! Out-of-band memory extraction: the composition-root wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! the shared Agent Run substrate, fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a turn, and reads memories back for recall.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
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
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::agent_runner::run_configured_agent;
use crate::background::BackgroundRuns;
use crate::judge::HostAgentTool;

// The config pieces the host wires (registering the default extractor agent).
pub use awaken_ext_memory::{DEFAULT_MEMORY_INSTRUCTIONS, default_memory_agent};

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
    fs: Arc<dyn awaken_memory_store::MemoryFs>,
    store_id: String,
    writable: bool,
}

impl PlatformMemoryHandle {
    pub(crate) fn new(
        fs: Arc<dyn awaken_memory_store::MemoryFs>,
        store_id: String,
        writable: bool,
    ) -> Self {
        Self {
            fs,
            store_id,
            writable,
        }
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
    provider: Arc<LocalProvider>,
    catalog: Arc<AgentCatalog>,
    background: Arc<BackgroundRuns>,
    model_ref: String,
}

/// One Session-scoped MemoryStore binding shared by recall and extraction.
pub struct BoundMemory {
    runtime: Arc<MemoryRuntime>,
    store: Arc<dyn MemoryStoreHandle>,
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
        model_ref: impl Into<String>,
    ) -> Self {
        Self {
            llm,
            provider,
            catalog,
            background,
            model_ref: model_ref.into(),
        }
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        store: Arc<dyn MemoryStoreHandle>,
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
            store,
            bounds,
            recall_enabled: config.recall_policy.enabled,
            extraction_enabled: writable && config.extraction_policy.enabled,
        }
    }

    /// Await every in-flight extraction started by any bound Session.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }
}

impl BoundMemory {
    pub(crate) fn recall_enabled(&self) -> bool {
        self.recall_enabled
    }

    pub(crate) fn extraction_enabled(&self) -> bool {
        self.extraction_enabled
    }

    /// Fire-and-forget: seed the extractor with `committed` (the finished turn's
    /// history) and let it save memories via `write_memory`, scoped to the store.
    /// Returns immediately; the run is tracked by the host [`MemoryRuntime`].
    pub async fn trigger(
        &self,
        thread: &str,
        committed: Vec<Message>,
        instructions: Option<&str>,
        extraction_prompt: Option<&str>,
    ) {
        if self.runtime.catalog.resolve(MEMORY_AGENT_ID).is_none() {
            return;
        }
        // Drop recalled-memory messages from the seed: they are injected context,
        // not new conversation facts. Without this the extractor re-saves what it
        // just recalled (a cross-thread self-copy loop).
        let mut seed: Vec<Message> = committed
            .into_iter()
            .filter(|m| !m.id.0.starts_with(RECALL_MSG_PREFIX))
            .collect();
        seed.push(Message {
            id: MessageId(format!("{thread}-mem-prompt")),
            role: Role::User,
            content: vec![ContentBlock::text(
                extraction_prompt
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(EXTRACT_PROMPT),
            )],
        });

        let llm = self.runtime.llm.clone();
        let provider = self.runtime.provider.clone();
        let catalog = instructions
            .filter(|value| !value.trim().is_empty())
            .map(|instructions| {
                Arc::new(
                    AgentCatalog::new()
                        .with_agent(default_memory_agent(&self.runtime.model_ref, instructions)),
                )
            })
            .unwrap_or_else(|| self.runtime.catalog.clone());
        let store = self.store.clone();
        let mem_thread = format!("{thread}::mem");

        self.runtime
            .background
            .spawn(async move {
                let tool = erase(WriteMemoryTool::from_handle(store));
                // The extractor injects a per-run `write_memory` tool scoped to this
                // store, which the ordinary Agent tool deliberately does not
                // carry — so it runs on the substrate directly. Fire-and-forget, and
                // its usage stays isolated (background housekeeping, not turn work).
                let _ = run_configured_agent(
                    &catalog,
                    crate::agent_runner::AgentRunSandbox::Fresh(&provider),
                    llm,
                    MEMORY_AGENT_ID,
                    &mem_thread,
                    seed,
                    vec![tool],
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            })
            .await;
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

impl crate::host::SharedHost {
    /// The Memory content data-plane port used by an outer composition root to
    /// construct a worker-side mounter. It carries no principal or policy state.
    pub fn memory_fs(&self) -> Arc<dyn awaken_memory_store::MemoryFs> {
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
    ) -> Arc<dyn MemoryStoreHandle> {
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
        config: &awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion,
        access: awaken_protocol_managed::resource_plane::ResourceAccess,
    ) {
        let writable = access == awaken_protocol_managed::resource_plane::ResourceAccess::ReadWrite;
        let handle = self.platform_memory_handle(config.memory_store_id.clone(), writable);
        let bound = self.memory.bind(handle, config, writable);
        self.register_thread_memory(thread, Some(Arc::new(bound)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_ext_memory::MemoryDir;
    use awaken_memory_store::MemoryFs as _;
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

    fn bound_test_memory(
        llm: Arc<dyn LlmExecutor>,
        sandbox_base: &std::path::Path,
        memory_root: &std::path::Path,
    ) -> (Arc<MemoryRuntime>, BoundMemory) {
        let catalog = Arc::new(
            AgentCatalog::new()
                .with_agent(default_memory_agent("stub", DEFAULT_MEMORY_INSTRUCTIONS)),
        );
        let runtime = Arc::new(MemoryRuntime::new(
            llm,
            Arc::new(LocalProvider::new(sandbox_base)),
            catalog,
            Arc::new(BackgroundRuns::new()),
            "stub",
        ));
        let config = awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion {
            memory_store_id: "test-store".into(),
            version: awaken_protocol_managed::resource_plane::ConfigVersion::INITIAL,
            recall_policy: Default::default(),
            extraction_policy: Default::default(),
            retention_policy: Default::default(),
        };
        let bound = runtime.bind(Arc::new(MemoryDir::new(memory_root)), &config, true);
        (runtime, bound)
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

        let (runtime, extraction) =
            bound_test_memory(Arc::new(ExtractorModel), &sandbox_base, &mem_root);

        extraction
            .trigger("thread-1", vec![user("I really like rust")], None, None)
            .await;
        assert!(runtime.drain(Duration::from_secs(10)).await);

        assert_eq!(
            std::fs::read_to_string(mem_root.join("user-prefs.md")).expect("memory file"),
            "user likes rust"
        );
        // The read side surfaces it through bounded recall.
        let entries = extraction.store().entries().await.unwrap();
        let block = awaken_ext_memory::recall::render(&entries, &extraction.bounds())
            .expect("recall block");
        assert!(block.contains("user likes rust"), "got: {block}");
    }

    #[tokio::test]
    async fn platform_handle_enforces_read_only_at_the_data_plane_boundary() {
        let fs = Arc::new(awaken_memory_store::InMemoryFs::new());
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
        let (runtime, extraction) =
            bound_test_memory(Arc::new(SeedEchoModel), &sandbox_base, &mem_root);

        // A committed history with a recalled-memory system message + a real turn.
        let recall = Message::text(
            MessageId(format!("{RECALL_MSG_PREFIX}1")),
            Role::System,
            "RECALLED SECRET",
        );
        extraction
            .trigger("t", vec![recall, user("please note this")], None, None)
            .await;
        assert!(runtime.drain(Duration::from_secs(10)).await);

        let seen = std::fs::read_to_string(mem_root.join("seen.md")).expect("seen file");
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
        let (runtime, extraction) =
            bound_test_memory(Arc::new(SeedEchoModel), &sandbox_base, &mem_root);

        extraction
            .trigger(
                "t-custom",
                vec![user("remember this")],
                Some("CUSTOM MEMORY SYSTEM"),
                Some("CUSTOM EXTRACTION TASK"),
            )
            .await;
        assert!(runtime.drain(Duration::from_secs(10)).await);

        let seen = std::fs::read_to_string(mem_root.join("seen.md")).expect("seen file");
        assert!(
            seen.contains("CUSTOM MEMORY SYSTEM"),
            "custom instructions reached extractor: {seen}"
        );
        assert!(
            seen.contains("CUSTOM EXTRACTION TASK"),
            "custom task reached extractor: {seen}"
        );
    }
}
