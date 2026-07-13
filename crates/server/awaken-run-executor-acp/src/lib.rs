//! `AcpRunExecutor`: an external ACP agent (Claude Code / Codex / …) as a peer
//! [`RunExecutor`].
//!
//! `Runtime` is the *native* [`RunExecutor`] (its own model/tool loop). This is a
//! *second* implementation whose brain is an opaque ACP CLI driven over a duplex
//! [`AgentChannel`] by the [`Supervisor`]. Because both are `RunExecutor`s, an
//! ACP-backed agent is reachable over every wire adapter exactly like a native
//! one — no separate "brain" abstraction is introduced (the driver *is* the
//! runtime).
//!
//! Boundaries (ADR-0043 D6/D9): this crate lives in the runtime plane and depends
//! only on foundation (contracts) + provisioning (channel) + the ACP protocol
//! crate. It never resolves config or names a secret — the host opens the channel
//! (local sandbox launch or remote dial) via an injected [`AgentChannelSource`]
//! and hands this executor an already-launched agent.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::run::{EndCause, Failure, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_protocol_acp::{
    AcpError, AcpFailure, AgentEvent, AllowAll, AppendError, Injection, LaunchSink, PermissionAsk,
    PermissionResolver, PermissionVerdict, RawAcpError, RunFactAppender, Stage, SupervisePolicy,
    Supervisor, TerminationReason, TurnConfig, classify_error,
};
// Re-exported (not just `use`d) so a host composition root selects the wire and
// observes agent bring-up without a direct dependency on the protocol crate. The
// executor also uses these names internally to emit lifecycle events.
pub use awaken_protocol_acp::{AcpLaunchEvent, AcpLaunchStage, Codec, LaunchObserver};
use awaken_provisioning_contract::ProcessHandle;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::boundary::{BoundaryOutcome, evaluate_boundary};
use awaken_runtime_contract::execution::{
    Cancellation, Error, ExecutorCapabilities, Result, RunExecutor, Wait,
};
use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};
use awaken_runtime_contract::permission::{
    PermissionContext, PermissionDecision, PermissionPolicy,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// An already-launched ACP agent: the duplex channel plus the process handle for
/// reaping. The host produces this — locally by launching the CLI into a sandbox,
/// remotely by dialing an intermediary — so this executor stays transport- and
/// provisioning-agnostic (it only drives a channel).
pub struct AgentSession {
    pub channel: Box<dyn AgentChannel>,
    pub process: Arc<dyn ProcessHandle>,
    /// Which wire to speak to this agent. The source declares it: a fixture/test
    /// agent speaks the newline stand-in ([`Codec::Newline`], the default); a real
    /// CLI opened by [`ProjectingChannelSource`] speaks official ACP JSON-RPC.
    pub codec: Codec,
    /// The interior working directory the sandbox launched the CLI in — held stable
    /// per thread so a cwd-keyed CLI's session is found on `session/load` across
    /// directories/machines. `None` → the CLI runs at `/` (the default).
    pub workspace_cwd: Option<String>,
}

/// Opens an [`AgentSession`] for a run. The one seam the host wires: local =
/// sandbox launch (provisioning), remote = connection dial. Injected so the
/// executor never depends on either directly.
#[async_trait]
pub trait AgentChannelSource: Send + Sync {
    async fn open(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError>;
}

/// Why opening the agent channel failed (launch/config/dial fault).
#[derive(Debug, thiserror::Error)]
#[error("agent channel open failed: {0}")]
pub struct OpenError(pub String);

/// Identifies a CLI's portable session-home: which conversation (thread) on which
/// adapter. The on-disk format is adapter-specific, so a Claude and a Codex blob on
/// the same thread never collide. The tenant / data-subject scope is applied by the
/// injected [`SessionHomeProvider`] (it is constructed knowing the scope), so this
/// key stays free of tenancy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHomeKey {
    pub thread_id: String,
    pub adapter: String,
}

/// What a [`SessionHomeProvider`] harvests/restores for one run: the config-home env
/// the CLI reads, the portable session subtree under it, the paths to exclude
/// (credentials / local config), and whether the CLI keys sessions by cwd (so
/// recovery needs a stable interior working directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHomePlan {
    pub config_home_env: String,
    pub session_subpath: String,
    pub exclude: Vec<String>,
    pub keyed_by_cwd: bool,
}

