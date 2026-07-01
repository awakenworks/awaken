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
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{Toolset, builtin_tools, executable_hand_tools};
use awaken_ext_goal::{
    DelegateError, DelegateGrader, DelegateReply, DelegateRequest, DelegateRunner, GoalPlugin,
    GoalSpec, Grader, KeywordGrader,
};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::ToolOutput;
use awaken_sandbox_local::{
    IsolatedRoot, LocalSandboxProvider, SandboxProvider, SandboxSpec, rooted_hand_tools,
};
use awaken_store_sqlite::SqliteCommitCoordinator;

use crate::delegate::{A2aTransport, DelegateOutcome};
use crate::hub::{ThreadEvent, ThreadEventHub};
use crate::store::HostCommit;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

pub(crate) static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A filesystem-safe database filename stem for a thread id (durable store).
fn sanitize_thread(thread: &str) -> String {
    thread
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

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
        // `agent_run` is intentionally absent: it falls through to `Ask`, so a
        // delegate call parks (no executor is registered for it) and the host
        // fulfills the park by running the sub-agent (delegation-as-park).
        rules: vec![allow("read"), allow("glob"), allow("grep")],
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

pub(crate) fn server_config(
    model_ref: &str,
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
    plugin_ids: &[String],
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
        .plugins(plugin_ids.iter().cloned())
        .build()
}

/// Build a per-thread runtime whose hand tools are rooted in `root`. When a
/// No `agent_run` executor is registered: a delegate call is advertised by the
/// config but parks (gate `Ask`), and the host fulfills it by running the
/// sub-agent — so delegation pauses durably instead of blocking inline.
pub(crate) fn build_runtime(llm: Arc<dyn LlmExecutor>, root: IsolatedRoot) -> Runtime {
    let gate = PermissionGate::new(Arc::new(server_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in rooted_hand_tools(root) {
        runtime = runtime.with_tool(tool);
    }
    runtime
}

/// A thread's mutable position: the run awaiting a decision (if any) and the
/// system messages buffered for the next turn.
#[derive(Default)]
struct SessionState {
    parked: Option<RunId>,
    pending_system: Vec<String>,
    /// How many committed `Continuation` (outcome) rounds have already been
    /// projected, so a second `define_outcome` on the thread reports only its own.
    consumed_rounds: usize,
    /// True when the parked delegate is waiting for the *user* to supply input to
    /// forward to a remote A2A agent (its task is `input-required`). A `resume`
    /// with a client result delivers it as a follow-up `message:send`.
    awaiting_remote_input: bool,
}

/// One thread's live state: an isolated runtime, its config, its commit
/// coordinator (the source of committed truth), its sandbox root, and its
/// position.
pub(crate) struct SessionCtx {
    pub(crate) runtime: Runtime,
    config: RunnableConfig,
    pub(crate) commit: Arc<HostCommit>,
    thread_id: ThreadId,
    /// The thread's isolated sandbox root, reused to build a goal-enabled runtime
    /// for `define_outcome` (same tools, same root).
    root: IsolatedRoot,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the state lock, so interrupt never blocks
    /// on the loop that holds `state`.
    cancel: std::sync::Mutex<Option<CancellationToken>>,
    state: tokio::sync::Mutex<SessionState>,
}

impl SessionCtx {
    /// A run context carrying a fresh cancellation token, registered on this ctx so
    /// a concurrent `interrupt` can cancel the run it drives. Only one run is in
    /// flight per thread at a time (the `state` lock serializes them), so the slot
    /// always holds the current run's token.
    pub(crate) fn context(&self) -> RuntimeRunContext {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(token.clone());
        RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_cancellation(token)
    }

    /// Register a fresh cancellation token in the thread's cancel slot and return
    /// it, so a concurrent `interrupt` cancels the work it guards (e.g. an
    /// in-flight remote delegation between parks).
    pub(crate) fn register_cancel(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(token.clone());
        token
    }
}

/// Read a string field from an opaque round detail, defaulting to empty.
fn detail_str(detail: &serde_json::Value, key: &str) -> String {
    detail
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Runs a judge sub-agent through the kernel for a [`DelegateGrader`]: a fresh
/// rooted runtime over the same model, driven to completion; its last assistant
/// line is the judge's reply. The judge sees only its prompt (a fresh window), so
/// its verdict is not biased by the doer's working state.
struct KernelJudgeRunner {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalSandboxProvider,
    seq: AtomicU64,
}

#[async_trait::async_trait]
impl DelegateRunner for KernelJudgeRunner {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateReply, DelegateError> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let env = self
            .provider
            .create(&SandboxSpec::new(format!("{}-judge-{n}", request.agent_id)))
            .await
            .map_err(|e| DelegateError(e.to_string()))?;
        let runtime = build_runtime(self.llm.clone(), env.root);
        let config = server_config(&self.model_ref, &HashSet::new(), &HashSet::new(), &[]);
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let thread = format!("judge-thread-{n}");
        let mut ctx = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone());
        // Forward the parent run's cancellation so cancelling the outcome run
        // cancels the judge sub-run too, rather than orphaning it.
        if let Some(token) = request.cancellation {
            ctx = ctx.with_cancellation(token);
        }
        runtime
            .run_to_completion(&config, thread.clone(), request.prompt, ctx, |_| {
                ResumeResult::allow()
            })
            .await
            .map_err(|e| DelegateError(e.to_string()))?;
        Ok(DelegateReply {
            text: Some(latest_assistant_text(
                &commit.committed_messages(&ThreadId(thread)),
            )),
        })
    }
}

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
pub struct SharedHost {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    pub(crate) provider: LocalSandboxProvider,
    grader: Arc<dyn Grader>,
    client_tools: HashSet<String>,
    pub(crate) delegates: HashSet<String>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionCtx>>>,
    hub: Arc<ThreadEventHub>,
    /// When set, each thread commits to a durable SQLite database at
    /// `store_dir/<thread>.db`, so a parked run survives a process restart. When
    /// `None`, sessions use an in-memory coordinator (ephemeral).
    store_dir: Option<PathBuf>,
    /// Delegate agents fulfilled over A2A (agent id → transport) instead of a local
    /// sub-run. `run_delegate` routes to these first.
    pub(crate) remote_agents: HashMap<String, Arc<dyn A2aTransport>>,
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

    /// A host whose outcomes are graded by a real judge sub-agent (`judge_agent_id`)
    /// run through the kernel, instead of the deterministic keyword grader. The
    /// judge grades in its own fresh context.
    pub fn with_judge(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        judge_agent_id: impl Into<String>,
    ) -> Self {
        let mut host = Self::configured(llm, model_ref, HashSet::new(), HashSet::new());
        let base: PathBuf = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!(
                "{}-judge-{}",
                std::process::id(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        let runner = Arc::new(KernelJudgeRunner {
            llm: host.llm.clone(),
            model_ref: host.model_ref.clone(),
            provider: LocalSandboxProvider::new(base),
            seq: AtomicU64::new(0),
        });
        host.grader = Arc::new(DelegateGrader::new(runner, judge_agent_id));
        host
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
            store_dir: None,
            remote_agents: HashMap::new(),
        }
    }

    /// Persist every thread's committed truth to a durable SQLite database under
    /// `dir` (one file per thread). A run parked on a thread survives a restart:
    /// a host rebuilt over the same directory recovers the parked position and can
    /// resume it. Without this, sessions are in-memory and lost on restart.
    pub fn with_store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.store_dir = Some(dir.into());
        self
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

    /// Build a thread's commit boundary: a durable SQLite database under the
    /// configured store directory, or an in-memory coordinator when none is set.
    fn build_commit(&self, thread: &str) -> Result<HostCommit, HostError> {
        match &self.store_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| HostError::internal(e.to_string()))?;
                let path = dir.join(format!("{}.db", sanitize_thread(thread)));
                let sqlite = SqliteCommitCoordinator::open(&path.to_string_lossy())
                    .map_err(|e| HostError::internal(e.to_string()))?;
                Ok(HostCommit::Sqlite(sqlite))
            }
            None => Ok(HostCommit::Memory(MemoryCommitCoordinator::new())),
        }
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
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread)?);
        let runtime = build_runtime(self.llm.clone(), env.root.clone());
        let config = server_config(&self.model_ref, &self.client_tools, &self.delegates, &[]);
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and a parked run (e.g. after a
        // restart). `consumed_rounds` starts past any prior outcome rounds so a new
        // `define_outcome` reports only the rounds it produces.
        let mut state = SessionState {
            consumed_rounds: commit.continuation_payloads(&thread_id).len(),
            ..SessionState::default()
        };
        if let Some((run_id, _)) = commit.open_wait_for_thread(&thread_id) {
            // Prime the fresh runtime so the parked run's snapshot resolves on
            // resume — `start_turn` would normally have installed it.
            runtime
                .install_for_resume(&config)
                .map_err(|e| HostError::internal(e.to_string()))?;
            state.parked = Some(run_id);
        }
        let ctx = Arc::new(SessionCtx {
            runtime,
            config,
            commit,
            thread_id,
            root: env.root.clone(),
            cancel: std::sync::Mutex::new(None),
            state: tokio::sync::Mutex::new(state),
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

    /// Interrupt the run in flight on `thread`, if any: cancel its token so the
    /// runtime observes it at the next step boundary and ends the run `Cancelled`
    /// (an outcome loop then reports `interrupted`). A no-op when nothing is
    /// running. Never blocks on the run's own state lock — it only touches the
    /// separate cancel slot — so it works from a concurrent request.
    pub async fn interrupt(&self, thread: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread).await?;
        if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
            token.cancel();
        }
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
        // A thread parked on a delegate call that was never fulfilled (e.g. a
        // restart recovered it from the store) is driven to completion first; a
        // non-delegate park (client tool, HITL, or awaiting remote input) must be
        // answered before a new turn.
        if let Some(run_id) = st.parked.clone() {
            let (phase, awaiting) = self
                .fulfill_delegations(&ctx, &run_id, Phase::Waiting)
                .await?;
            if matches!(phase, Phase::Waiting) {
                st.awaiting_remote_input = awaiting;
                return Err(HostError::bad_request("thread is awaiting a tool decision"));
            }
            st.parked = None;
            st.awaiting_remote_input = false;
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
        let (phase, awaiting) = self.fulfill_delegations(&ctx, &run_id, phase).await?;
        st.awaiting_remote_input = awaiting;
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

        // Deliver user input to a remote A2A delegate that asked for it: a
        // follow-up `message:send` on the same context, then continue the parent.
        if st.awaiting_remote_input
            && let HostResume::ClientResult { content, is_error } = &resume
        {
            if ticket.call_id.as_deref() != Some(tool_use_id) {
                return Err(HostError::bad_request(format!(
                    "tool_use_id {tool_use_id:?} does not match the pending delegate"
                )));
            }
            let call_id = ticket.call_id.clone().unwrap_or_default();
            let agent_id = ticket
                .pending_tool
                .as_ref()
                .and_then(|tool| tool.arguments.get("agent_id"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let before = ctx.commit.committed_messages(&ctx.thread_id).len();
            let token = ctx.register_cancel();
            let outcome = if *is_error {
                Err(HostError::bad_request("delegation aborted by the user"))
            } else {
                self.deliver_remote_input(&agent_id, content, Some(&token))
                    .await
            };
            let output = match outcome {
                // Still needs input: stay parked for the user, do not re-send.
                Ok(DelegateOutcome::NeedsInput) => {
                    return Ok(self.finish_step(
                        &ctx,
                        &mut st,
                        run_id,
                        Phase::Waiting,
                        before,
                        thread,
                    ));
                }
                Ok(DelegateOutcome::Done(text)) => ToolOutput::ok(&call_id, text),
                Err(err) => ToolOutput::error(&call_id, err.to_string()),
            };
            st.awaiting_remote_input = false;
            let phase = self.resume_parent(&ctx, &ticket, output).await?;
            let (phase, awaiting) = self.fulfill_delegations(&ctx, &run_id, phase).await?;
            st.awaiting_remote_input = awaiting;
            return Ok(self.finish_step(&ctx, &mut st, run_id, phase, before, thread));
        }

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
        let (phase, awaiting) = self.fulfill_delegations(&ctx, &run_id, phase).await?;
        st.awaiting_remote_input = awaiting;
        Ok(self.finish_step(&ctx, &mut st, run_id, phase, before, thread))
    }

    /// Resume the parent run parked on `ticket` with a delegate tool result.
    async fn resume_parent(
        &self,
        ctx: &SessionCtx,
        ticket: &WaitingTicket,
        output: ToolOutput,
    ) -> Result<Phase, HostError> {
        let command = ResumeCommand::from_ticket(ticket, ResumeResult::ToolResult(output), 0);
        ctx.runtime
            .resume(command, &*ctx.commit, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))
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
        let mut st = ctx.state.lock().await;
        let goal = GoalSpec::new(description, rubric, max_iterations);

        // The runtime owns the grade->revise loop: a goal-enabled runtime whose
        // run-end guard steers revisions until the goal is met or the budget is
        // spent. The host drives one run and projects the rounds it committed. The
        // guard shares the thread's committed history and sandbox root.
        let goal_runtime = build_runtime(self.llm.clone(), ctx.root.clone())
            .with_plugin(Arc::new(GoalPlugin::new(goal, self.grader.clone())));
        // The goal run auto-approves tools to drive to a deliverable, so it does not
        // advertise `agent_run` (which parks and is host-fulfilled, not auto-run).
        let config = server_config(
            &self.model_ref,
            &self.client_tools,
            &HashSet::new(),
            &["goal".to_string()],
        );

        // One run: the guard re-derives and grades the deliverable, then steers
        // revisions. Outcome rounds auto-approve tools. Empty input re-infers over
        // the committed history. A concurrent `interrupt` cancels this run.
        let phase = goal_runtime
            .run_to_completion(
                &config,
                thread,
                Vec::<Message>::new(),
                ctx.context(),
                |_| ResumeResult::allow(),
            )
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;

        // Project from DURABLE truth: the committed `Continuation` events the run
        // recorded, each carrying the round's opaque detail (result + explanation).
        // A `consumed_rounds` cursor scopes this to the rounds this call produced.
        let rounds: Vec<serde_json::Value> = ctx.commit.continuation_payloads(&ctx.thread_id);
        let fresh = &rounds[st.consumed_rounds.min(rounds.len())..];
        st.consumed_rounds = rounds.len();

        let outcome_id = format!("outc_{thread}");
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let mut iterations: Vec<HostOutcomeIteration> = fresh
            .iter()
            .enumerate()
            .map(|(i, detail)| HostOutcomeIteration {
                messages: if i == 0 { all.clone() } else { Vec::new() },
                outcome_id: outcome_id.clone(),
                iteration: i as u32 + 1,
                result: detail_str(detail, "result"),
                explanation: detail_str(detail, "explanation"),
            })
            .collect();
        // An interrupted run ends `Cancelled` before the guard can conclude, so
        // no terminal `Continuation` was committed. Report the outcome as
        // `interrupted` — distinct from satisfied/failed/max_iterations.
        if matches!(phase, Phase::Ended(EndCause::Cancelled)) {
            iterations.push(HostOutcomeIteration {
                messages: Vec::new(),
                outcome_id: outcome_id.clone(),
                iteration: iterations.len() as u32 + 1,
                result: "interrupted".to_string(),
                explanation: "the outcome was interrupted".to_string(),
            });
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
                let mut pending = ctx
                    .commit
                    .waiting_ticket(&run_id)
                    .and_then(|t| pending_from_ticket(&t, &self.client_tools));
                // A delegate parked awaiting remote input is client-executed from the
                // caller's view: the user supplies the content to forward.
                if st.awaiting_remote_input
                    && let Some(pending) = pending.as_mut()
                {
                    pending.client_executed = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegate::AGENT_RUN;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use std::sync::atomic::AtomicUsize;

    /// A model that blocks on its second inference (the first revision round) until
    /// a gate is released, so a concurrent `interrupt` can land while the outcome
    /// loop is mid-run. Its reply never contains the rubric, so the guard steers.
    struct GatedModel {
        gate: Arc<tokio::sync::Notify>,
        reached: Arc<tokio::sync::Notify>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmExecutor for GatedModel {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.reached.notify_one();
                self.gate.notified().await;
            }
            Ok(ChatResponse {
                output: AssistantOutput::text("a rough draft"),
                usage: None,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_cancels_the_run_and_reports_interrupted() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let reached = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(GatedModel {
            gate: gate.clone(),
            reached: reached.clone(),
            calls: AtomicUsize::new(0),
        });
        let host = Arc::new(SharedHost::new(model, "scripted"));

        // Drive an outcome whose rubric is never met, so it would loop; the model
        // blocks it mid second round.
        let driver = host.clone();
        let task =
            tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

        // Once the loop is blocked mid-run, interrupt it, then release the gate.
        reached.notified().await;
        host.interrupt("t1").await.expect("interrupt");
        gate.notify_one();

        let report = task.await.expect("join").expect("define_outcome");
        // Round 1 graded needs_revision; the interrupt ended the run before the
        // second round could conclude, so the outcome reports interrupted.
        assert_eq!(report.iterations[0].result, "needs_revision");
        assert_eq!(
            report.iterations.last().expect("a round").result,
            "interrupted"
        );
    }

    #[tokio::test]
    async fn interrupt_is_a_noop_when_nothing_runs() {
        let host = SharedHost::new(
            Arc::new(GatedModel {
                gate: Arc::new(tokio::sync::Notify::new()),
                reached: Arc::new(tokio::sync::Notify::new()),
                calls: AtomicUsize::new(0),
            }),
            "scripted",
        );
        // No run in flight → interrupt succeeds and does nothing.
        host.interrupt("idle-thread")
            .await
            .expect("interrupt is a no-op");
    }

    /// A delegate `agent_run` call parks durably; a crash before it is fulfilled is
    /// recovered by a rebuilt host, which drives the sub-agent and completes the
    /// parent — the sub-result reaches it exactly as if there had been no restart.
    #[tokio::test]
    async fn delegate_park_survives_a_restart_and_is_fulfilled_on_recovery() {
        let dir = std::env::temp_dir().join(format!(
            "awaken-deleg-{}-{}",
            std::process::id(),
            BASE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let thread = "deleg-durable";
        let delegates = HashSet::from(["researcher".to_string()]);

        // 1. First process: start a turn that parks on `agent_run`, then drop the
        //    host WITHOUT fulfilling — a crash right after the durable park commit.
        let run_id = {
            let host = SharedHost::with_delegates(
                Arc::new(crate::DelegatingModel),
                "scripted",
                delegates.clone(),
            )
            .with_store_dir(dir.clone());
            let ctx = host.ctx_for(thread).await.unwrap();
            let msg = Message::text(MessageId("u1".into()), Role::User, "research the answer");
            let (run_id, phase) = ctx
                .runtime
                .start_turn(&ctx.config, thread, vec![msg], ctx.context())
                .await
                .unwrap();
            assert!(matches!(phase, Phase::Waiting), "the delegate call parks");
            let ticket = ctx.commit.waiting_ticket(&run_id).unwrap();
            assert_eq!(ticket.pending_tool.as_ref().unwrap().tool_id, AGENT_RUN);
            run_id
        };

        // 2. A new host over the same store recovers the parked delegate call and
        //    fulfills it — the sub-agent's result reaches the parent.
        let host =
            SharedHost::with_delegates(Arc::new(crate::DelegatingModel), "scripted", delegates)
                .with_store_dir(dir.clone());
        let ctx = host.ctx_for(thread).await.unwrap();
        assert_eq!(
            ctx.state.lock().await.parked.as_ref(),
            Some(&run_id),
            "the rebuilt host recovers the parked delegate run"
        );
        let (phase, _awaiting) = host
            .fulfill_delegations(&ctx, &run_id, Phase::Waiting)
            .await
            .unwrap();
        assert!(
            matches!(phase, Phase::Ended(_)),
            "the recovered delegation drives to completion"
        );
        let history = ctx.commit.committed_messages(&ctx.thread_id);
        assert!(
            history
                .iter()
                .any(|m| block_text(&m.content).contains("delegate said: researched: 42")),
            "the sub-agent result reached the parent after recovery"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
