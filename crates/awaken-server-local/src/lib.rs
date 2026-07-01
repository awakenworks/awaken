//! `awaken-server-local` — the single-machine assembly.
//!
//! It implements the adapter's [`SessionRuntime`] port over the neutral kernel:
//! one `Runtime` (built-in hand tools + permission gate + a model port) and a
//! shared commit coordinator keyed by thread. A public session is a thread; a
//! `user.message` runs one turn to a terminal phase and the committed messages
//! are projected to public events by the adapter.
//!
//! Distribution stays out (per the architecture overview): sandboxing, remote
//! relays, and durable multi-node ingress plug in through seams, not here.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{Toolset, builtin_tools, executable_hand_tools};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{ManagedState, RunError, SessionRuntime, TurnOutcome, router};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use axum::Router;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

/// A deterministic, network-free model: it replies with the last user turn's
/// text, so the server runs end-to-end in CI and under the TypeScript SDK e2e
/// without an API key. Swap in a provider executor for real capability.
pub struct EchoModel;

#[async_trait::async_trait]
impl LlmExecutor for EchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
        })
    }
}

fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The server's permission policy: read/glob/grep allowed, mutations asked
/// (ADR-0030). Under `approval_mode: auto` (M1) asked tools are auto-approved.
fn server_policy() -> RulePermissionPolicy {
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

fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
    builtin_tools()
        .into_iter()
        .filter(|t| t.toolset == Toolset::Hand)
        .map(|t| t.descriptor)
        .collect()
}

fn server_config(model_ref: &str) -> RunnableConfig {
    RunnableConfig::builder("assistant")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .tools(hand_tool_descriptors())
        .max_steps(20)
        .build()
}

fn build_runtime(llm: Arc<dyn LlmExecutor>) -> Runtime {
    let gate = PermissionGate::new(Arc::new(server_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in executable_hand_tools() {
        runtime = runtime.with_tool(tool);
    }
    runtime
}

/// The `SessionRuntime` implementation over the kernel. Threads share one commit
/// coordinator, so each session's history accumulates across turns.
pub struct RuntimeSession {
    runtime: Runtime,
    config: RunnableConfig,
    commit: Arc<MemoryCommitCoordinator>,
    model_ref: String,
}

impl RuntimeSession {
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        let model_ref = model_ref.into();
        Self {
            runtime: build_runtime(llm),
            config: server_config(&model_ref),
            commit: Arc::new(MemoryCommitCoordinator::new()),
            model_ref,
        }
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RuntimeSession {
    async fn run_turn(
        &self,
        _agent: &str,
        thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError> {
        let thread_id = ThreadId(thread.to_string());
        let before = self.commit.committed_messages(&thread_id).len();
        let context = RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone());
        // M1 approval_mode `auto`: an asked tool is auto-approved. HITL arrives in M3.
        let phase = self
            .runtime
            .run_to_completion(&self.config, thread, user_text, context, |_ticket| {
                ResumeResult::allow()
            })
            .await
            .map_err(|e| RunError(e.to_string()))?;
        let all = self.commit.committed_messages(&thread_id);
        let messages = all[before..].to_vec();
        let stop = match phase {
            Phase::Ended(EndCause::MaxSteps) => StopReason::RetriesExhausted,
            Phase::Waiting => StopReason::RequiresAction {
                event_ids: Vec::new(),
            },
            _ => StopReason::EndTurn,
        };
        Ok(TurnOutcome { messages, stop })
    }

    fn model(&self) -> String {
        self.model_ref.clone()
    }
}

/// Build the Managed Agents router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    let state = Arc::new(ManagedState::new(RuntimeSession::new(llm, model_ref)));
    router(state)
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model")
}
