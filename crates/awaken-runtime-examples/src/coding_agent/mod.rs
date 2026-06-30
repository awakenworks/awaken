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
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{Toolset, builtin_tools, executable_hand_tools};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::catalog::RuntimeCatalogInstaller;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
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
/// bash) asked, so a mutation parks for the caller's approval (ADR-0030).
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
/// conversation.
pub struct CodingSession {
    runtime: Runtime,
    config: RunnableConfig,
    commit: Arc<MemoryCommitCoordinator>,
    thread_id: ThreadId,
    seq: AtomicU64,
}

impl CodingSession {
    pub fn new(runtime: Runtime, config: RunnableConfig) -> Self {
        // Install the config's catalog once; turns resolve the snapshot against it.
        // Register the snapshot too, so a resumed run can resolve it by id.
        runtime
            .install_catalog(config.install().clone())
            .expect("the coding config's catalog installs");
        runtime.register_snapshot(config.snapshot().clone());
        Self {
            runtime,
            config,
            commit: Arc::new(MemoryCommitCoordinator::new()),
            thread_id: ThreadId("coding".to_string()),
            seq: AtomicU64::new(1),
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

    /// Run one user turn to a terminal phase, asking `approve` for each mutation
    /// the agent wants to perform. Returns the messages committed this turn.
    pub async fn turn<F>(&self, input: &str, mut approve: F) -> Result<Vec<Message>, Error>
    where
        F: FnMut(&WaitingTicket) -> Approval,
    {
        let run_id = RunId(format!("run-{}", self.seq.fetch_add(1, Ordering::Relaxed)));
        let before = self.commit.committed_messages(&self.thread_id).len();

        let activation = RunActivation {
            run_id: run_id.clone(),
            thread_id: self.thread_id.clone(),
            snapshot: self.config.snapshot().clone(),
            input: vec![Message {
                id: MessageId(format!("{}-in", run_id.0)),
                role: Role::User,
                content: vec![ContentBlock::text(input)],
            }],
            trace: Default::default(),
        };

        let mut phase = self.runtime.execute(activation, self.context()).await?;

        // A mutating tool parks the run on a permission ticket; resume with the
        // caller's decision until the run reaches a terminal phase.
        while phase == Phase::Waiting {
            let ticket = self
                .commit
                .waiting_ticket(&run_id)
                .ok_or_else(|| Error::Execution("waiting run has no ticket".to_string()))?;
            let allow = matches!(approve(&ticket), Approval::Allow);
            let command = ResumeCommand {
                correlation_id: ticket.correlation_id.clone(),
                run_id: run_id.clone(),
                thread_id: ticket.thread_id.clone(),
                snapshot_id: ticket.snapshot_id.clone(),
                catalog_fingerprint: ticket.catalog_fingerprint.clone(),
                result: ResumeResult::Decision { allow, note: None },
                now_ms: 0,
            };
            phase = self
                .runtime
                .resume(command, self.commit.as_ref(), self.context())
                .await?;
        }

        Ok(self.commit.committed_messages(&self.thread_id)[before..].to_vec())
    }
}