/// Recovers a local-dir CLI's session across directories and machines: restore the
/// thread's portable session into the config home before launch, harvest it back to
/// durable (content-addressed, cross-machine) storage after the run. Injected like
/// [`AgentChannelSource`]; a host wires a `ContentStore`-backed, tenant-scoped,
/// credential-excluding implementation. Gateway / stateless adapters never reach
/// this seam (their [`SessionPersistence`] is not `LocalDir`).
#[async_trait]
pub trait SessionHomeProvider: Send + Sync {
    async fn restore(&self, key: &SessionHomeKey, plan: &SessionHomePlan);
    async fn harvest(&self, key: &SessionHomeKey, plan: &SessionHomePlan);
}

/// The default: no cross-machine session-home. The CLI's local config home is used
/// as-is and recovery falls back to the neutral thread history — unchanged behaviour
/// until a host wires a real provider.
pub struct NoSessionHome;

#[async_trait]
impl SessionHomeProvider for NoSessionHome {
    async fn restore(&self, _key: &SessionHomeKey, _plan: &SessionHomePlan) {}
    async fn harvest(&self, _key: &SessionHomeKey, _plan: &SessionHomePlan) {}
}

/// How a backend handles a mid-conversation model switch (R7). Reported so the
/// host can gate an override fail-closed against a backend that cannot honor it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitch {
    /// Native: each turn resolves its own executor; switching is free (O(1)).
    FreePerTurn,
    /// ACP CLI: a live session is a process, so switching relaunches it with the
    /// new env. Correct but not free — a fresh channel per turn.
    Relaunch,
    /// The backend cannot switch mid-conversation; a per-turn override must fail.
    Unsupported,
}

/// Drives an external ACP agent as a [`RunExecutor`].
///
/// Each [`execute`](RunExecutor::execute) opens a fresh channel via the source, so
/// an ACP thread relaunches its CLI every turn — which is exactly how a model
/// switch takes effect (R7): the host re-stages the model, evicts the context, and
/// the next turn's launch carries the new env. Reported as [`ModelSwitch::Relaunch`].
pub struct AcpRunExecutor {
    source: Arc<dyn AgentChannelSource>,
    policy: SupervisePolicy,
    observer: Option<Arc<dyn LaunchObserver>>,
    /// Authorizes the external CLI's mid-turn tool requests. Defaults to allow (the
    /// sandbox is the enforcement boundary); a host wires a neutral `PermissionPolicy`
    /// via [`with_permission_policy`](Self::with_permission_policy) to apply org
    /// policy / HITL uniformly across native and ACP runs.
    permission: Arc<dyn PermissionResolver>,
    /// The ACP session mode to pin (adapter-local; `None` leaves the agent's
    /// default). Validated fail-closed against the agent's advertised modes.
    session_mode: Option<String>,
    /// Recovers a local-dir CLI's session across directories/machines. Defaults to
    /// no-op (local config home as-is); a host wires a durable, cross-machine one.
    session_home: Arc<dyn SessionHomeProvider>,
}

impl AcpRunExecutor {
    /// This backend's mid-switch capability (R7): an ACP CLI relaunches.
    #[must_use]
    pub fn model_switch(&self) -> ModelSwitch {
        ModelSwitch::Relaunch
    }
}

impl AcpRunExecutor {
    pub fn new(source: Arc<dyn AgentChannelSource>) -> Self {
        Self {
            source,
            policy: SupervisePolicy::default(),
            observer: None,
            permission: Arc::new(AllowAll),
            session_mode: None,
            session_home: Arc::new(NoSessionHome),
        }
    }

    /// Recover a local-dir CLI's session across directories/machines: the host wires
    /// a durable, content-addressed, tenant-scoped, credential-excluding provider.
    /// Only `LocalDir` adapters reach it; Gateway/stateless ones are untouched.
    #[must_use]
    pub fn with_session_home(mut self, provider: Arc<dyn SessionHomeProvider>) -> Self {
        self.session_home = provider;
        self
    }

