//! `awaken-server-local` — the single-machine assembly.
//!
//! It implements the adapter's [`SessionRuntime`] port over the neutral kernel.
//! Each public session gets its own **sandboxed environment** (an isolated root)
//! and a per-session runtime whose built-in tools are *rooted* in that
//! environment. A `user.message` runs one turn; if a tool needs approval the run
//! **parks** (`session.status_idle{requires_action}`) and a later
//! `user.tool_confirmation` resumes it — the durable park/resume path, delivered
//! out-of-band (ADR-0033), the twin of `run_to_completion`.
//!
//! Per-environment composition keeps the kernel sandbox-agnostic (ADR-0034 D6);
//! distribution stays out — remote relays and multi-node ingress plug in through
//! seams, not here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{Toolset, builtin_tools};
use awaken_ext_goal::{GoalSpec, Grader, KeywordGrader, classify};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeIteration, OutcomeReport, RunError, SessionRuntime, TurnOutcome,
    router,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_sandbox_local::{
    IsolatedRoot, LocalSandboxProvider, SandboxProvider, SandboxSpec, rooted_hand_tools,
};
use axum::Router;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

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

/// A deterministic probe model for the HITL e2e: it writes the user's text to a
/// relative `probe.txt` (asked -> parks for confirmation), reads it back (allowed
/// -> runs), then replies. Stateless: it decides from the transcript.
pub struct ProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for ProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "probe.txt", "content": user_text }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": "probe.txt" }),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

/// A deterministic model for the outcome e2e: replies with a draft, and revises to
/// include "FINAL" once it sees the goal loop's feedback. Stateless.
pub struct ReviseModel;

