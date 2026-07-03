//! Out-of-band memory extraction: the composition-root wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! [`run_configured_subrun`], fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a turn, and reads memories back for recall.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    EXTRACT_PROMPT, MEMORY_AGENT_ID, MemoryStore, RecallBounds, WriteMemoryTool, recall_block,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::subagent::run_configured_subrun;

// The config pieces the host wires (registering the default extractor agent).
pub use awaken_ext_memory::{DEFAULT_MEMORY_INSTRUCTIONS, default_memory_agent};

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
        let mut seed = committed;
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

    /// Load saved memories as one bounded recall block for injection into a new
    /// conversation, or `None` when nothing is saved. Bounding (per-entry cap,
    /// total cap, newest-first) lives in `awaken-ext-memory`.
    pub fn recall_block(&self) -> Option<String> {
        recall_block(&self.store, &self.bounds)
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
        let block = extraction.recall_block().expect("recall block");
        assert!(block.contains("user likes rust"), "got: {block}");
    }
}