    /// Authorize the external CLI's tool requests through the single neutral
    /// [`PermissionPolicy`] (G21) — the same authority that governs native tools —
    /// instead of the default allow. The CLI's `session/request_permission` is
    /// projected onto this policy and its decision projected back onto the agent's
    /// own allow/reject option.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: Arc<dyn PermissionPolicy>) -> Self {
        self.permission = Arc::new(NeutralPermissionResolver { policy });
        self
    }

    /// Pin the ACP session mode (e.g. `plan`), applied via `session/set_mode` after
    /// the handshake and validated fail-closed against the agent's advertised modes.
    #[must_use]
    pub fn with_session_mode(mut self, mode: impl Into<String>) -> Self {
        self.session_mode = Some(mode.into());
        self
    }

    /// Observe this executor's agent bring-up (install → launch → initialize →
    /// ready → failed), so a host can publish progress to a UI. Without one, the
    /// lifecycle notifications are dropped (no behavior change).
    #[must_use]
    pub fn with_launch_observer(mut self, observer: Arc<dyn LaunchObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// A scoped lifecycle sink for one run (`None` without a wired observer): the
    /// observer plus the thread scope, so a UI channel can route the events.
    fn launch_sink<'a>(&'a self, scope: &'a str) -> Option<LaunchSink<'a>> {
        self.observer
            .as_ref()
            .map(|observer| LaunchSink::new(observer.as_ref(), scope))
    }
}

/// Emit a lifecycle event to an optional scoped sink (no-op without an observer).
fn notify(sink: Option<LaunchSink<'_>>, event: AcpLaunchEvent) {
    if let Some(sink) = sink {
        sink.emit(event);
    }
}

/// Whether the run's ACP backend installs dynamically on launch (an `npx` adapter),
/// so the executor can surface an `Installing` phase before the process is usable.
fn dynamic_install_backend(activation: &RunActivation) -> bool {
    match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
        Backend::Acp { cli } => acp_cli(&cli).is_some_and(is_dynamic_install),
        _ => false,
    }
}

/// The prompt for this turn: the concatenated text of the activation's input.
fn prompt_of(input: &[Message]) -> String {
    input
        .iter()
        .map(Message::text_content)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map a clean ACP turn outcome to a terminal cause.
fn end_cause(reason: TerminationReason) -> EndCause {
    match reason {
        TerminationReason::NaturalEnd => EndCause::NaturalEnd,
        TerminationReason::Cancelled => EndCause::Cancelled,
        TerminationReason::Refusal => EndCause::Stopped("agent refused".to_string()),
        TerminationReason::Error => EndCause::Error(Failure::Inference {
            code: "acp_error".to_string(),
            message: "agent reported an error".to_string(),
        }),
        TerminationReason::TimedOut => EndCause::Stopped("turn deadline exceeded".to_string()),
    }
}

/// Map a classified ACP failure to a terminal cause. The failure's prompt is
/// committed as an assistant message (below) so the run surface explains it.
fn failure_cause(failure: &AcpFailure) -> EndCause {
    match failure.termination() {
        TerminationReason::TimedOut => EndCause::Stopped("turn deadline exceeded".to_string()),
        TerminationReason::Refusal => EndCause::Stopped("agent refused".to_string()),
        _ => EndCause::Error(Failure::Inference {
            code: "acp_failure".to_string(),
            message: failure.message.clone(),
        }),
    }
}

#[async_trait]
impl RunExecutor for AcpRunExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        // The supervisor interrupts the opaque CLI turn when the cancel future
        // resolves, and the ACP permission flow parks a turn on an authorization
        // decision. Mid-turn input steer is not delivered into the CLI, so `wait`
        // is `Auth`, not `Both`.
        ExecutorCapabilities {
            cancellation: Cancellation::RemoteAbort,
            wait: Wait::Auth,
        }
    }

    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        // Restore the CLI's portable session-home before launch and harvest it after,
        // so a local-dir CLI's session recovers across directories/machines. A
        // Gateway (server-side) or stateless adapter has no local session-home → the
        // resolution returns `None` and this is a no-op (as is the default provider).
        let home = self.session_home_binding(&activation);
        if let Some((key, plan)) = &home {
            self.session_home.restore(key, plan).await;
        }
        let result = self.drive(activation, context).await;
        if let Some((key, plan)) = &home {
            self.session_home.harvest(key, plan).await;
        }
        result
    }
}