#[async_trait::async_trait]
impl LlmExecutor for ReviseModel {
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
        let reply = if last_user.contains("did not meet the goal") {
            "FINAL answer"
        } else {
            "a rough draft"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
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

/// read/glob/grep allowed, mutations asked (ADR-0030). With `approval_mode:
/// human_approval` an asked tool parks for a `user.tool_confirmation`.
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

/// Build a per-session runtime whose hand tools are rooted in `root`.
fn build_runtime(llm: Arc<dyn LlmExecutor>, root: IsolatedRoot) -> Runtime {
    let gate = PermissionGate::new(Arc::new(server_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in rooted_hand_tools(root) {
        runtime = runtime.with_tool(tool);
    }
    runtime
}

/// A session's mutable position: the run awaiting confirmation (if any) and how
/// many committed messages have already been projected.
#[derive(Default)]
struct SessionState {
    parked: Option<RunId>,
    consumed: usize,
}

/// One session's live state: an isolated runtime, its config, its history, and
/// its position.
struct SessionCtx {
    runtime: Runtime,
    config: RunnableConfig,
    commit: Arc<MemoryCommitCoordinator>,
    thread_id: ThreadId,
    state: tokio::sync::Mutex<SessionState>,
}

/// Turn a step's terminal phase into an outcome: project only the newly committed
/// messages, and set `parked` / `pending` when the run stopped for approval.
fn build_outcome(
    ctx: &SessionCtx,
    st: &mut SessionState,
    run_id: RunId,
    phase: Phase,
) -> TurnOutcome {
    let all = ctx.commit.committed_messages(&ctx.thread_id);
    let messages = all[st.consumed..].to_vec();
    st.consumed = all.len();
    match phase {
        Phase::Waiting => {
            let pending = ctx.commit.waiting_ticket(&run_id).and_then(|t| t.call_id);
            st.parked = Some(run_id);
            TurnOutcome {
                messages,
                stop: StopReason::RequiresAction {
                    event_ids: Vec::new(),
                },
                pending,
            }
        }
        Phase::Ended(EndCause::MaxSteps) => {
            st.parked = None;
            TurnOutcome {
                messages,
                stop: StopReason::RetriesExhausted,
                pending: None,
            }
        }
        _ => {
            st.parked = None;
            TurnOutcome {
                messages,
                stop: StopReason::EndTurn,
                pending: None,
            }
        }
    }
}

/// The `SessionRuntime` implementation over the kernel. Each session is a
/// sandboxed environment with its own rooted runtime; sessions are created lazily.
pub struct RuntimeSession {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalSandboxProvider,
    grader: Arc<dyn Grader>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionCtx>>>,
}

impl RuntimeSession {
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        let base: PathBuf = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!(
                "{}-{}",
                std::process::id(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        Self {
            llm: llm.clone(),
            model_ref: model_ref.into(),
            provider: LocalSandboxProvider::new(base),
            grader: Arc::new(KeywordGrader),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn ctx_for(&self, session: &str) -> Result<Arc<SessionCtx>, RunError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(ctx) = sessions.get(session) {
            return Ok(ctx.clone());
        }
        let env = self
            .provider
            .create(&SandboxSpec::new(session))
            .await
            .map_err(|e| RunError(e.to_string()))?;
        let ctx = Arc::new(SessionCtx {
            runtime: build_runtime(self.llm.clone(), env.root.clone()),
            config: server_config(&self.model_ref),
            commit: Arc::new(MemoryCommitCoordinator::new()),
            thread_id: ThreadId(session.to_string()),
            state: tokio::sync::Mutex::new(SessionState::default()),
        });
        sessions.insert(session.to_string(), ctx.clone());
        Ok(ctx)
    }

    fn context(ctx: &SessionCtx) -> RuntimeRunContext {
        RuntimeRunContext::new()
            .with_commit(ctx.commit.clone())
            .with_reader(ctx.commit.clone())
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
        let ctx = self.ctx_for(thread).await?;
        let mut st = ctx.state.lock().await;
        if st.parked.is_some() {
            return Err(RunError(
                "session is awaiting a tool confirmation".to_string(),
            ));
        }
        let (run_id, phase) = ctx
            .runtime
            .start_turn(&ctx.config, thread, user_text, Self::context(&ctx))
            .await
            .map_err(|e| RunError(e.to_string()))?;
        Ok(build_outcome(&ctx, &mut st, run_id, phase))
    }

    async fn resume(&self, thread: &str, decision: Decision) -> Result<TurnOutcome, RunError> {
        let ctx = self.ctx_for(thread).await?;
        let mut st = ctx.state.lock().await;
        let run_id = st
            .parked
            .clone()
            .ok_or_else(|| RunError("no parked run to resume".to_string()))?;
        let ticket = ctx
            .commit
            .waiting_ticket(&run_id)
            .ok_or_else(|| RunError("parked run has no waiting ticket".to_string()))?;
        let result = if decision.allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(decision.note)
        };
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let phase = ctx
            .runtime
            .resume(command, &*ctx.commit, Self::context(&ctx))
            .await
            .map_err(|e| RunError(e.to_string()))?;
        Ok(build_outcome(&ctx, &mut st, run_id, phase))
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        let ctx = self.ctx_for(thread).await?;
        let mut st = ctx.state.lock().await;
        let goal = GoalSpec::new(description, rubric, max_iterations);
        let outcome_id = format!("outc_{thread}");
        let mut iterations = Vec::new();
        let mut iteration = 1;
        loop {
            let all = ctx.commit.committed_messages(&ctx.thread_id);
            let messages = all[st.consumed..].to_vec();
            st.consumed = all.len();
            let deliverable = latest_assistant_text(&all);
            let verdict = self.grader.grade(&goal, &deliverable);
            let outcome = classify(&verdict, iteration, goal.max_iterations);
            iterations.push(OutcomeIteration {
                messages,
                outcome_id: outcome_id.clone(),
                iteration,
                result: outcome.token().to_string(),
                explanation: verdict.explanation.clone(),
            });
            if outcome.is_terminal() {
                break;
            }
            // Re-dispatch a revision round with feedback; outcome rounds auto-approve
            // tools (the goal loop drives to a deliverable).
            let feedback = format!(
                "Your previous answer did not meet the goal ({description}). {} Revise it.",
                verdict.explanation
            );
            ctx.runtime
                .run_to_completion(&ctx.config, thread, feedback, Self::context(&ctx), |_| {
                    ResumeResult::allow()
                })
                .await
                .map_err(|e| RunError(e.to_string()))?;
            iteration += 1;
        }
        Ok(OutcomeReport { iterations })
    }

    fn model(&self) -> String {
        self.model_ref.clone()
    }
}

/// The text of the last assistant message in a transcript.
fn latest_assistant_text(messages: &[awaken_agent_contract::agent::message::Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default()
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
