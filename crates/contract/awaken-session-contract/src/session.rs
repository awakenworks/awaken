//! Neutral value types and the [`SessionRuntime`] interface the adapter drives:
//! pending tools, turn outcomes, capabilities, session init, and run errors.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::DelegationStatus;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};

use crate::mcp_binding::McpRefreshBinding;

/// The tool a run awaits: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// Stable child-Run relationship projected at a session boundary. Runtime owns
/// the relationship; Managed and other adapters only render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedRun {
    pub run_id: RunId,
    pub parent_call_id: String,
    pub agent_id: String,
    pub status: DelegationStatus,
}

/// The result of running one settled step (a new turn, or a resume).
///
/// `state` reuses the run's sole lifecycle authority instead of storing a second
/// terminal classification. It is private: callers can construct only
/// `Awaiting` or `Ended` outcomes, so `Running` cannot escape a step boundary.
pub struct StepOutcome {
    pub messages: Vec<Message>,
    state: RunState,
    pending: Option<Pending>,
    /// `true` when this turn folded its context — projected as an
    /// `agent.thread_context_compacted` event ahead of the turn's messages.
    pub compacted: bool,
    /// `true` when the runtime transparently retried a transient inference failure
    /// during this turn (auto-recovery) — projected as a `session.status_rescheduled`
    /// event ahead of the turn's messages, so a client observes the recovery.
    pub rescheduled: bool,
    delegated_runs: Vec<DelegatedRun>,
}

