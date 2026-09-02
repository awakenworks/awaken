//! Neutral value types and the [`SessionRuntime`] interface the adapter drives:
//! pending tools, Run outcomes, capabilities, Session initialization, and Run errors.

mod pending;
mod run_coordination;

pub use pending::Pending;
pub use run_coordination::{
    DelegatedRun, DelegatedRunSnapshot, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
};

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::{RunLifecycleCursor, RunLifecyclePage};

/// The result of running one settled Step (a new Run, or a resume).
///
/// `state` reuses the run's sole lifecycle authority instead of storing a second
/// terminal classification. It is private: callers can construct only
/// `Awaiting` or `Ended` outcomes, so `Running` cannot escape a step boundary.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub new_messages: Vec<Message>,
    /// Exact identity of the committed Run when supplied by a durable runtime.
    /// Protocol projections use it only to deduplicate the same terminal fact
    /// observed locally and through the committed lifecycle feed.
    run_id: Option<RunId>,
    state: RunState,
    pending: Option<Pending>,
    await_reason: Option<awaken_agent_contract::agent::awaiting::AwaitReason>,
    delegated_runs: Vec<DelegatedRun>,
}

impl StepOutcome {
    #[must_use]
    pub fn awaiting(messages: Vec<Message>, pending: Option<Pending>) -> Self {
        Self {
            new_messages: messages,
            run_id: None,
            state: RunState::Awaiting,
            pending,
            await_reason: None,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn ended(messages: Vec<Message>, cause: EndCause) -> Self {
        Self {
            new_messages: messages,
            run_id: None,
            state: RunState::Ended(cause),
            pending: None,
            await_reason: None,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &RunState {
        &self.state
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    #[must_use]
    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    /// Neutral terminal fact derived from the authoritative Run state.
    #[must_use]
    pub fn terminal_event(&self) -> awaken_agent_contract::event::Fact {
        awaken_agent_contract::event::terminal(
            &self.state,
            self.pending
                .as_ref()
                .map(|pending| (pending.tool_use_id.as_str(), pending.client_executed)),
        )
    }

    #[must_use]
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    #[must_use]
    pub fn with_await_reason(
        mut self,
        reason: awaken_agent_contract::agent::awaiting::AwaitReason,
    ) -> Self {
        self.await_reason = Some(reason);
        self
    }

    #[must_use]
    pub fn await_reason(&self) -> Option<&awaken_agent_contract::agent::awaiting::AwaitReason> {
        self.await_reason.as_ref()
    }

    #[must_use]
    pub fn failure(&self) -> Option<&Failure> {
        match &self.state {
            RunState::Ended(EndCause::Error(failure)) => Some(failure),
            RunState::Running | RunState::Awaiting | RunState::Ended(_) => None,
        }
    }

    #[must_use]
    pub fn with_delegated_runs(mut self, delegated_runs: Vec<DelegatedRun>) -> Self {
        self.delegated_runs = delegated_runs;
        self
    }

    #[must_use]
    pub fn delegated_runs(&self) -> &[DelegatedRun] {
        &self.delegated_runs
    }
}

/// A human-in-the-loop tool decision, delivered by `user.tool_confirmation`.
pub use awaken_agent_contract::agent::awaiting::PermissionDecision as ToolPermissionDecision;

/// The advertised capability surface echoed in a session's agent object. The adapter
/// reads this once at session creation so the public agent object reports what the run
/// can actually do. This is neutral data; the public Managed Agents wire shaping (the
/// built-in `agent_toolset` fold, `custom` tools, `skills`, `multiagent`) lives in
/// the Managed protocol projector. Deliberately absent: MCP servers (the host wires none) and
/// session resources (the host has no Files-API-backed resource to reference yet), so
/// those wire fields stay empty until a real producer exists.
#[derive(Default)]
pub struct AgentCapabilities {
    /// The registered built-in tools (the hand toolset). Each names a tool of the
    /// versioned agent toolset and whether its calls require human confirmation.
    pub builtin_tools: Vec<BuiltinTool>,
    /// Client-executed tools: the caller runs them and returns the result.
    pub custom_tools: Vec<CustomTool>,
    /// Skills the agent offers (activated on demand, not model-visible as tools).
    pub skills: Vec<String>,
    /// Delegate agents the agent may coordinate (the multiagent roster).
    pub delegates: Vec<String>,
}

/// One registered built-in tool: its name and whether calls require confirmation
/// (`ask` = the permission gate awaits the call for an approval).
pub struct BuiltinTool {
    pub name: String,
    pub ask: bool,
}

/// One client-executed custom tool: the model-visible name, description, and input
/// schema the runtime pins for it.
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One evaluation round of a goal: the agent's revision messages committed this
/// round (empty when grading the existing deliverable), and the verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub description: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// Neutral infrastructure failure committed by the Outcome aggregate. The
/// optional source Run correlates an ordinary lifecycle fact so adapters can
/// project one public error without parsing identities or duplicating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeFailure {
    pub code: String,
    pub message: String,
    pub source_run_id: Option<RunId>,
}

/// One terminal Outcome rebuilt from committed Thread truth. A rubric `failed`
/// remains a completed report; only execution/schema/persistence faults use the
/// infrastructure-error branch.
#[derive(Debug, Clone, PartialEq)]
pub enum CommittedOutcomeProjection {
    Completed(OutcomeReport),
    Errored(OutcomeFailure),
}

/// The next durable boundary reached while driving an Outcome. Awaiting keeps
/// the active aggregate in Thread state; completed carries its terminal report.
#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeDrive {
    Awaiting,
    Completed(OutcomeReport),
}

/// Immutable baseline and Resource inputs a new Session provisions before MCP
/// generations are staged through the separate exact-generation port.
#[derive(Debug, Clone)]
pub struct SessionInit {
    /// Trusted owning workspace resolved by the platform edge before runtime
    /// preparation. Resource stores never infer or hard-code it.
    pub workspace_id: String,
    pub agent_id: String,
    /// Exact roster frozen by the published Agent.
    pub delegate_ids: Vec<String>,
    /// Complete Session-local replacement of the published tool configuration.
    /// `None` inherits the Agent snapshot; `Some(default())` explicitly clears
    /// all toolsets and client-executed tools.
    pub tools: Option<crate::SessionToolConfiguration>,
    /// Exact generation owned by `SessionResourceState`; zero is reserved for
    /// legacy callers. Runtime must preserve this independently from the Session
    /// root revision when it installs `resources`.
    pub resource_revision: u64,
    /// The session's mounted resources (ADR-0038), parsed from the wire `resources[]`:
    /// files, memory stores, repos. The host realizes each into the run's sandbox and
    /// appends a prompt fragment to the system prompt (A3a). Empty = no mounts.
    pub resources: crate::ResolvedSessionResources,
    /// Exact publication-frozen runtime model coordinate. Public Managed model
    /// syntax is retained in the Session baseline and never enters this port.
    pub model: Option<String>,
    /// Exact backend projected from the immutable Agent publication (R3). The
    /// Session baseline copies it for recovery; request metadata cannot override it.
    pub runtime: Option<String>,
    /// The one exact frozen Environment authority. Runtime projects its network,
    /// Sandbox, and credential-realization facts without deriving a second policy
    /// or accepting a late override.
    pub environment: crate::EnvironmentSnapshot,
}

/// Pure, prospective Sandbox layout compiled before a Session root or Resource
/// manifest is persisted.
///
/// Resource bindings deliberately remain in their neutral typed form here. The
/// Runtime implementation owns the one File/Memory/Repository path projection
/// and evaluates it together with provider-owned mounts and environment paths;
/// protocol adapters must not approximate that final layout themselves.
#[derive(Debug, Clone)]
pub struct SessionSandboxLayout {
    /// Owning workspace used by the Runtime's sole exact publication/provider
    /// resolver. Keeping it inside this prospective value prevents callers from
    /// selecting a provider with a parallel ambient workspace argument.
    pub workspace_id: String,
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub runtime_placement: crate::SessionRuntimePlacement,
    pub model_override: Option<crate::SessionModelOverride>,
    /// Exact backend frozen by the prospective baseline, when already known.
    pub runtime: Option<String>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub environment: crate::EnvironmentSnapshot,
    pub resources: Vec<awaken_resource_contract::InputBinding>,
}

/// A queued live-inbox message on the Session's in-flight Run. `id` is the
/// runtime's queue identity — targetable until the engine consumes the entry.
/// Neutral: the adapter projects it onto the wire snapshot at the route.
#[derive(Debug, Clone)]
pub struct LiveInboxEntry {
    pub id: u64,
    pub content: Vec<ContentBlock>,
}

/// The session's live-inbox resource: the editable queue of messages addressed
/// to the in-flight Run. `active: false` means no native Run is executing (the
/// queue shows empty; sends go through the normal event path instead). Neutral —
/// the wire shaping (`Json`) lives in the `ext::live_inbox` route.
#[derive(Debug, Clone)]
pub struct LiveInboxSnapshot {
    pub active: bool,
    pub version: u64,
    pub messages: Vec<LiveInboxEntry>,
}

impl LiveInboxSnapshot {
    pub fn inactive() -> Self {
        Self {
            active: false,
            version: 0,
            messages: Vec::new(),
        }
    }
}

/// Why a live-inbox operation failed. Mirrors the runtime contract's edit
/// errors, plus `Inactive` for "no native Run in flight on this Session".
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveInboxError {
    #[error("no Run is in flight; send the message as a normal event")]
    Inactive,
    #[error("no queued message with that id")]
    UnknownMessage,
    #[error("proposed order does not match the current queue")]
    StaleOrder,
}

/// Protocol-neutral application failure for the editable in-flight inbox.
/// Wire adapters decide how these three domain outcomes map to their envelopes.
#[derive(Debug, thiserror::Error)]
pub enum LiveInboxApplicationError {
    #[error("session not found")]
    NotFound,
    #[error(transparent)]
    Edit(#[from] LiveInboxError),
    #[error("live inbox unavailable: {0}")]
    Unavailable(String),
}

/// Driving port for Awaken's live-inbox protocol. The Session application is
/// the sole owner: protocol adapters supply the trusted Workspace scope and do
/// not perform a second ownership lookup or reach through protocol state.
#[async_trait]
pub trait LiveInboxApplication: Send + Sync {
    async fn snapshot(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError>;

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError>;

    async fn remove(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: u64,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn replace(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn reorder(
        &self,
        workspace_id: &str,
        session_id: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxApplicationError>;
}

/// Public exact-generation realization port owned by the Session application
/// boundary. Local Host relay/connection code and downstream platform gateways
/// implement this same contract; neither becomes Session desired-state authority.
#[async_trait]
pub trait McpAttachmentRealizer: Send + Sync {
    /// Stage one exact generation without making it tool-visible.
    async fn stage_mcp_attachment(
        &self,
        _request: crate::StageMcpAttachment,
    ) -> Result<crate::McpRealizationReceipt, RunError> {
        Err(RunError::classified(
            "mcp_runtime_unsupported",
            "runtime does not support generation-fenced MCP realization",
        ))
    }

    /// Publish one durably active exact generation at a safe boundary.
    async fn publish_mcp_generation(
        &self,
        _generation: crate::McpGenerationRef,
    ) -> Result<(), RunError> {
        Err(RunError::classified(
            "mcp_runtime_unsupported",
            "runtime does not support generation-fenced MCP publication",
        ))
    }

    async fn publish_mcp_generation_receipt(
        &self,
        generation: crate::McpGenerationRef,
    ) -> Result<crate::McpProjectionReceipt, RunError> {
        self.publish_mcp_generation(generation.clone()).await?;
        Ok(crate::McpProjectionReceipt::new(
            generation,
            crate::McpProjectionEffectKind::Publish,
        ))
    }

    /// Hide and dispose one exact generation idempotently.
    async fn drain_mcp_generation(
        &self,
        _generation: crate::McpGenerationRef,
    ) -> Result<(), RunError> {
        Err(RunError::classified(
            "mcp_runtime_unsupported",
            "runtime does not support generation-fenced MCP drain",
        ))
    }

    async fn drain_mcp_generation_receipt(
        &self,
        generation: crate::McpGenerationRef,
    ) -> Result<crate::McpProjectionReceipt, RunError> {
        self.drain_mcp_generation(generation.clone()).await?;
        Ok(crate::McpProjectionReceipt::new(
            generation,
            crate::McpProjectionEffectKind::Drain,
        ))
    }
}

/// One best-effort subscription to a logical Session Thread's neutral live
/// observations. The transport/runtime adapter owns buffering and fan-out; the
/// protocol sees neither Tokio channels nor Host types.
#[async_trait]
pub trait SessionThreadLiveSubscription: Send {
    /// Wait for the next live observation. `None` means the producer has closed;
    /// lag is implementation-defined and never affects committed truth.
    async fn recv(
        &mut self,
    ) -> Result<Option<awaken_agent_contract::stream::event::Observation>, RunError>;
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime. MCP realization is kept on
/// [`McpAttachmentRealizer`] because it has a distinct hot-attachment lifecycle.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        _thread: &str,
        layout: &crate::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        if layout.resources.iter().any(|binding| {
            matches!(
                binding.target,
                awaken_resource_contract::InputResourceId::Repository(_)
            )
        }) {
            return Err(RunError::unavailable(
                "runtime does not implement Repository Sandbox-layout validation",
            ));
        }
        Ok(())
    }

    /// Install one complete Control-frozen Session projection. Dispatch and
    /// realization are explicit modes of this single port; callers must never
    /// lower publication, baseline, request context, Environment, or lease
    /// fields independently.
    async fn install_session_projection(
        &self,
        _thread: &str,
        _projection: crate::FrozenSessionProjection,
        _mode: crate::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "runtime does not implement complete Session projection installation",
        ))
    }

    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        _thread: &str,
        _lease: crate::SessionRealizationLease,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "runtime does not implement Session realization lease renewal",
        ))
    }

    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(&self, _sink: Arc<dyn SessionEnvironmentBindingSink>) {}

    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        _command: crate::AdmitSessionRun,
    ) -> Result<crate::SessionRunReservation, RunError> {
        Err(RunError::unavailable(
            "runtime does not implement durable Session Run reservations",
        ))
    }

    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        _delivery: crate::SessionRunDelivery,
    ) -> Result<crate::SessionRunActivation, RunError> {
        Err(RunError::unavailable(
            "runtime does not implement durable Session Run activation",
        ))
    }

    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        _admission: crate::AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable(
            "runtime does not implement durable Session Run observation",
        ))
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        _session_id: &str,
        _run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        Err(RunError::unavailable(
            "runtime does not implement Session Run recovery",
        ))
    }
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        Ok(Vec::new())
    }

    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        _command: crate::CoordinatedRunCommand,
    ) -> Result<crate::SessionAgentMessageReceipt, RunError> {
        Err(RunError::unavailable(
            "asynchronous Agent coordination is unsupported",
        ))
    }

    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    async fn coordinated_threads(
        &self,
        _session_id: &str,
    ) -> Result<Vec<crate::CoordinatedThreadLink>, RunError> {
        Ok(Vec::new())
    }

    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError> {
        Ok(None)
    }

    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        _command: crate::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "asynchronous Agent report continuation is unsupported",
        ))
    }

    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        _session_id: &str,
        _child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "coordinated Session Thread interruption is unsupported",
        ))
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        _command: &crate::SessionThreadToolReplyCommand,
    ) -> Result<crate::SessionThreadToolReplyFence, RunError> {
        Err(RunError::unavailable(
            "Session Thread tool reply fencing is unsupported",
        ))
    }

    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        _delivery: crate::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "Session Thread tool replies are unsupported",
        ))
    }

    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        _delivery: crate::SessionThreadToolReplyDelivery,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable(
            "Session Thread tool reply observation is unsupported",
        ))
    }

    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        _delivery: SessionBudgetResumeDelivery,
    ) -> Result<SessionBudgetResumeDisposition, RunError> {
        Err(RunError::unavailable(
            "budget-paused Run resumption is unsupported",
        ))
    }

    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        _session_id: &str,
    ) -> Result<Vec<SessionBudgetResumeTicket>, RunError> {
        Err(RunError::unavailable(
            "budget-paused Run discovery is unsupported",
        ))
    }

    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<DelegatedRunSnapshot, RunError> {
        self.interrupt(thread).await?;
        Ok(DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(thread, thread)
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    /// Run one user request on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError>;

    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        self.run(agent, thread, content).await
    }

    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.run(agent, thread, content).await
    }

    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        _data_subject_id: Option<String>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.run_streaming(agent, thread, content, sink).await
    }

    /// Answer a built-in tool the run awaits (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError>;

    /// Deliver a client-executed tool's result to the awaiting run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError>;

    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        _thread: &str,
        _tools: crate::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        Err(RunError::internal("session tool runtime is unsupported"))
    }

    /// Adopt a previously persisted environment before reopening a Session.
    /// Implementations must validate that the binding belongs to `thread` and
    /// fail closed when it is malformed, unavailable, or owned elsewhere.
    async fn adopt_session_environment(
        &self,
        _agent: &str,
        _thread: &str,
        _binding: &str,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        _thread: &str,
        _operation: &crate::SessionEnvironmentOperation,
        _source_effect_id: &str,
        _source_binding: &str,
        _generation: &crate::SandboxGeneration,
        _expected_mcp_generations: &[crate::McpGenerationRef],
    ) -> Result<crate::QuiescenceReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement Session environment quiescence",
        ))
    }

    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        _thread: &str,
        _request: crate::SandboxCheckpointRequest,
    ) -> Result<crate::CheckpointReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement Session environment checkpointing",
        ))
    }

    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        _thread: &str,
        _preparation: &crate::SourceReleasePreparationEffect,
        _generation: &crate::SandboxGeneration,
        _source_binding: &str,
    ) -> Result<crate::SourceReleasePreparedReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement checkpoint source release preparation",
        ))
    }

    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        _thread: &str,
        _disposal: &crate::SourceReleaseDisposal,
    ) -> Result<crate::SourceDisposedReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement prepared checkpoint source disposal",
        ))
    }

    /// Restore a distinct environment from the exact durable checkpoint.
    async fn restore_checkpointed_session_environment(
        &self,
        _request: crate::SandboxRestoreRequest,
    ) -> Result<crate::RestoreReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement Session environment restore",
        ))
    }

    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        _checkpoint: &crate::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable_classified(
            "session_environment_checkpoint_unsupported",
            "Runtime does not implement Session environment checkpoint deletion",
        ))
    }

    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        _workspace_id: &str,
        _skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError> {
        Ok(Vec::new())
    }

    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(&self, _thread: &str, _model: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Apply one complete, aggregate-authored Resource transition. Live
    /// add/update/delete, rollback, and restart restoration converge here;
    /// Runtime never guesses the prior generation from a process-local cache or
    /// re-resolves Agent defaults/current Resource configuration.
    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _transition: &crate::SessionResourceTransition,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, _thread: &str) -> Result<bool, RunError> {
        Ok(false)
    }

    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. The fail-closed default keeps lightweight
    /// runtimes from silently becoming a second trusted command source.
    async fn validate_session_agent_message_source(
        &self,
        _command: &crate::SessionAgentMessageCommand,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "durable Agent message source validation is unsupported",
        ))
    }

    /// Read one internally consistent logical Thread prefix through the parent
    /// Session's physical commit partition. The primary Thread is addressed by
    /// `session_id == thread_id`; coordinated children use their own logical
    /// Thread id. Implementations must reuse
    /// the Runtime's authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// rather than assembling messages, state, and tickets from separate reads.
    /// `None` means the logical Thread has no committed Run.
    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Err(RunError::unavailable(
            "consistent coordinated Thread recovery is unsupported",
        ))
    }

    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
        run_id: &RunId,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    /// Read committed Run lifecycle facts after `cursor`. The commit log remains
    /// the sole authority; Managed uses this projection to observe Runs accepted
    /// through AI SDK, AG-UI, A2A, or another Coordinator replica.
    async fn committed_run_lifecycle(
        &self,
        _thread: &str,
        cursor: RunLifecycleCursor,
        _limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        Ok(RunLifecyclePage {
            events: Vec::new(),
            next_cursor: cursor,
        })
    }

    /// The Session's accumulated token usage across all Runs, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(&self, _thread: &str) -> Result<SessionUsage, RunError> {
        Ok(SessionUsage::default())
    }

    /// Read usage for one logical Thread from its parent Session partition.
    /// Global stores may use `thread_id` directly; filesystem/SQLite adapters
    /// must retain `session_id` as the physical read owner.
    async fn session_thread_usage(
        &self,
        _session_id: &str,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        self.session_usage(thread_id).await
    }

    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        Ok(awaken_agent_contract::ThreadDisposition::Active)
    }

    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable(
            "durable coordinated Thread archive is unsupported",
        ))
    }

    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, _thread: &str) -> bool {
        true
    }

    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }

    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        _command: crate::SessionRepositoryPublicationCommand,
        _lease: &crate::SessionRealizationLease,
    ) -> Result<crate::SessionRepositoryPublicationEffect, RunError> {
        Err(RunError::unavailable_classified(
            "session_repository_publication_effect_runtime_unsupported",
            "runtime does not implement realization-fenced terminal Repository publication",
        ))
    }

    /// Make every source-dependent effect durable under the exact aggregate
    /// preparation fence without deleting the physical realization.
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        _effect: crate::SessionTerminalCleanupEffect,
        _authorization: crate::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<crate::SessionCleanupPreparation, RunError> {
        Err(RunError::unavailable_classified(
            "session_terminal_cleanup_preparation_runtime_unsupported",
            "runtime does not implement realization-fenced Session cleanup preparation",
        ))
    }

    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        _effect: &crate::SessionTerminalCleanupEffect,
    ) {
    }

    /// Execute the one aggregate-wide physical disposal after every
    /// preparation is durable. The typed effect carries the exact prepared
    /// predecessor plus the current live successor lease.
    async fn dispose_terminal_cleanup_for_effect(
        &self,
        _effect: crate::SessionTerminalCleanupDisposalEffect,
    ) -> Result<crate::SessionCleanupDisposalReceipt, RunError> {
        Err(RunError::unavailable_classified(
            "session_terminal_cleanup_disposal_runtime_unsupported",
            "runtime does not implement realization-fenced Session cleanup disposal",
        ))
    }

    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        _effect: &crate::SessionTerminalCleanupDisposalEffect,
    ) {
    }

    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        _session_id: &str,
        _lease: &crate::SessionRealizationLease,
    ) {
    }

    /// Install the frozen facts carried by one terminal assignment without
    /// realizing Resources, adopting an Environment, or writing an ordinary
    /// Environment receipt. Cleanup effects remain gated by the exact action
    /// and lease passed to the preparation/disposal ports above.
    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &crate::SessionTerminalCleanupAssignment,
    ) -> Result<(), RunError> {
        Err(RunError::unavailable_classified(
            "session_terminal_projection_runtime_unsupported",
            "runtime does not implement frozen terminal Session projection installation",
        ))
    }

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        _thread: &str,
        _outcome_id: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<u64, RunError> {
        Err(RunError::unavailable(
            "runtime does not implement durable Outcome preparation",
        ))
    }

    /// Convenience composition over the canonical prepare/continue phases.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        let outcome_id =
            crate::session_outcome_convenience_id(thread, description, rubric, max_iterations);
        let _ = self
            .prepare_outcome(thread, &outcome_id, description, rubric, max_iterations)
            .await?;
        self.continue_outcome(thread)
            .await?
            .ok_or_else(|| RunError::internal("prepared Outcome is not active"))
    }

    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(&self, _thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        Ok(None)
    }

    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        _thread: &str,
        _outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError> {
        Ok(None)
    }

    /// The live-inbox queue on `thread`'s in-flight Run. The default reports
    /// an inactive queue, so a host without live-inbox wiring is unaffected.
    async fn live_inbox_snapshot(&self, _thread: &str) -> LiveInboxSnapshot {
        LiveInboxSnapshot::inactive()
    }

    /// Queue a message onto `thread`'s in-flight Run; it is folded into the
    /// running transcript at the next safe boundary. Fails `Inactive` when no
    /// native Run is executing (the caller should send a normal event instead).
    async fn live_inbox_queue(
        &self,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Delete one queued (not yet consumed) message.
    async fn live_inbox_remove(&self, _thread: &str, _id: u64) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Replace one queued message's content, keeping its id and position.
    async fn live_inbox_replace(
        &self,
        _thread: &str,
        _id: u64,
        _content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Reorder the queue to exactly `order` (a full permutation of current ids).
    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        _order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;

    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    /// The capability surface visible to one prepared session. Runtimes whose
    /// catalogs are workspace-scoped override this; simple runtimes inherit the
    /// process-wide view for backwards compatibility.
    fn capabilities_for(&self, _thread: &str) -> AgentCapabilities {
        self.capabilities()
    }
}

/// Immediate durability boundary for a newly materialized Session environment.
/// The binding must commit before the runtime exposes that environment to work.
#[async_trait]
pub trait SessionEnvironmentBindingSink: Send + Sync {
    /// Atomically determine whether the durable aggregate owns this effect and
    /// whether the exact effect already committed. Internal runtime threads
    /// deliberately return `Unowned`; callers must not split this check into a
    /// separate existence read and a later authorization decision.
    async fn authorize(
        &self,
        _intent: &crate::SessionEnvironmentEffectIntent,
    ) -> Result<crate::SessionEnvironmentEffectAuthorization, RunError> {
        Err(RunError::classified(
            "session_environment_effect_authorization_unsupported",
            "Session Environment effects require an explicit durable authorization implementation",
        ))
    }

    /// Commit one authorized receipt and return the exact Store-read
    /// Environment authority. Runtime owner installation must consume this
    /// value rather than reconstructing generation state from local time.
    async fn persist(
        &self,
        receipt: crate::SessionEnvironmentReceipt,
    ) -> Result<crate::SessionEnvironmentState, RunError>;
}

/// A runtime failure. `kind` classifies who is at fault so the router can map it
/// to the right HTTP status: a `BadRequest` is the caller's (an unknown await, a
/// mismatched id, a wrong-binding resume); `Internal` is the runtime's.
#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
    /// Stable neutral fault code consumed by protocol transcoders. This keeps
    /// provider/MCP classification out of any one public adapter.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
    Unavailable,
}

