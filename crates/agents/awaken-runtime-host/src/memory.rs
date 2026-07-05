//! Out-of-band memory extraction: the composition-root wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! [`run_configured_subrun`], fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a turn, and reads memories back for recall.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    DEFAULT_SELECTOR_INSTRUCTIONS, EXTRACT_PROMPT, MEMORY_AGENT_ID, MemoryStore, RecallBounds,
    RecallSelector, SELECTOR_AGENT_ID, WriteMemoryTool, default_selector_agent, parse_indices,
    select_input,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::subagent::run_configured_subrun;

// The config pieces the host wires (registering the default extractor agent).
pub use awaken_ext_memory::{DEFAULT_MEMORY_INSTRUCTIONS, default_memory_agent};

/// A [`RecallSelector`] backed by the `memory-selector` sub-agent: a single-step,
/// tool-free, plugin-free agent run through the shared aux-agent substrate. Because
/// it activates no plugins, it cannot recurse into memory recall.
pub(crate) struct AgentSelector {
    llm: Arc<dyn LlmExecutor>,
    provider: Arc<LocalSandboxProvider>,
    catalog: Arc<AgentCatalog>,
    seq: AtomicU64,
}

impl AgentSelector {
    pub(crate) fn new(llm: Arc<dyn LlmExecutor>, model_ref: &str) -> Self {
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_selector_agent(
            model_ref,
            DEFAULT_SELECTOR_INSTRUCTIONS,
        )));
        let base = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!("{}-mem-select", std::process::id()));
        Self {
            llm,
            provider: Arc::new(LocalSandboxProvider::new(base)),
            catalog,
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl RecallSelector for AgentSelector {
    async fn select(&self, query: &str, manifest: &[(usize, String)], max: usize) -> Vec<usize> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let input = select_input(query, manifest, max);
        let reply = run_configured_subrun(
            &self.catalog,
            &self.provider,
            self.llm.clone(),
            SELECTOR_AGENT_ID,
            &format!("mem-select-{n}"),
            input,
            Vec::new(),
            None,
        )
        .await
        .unwrap_or_default();
        parse_indices(&reply, manifest.len(), max)
    }
}

/// Message-id prefix for injected recall blocks. Shared so the host stamps it and
/// extraction filters it (recall is context, not a conversation fact).
pub const RECALL_MSG_PREFIX: &str = "mem-recall-";

/// Triggers out-of-band memory extraction after a main turn, and reads memories
/// back for recall. Owns no memory logic itself — it delegates to
/// `awaken-ext-memory` and only orchestrates the sub-run.
pub struct MemoryExtraction {
    llm: Arc<dyn LlmExecutor>,
    provider: Arc<LocalSandboxProvider>,
    catalog: Arc<AgentCatalog>,
    background: Arc<BackgroundRuns>,
    store: MemoryStore,
    bounds: RecallBounds,
}

impl MemoryExtraction {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<LocalSandboxProvider>,
        catalog: Arc<AgentCatalog>,
        background: Arc<BackgroundRuns>,
        root: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            llm,
            provider,
            catalog,
            background,
            store: MemoryStore::new(root),
            bounds: RecallBounds::default(),
        }
    }

    /// Fire-and-forget: seed the extractor with `committed` (the finished turn's
    /// history) and let it save memories via `write_memory`, scoped to the store.
    /// Returns immediately; the run is tracked for [`drain`](Self::drain).
    pub async fn trigger(&self, thread: &str, committed: Vec<Message>) {
        if self.catalog.resolve(MEMORY_AGENT_ID).is_none() {
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
            content: vec![ContentBlock::text(EXTRACT_PROMPT)],
        });

        let llm = self.llm.clone();
        let provider = self.provider.clone();
        let catalog = self.catalog.clone();
        let store = self.store.clone();
        let mem_thread = format!("{thread}::mem");

        self.background
            .spawn(async move {
                let tool = erase(WriteMemoryTool::new(store));
                let _ = run_configured_subrun(
                    &catalog,
                    &provider,
                    llm,
                    MEMORY_AGENT_ID,
                    &mem_thread,
                    seed,
                    vec![tool],
                    None,
                )
                .await;
            })
            .await;
    }

    /// The memory store (shared with the recall plugin, which reads it at
    /// `BeforeInference`).
    pub fn store(&self) -> MemoryStore {
        self.store.clone()
    }

    /// The recall bounds (shared with the recall plugin).
    pub fn bounds(&self) -> RecallBounds {
        self.bounds.clone()
    }

    /// Await in-flight extractions up to `timeout` (shutdown flush).
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, ChatRole, Result as LlmResult, ToolCall,
    };

    /// A stub extractor model: first turn emits a write_memory call; once it sees
    /// the tool result, it replies done.
    struct ExtractorModel;

    #[async_trait]
    impl LlmExecutor for ExtractorModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let saw_tool_result = request.messages.iter().any(|m| m.role == ChatRole::Tool);
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

        let catalog = Arc::new(
            AgentCatalog::new()
                .with_agent(default_memory_agent("stub", DEFAULT_MEMORY_INSTRUCTIONS)),
        );
        let extraction = MemoryExtraction::new(
            Arc::new(ExtractorModel),
            Arc::new(LocalSandboxProvider::new(&sandbox_base)),
            catalog,
            Arc::new(BackgroundRuns::new()),
            &mem_root,
        );

        extraction
            .trigger("thread-1", vec![user("I really like rust")])
            .await;
        assert!(extraction.drain(Duration::from_secs(10)).await);

        assert_eq!(
            std::fs::read_to_string(mem_root.join("user-prefs.md")).expect("memory file"),
            "user likes rust"
        );
        // The read side surfaces it through bounded recall.
        let block = awaken_ext_memory::recall_block(&extraction.store(), &extraction.bounds())
            .expect("recall block");
        assert!(block.contains("user likes rust"), "got: {block}");
    }

    /// An extractor that saves whatever non-prompt text it was seeded with, so the
    /// test can see what reached it.
    struct SeedEchoModel;

    #[async_trait]
    impl LlmExecutor for SeedEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let saved = request.messages.iter().any(|m| {
                m.role == ChatRole::Tool
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
                .filter(|m| m.role == ChatRole::User || m.role == ChatRole::System)
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
        let catalog = Arc::new(
            AgentCatalog::new()
                .with_agent(default_memory_agent("stub", DEFAULT_MEMORY_INSTRUCTIONS)),
        );
        let extraction = MemoryExtraction::new(
            Arc::new(SeedEchoModel),
            Arc::new(LocalSandboxProvider::new(&sandbox_base)),
            catalog,
            Arc::new(BackgroundRuns::new()),
            &mem_root,
        );

        // A committed history with a recalled-memory system message + a real turn.
        let recall = Message::text(
            MessageId(format!("{RECALL_MSG_PREFIX}1")),
            Role::System,
            "RECALLED SECRET",
        );
        extraction
            .trigger("t", vec![recall, user("please note this")])
            .await;
        assert!(extraction.drain(Duration::from_secs(10)).await);

        let seen = std::fs::read_to_string(mem_root.join("seen.md")).expect("seen file");
        assert!(seen.contains("please note this"), "real turn seen: {seen}");
        assert!(
            !seen.contains("RECALLED SECRET"),
            "recalled content must not reach the extractor: {seen}"
        );
    }
}
