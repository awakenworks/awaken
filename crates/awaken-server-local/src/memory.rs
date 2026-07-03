//! Out-of-band memory extraction: an ordinary sub-agent that, after a main turn
//! finishes, reads the conversation and writes durable memories.
//!
//! It is "just an agent" (a `memory-extractor` entry in the [`AgentCatalog`]) run
//! through [`run_configured_subrun`] — no bespoke mechanism. Two things make it
//! out-of-band rather than a delegation: it is triggered by the host after a turn
//! (not by the model calling a tool), and it runs fire-and-forget through
//! [`BackgroundRuns`] so it never blocks the turn, yet is drained before shutdown.
//!
//! Persistence is the one wrinkle a sub-run needs help with: its sandbox is
//! ephemeral, so memories written there would vanish. [`WriteMemoryTool`] is a
//! scoped write whose root is a stable directory *outside* the sandbox, so the
//! extractor's writes survive.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::erase;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::tool::{Tool, ToolError};
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::subagent::run_configured_subrun;

/// The agent id under which the memory extractor is registered.
pub const MEMORY_AGENT_ID: &str = "memory-extractor";

/// A scoped write tool: it writes `content` to `<root>/<name>.md`, where `root` is
/// a stable directory the caller fixes at construction. `name` is sanitized to a
/// single file component, so the extractor cannot escape the memory directory.
pub struct WriteMemoryTool {
    root: PathBuf,
}

impl WriteMemoryTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Reduce `name` to a safe single file stem: keep alphanumerics, `-` and `_`;
/// map everything else to `-`; never empty.
fn sanitize_stem(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memory".to_string()
    } else {
        trimmed
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    // serde_json::Value rather than a derived struct: server-local is not permitted
    // a direct `serde` dependency, so arguments are read from the JSON value.
    type Args = serde_json::Value;
    type Output = String;
    fn id(&self) -> &str {
        "write_memory"
    }
    async fn call(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("write_memory needs a `name`".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("write_memory needs `content`".into()))?;
        std::fs::create_dir_all(&self.root)
            .map_err(|e| ToolError::Execution(format!("create memory dir: {e}")))?;
        let path = self.root.join(format!("{}.md", sanitize_stem(name)));
        std::fs::write(&path, content)
            .map_err(|e| ToolError::Execution(format!("write memory {}: {e}", path.display())))?;
        Ok(format!("saved memory {}", path.display()))
    }
}

/// The model-visible descriptor for [`WriteMemoryTool`].
pub fn write_memory_descriptor() -> ToolDescriptor {
    ToolDescriptor::pinned(
        "server:memory",
        "write_memory",
        "Save one durable memory. `name` is a short slug; `content` is the memory text. \
         Call once per distinct memory.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "short slug naming the memory" },
                "content": { "type": "string", "description": "the memory text" }
            },
            "required": ["name", "content"]
        }),
    )
}

/// A default `memory-extractor` agent config: it advertises only `write_memory`
/// and carries extraction instructions. A host may override this by registering a
/// different config under [`MEMORY_AGENT_ID`].
pub fn default_memory_agent(model_ref: &str, instructions: &str) -> RunnableConfig {
    RunnableConfig::builder(MEMORY_AGENT_ID)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(6)
        .tools([write_memory_descriptor()])
        .build()
}

/// The default extraction instructions. Adapted from Claude Code's memory
/// taxonomy (four types + a what-NOT-to-save gate), mapped onto the single-file
/// `write_memory` tool. A host may override this by registering its own
/// `memory-extractor` config.
pub const DEFAULT_MEMORY_INSTRUCTIONS: &str = "\
You are the memory extraction sub-agent. Analyze the conversation you are given \
and update a persistent memory so future conversations understand who the user \
is, how they want you to work, and the context behind their tasks.\n\n\
## Types of memory to save\n\
- user: the user's role, goals, responsibilities, preferences, and knowledge — \
so you can tailor future behavior to them specifically.\n\
- feedback: guidance on how to approach work — corrections (\"no, not that\", \
\"stop doing X\") AND confirmations (\"yes, exactly\"). Lead with the rule, then a \
Why: line (the reason given) and a How to apply: line (when it kicks in).\n\
- project: ongoing work, goals, decisions, or incidents not derivable from the \
code or git history. Convert relative dates to absolute (e.g. \"Thursday\" -> a \
real date). Lead with the fact, then Why: and How to apply: lines.\n\
- reference: pointers to where information lives in external systems (a Linear \
project, a Slack channel, a dashboard) and their purpose.\n\n\
## What NOT to save\n\
- Code patterns, conventions, architecture, file paths, project structure — \
derivable by reading the project.\n\
- Git history or who-changed-what — git log/blame are authoritative.\n\
- Debugging solutions or fix recipes — the fix is in the code.\n\
- Ephemeral task state, current-conversation context, or anything trivial or \
easily re-derived.\n\n\
## How to save\n\
Save each memory with the write_memory tool: a short kebab-case slug name and \
the memory text. Prefer one memory per distinct fact. Be specific — the text is \
what a future conversation reads. When done, reply with a one-line summary of \
what you saved (or that nothing was worth saving).";

/// Triggers out-of-band memory extraction after a main turn.
pub struct MemoryExtraction {
    llm: Arc<dyn LlmExecutor>,
    provider: Arc<LocalSandboxProvider>,
    catalog: Arc<AgentCatalog>,
    background: Arc<BackgroundRuns>,
    root: PathBuf,
}

impl MemoryExtraction {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<LocalSandboxProvider>,
        catalog: Arc<AgentCatalog>,
        background: Arc<BackgroundRuns>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            llm,
            provider,
            catalog,
            background,
            root: root.into(),
        }
    }

    /// Fire-and-forget: seed the extractor with `committed` (the finished turn's
    /// history) and let it save memories via `write_memory`, scoped to `root`.
    /// Returns immediately; the run is tracked for [`drain`](Self::drain).
    pub async fn trigger(&self, thread: &str, committed: Vec<Message>) {
        if self.catalog.resolve(MEMORY_AGENT_ID).is_none() {
            return;
        }
        let mut seed = committed;
        seed.push(Message {
            id: MessageId(format!("{thread}-mem-prompt")),
            role: Role::User,
            content: vec![ContentBlock::text(
                "Extract durable memories from the conversation above and save each via write_memory.",
            )],
        });

        let llm = self.llm.clone();
        let provider = self.provider.clone();
        let catalog = self.catalog.clone();
        let root = self.root.clone();
        let mem_thread = format!("{thread}::mem");

        self.background
            .spawn(async move {
                let tool = erase(WriteMemoryTool::new(root));
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

    /// Await in-flight extractions up to `timeout` (shutdown flush).
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn extraction_writes_a_memory_file_to_the_scoped_root() {
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
        let drained = extraction.drain(Duration::from_secs(10)).await;
        assert!(drained, "extraction should finish within the timeout");

        let saved = std::fs::read_to_string(mem_root.join("user-prefs.md")).expect("memory file");
        assert_eq!(saved, "user likes rust");
    }

    #[test]
    fn sanitize_stem_is_safe() {
        assert_eq!(sanitize_stem("user prefs"), "user-prefs");
        assert_eq!(sanitize_stem("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_stem("   "), "memory");
        assert_eq!(sanitize_stem(""), "memory");
    }
}
