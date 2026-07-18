//! A small coding agent assembled from the runtime's own pieces.
//!
//! This is the *core*, with no TUI and no real model — light enough for the
//! offline smoke test. It wires:
//!
//! - the built-in **hand tools** (read/write/edit/glob/grep/bash) as both the
//!   model-visible [`ToolDescriptor`]s the [`RunnableConfig`] carries and the
//!   executable [`RawTool`]s the runtime registers (ids match by construction);
//! - a Claude-Code-style **permission policy** (ADR-0030): read/glob/grep are
//!   allowed, write/edit/bash are asked, so the caller approves mutations;
//! - a caller-owned **turn loop** ([`CodingSession::turn`]) that drives one run
//!   and resumes it through each permission decision.
//!
//! The interactive TUI ([`crate::coding_agent::tui`]) and the real model live
//! behind the `coding-agent-tui` feature; this module needs neither.

#[cfg(feature = "coding-agent-tui")]
pub mod model;
#[cfg(feature = "coding-agent-tui")]
pub mod tui;

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{Toolset, builtin_tools, executable_hand_tools};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::execution::Error;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

pub mod scripted;
pub use scripted::ScriptedCoder;

/// The hand tools a coding agent uses (web tools are excluded).
pub const CODING_TOOLS: &[&str] = &["read", "write", "edit", "glob", "grep", "bash"];

const SYSTEM_PROMPT: &str = "\
You are a coding assistant working in a local repository. Use the tools to read, \
search, and edit files. Read a file before editing it, make minimal exact edits, \
and explain what you changed. Prefer `edit` over `write` for existing files.";

/// The model-visible descriptors for the coding tools, in `CODING_TOOLS` order.
pub fn coding_tool_descriptors() -> Vec<ToolDescriptor> {
    builtin_tools()
        .into_iter()
        .filter(|t| t.toolset == Toolset::Hand && CODING_TOOLS.contains(&t.descriptor.id.as_str()))
        .map(|t| t.descriptor)
        .collect()
}

/// A coding agent's runnable config for `model_ref` (the model the binding selects).
pub fn coding_config(model_ref: &str) -> RunnableConfig {
    RunnableConfig::builder("coder")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .tools(coding_tool_descriptors())
        .max_steps(40)
        .build()
}

/// The permission policy: read/glob/grep allowed, everything else (write, edit,
/// bash) asked, so a mutation awaits for the caller's approval (ADR-0030).
pub fn coding_policy() -> RulePermissionPolicy {
    let allow = |name: &str| {
        PermissionRule::new(
            ToolCallPattern::parse(name).expect("static pattern"),
            ToolPermissionBehavior::Allow,
        )
    };
    RulePermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
        mode: Mode::Default,
        rules: vec![allow("read"), allow("glob"), allow("grep")],
    })
}

/// Assemble a runtime: the model port, the permission gate, and the executable
/// hand tools. The tool ids match [`coding_tool_descriptors`].
pub fn build_runtime(llm: Arc<dyn LlmExecutor>) -> Runtime {
    let gate = PermissionGate::new(Arc::new(coding_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in executable_hand_tools() {
        runtime = runtime.with_tool(tool);
    }
    runtime
}

/// What the caller decides when the agent asks to run a mutating tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
}

/// One coding conversation on a single thread. Turns share the commit coordinator
/// (which is also the history reader), so each fresh run continues the
/// conversation. This holds only the session's state — the runtime owns the
/// install/register, id generation, and the await→resume loop.
pub struct CodingSession {
    runtime: Runtime,
    config: RunnableConfig,
    commit: Arc<MemoryCommitCoordinator>,
    thread_id: ThreadId,
}

impl CodingSession {
    pub fn new(runtime: Runtime, config: RunnableConfig) -> Self {
        Self {
            runtime,
            config,
            commit: Arc::new(MemoryCommitCoordinator::new()),
            thread_id: ThreadId("coding".to_string()),
        }
    }

    /// The committed conversation so far (user, assistant, and tool messages).
    pub fn transcript(&self) -> Vec<Message> {
        self.commit.committed_messages(&self.thread_id)
    }

    fn context(&self) -> RuntimeRunContext {
        RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
    }

    /// Run one user turn to a terminal phase, asking `approve` before each mutating
    /// tool. Returns the messages committed this turn.
    pub async fn turn<F>(&self, input: &str, mut approve: F) -> Result<Vec<Message>, Error>
    where
        F: FnMut(&ResumeTicket) -> Approval,
    {
        let before = self.commit.committed_messages(&self.thread_id).len();
        self.runtime
            .run_to_completion(
                &self.config,
                self.thread_id.0.as_str(),
                input,
                self.context(),
                |ticket| match approve(ticket) {
                    Approval::Allow => ResumeResult::allow(),
                    Approval::Deny => ResumeResult::deny(None),
                },
            )
            .await?;
        Ok(self.commit.committed_messages(&self.thread_id)[before..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_exactly_the_coding_tools_and_carry_no_web_tools() {
        let ids: Vec<String> = coding_tool_descriptors()
            .iter()
            .map(|d| d.id.as_str().to_string())
            .collect();
        // Exactly the CODING_TOOLS set — read/write/edit/glob/grep/bash, no web tools.
        assert_eq!(ids.len(), CODING_TOOLS.len());
        for id in &ids {
            assert!(
                CODING_TOOLS.contains(&id.as_str()),
                "{id} is not a coding tool"
            );
        }
        // The mutating tools a coding agent needs are present.
        for expected in ["read", "edit", "bash"] {
            assert!(ids.iter().any(|i| i == expected), "missing {expected}");
        }
        assert!(
            !ids.iter().any(|i| i == "web_fetch" || i == "web_search"),
            "web tools must be excluded"
        );
    }

    #[test]
    fn coding_config_carries_the_model_ref_tools_and_step_ceiling() {
        let spec = coding_config("some-model").snapshot().resolved_spec.clone();
        // The binding selects the passed model ref.
        assert_eq!(spec.model_binding.model_ref, "some-model");
        // The config carries the coding descriptors (ids match the executable tools).
        let tool_ids: Vec<&str> = spec
            .tool_descriptors
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(tool_ids.len(), CODING_TOOLS.len());
        // A sensible step ceiling and non-empty coding instructions.
        assert_eq!(spec.max_steps, 40);
        assert!(spec.instructions.contains("coding"));
    }

    #[tokio::test]
    async fn the_policy_allows_reads_and_asks_before_mutations() {
        use awaken_runtime_contract::permission::{
            PermissionContext, PermissionDecision, PermissionPolicy,
        };
        let policy = coding_policy();
        let decide = |tool: &str| {
            let policy = &policy;
            let tool = tool.to_string();
            async move {
                policy
                    .decide(&PermissionContext {
                        tool_id: tool,
                        call_id: "c1".to_string(),
                        arguments: serde_json::json!({}),
                    })
                    .await
            }
        };
        // Read-only tools are pre-allowed (no prompt).
        for ro in ["read", "glob", "grep"] {
            assert!(
                matches!(decide(ro).await, PermissionDecision::Allow),
                "{ro} should be allowed"
            );
        }
        // Mutating / unlisted tools fall through to the default: Ask.
        for mutate in ["write", "edit", "bash"] {
            assert!(
                matches!(decide(mutate).await, PermissionDecision::Ask { .. }),
                "{mutate} should await for approval"
            );
        }
    }
}