impl StepOutcome {
    #[must_use]
    pub fn awaiting(
        messages: Vec<Message>,
        pending: Option<Pending>,
        compacted: bool,
        rescheduled: bool,
    ) -> Self {
        Self {
            messages,
            state: RunState::Awaiting,
            pending,
            compacted,
            rescheduled,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn ended(
        messages: Vec<Message>,
        cause: EndCause,
        compacted: bool,
        rescheduled: bool,
    ) -> Self {
        Self {
            messages,
            state: RunState::Ended(cause),
            pending: None,
            compacted,
            rescheduled,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &RunState {
        &self.state
    }

    #[must_use]
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
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
pub struct ToolPermissionDecision {
    pub allow: bool,
    pub note: Option<String>,
}

/// The advertised capability surface echoed in a session's agent object. The adapter
/// reads this once at session creation so the public agent object reports what the run
/// can actually do. This is neutral data; the public Managed Agents wire shaping (the
/// built-in `agent_toolset` fold, `custom` tools, `skills`, `multiagent`) lives in
/// [`crate::project`]. Deliberately absent: MCP servers (the host wires none) and
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
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// What a new session provisions on its thread before the first turn (ADR-0043
/// Phase 3): the agent it runs and the MCP servers it connects to, each already
/// bound to a vault credential's neutral domain id (or none). Consumed by the
/// server's `ManagedHost` through [`SessionRuntime::prepare_session`].
#[derive(Debug, Clone)]
pub struct SessionInit {
    /// Trusted owning workspace resolved by the platform edge before runtime
    /// preparation. Resource stores never infer or hard-code it.
    pub workspace_id: String,
    pub agent_id: String,
    pub mcp_servers: Vec<McpServerBinding>,
    /// The session's mounted resources (ADR-0038), parsed from the wire `resources[]`:
    /// files, memory stores, repos. The host realizes each into the run's sandbox and
    /// appends a prompt fragment to the system prompt (A3a). Empty = no mounts.
    pub resources: crate::ResolvedSessionResources,
    /// The session's requested model (R2), staged so the run binds it; `None` →
    /// the host default.
    pub model: Option<String>,
    /// The session's requested runtime adapter (R3): `"acp:*"` routes to an ACP
    /// CLI; `None`/`"awaken"` → native.
    pub runtime: Option<String>,
    /// Deny network egress for the session's sandbox, resolved from its environment's
    /// networking policy (a non-`unrestricted` policy → `true`). The host runs the
    /// `bash` tool under a `bwrap --unshare-net` namespace. `false` = host network.
    pub deny_egress: bool,
    /// The session environment's raw `config.sandbox` blob (isolation/network/limits),
    /// opaque here — the host parses it into a provisioning `SandboxOverride` and applies
    /// it onto the synthesized sandbox spec for both the native jail and the ACP CLI.
    /// `None` = host default spec. Kept as a `Value` so this leaf stays free of the
    /// provisioning contract; `deny_egress` remains for the coarse bwrap on/off.
    pub sandbox: Option<serde_json::Value>,
}

/// One session MCP server, bound at creation: the wire name/url plus the vault
/// credential the URL matched (`None` when no vault credential matches — the
/// host then connects unauthenticated and the server decides). Consumed by
/// `ManagedHost::prepare_session` in the server assembly.
#[derive(Debug, Clone)]
pub struct McpServerBinding {
    pub name: String,
    pub url: String,
    /// The matched vault credential's neutral row id, as a plain string (the port
    /// speaks no control-plane vocabulary — the host re-types it into the vault's
    /// `CredentialSourceId` at the lookup). `None` = no vault credential matched.
    pub credential_source_id: Option<String>,
    /// The matched credential's stored refresh configuration
    /// ([`VaultState::mcp_refresh_for_source`]), so the host can register a
    /// transport-level refresher next to the bearer. `None` when the credential
    /// is not refreshable (entered without a refresh object).
    pub refresh: Option<McpRefreshBinding>,
}

/// A queued live-inbox message on the session's in-flight turn. `id` is the
/// runtime's queue identity — targetable until the engine consumes the entry.
/// Neutral: the adapter projects it onto the wire snapshot at the route.
#[derive(Debug, Clone)]
pub struct LiveInboxEntry {
    pub id: u64,
    pub content: Vec<ContentBlock>,
}

/// The session's live-inbox resource: the editable queue of messages addressed
/// to the in-flight turn. `active: false` means no native turn is running (the
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
/// errors, plus `Inactive` for "no native turn in flight on this session".
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveInboxError {
    #[error("no turn is in flight; send the message as a normal event")]
    Inactive,
    #[error("no queued message with that id")]
    UnknownMessage,
    #[error("proposed order does not match the current queue")]
    StaleOrder,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError>;

    /// Run one user turn, installing `sink` as the run's best-effort live-progress
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
        content: &str,
        is_error: bool,
    ) -> Result<StepOutcome, RunError>;

    /// Provision `thread` for a new session BEFORE its record exists (ADR-0043
    /// Phase 3): the host materializes the init's MCP credential bindings and
    /// stages the servers for the thread's first turn. A failure fails the
    /// create (fail closed). The default is a no-op, so every host without MCP
    /// wiring is unaffected.
    async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
        Ok(())
    }

    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        _workspace_id: &str,
        _skill_ids: &[String],
    ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError> {
        Ok(Vec::new())
    }

    /// Rebind `thread` to `model` for its subsequent turns (R5, per-turn override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next turn resolves the new executor.
    async fn rebind_model(&self, _thread: &str, _model: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Apply the complete, already-resolved input manifest. Live add/update/delete
    /// and restart restoration converge here; runtime never re-resolves Agent
    /// defaults or current resource configuration.
    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _workspace_id: &str,
        _inputs: &crate::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, _thread: &str) -> bool {
        false
    }

    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    /// The session's accumulated token usage across all turns, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(&self, _thread: &str) -> SessionUsage {
        SessionUsage::default()
    }

    /// Buffer a system message; it is prepended to the next turn's input.
    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError>;

    /// End `thread`'s session at a terminal edge (session delete/archive): dispose
    /// its sandbox at the OS boundary — flush memory/skills back to durable truth
    /// while it is still live, then shred any materialized secrets and reap the
    /// workspace — and drop the cached context. Distinct from the evict-to-rebuild
    /// edges such as [`apply_session_inputs`](Self::apply_session_inputs), which deliberately
    /// keep the per-thread workspace so the next turn reuses it. Idempotent: a
    /// thread with no live session is a no-op. The default is a no-op, so a host
    /// without sandbox lifecycle is unaffected.
    async fn end_session(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded by
    /// `max_iterations`; `rubric` is the normalized requirement text.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError>;

    /// The live-inbox queue on `thread`'s in-flight turn. The default reports
    /// an inactive queue, so a host without live-inbox wiring is unaffected.
    async fn live_inbox_snapshot(&self, _thread: &str) -> LiveInboxSnapshot {
        LiveInboxSnapshot::inactive()
    }

    /// Queue a message onto `thread`'s in-flight turn; it is folded into the
    /// running transcript at the next safe boundary. Fails `Inactive` when no
    /// native turn is running (the caller should send a normal event instead).
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

/// A runtime failure. `kind` classifies who is at fault so the router can map it
/// to the right HTTP status: a `BadRequest` is the caller's (an unknown await, a
/// mismatched id, a wrong-binding resume); `Internal` is the runtime's.
#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
}

impl RunError {
    /// A runtime-side failure (provider error, corrupt state) — maps to `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
        }
    }

    /// A caller-side failure (bad id, wrong binding, no await) — maps to `400`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
        }
    }
}