impl AcpRunExecutor {
    /// Resolve this run's portable session-home binding from the ACP CLI catalog:
    /// the thread+adapter key and the harvest plan, or `None` when the backend is
    /// not an ACP CLI or the CLI's session is server-side / absent (not `LocalDir`).
    fn session_home_binding(
        &self,
        activation: &RunActivation,
    ) -> Option<(SessionHomeKey, SessionHomePlan)> {
        let Backend::Acp { cli } =
            Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref)
        else {
            return None;
        };
        let row = acp_cli(&cli)?;
        let SessionPersistence::LocalDir {
            session_subpath,
            keyed_by,
        } = row.session_persistence
        else {
            return None;
        };
        Some((
            SessionHomeKey {
                thread_id: activation.thread_id.0.clone(),
                adapter: cli,
            },
            SessionHomePlan {
                config_home_env: row.config_home_env.to_string(),
                session_subpath: session_subpath.to_string(),
                exclude: row
                    .retained_paths
                    .iter()
                    .map(|p| (*p).to_string())
                    .collect(),
                keyed_by_cwd: matches!(keyed_by, SessionKey::Cwd),
            },
        ))
    }

    async fn drive(&self, activation: RunActivation, context: RuntimeRunContext) -> Result<Phase> {
        // Lifecycle bring-up (observed for UI progress): an npx-wrapped adapter may
        // dynamically install on a cold cache (the slow step) before it launches.
        // Scope events to the thread so a per-session UI channel routes them.
        let scope = activation.thread_id.0.clone();
        let launch_sink = self.launch_sink(&scope);
        if dynamic_install_backend(&activation) {
            notify(
                launch_sink,
                AcpLaunchEvent::stage(AcpLaunchStage::Installing),
            );
        }
        notify(
            launch_sink,
            AcpLaunchEvent::stage(AcpLaunchStage::Launching),
        );

        // The host opens the channel (sandbox launch / remote dial). A failure here
        // is a launch/config fault → classify at the Initialize stage.
        let mut session = match self.source.open(&activation).await {
            Ok(session) => session,
            Err(open) => {
                notify(
                    launch_sink,
                    AcpLaunchEvent::with_detail(AcpLaunchStage::Failed, open.0.clone()),
                );
                let failure = classify_error(Stage::Initialize, &RawAcpError::message(open.0));
                return finish_failure(&context, &activation, &failure).await;
            }
        };

        // ADR-0054 P4: drive turns in a boundary loop. After each turn the shared
        // `evaluate_boundary` drains any live-inbox steer; queued input becomes the
        // next turn's prompt and the CLI relaunches (ACP is per-turn — R7), so
        // steer/redirect reaches external-CLI runs. Messages accumulate and commit
        // once at the terminal phase, matching the executor's single-commit model.
        let run_id = activation.run_id.clone();
        let model_ref = activation
            .snapshot
            .resolved_spec
            .model_binding
            .model_ref
            .clone();
        let mut prompt = prompt_of(&activation.input);
        let mut committed: Vec<Message> = Vec::new();
        // The ACP session id, carried across the per-turn relaunches so a resumed
        // turn reloads the CLI's own session (`session/load`) instead of starting
        // fresh — context survives the relaunch (the newline stand-in leaves it
        // `None`, so it always starts fresh, unchanged from before).
        let mut acp_session_id: Option<String> = None;
        // The run's token usage, accumulated across turns and committed as thread
        // state at the terminal phase (matching the native engine's `__usage`).
        let mut run_usage = TokenUsage::default();

        loop {
            let mut appender = CollectingAppender::default();
            let (_tx, mut injections) = tokio::sync::mpsc::channel::<Injection>(1);
            let cancel = async {
                match &context.cancellation {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };

            let process = session.process.clone();
            let mut config = TurnConfig::new(self.permission.as_ref());
            config.session_id = acp_session_id.take();
            config.session_mode = self.session_mode.clone();
            config.session_cwd = session.workspace_cwd.clone();
            let outcome = Supervisor::supervise_with_config(
                session.channel.as_mut(),
                process.as_ref(),
                &prompt,
                &mut appender,
                cancel,
                &mut injections,
                self.policy,
                session.codec,
                &mut config,
                launch_sink,
            )
            .await;
            // Keep the negotiated session id for the next relaunched turn, and fold
            // this turn's token usage into the run total.
            acp_session_id = config.session_id.take();
            run_usage = run_usage.saturating_add(appender.usage);

            let reason = match outcome {
                Ok(reason) => reason,
                // A driver error mid-turn: classify it (oversight taxonomy), surface
                // its prompt, commit everything so far + the error turn, and end. No
                // retry/reschedule — that is a host concern above us.
                Err(err) => {
                    let failure = classify_from_acp_error(&err);
                    committed.extend(appender.messages);
                    committed.push(Message::text(
                        MessageId(format!("acp-err-{}", committed.len() + 1)),
                        Role::Assistant,
                        failure.prompt(),
                    ));
                    let phase = Phase::Ended(failure_cause(&failure));
                    commit(
                        &context,
                        &activation.thread_id,
                        run_id.clone(),
                        committed,
                        &phase,
                        None,
                        usage_state(&run_usage, &model_ref),
                    )
                    .await?;
                    return Ok(phase);
                }
            };
            committed.extend(appender.messages);

            // The safe boundary, shared with the native engine: fold queued live
            // steer into the next turn, or end.
            match evaluate_boundary(&context, &run_id, &committed) {
                BoundaryOutcome::Continue { fold } => {
                    prompt = prompt_of(&fold);
                    committed.extend(fold);
                    // Relaunch the CLI for the next turn (ACP is per-turn).
                    session = match self.source.open(&activation).await {
                        Ok(session) => session,
                        Err(open) => {
                            let failure =
                                classify_error(Stage::Initialize, &RawAcpError::message(open.0));
                            let phase = Phase::Ended(failure_cause(&failure));
                            commit(
                                &context,
                                &activation.thread_id,
                                run_id.clone(),
                                committed,
                                &phase,
                                None,
                                usage_state(&run_usage, &model_ref),
                            )
                            .await?;
                            return Ok(phase);
                        }
                    };
                }
                // Operator pause (ADR-0054 P5/U2): commit any in-flight steer that
                // rode out with the park, then park durably on a no-tool waiting
                // ticket — the same clean commit-then-park the native engine does,
                // resumed by an explicit operator resume, not a tool result.
                BoundaryOutcome::Park { fold, reason } => {
                    committed.extend(fold);
                    let phase = Phase::Waiting;
                    let ticket = pause_ticket(&activation, &run_id, reason);
                    commit(
                        &context,
                        &activation.thread_id,
                        run_id.clone(),
                        committed,
                        &phase,
                        Some(ticket),
                        usage_state(&run_usage, &model_ref),
                    )
                    .await?;
                    return Ok(phase);
                }
                // Idle: no queued input, no pause — the turn's own reason is terminal.
                BoundaryOutcome::Idle => {
                    let phase = Phase::Ended(end_cause(reason));
                    commit(
                        &context,
                        &activation.thread_id,
                        run_id.clone(),
                        committed,
                        &phase,
                        None,
                        usage_state(&run_usage, &model_ref),
                    )
                    .await?;
                    return Ok(phase);
                }
            }
        }
    }
}

