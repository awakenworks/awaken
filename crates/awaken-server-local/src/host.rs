//! `SharedHost` — the protocol-neutral, thread-keyed session substrate.
//!
//! This is the "waist" both protocol adapters (Managed Agents, AI SDK) drive. It
//! owns one sandboxed runtime + commit coordinator per **thread id**, and exposes
//! neutral operations — `run_turn`, `resume`, `committed_messages` — over that
//! shared state. Because both adapters key by the same thread id and mutate the
//! same coordinator and parked-run position, a turn started through one protocol
//! can be observed or resumed through the other on the *same thread*.
//!
//! It names no protocol vocabulary: outcomes are the neutral [`Phase`] plus an
//! optional [`PendingTool`]; each adapter maps those onto its own wire shape.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{
    AgentRunner, Toolset, builtin_tools, delegation_tools, executable_hand_tools,
};
use awaken_ext_goal::{GoalSpec, Grader, KeywordGrader, classify};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::{ToolError, ToolOutput};
use awaken_sandbox_local::{
    IsolatedRoot, LocalSandboxProvider, SandboxProvider, SandboxSpec, rooted_hand_tools,
};

use crate::hub::{ThreadEvent, ThreadEventHub};

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

pub(crate) static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Concatenate the text of a content-block list.
pub(crate) fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The text of the last assistant message in a transcript.
pub(crate) fn latest_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default()
}

/// A tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (the caller runs it and returns a result) or a built-in tool
/// awaiting a permission decision.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The neutral result of one step (a turn or a resume): the messages committed
/// during the step, the terminal phase, and the pending tool when the run parked.
pub struct TurnResult {
    pub new_messages: Vec<Message>,
    pub phase: Phase,
    pub pending: Option<PendingTool>,
}

/// The neutral resume command: answer a built-in tool's permission gate, or
/// deliver a client-executed tool's result.
pub enum HostResume {
    /// Built-in tool awaiting approval (Managed `user.tool_confirmation`; AI SDK
    /// `approval-responded` / `output-denied`).
    Confirm { allow: bool, note: Option<String> },
    /// Client-executed tool result (Managed `user.custom_tool_result`; AI SDK
    /// `output-available` / `output-error` on a client tool part).
    ClientResult { content: String, is_error: bool },
}

impl HostResume {
    fn wants_client(&self) -> bool {
        matches!(self, HostResume::ClientResult { .. })
    }
}

/// A host failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no park), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub message: String,
    pub kind: HostErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    Internal,
    BadRequest,
}

impl HostError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::BadRequest,
        }
    }
}

/// One evaluation round of a goal (neutral): the revision messages committed this
/// round, the round index, the classification token, and the grader explanation.
pub struct HostOutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The neutral outcome report: the ordered evaluation rounds.
pub struct HostOutcomeReport {
    pub iterations: Vec<HostOutcomeIteration>,
}

/// read/glob/grep allowed, mutations asked (ADR-0030). With `approval_mode:
/// human_approval` an asked tool parks for a confirmation.
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
        rules: vec![
            allow("read"),
            allow("glob"),
            allow("grep"),
            allow("agent_run"),
        ],
    })
}

fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
    let registered: HashSet<String> = executable_hand_tools()
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    builtin_tools()
        .into_iter()
        .filter(|t| t.toolset == Toolset::Hand && registered.contains(&t.descriptor.id))
        .map(|t| t.descriptor)
        .collect()
}

/// A client-executed tool descriptor: model-visible, but no `RawTool` is
/// registered, so a call parks (gate `ask`) and the *client* supplies the result.
fn client_tool_descriptor(id: &str) -> ToolDescriptor {
    ToolDescriptor::pinned(
        "client",
        id,
        format!("Client-executed tool `{id}`; the caller runs it and returns the result."),
        serde_json::json!({ "type": "object" }),
    )
}

/// The `agent_run` delegation descriptor (advertised only when a roster is set).
fn delegation_descriptor() -> ToolDescriptor {
    builtin_tools()
        .into_iter()
        .find(|t| t.toolset == Toolset::Delegation)
        .map(|t| t.descriptor)
        .expect("agent_run descriptor exists")
}