/// The session-level token usage the managed wire reports (the port's neutral shape;
/// the runtime's per-model `TokenUsage` totals are mapped onto this by the host, so
/// this crate needs no runtime-plane type). Cumulative across all turns and models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::stream::event::Event;
    use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};

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
            true,
            true,
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
            _content: &str,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
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
        let rt = MinimalRuntime;
        assert!(!rt.owns_thread("t").await, "an ephemeral host owns nothing");
        assert!(
            rt.committed_messages("t").await.is_empty(),
            "no durable transcript by default"
        );
        assert_eq!(
            rt.session_usage("t").await,
            SessionUsage::default(),
            "no usage reported by default"
        );
        // The lifecycle no-op defaults succeed without a host wiring them.
        assert!(rt.prepare_session("t", init()).await.is_ok());
        assert!(rt.rebind_model("t", "m").await.is_ok());
        assert!(rt.end_session("t").await.is_ok());
        assert!(rt.interrupt("t").await.is_ok());
        // The default capability surface is empty.
        let caps = rt.capabilities();
        assert!(caps.builtin_tools.is_empty());
        assert!(caps.custom_tools.is_empty());
        assert!(caps.skills.is_empty());
        assert!(caps.delegates.is_empty());
    }

    fn init() -> SessionInit {
        SessionInit {
            workspace_id: "ws_test".into(),
            agent_id: "a".into(),
            mcp_servers: Vec::new(),
            resources: crate::ResolvedSessionResources::default(),
            model: None,
            runtime: None,
            deny_egress: false,
            sandbox: None,
        }
    }

    // Item 1: `run_streaming`'s default delegates to `run` — the committed outcome is
    // identical (the sink only mirrors in-flight events, which the default ignores).
    #[tokio::test]
    async fn run_streaming_default_delegates_identically_to_run() {
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
        assert_eq!(streamed.messages.len(), direct.messages.len());
        assert_eq!(streamed.messages, direct.messages);
        assert_eq!(streamed.state(), direct.state());
        assert_eq!(
            streamed.pending().map(|p| &p.tool_use_id),
            direct.pending().map(|p| &p.tool_use_id),
        );
        assert_eq!(streamed.compacted, direct.compacted);
        assert_eq!(streamed.rescheduled, direct.rescheduled);
        assert_eq!(
            streamed.failure().map(Failure::code),
            direct.failure().map(Failure::code),
        );
    }

    // Item 4: `LiveInboxError` Display messages are stable, distinct wire text.
    #[test]
    fn live_inbox_error_display_messages_are_pinned() {
        assert_eq!(
            LiveInboxError::Inactive.to_string(),
            "no turn is in flight; send the message as a normal event",
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

    // Item 4: `RunError` Display + the `internal`/`bad_request` constructor→kind
    // mapping. The kind is what the router turns into a 500 / 400 status (the status
    // mapping itself lives in the wire adapter's route, not in this contract crate).
    #[test]
    fn run_error_display_and_kind_mapping() {
        let internal = RunError::internal("provider blew up");
        assert_eq!(internal.to_string(), "run failed: provider blew up");
        assert_eq!(internal.kind, RunErrorKind::Internal);

        let bad = RunError::bad_request("no such await");
        assert_eq!(bad.to_string(), "run failed: no such await");
        assert_eq!(bad.kind, RunErrorKind::BadRequest);

        // The two kinds are distinct.
        assert_ne!(internal.kind, bad.kind);
    }

    // Item 5: `LiveInboxSnapshot::inactive()` invariants.
    #[test]
    fn inactive_snapshot_is_empty_versionless_and_inactive() {
        let snap = LiveInboxSnapshot::inactive();
        assert!(!snap.active);
        assert_eq!(snap.version, 0);
        assert!(snap.messages.is_empty());
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn awaiting_constructor_cannot_create_a_terminal_or_failed_outcome() {
        let outcome = StepOutcome::awaiting(Vec::new(), None, kani::any(), kani::any());
        assert!(matches!(outcome.state(), RunState::Awaiting));
        assert!(outcome.failure().is_none());
        std::mem::forget(outcome);
    }

    #[kani::proof]
    fn ended_constructor_carries_the_only_failure_authority_and_no_pending_tool() {
        let cause = if kani::any::<bool>() {
            EndCause::Error(Failure::CapabilityBound)
        } else {
            EndCause::NaturalEnd
        };
        let is_error = matches!(&cause, EndCause::Error(_));
        let outcome = StepOutcome::ended(Vec::new(), cause, kani::any(), kani::any());
        assert!(matches!(outcome.state(), RunState::Ended(_)));
        assert!(outcome.pending().is_none());
        assert_eq!(outcome.failure().is_some(), is_error);
        std::mem::forget(outcome);
    }
}