/// Classify a bridge/supervisor [`AcpError`] into a neutral [`AcpFailure`]. A
/// truncated stream or an IO drop mid-turn is the backend cutting the turn
/// (Prompt stage); a malformed frame is a permanent adapter fault.
fn classify_from_acp_error(err: &AcpError) -> AcpFailure {
    classify_error(Stage::Prompt, &RawAcpError::message(err.to_string()))
}

/// Commit a terminal failure that occurred before/instead of a turn (open fault):
/// one assistant message with the prompt, plus the terminal phase.
async fn finish_failure(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    failure: &AcpFailure,
) -> Result<Phase> {
    let phase = Phase::Ended(failure_cause(failure));
    let messages = vec![Message::text(
        MessageId("acp-err-1".to_string()),
        Role::Assistant,
        failure.prompt(),
    )];
    commit(
        context,
        &activation.thread_id,
        activation.run_id.clone(),
        messages,
        &phase,
        None,
        Vec::new(),
    )
    .await?;
    Ok(phase)
}

/// Commit the turn's messages + terminal phase through the one boundary (G13). A
/// `Phase::Waiting` park carries its resumable [`WaitingTicket`]; a terminus passes
/// `None`. `state` carries the run's accumulated token usage (empty when none).
async fn commit(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    messages: Vec<Message>,
    phase: &Phase,
    waiting: Option<WaitingTicket>,
    state: Vec<StateCommand>,
) -> Result<()> {
    if let Some(coordinator) = &context.commit {
        awaken_agent_contract::commit::commit_run(
            coordinator.as_ref(),
            thread_id,
            &run_id,
            messages,
            phase.clone(),
            waiting,
            state,
        )
        .await
        .map_err(|e| Error::Commit(e.to_string()))?;
    }
    Ok(())
}