impl RunError {
    /// A runtime-side failure (provider error, corrupt state) — maps to `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
            code: "internal".into(),
        }
    }

    /// A caller-side failure (bad id, wrong binding, no await) — maps to `400`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }

    pub fn bad_request_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
            code: code.into(),
        }
    }

    /// A temporary dependency/readiness failure — maps to `503` and is safe to retry.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Unavailable,
            code: "unavailable".into(),
        }
    }

    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Unavailable,
            code: code.into(),
        }
    }

    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
            code: code.into(),
        }
    }
}

/// The session-level token usage the managed wire reports (the port's neutral shape).
/// Cumulative across all Runs and models.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Exact per-model attribution used by list-cost accounting. Protocol
    /// projections continue to expose the cumulative totals above.
    pub by_model: std::collections::BTreeMap<String, SessionModelUsage>,
    pub active_seconds: u64,
    pub web_fetch_requests: u64,
    pub web_search_requests: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl From<awaken_runtime_contract::llm::ThreadUsage> for SessionUsage {
    fn from(attributed: awaken_runtime_contract::llm::ThreadUsage) -> Self {
        let total = attributed.total();
        Self {
            input_tokens: total.prompt_tokens,
            output_tokens: total.completion_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_creation_tokens: total.cache_creation_tokens,
            by_model: attributed
                .by_model
                .into_iter()
                .map(|(model, usage)| {
                    (
                        model,
                        SessionModelUsage {
                            input_tokens: usage.prompt_tokens,
                            output_tokens: usage.completion_tokens,
                            cache_read_tokens: usage.cache_read_tokens,
                            cache_creation_tokens: usage.cache_creation_tokens,
                        },
                    )
                })
                .collect(),
            active_seconds: 0,
            web_fetch_requests: 0,
            web_search_requests: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::stream::event::Event;
    use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};

    #[test]
    fn thread_usage_has_one_lossless_session_projection() {
        // Cause/effect graph: C1 no model has committed usage; C2 one model has
        // committed usage; C3 several models have committed usage. Effects: E1
        // the projection is empty; E2 each model retains exact attribution; E3
        // totals are the saturating sum; E4 non-token counters remain zero
        // because ThreadUsage does not own them. Decision rules:
        // R1=C1=>E1,E4; R2=C2=>E2,E3,E4; R3=C3=>E2,E3,E4.
        let empty = SessionUsage::from(awaken_runtime_contract::llm::ThreadUsage::default());
        assert_eq!(empty, SessionUsage::default(), "R1/E1,E4");

        let attributed = awaken_runtime_contract::llm::ThreadUsage {
            by_model: std::collections::BTreeMap::from([
                (
                    "model-a".into(),
                    awaken_runtime_contract::llm::TokenUsage {
                        prompt_tokens: 3,
                        completion_tokens: 5,
                        cache_read_tokens: 7,
                        cache_creation_tokens: 11,
                    },
                ),
                (
                    "model-b".into(),
                    awaken_runtime_contract::llm::TokenUsage {
                        prompt_tokens: 13,
                        completion_tokens: 17,
                        cache_read_tokens: 19,
                        cache_creation_tokens: 23,
                    },
                ),
            ]),
        };

        let one_model = SessionUsage::from(awaken_runtime_contract::llm::ThreadUsage {
            by_model: std::collections::BTreeMap::from([(
                "model-a".into(),
                awaken_runtime_contract::llm::TokenUsage {
                    prompt_tokens: 3,
                    completion_tokens: 5,
                    cache_read_tokens: 7,
                    cache_creation_tokens: 11,
                },
            )]),
        });
        assert_eq!(one_model.input_tokens, 3, "R2/E3");
        assert_eq!(one_model.by_model["model-a"].output_tokens, 5, "R2/E2");
        assert_eq!(one_model.active_seconds, 0, "R2/E4");

        let projected = SessionUsage::from(attributed);
        assert_eq!(projected.input_tokens, 16, "R3/E3");
        assert_eq!(projected.output_tokens, 22, "R3/E3");
        assert_eq!(projected.cache_read_tokens, 26, "R3/E3");
        assert_eq!(projected.cache_creation_tokens, 34, "R3/E3");
        assert_eq!(projected.by_model["model-a"].input_tokens, 3, "R3/E2");
        assert_eq!(projected.by_model["model-b"].output_tokens, 17, "R3/E2");
        assert_eq!(projected.active_seconds, 0, "R3/E4");
        assert_eq!(projected.web_fetch_requests, 0, "R3/E4");
        assert_eq!(projected.web_search_requests, 0, "R3/E4");
    }

    /// A minimal double that overrides ONLY [`SessionRuntime::run`] (plus the trait's
    /// other *required* methods, implemented minimally). Every fail-closed DEFAULT
    /// method (`live_inbox_*`, `owns_thread`, `committed_messages`, `run_streaming`,
    /// …) is left at the trait's default so the tests below exercise those defaults.
    struct MinimalRuntime;

    /// The distinctive outcome `run` returns — used to prove `run_streaming` delegates
    /// to `run` identically (the default just ignores the sink).
    fn sample_outcome() -> StepOutcome {
        StepOutcome::ended(
            vec![
                Message::text(MessageId("m1".into()), Role::Assistant, "one"),
                Message::text(MessageId("m2".into()), Role::Assistant, "two"),
            ],
            EndCause::Error(Failure::Inference {
                code: "boom".into(),
                message: "it failed".into(),
            }),
        )
    }

    #[async_trait]
    impl SessionRuntime for MinimalRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            Ok(sample_outcome())
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeDrive, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    /// A sink that records nothing — the default `run_streaming` never touches it, so
    /// its `send` is never called; it exists only to satisfy the `Arc<dyn Sink>` arg.
    struct NoopSink;

    #[async_trait]
    impl Sink for NoopSink {
        async fn send(&self, _event: Event) -> Result<(), SinkError> {
            unreachable!("the default run_streaming ignores the sink")
        }
    }

    #[tokio::test]
    async fn default_terminal_repository_publication_is_fail_closed() {
        // Port-default cause/effect table: C1 a Runtime has not explicitly
        // implemented the Repository publication effect; C2 an exact command
        // and realization lease are presented. E1 is a classified retryable
        // failure and no synthetic receipt. Rule P1: C1+C2 => E1. There is no
        // command-only port and deliberately no compatibility success rule.
        let command: crate::SessionRepositoryPublicationCommand =
            serde_json::from_value(serde_json::json!({
                "session_id": "session",
                "effect_id": "effect",
                "intent": {
                    "input": {
                        "binding_id": "source",
                        "source": { "kind": "file", "file_id": "file" },
                        "mount_path": "/workspace/source",
                        "access": "read_only"
                    },
                    "expectation": {
                        "branch": "awf/work",
                        "commit": "0123456789abcdef0123456789abcdef01234567"
                    }
                }
            }))
            .unwrap();
        let lease = crate::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker:incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let error = MinimalRuntime
            .execute_terminal_repository_publication_for_lease(command, &lease)
            .await
            .expect_err("P1/E1");
        assert_eq!(error.kind, RunErrorKind::Unavailable, "P1/E1");
        assert_eq!(
            error.code, "session_repository_publication_effect_runtime_unsupported",
            "P1/E1"
        );
    }

    // Item 1: the fail-closed DEFAULT live-inbox methods all reject with `Inactive`.
    #[tokio::test]
    async fn default_live_inbox_edits_fail_closed_inactive() {
        let rt = MinimalRuntime;
        assert_eq!(
            rt.live_inbox_queue("t", vec![ContentBlock::text("hi")])
                .await,
            Err(LiveInboxError::Inactive),
        );
        assert_eq!(
            rt.live_inbox_remove("t", 7).await,
            Err(LiveInboxError::Inactive),
        );
        assert_eq!(
            rt.live_inbox_replace("t", 7, vec![ContentBlock::text("x")])
                .await,
            Err(LiveInboxError::Inactive),
        );
        assert_eq!(
            rt.live_inbox_reorder("t", vec![1, 2, 3]).await,
            Err(LiveInboxError::Inactive),
        );
        // The read-side default reports an inactive queue.
        let snap = rt.live_inbox_snapshot("t").await;
        assert!(!snap.active);
        assert!(snap.messages.is_empty());
    }

    // Item 1: the other fail-closed defaults — ownership false, committed empty.
    #[tokio::test]
    async fn default_probes_report_nothing() {
        // Test design — Causes: the minimal Runtime leaves every optional probe
        // and lifecycle hook unimplemented. Effects: ownership/history/usage and
        // capabilities are empty while no-op lifecycle commands succeed.
        // Constraints/invariants: defaults cannot invent durable ownership,
        // transcript, usage, or executable capability. Decision rule D1:
        // absent adapter=>neutral read values plus side-effect-free command OK.
        let rt = MinimalRuntime;
        assert!(
            !rt.owns_thread("t")
                .await
                .expect("default ownership query remains available"),
            "an ephemeral host owns nothing"
        );
        assert!(
            rt.committed_messages("t")
                .await
                .expect("default history query remains available")
                .is_empty(),
            "no durable transcript by default"
        );
        assert_eq!(
            rt.session_usage("t")
                .await
                .expect("default usage query remains available"),
            SessionUsage::default(),
            "no usage reported by default"
        );
        // The nonterminal lifecycle no-op defaults succeed without a host wiring them.
        assert!(rt.rebind_model("t", "m").await.is_ok());
        assert!(rt.interrupt("t").await.is_ok());
        // The default capability surface is empty.
        let caps = rt.capabilities();
        assert!(caps.builtin_tools.is_empty());
        assert!(caps.custom_tools.is_empty());
        assert!(caps.skills.is_empty());
        assert!(caps.delegates.is_empty());
    }

    /// Cause-effect graph for a Runtime without generation support:
    ///
    /// C1 exact generation command reaches the default SessionRuntime port
    ///   -> C2 no concrete generation adapter is installed
    ///   -> E1 reject before any stage/publish/drain effect.
    ///
    /// Decision table (the test cases below are generated one-for-one):
    ///
    /// | Rule | command | C1 | C2 | result |
    /// |---|---|---|---|---|
    /// | R1 | stage | T | T | `mcp_runtime_unsupported` |
    /// | R2 | publish | T | T | `mcp_runtime_unsupported` |
    /// | R3 | drain | T | T | `mcp_runtime_unsupported` |
    #[tokio::test]
    async fn default_mcp_generation_port_fails_closed_from_decision_table() {
        use crate::{
            McpAttachmentId, McpGeneration, McpGenerationRef, McpTarget, StageMcpAttachment,
        };

        struct UnsupportedRealizer;
        #[async_trait]
        impl McpAttachmentRealizer for UnsupportedRealizer {}
        let runtime = UnsupportedRealizer;
        let generation = McpGenerationRef {
            session_id: "session-1".into(),
            attachment_id: McpAttachmentId("mcp-1".into()),
            generation: McpGeneration(1),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 7,
            lease_expires_at_unix_ms: u64::MAX,
        };
        let stage = StageMcpAttachment {
            workspace_id: "workspace-1".into(),
            generation: generation.clone(),
            realization_id: "realization-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "calculator".into(),
            target: McpTarget::parse_http("https://mcp.example.test").unwrap(),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };

        let stage_error = runtime.stage_mcp_attachment(stage).await.unwrap_err();
        let publish_error = runtime
            .publish_mcp_generation(generation.clone())
            .await
            .unwrap_err();
        let drain_error = runtime.drain_mcp_generation(generation).await.unwrap_err();
        for (rule, error) in [
            ("R1", stage_error),
            ("R2", publish_error),
            ("R3", drain_error),
        ] {
            assert_eq!(error.code, "mcp_runtime_unsupported", "{rule}");
            assert_eq!(error.kind, RunErrorKind::Internal, "{rule}");
        }
    }

    // Item 1: `run_streaming`'s default delegates to `run` — the committed outcome is
    // identical (the sink only mirrors in-flight events, which the default ignores).
    #[tokio::test]
    async fn run_streaming_default_delegates_identically_to_run() {
        // Test design — Causes: the default streaming port receives the same
        // input as the direct Run port with a no-op sink. Effects: every committed
        // StepOutcome field is identical. Constraints/invariants: streaming is
        // observational only and cannot create a second execution path.
        // Decision rule: S1 default streaming=>delegate once to `run` and
        // preserve outcome.
        let rt = MinimalRuntime;
        let direct = rt
            .run("a", "t", vec![ContentBlock::text("go")])
            .await
            .unwrap();
        let streamed = rt
            .run_streaming("a", "t", vec![ContentBlock::text("go")], Arc::new(NoopSink))
            .await
            .unwrap();
        // `StepOutcome` has no `PartialEq`; compare it field-by-field.
        assert_eq!(streamed.new_messages.len(), direct.new_messages.len());
        assert_eq!(streamed.new_messages, direct.new_messages);
        assert_eq!(streamed.state(), direct.state());
        assert_eq!(
            streamed.pending().map(|p| &p.tool_use_id),
            direct.pending().map(|p| &p.tool_use_id),
        );
        assert_eq!(
            streamed.failure().map(Failure::code),
            direct.failure().map(Failure::code),
        );
    }

    // Item 4: `LiveInboxError` Display messages are stable, distinct wire text.
    #[test]
    fn live_inbox_error_display_messages_are_pinned() {
        // Test design — Causes: each closed LiveInbox failure variant is rendered.
        // Effects: it produces its exact distinct operator-facing text.
        // Constraints/invariants: display text is a pinned wire/debug boundary;
        // variants never alias. Decision rule L1: enumerate all variants=>three
        // exact, pairwise-distinct messages.
        assert_eq!(
            LiveInboxError::Inactive.to_string(),
            "no Run is in flight; send the message as a normal event",
        );
        assert_eq!(
            LiveInboxError::UnknownMessage.to_string(),
            "no queued message with that id",
        );
        assert_eq!(
            LiveInboxError::StaleOrder.to_string(),
            "proposed order does not match the current queue",
        );
    }

    // Cause/effect decision table: R1 runtime faults map to Internal/internal;
    // R2 caller faults map to BadRequest/invalid_request; R3 temporary dependency
    // faults map to Unavailable/unavailable. The wire adapter owns 500/400/503.
    // Constraints/invariants: the contract preserves three distinct stable kinds
    // and codes; transport status selection remains outside this value object.
    #[test]
    fn run_error_display_and_kind_mapping() {
        let internal = RunError::internal("provider blew up");
        assert_eq!(internal.to_string(), "run failed: provider blew up");
        assert_eq!(internal.kind, RunErrorKind::Internal);

        let bad = RunError::bad_request("no such await");
        assert_eq!(bad.to_string(), "run failed: no such await");
        assert_eq!(bad.kind, RunErrorKind::BadRequest);

        let unavailable = RunError::unavailable("image is still building");
        assert_eq!(unavailable.kind, RunErrorKind::Unavailable);
        assert_eq!(unavailable.code, "unavailable");

        // The three causes remain distinct.
        assert_ne!(internal.kind, bad.kind);
        assert_ne!(bad.kind, unavailable.kind);
    }

    // Item 5: `LiveInboxSnapshot::inactive()` invariants.
    #[test]
    fn inactive_snapshot_is_empty_versionless_and_inactive() {
        // Test design — Cause: no Run is in flight, so the canonical inactive
        // constructor is selected. Effects: active=false, version=0, and no
        // queued messages. Constraints/invariants: absence carries neither a
        // resumable version nor latent inbox state. Decision rule I1: inactive
        // constructor=>assert the complete three-field boundary tuple.
        let snap = LiveInboxSnapshot::inactive();
        assert!(!snap.active);
        assert_eq!(snap.version, 0);
        assert!(snap.messages.is_empty());
    }

    #[test]
    fn committed_resume_ticket_has_one_pending_projection_decision_table() {
        use awaken_agent_contract::agent::awaiting::{
            AwaitTarget, PauseReason, PendingTool as TicketPendingTool, RemoteInputReason,
            ResumeTicket, ToolAwaitReason,
        };

        // Causes: C1 each closed ToolCall reason is Permission, ClientExecution,
        // ScheduledAction, or Delegation; C2 each RemoteInput reason is UserInput
        // or ExternalEvent; C3 each Pause reason is Manual, RateLimit, or
        // BudgetReached. Effects: E1 preserve the exact call/tool and classify a
        // built-in confirmation; E2 preserve the exact call/tool and classify a
        // client execution; E3 synthesize client-executed agent_input with the
        // exact reason token; E4 omit system-owned waits. This is the sole
        // foreground/cold decoder.
        //
        // Decision table:
        // | Rule | Closed target | Effect |
        // |---|---|---|
        // | P1 | ToolCall(Permission) | E1 |
        // | P2 | ToolCall(ClientExecution) | E2 |
        // | P3 | RemoteInput(UserInput) | E3 user_input |
        // | P4 | RemoteInput(ExternalEvent) | E3 external_event |
        // | P5 | ToolCall(ScheduledAction) | E4 |
        // | P6 | ToolCall(Delegation) | E4 |
        // | P7 | Pause(Manual) | E4 |
        // | P8 | Pause(RateLimit) | E4 |
        // | P9 | Pause(BudgetReached) | E4 |
        // Constraints/invariants: K1 ToolCall necessarily carries call id and
        // PendingTool, so projection has no malformed-payload error; K2 internal
        // waits never become required action; K3 exhaustive matching makes a new
        // closed target fail compilation until this authority classifies it.
        let ticket = |target| {
            ResumeTicket::new(
                "correlation",
                RunId("run".into()),
                awaken_agent_contract::agent::thread::Id("thread".into()),
                "snapshot",
                "catalog",
                target,
            )
        };
        let concrete = || TicketPendingTool {
            tool_id: "calculator".into(),
            arguments: serde_json::json!({"value": 7}),
        };

        let tool = |reason| AwaitTarget::ToolCall {
            reason,
            call_id: "call".into(),
            tool: concrete(),
        };
        let exact_tool = |client_executed| {
            Some(Pending {
                tool_use_id: "call".into(),
                name: "calculator".into(),
                input: serde_json::json!({"value": 7}),
                client_executed,
            })
        };
        let agent_input = |reason| {
            Some(Pending {
                tool_use_id: "call".into(),
                name: "agent_input".into(),
                input: serde_json::json!({"reason": reason}),
                client_executed: true,
            })
        };
        let cases = [
            (
                "P1/E1",
                tool(ToolAwaitReason::Permission),
                exact_tool(false),
            ),
            (
                "P2/E2",
                tool(ToolAwaitReason::ClientExecution),
                exact_tool(true),
            ),
            (
                "P3/E3",
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "call".into(),
                },
                agent_input("user_input"),
            ),
            (
                "P4/E3",
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::ExternalEvent,
                    call_id: "call".into(),
                },
                agent_input("external_event"),
            ),
            ("P5/E4", tool(ToolAwaitReason::ScheduledAction), None),
            ("P6/E4", tool(ToolAwaitReason::Delegation), None),
            ("P7/E4", AwaitTarget::Pause(PauseReason::Manual), None),
            ("P8/E4", AwaitTarget::Pause(PauseReason::RateLimit), None),
            (
                "P9/E4",
                AwaitTarget::Pause(PauseReason::BudgetReached),
                None,
            ),
        ];

        for (rule, target, expected) in cases {
            assert_eq!(
                Pending::from_resume_ticket(&ticket(target)),
                expected,
                "{rule}"
            );
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn awaiting_constructor_cannot_create_a_terminal_or_failed_outcome() {
        // Test design — Causes: the Awaiting constructor receives no pending tool.
        // Effects: state is Awaiting and failure is absent. Constraints/invariants:
        // Awaiting cannot also be terminal. Decision rule K1: any empty message
        // vector+None pending=>Awaiting and no failure authority.
        let outcome = StepOutcome::awaiting(Vec::new(), None);
        assert!(matches!(outcome.state(), RunState::Awaiting));
        assert!(outcome.failure().is_none());
        std::mem::forget(outcome);
    }

    #[kani::proof]
    fn ended_constructor_carries_the_only_failure_authority_and_no_pending_tool() {
        // Test design — Causes: an Ended constructor receives either a classified
        // error or natural completion. Effects: both are terminal, only the error
        // exposes Failure, and neither has pending work. Constraints/invariants:
        // EndCause is the sole failure authority. Decision rule K2: error=>failure;
        // natural=>none; both=>Ended+no pending.
        let cause = if kani::any::<bool>() {
            EndCause::Error(Failure::CapabilityBound)
        } else {
            EndCause::NaturalEnd
        };
        let is_error = matches!(&cause, EndCause::Error(_));
        let outcome = StepOutcome::ended(Vec::new(), cause);
        assert!(matches!(outcome.state(), RunState::Ended(_)));
        assert!(outcome.pending().is_none());
        assert_eq!(outcome.failure().is_some(), is_error);
        std::mem::forget(outcome);
    }

    #[kani::proof]
    fn settled_step_has_no_parallel_observation_authority() {
        // Cause/effect decision rule: a terminal committed Run cause creates an
        // Ended Step outcome; compaction, retry, and model-request observations
        // remain exclusively in the committed recovery snapshot audit stream.
        // Constraints/invariants: StepOutcome carries no parallel observation
        // authority and a settled Step cannot retain pending work.
        let ended = StepOutcome::ended(Vec::new(), EndCause::NaturalEnd);
        assert!(matches!(
            ended.state(),
            RunState::Ended(EndCause::NaturalEnd)
        ));
        assert!(ended.pending().is_none());
        std::mem::forget(ended);
    }
}