fn server_config(
    model_ref: &str,
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
) -> RunnableConfig {
    let mut tools = hand_tool_descriptors();
    tools.extend(client_tools.iter().map(|id| client_tool_descriptor(id)));
    if !delegates.is_empty() {
        tools.push(delegation_descriptor());
    }
    RunnableConfig::builder("assistant")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .tools(tools)
        .max_steps(20)
        .build()
}

/// Build a per-thread runtime whose hand tools are rooted in `root`. When a
/// `delegation` runner is supplied, `agent_run` is registered too.
fn build_runtime(
    llm: Arc<dyn LlmExecutor>,
    root: IsolatedRoot,
    delegation: Option<Arc<dyn AgentRunner>>,
) -> Runtime {
    let gate = PermissionGate::new(Arc::new(server_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in rooted_hand_tools(root) {
        runtime = runtime.with_tool(tool);
    }
    if let Some(runner) = delegation {
        for tool in delegation_tools(runner) {
            runtime = runtime.with_tool(tool);
        }
    }
    runtime
}

/// A thread's mutable position: the run awaiting a decision (if any) and the
/// system messages buffered for the next turn.
#[derive(Default)]
struct SessionState {
    parked: Option<RunId>,
    pending_system: Vec<String>,
}

/// One thread's live state: an isolated runtime, its config, its commit
/// coordinator (the source of committed truth), and its position.
struct SessionCtx {
    runtime: Runtime,
    config: RunnableConfig,
    commit: Arc<MemoryCommitCoordinator>,
    thread_id: ThreadId,
    state: tokio::sync::Mutex<SessionState>,
}

impl SessionCtx {
    fn context(&self) -> RuntimeRunContext {
        RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
    }
}

/// Backs `agent_run` with an in-process sub-run: validates the target against the
/// roster (fail closed), drives a fresh rooted runtime over the same model to
/// completion, and returns the delegate's last assistant line. The sub-runtime
/// has no delegation tool, so a delegate cannot recurse.
struct LocalAgentRunner {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    roster: HashSet<String>,
    provider: LocalSandboxProvider,
    seq: AtomicU64,
}

#[async_trait::async_trait]
impl AgentRunner for LocalAgentRunner {
    async fn run(&self, agent_id: &str, input: &str) -> Result<String, ToolError> {
        if !self.roster.contains(agent_id) {
            return Err(ToolError::Execution(format!(
                "delegate agent {agent_id:?} is not in the roster"
            )));
        }
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let env = self
            .provider
            .create(&SandboxSpec::new(format!("{agent_id}-sub-{n}")))
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let runtime = build_runtime(self.llm.clone(), env.root, None);
        let config = server_config(&self.model_ref, &HashSet::new(), &HashSet::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let thread = format!("sub-thread-{n}");
        let ctx = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone());
        runtime
            .run_to_completion(&config, thread.clone(), input, ctx, |_| {
                ResumeResult::allow()
            })
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(latest_assistant_text(
            &commit.committed_messages(&ThreadId(thread)),
        ))
    }
}

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
pub struct SharedHost {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalSandboxProvider,
    grader: Arc<dyn Grader>,
    client_tools: HashSet<String>,
    delegates: HashSet<String>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionCtx>>>,
    hub: Arc<ThreadEventHub>,
}

impl SharedHost {
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self::configured(llm, model_ref, HashSet::new(), HashSet::new())
    }

    /// A host with client-executed tools: those ids are model-visible but
    /// unregistered, so a call parks and the client supplies the result.
    pub fn with_client_tools(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        client_tools: HashSet<String>,
    ) -> Self {
        Self::configured(llm, model_ref, client_tools, HashSet::new())
    }

    /// A host that can delegate to the agents in `delegates` via `agent_run`.
    pub fn with_delegates(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        delegates: HashSet<String>,
    ) -> Self {
        Self::configured(llm, model_ref, HashSet::new(), delegates)
    }

    fn configured(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        client_tools: HashSet<String>,
        delegates: HashSet<String>,
    ) -> Self {
        let base: PathBuf = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!(
                "{}-{}",
                std::process::id(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        Self {
            llm,
            model_ref: model_ref.into(),
            provider: LocalSandboxProvider::new(base),
            grader: Arc::new(KeywordGrader),
            client_tools,
            delegates,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            hub: Arc::new(ThreadEventHub::new()),
        }
    }

    /// The model id echoed by adapters in their session/agent objects.
    pub fn model(&self) -> String {
        self.model_ref.clone()
    }

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &HashSet<String> {
        &self.client_tools
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<ThreadEventHub> {
        &self.hub
    }

    /// The delegation runner for a thread, or `None` when no roster is set.
    fn agent_runner(&self) -> Option<Arc<dyn AgentRunner>> {
        if self.delegates.is_empty() {
            return None;
        }
        let base: PathBuf = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!(
                "{}-sub-{}",
                std::process::id(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        Some(Arc::new(LocalAgentRunner {
            llm: self.llm.clone(),
            model_ref: self.model_ref.clone(),
            roster: self.delegates.clone(),
            provider: LocalSandboxProvider::new(base),
            seq: AtomicU64::new(0),
        }))
    }

    async fn ctx_for(&self, thread: &str) -> Result<Arc<SessionCtx>, HostError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(ctx) = sessions.get(thread) {
            return Ok(ctx.clone());
        }
        let env = self
            .provider
            .create(&SandboxSpec::new(thread))
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        let ctx = Arc::new(SessionCtx {
            runtime: build_runtime(self.llm.clone(), env.root.clone(), self.agent_runner()),
            config: server_config(&self.model_ref, &self.client_tools, &self.delegates),
            commit: Arc::new(MemoryCommitCoordinator::new()),
            thread_id: ThreadId(thread.to_string()),
            state: tokio::sync::Mutex::new(SessionState::default()),
        });
        sessions.insert(thread.to_string(), ctx.clone());
        Ok(ctx)
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet.
    pub async fn committed_messages(&self, thread: &str) -> Vec<Message> {
        let sessions = self.sessions.lock().await;
        match sessions.get(thread) {
            Some(ctx) => ctx.commit.committed_messages(&ctx.thread_id),
            None => Vec::new(),
        }
    }

    /// True when `thread` has a run parked awaiting a decision.
    pub async fn is_parked(&self, thread: &str) -> bool {
        let ctx = match self.ctx_for(thread).await {
            Ok(ctx) => ctx,
            Err(_) => return false,
        };
        ctx.state.lock().await.parked.is_some()
    }

    /// The tool a parked run on `thread` is waiting on, if any.
    pub async fn pending_tool(&self, thread: &str) -> Option<PendingTool> {
        let ctx = self.ctx_for(thread).await.ok()?;
        let st = ctx.state.lock().await;
        let run_id = st.parked.clone()?;
        pending_from_ticket(&ctx.commit.waiting_ticket(&run_id)?, &self.client_tools)
    }

    /// Buffer a system message; it is prepended to the next turn's input.
    pub async fn add_system(&self, thread: &str, text: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread).await?;
        ctx.state.lock().await.pending_system.push(text.to_string());
        Ok(())
    }

    /// Run one turn on `thread`: buffered system messages first, then `input`.
    /// Runs to the first pause (a parked tool) or the natural end.
    pub async fn run_turn(
        &self,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<TurnResult, HostError> {
        let ctx = self.ctx_for(thread).await?;
        let mut st = ctx.state.lock().await;
        if st.parked.is_some() {
            return Err(HostError::bad_request("thread is awaiting a tool decision"));
        }
        let mut messages: Vec<Message> = std::mem::take(&mut st.pending_system)
            .into_iter()
            .map(|text| {
                Message::text(
                    MessageId(format!("sys-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst))),
                    Role::System,
                    text,
                )
            })
            .collect();
        messages.extend(input);
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        let (run_id, phase) = ctx
            .runtime
            .start_turn(&ctx.config, thread, messages, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(self.finish_step(&ctx, &mut st, run_id, phase, before, thread))
    }

    /// Resume the run parked on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: HostResume,
    ) -> Result<TurnResult, HostError> {
        let ctx = self.ctx_for(thread).await?;
        let mut st = ctx.state.lock().await;
        let run_id = st
            .parked
            .clone()
            .ok_or_else(|| HostError::bad_request("no parked run to resume"))?;
        let ticket = ctx
            .commit
            .waiting_ticket(&run_id)
            .ok_or_else(|| HostError::internal("parked run has no waiting ticket"))?;
        self.check_pending(&ticket, tool_use_id, resume.wants_client())?;
        let result = match resume {
            HostResume::Confirm { allow, note } => {
                if allow {
                    ResumeResult::allow()
                } else {
                    ResumeResult::deny(note)
                }
            }
            HostResume::ClientResult { content, is_error } => {
                let call_id = ticket.call_id.clone().unwrap_or_default();
                let output = if is_error {
                    ToolOutput::error(call_id, content)
                } else {
                    ToolOutput::ok(call_id, content)
                };
                ResumeResult::ToolResult(output)
            }
        };
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let phase = ctx
            .runtime
            .resume(command, &*ctx.commit, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(self.finish_step(&ctx, &mut st, run_id, phase, before, thread))
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded
    /// by `max_iterations`. Revision rounds auto-approve tools (the goal loop
    /// drives to a deliverable).
    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<HostOutcomeReport, HostError> {
        let ctx = self.ctx_for(thread).await?;
        let _st = ctx.state.lock().await;
        let goal = GoalSpec::new(description, rubric, max_iterations);
        let outcome_id = format!("outc_{thread}");
        let mut iterations = Vec::new();
        let mut iteration = 1;
        let mut consumed = ctx.commit.committed_messages(&ctx.thread_id).len();
        loop {
            let all = ctx.commit.committed_messages(&ctx.thread_id);
            let messages = all[consumed..].to_vec();
            consumed = all.len();
            let deliverable = latest_assistant_text(&all);
            let verdict = self.grader.grade(&goal, &deliverable);
            let outcome = classify(&verdict, iteration, goal.max_iterations);
            iterations.push(HostOutcomeIteration {
                messages,
                outcome_id: outcome_id.clone(),
                iteration,
                result: outcome.token().to_string(),
                explanation: verdict.explanation.clone(),
            });
            if outcome.is_terminal() {
                break;
            }
            let feedback = format!(
                "Your previous answer did not meet the goal ({description}). {} Revise it.",
                verdict.explanation
            );
            ctx.runtime
                .run_to_completion(&ctx.config, thread, feedback, ctx.context(), |_| {
                    ResumeResult::allow()
                })
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            iteration += 1;
        }
        Ok(HostOutcomeReport { iterations })
    }

    /// Project the step's delta, update the parked position, and publish the
    /// delta to the thread hub for any observing protocol.
    fn finish_step(
        &self,
        ctx: &SessionCtx,
        st: &mut SessionState,
        run_id: RunId,
        phase: Phase,
        before: usize,
        thread: &str,
    ) -> TurnResult {
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let new_messages = all[before.min(all.len())..].to_vec();
        let (pending, waiting) = match &phase {
            Phase::Waiting => {
                st.parked = Some(run_id.clone());
                let pending = ctx
                    .commit
                    .waiting_ticket(&run_id)
                    .and_then(|t| pending_from_ticket(&t, &self.client_tools));
                (pending, true)
            }
            _ => {
                st.parked = None;
                (None, false)
            }
        };
        if !new_messages.is_empty() {
            self.hub
                .publish(thread, ThreadEvent::Committed(new_messages.clone()));
        }
        self.hub.publish(thread, ThreadEvent::StepEnded { waiting });
        TurnResult {
            new_messages,
            phase,
            pending,
        }
    }

    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// run's pending tool, and that tool's binding must match the inbound resume
    /// — a client result may only answer a client-executed tool, a confirmation
    /// only a built-in one.
    fn check_pending(
        &self,
        ticket: &WaitingTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        if ticket.call_id.as_deref() != Some(tool_use_id) {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        let pending_tool_id = ticket
            .pending_tool
            .as_ref()
            .map(|t| t.tool_id.as_str())
            .ok_or_else(|| HostError::internal("parked run has no pending tool"))?;
        let is_client = self.client_tools.contains(pending_tool_id);
        if is_client != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(HostError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}

/// Read the pending tool off a waiting ticket, classifying it client-executed
/// when its id is in `client_tools`.
fn pending_from_ticket(
    ticket: &WaitingTicket,
    client_tools: &HashSet<String>,
) -> Option<PendingTool> {
    let tool_use_id = ticket.call_id.clone()?;
    let tool = ticket.pending_tool.clone()?;
    let client_executed = client_tools.contains(&tool.tool_id);
    Some(PendingTool {
        tool_use_id,
        name: tool.tool_id,
        input: tool.arguments,
        client_executed,
    })
}