/// The committed thread-state command recording `usage` under the bound model —
/// the same `__usage` `ThreadUsage` tally the native engine writes, so a session's
/// ACP usage is readable from thread state exactly like a native run's. Empty when
/// no usage was reported (nothing to record).
fn usage_state(usage: &TokenUsage, model_ref: &str) -> Vec<StateCommand> {
    if *usage == TokenUsage::default() {
        return Vec::new();
    }
    let mut tally = ThreadUsage::default();
    tally.record(model_ref, *usage);
    vec![StateCommand::set(
        Scope::Thread,
        MergePolicy::Commutative,
        THREAD_USAGE_STATE_KEY,
        serde_json::to_value(tally).expect("thread usage serializes"),
    )]
}

/// A no-tool waiting ticket for an operator pause (ADR-0054): the ACP run parks
/// with no pending tool and no call id, correlated by run id, resumed by an
/// explicit operator resume rather than a tool result. Mirrors the native
/// engine's `pause_ticket` so a paused ACP run is resumable identically.
fn pause_ticket(
    activation: &RunActivation,
    run_id: &RunId,
    reason: WaitingReason,
) -> WaitingTicket {
    WaitingTicket {
        correlation_id: run_id.0.clone(),
        run_id: run_id.clone(),
        thread_id: activation.thread_id.clone(),
        snapshot_id: activation.snapshot.id.0.clone(),
        catalog_fingerprint: activation
            .snapshot
            .resolved_spec
            .catalog_fingerprint
            .0
            .clone(),
        reason,
        call_id: None,
        pending_tool: None,
        deadline_ms: None,
    }
}

/// Bridges the ACP driver's [`PermissionResolver`] port onto the single neutral
/// [`PermissionPolicy`] authority (G21). It projects the wire ask into a neutral
/// [`PermissionContext`], asks the policy, and maps the decision back to a wire
/// verdict — so an external CLI's tool requests are decided by the same policy that
/// governs native tools. `Ask` (out-of-band/HITL) has no synchronous answer over
/// the held ACP turn yet, so it fails safe to `Deny` (a turn-holding HITL resolve
/// is a follow-up); `Allow`/`Deny` pass straight through.
struct NeutralPermissionResolver {
    policy: Arc<dyn PermissionPolicy>,
}

#[async_trait]
impl PermissionResolver for NeutralPermissionResolver {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
        let ctx = PermissionContext {
            tool_id: ask.tool.clone(),
            call_id: ask.call_id.clone(),
            arguments: ask.arguments.clone(),
        };
        match self.policy.decide(&ctx).await {
            PermissionDecision::Allow => PermissionVerdict::Allow,
            PermissionDecision::Deny { .. } | PermissionDecision::Ask { .. } => {
                PermissionVerdict::Deny
            }
        }
    }
}

/// A [`RunFactAppender`] that projects an ACP turn's facts into committable neutral
/// messages, enforcing the monotonic-seq contract. Assistant text, tool calls, and
/// tool results each become a message so the committed transcript mirrors the
/// external agent's turn (a tool call is an assistant `ToolUse`; its result is a
/// `Role::Tool` `ToolResult` addressed to that call). `TurnEnd` carries no message —
/// it is returned as the turn's reason.
#[derive(Default)]
struct CollectingAppender {
    last: u64,
    messages: Vec<Message>,
    /// The turn's token usage, accumulated from any `Usage` events (kept out of the
    /// committed messages — it lands as thread state, matching the native engine).
    usage: TokenUsage,
}

#[async_trait]
impl RunFactAppender for CollectingAppender {
    async fn append(
        &mut self,
        seq: u64,
        event: &AgentEvent,
    ) -> std::result::Result<(), AppendError> {
        use awaken_agent_contract::agent::content::ContentBlock;

        if seq <= self.last {
            return Err(AppendError::NonMonotonic {
                got: seq,
                last: self.last,
            });
        }
        self.last = seq;
        match event {
            AgentEvent::Message { text } => self.messages.push(Message::text(
                MessageId(format!("acp-{seq}")),
                Role::Assistant,
                text.clone(),
            )),
            AgentEvent::ToolCall { id, name, input } => self.messages.push(Message {
                id: MessageId(format!("acp-{seq}")),
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    tool_use_id(id, seq),
                    name.clone(),
                    input.clone(),
                )],
            }),
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                // The neutral `ToolResult` has no error flag, so a failed call's
                // error surfaces in the result text (marked) rather than being lost.
                let body = if *is_error {
                    format!("[tool error] {content}")
                } else {
                    content.clone()
                };
                self.messages.push(Message {
                    id: MessageId(format!("acp-{seq}")),
                    role: Role::Tool,
                    content: vec![ContentBlock::tool_result(
                        tool_use_id(id, seq),
                        vec![ContentBlock::text(body)],
                    )],
                });
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                self.usage = self.usage.saturating_add(TokenUsage {
                    prompt_tokens: *prompt_tokens,
                    completion_tokens: *completion_tokens,
                    cache_read_tokens: *cache_read_tokens,
                    cache_creation_tokens: *cache_creation_tokens,
                });
            }
            AgentEvent::TurnEnd { .. } => {}
        }
        Ok(())
    }
}

/// The neutral tool-use id for an ACP tool call: the ACP `tool_call_id` when the
/// agent supplied one, else a per-seq fallback so a call and its result still
/// correlate within the turn.
fn tool_use_id(acp_id: &str, seq: u64) -> String {
    if acp_id.is_empty() {
        format!("acp-tool-{seq}")
    } else {
        acp_id.to_string()
    }
}

/// Routes each run to the native / ACP / remote executor by its [`Backend`] (R3).
/// Each peer is a `RunExecutor` (the remote one held as a trait object, so this
/// crate needs no dependency on the A2A executor), so the match on the backend sum
/// is the entire "which runtime serves this agent" mechanism — every backend is
/// reachable over every wire adapter exactly like a native one.
pub struct DispatchRunExecutor {
    native: Arc<dyn RunExecutor>,
    acp: Arc<dyn RunExecutor>,
    remote: Arc<dyn RunExecutor>,
}

impl DispatchRunExecutor {
    pub fn new(
        native: Arc<dyn RunExecutor>,
        acp: Arc<dyn RunExecutor>,
        remote: Arc<dyn RunExecutor>,
    ) -> Self {
        Self {
            native,
            acp,
            remote,
        }
    }
}

#[async_trait]
impl RunExecutor for DispatchRunExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
            Backend::Native => self.native.execute(activation, context).await,
            Backend::Acp { .. } => self.acp.execute(activation, context).await,
            Backend::Remote { .. } => self.remote.execute(activation, context).await,
        }
    }
}

mod acp_cli;
mod subprocess;
pub use acp_cli::{
    AcpCli, McpInterface, ModelDelivery, ResolvedModel, SessionKey, SessionPersistence, acp_cli,
    is_dynamic_install, known_acp_clis,
};
pub use subprocess::{AcpLaunch, LaunchResolver, ProjectingChannelSource, SubprocessChannelSource};

#[cfg(test)]
mod tests;
