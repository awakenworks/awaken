// @formal entrypoint acp.mcp_alias_permission_consensus_never_widens_authority
// @formal entrypoint acp.process_cleanup_requires_observed_exit_and_retained_owner
// @formal entrypoint acp.prompt_is_never_sent_before_bounded_handshake_completion
// @formal evidence acp.boundary_preserves_permission_authority_and_effect_order
// @formal evidence acp.permission_outcome_never_widens_resolved_authority
//! ACP bridge + supervisor (ADR-0041 Slice 3), agents plane.
//!
//! An **anti-corruption layer** over an opaque agent's protocol stream. It reads
//! the agent's events off an [`AgentChannel`], projects them into neutral
//! [`AcpProjectedEvent`]s, and commits them through the [`RunFactAppender`] binding seam —
//! the sole path by which projected truth reaches the runtime store (G13: the
//! bridge appends, it never owns the commit). The [`Supervisor`] drives one turn
//! and reaps the process on cancel, mapping the outcome to a [`TerminationReason`].
//!
//! The newline-delimited framing in this module is a deterministic fixture wire.
//! Production compositions enable `real-acp` (the ACP executor enables it by
//! default, and the CLI requests it explicitly) and select the official
//! `agent-client-protocol` JSON-RPC codec for projected launches. Both wires
//! converge through the same projection + appender boundary. The agent process
//! runs OUTSIDE us on a `tool_transparent` tier — its own syscalls are OS-jailed,
//! not routed through our tool layer.

use async_trait::async_trait;
#[cfg(feature = "real-acp")]
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityProbeConfig, NegotiatedAcpCapabilities,
};
use awaken_acp_contract::{AcpLaunchEvent, AcpLaunchStage, LaunchObserver, SessionMcpServer};
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Codex ACP reports MCP calls as a generic executor envelope. Unwrap only when
/// the duplicated server/tool identity exactly matches the protocol title;
/// otherwise retain the whole value so malformed or substituted input cannot
/// borrow another tool's permission.
#[cfg(feature = "real-acp")]
fn exact_mcp_tool_arguments(name: &str, input: &serde_json::Value) -> serde_json::Value {
    // The title is decomposed by the one owning type, so "which server and tool
    // does this envelope claim" has the same answer here as it does at the
    // permission decision.
    let Some(identity) = awaken_agent_contract::McpToolId::from_dotted(name) else {
        return input.clone();
    };
    let (server, tool) = (identity.server_name(), identity.tool_name());
    let exact = input.get("server").and_then(serde_json::Value::as_str) == Some(server.as_str())
        && input.get("tool").and_then(serde_json::Value::as_str) == Some(tool.as_str());
    if exact {
        input
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| input.clone())
    } else {
        input.clone()
    }
}

/// Official ACP codec projection (`real-acp` feature): the same ACL over the real
/// `agent-client-protocol` `SessionUpdate`/`StopReason` types.
#[cfg(feature = "real-acp")]
pub mod real_acp;

/// The official JSON-RPC 2.0 ACP client driver (`real-acp` feature).
#[cfg(feature = "real-acp")]
mod capabilities;
#[cfg(feature = "real-acp")]
pub mod jsonrpc;
#[cfg(feature = "real-acp")]
pub use jsonrpc::negotiate_capabilities;

/// Official ACP JSON-RPC implementation of the neutral capability handshake
/// port. Composition roots inject this leaf adapter into the Worker application.
#[cfg(feature = "real-acp")]
pub struct ProtocolAcpCapabilityHandshake;

#[cfg(feature = "real-acp")]
#[async_trait]
impl AcpCapabilityHandshake for ProtocolAcpCapabilityHandshake {
    async fn negotiate(
        &self,
        channel: &mut dyn AgentChannel,
        config: &AcpCapabilityProbeConfig,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        negotiate_capabilities(channel, config)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Which wire the bridge speaks to the agent. A per-session datum (each ACP CLI
/// row declares its own), not a build-time global: a test/fixture agent speaks the
/// newline stand-in; a real native or adapter-backed ACP CLI speaks official JSON-RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// The minimal newline-delimited `{type,text}` stand-in (fixtures, tests).
    #[default]
    Newline,
    /// The official `agent-client-protocol` JSON-RPC 2.0 wire (`real-acp`).
    #[cfg(feature = "real-acp")]
    Acp,
}

/// ACP turn-failure classification + error prompts (ported from oversight-next,
/// minus its rescheduling/retry).
pub mod error;

pub use error::{
    AcpFailure, AcpFailureClass, CredentialKind, RawAcpError, Stage, classify_error,
    deadline_exceeded, refusal, streamed_hard_limit,
};

/// Why a turn ended — projected from the agent's terminal frame or the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// The agent finished the turn on its own.
    NaturalEnd,
    /// The supervisor cancelled the turn (lease revoked, interrupt, …).
    Cancelled,
    /// The agent refused (policy/guardrail).
    Refusal,
    /// The agent reported an error.
    Error,
    /// The turn exceeded its hard wall-clock deadline and was reaped.
    TimedOut,
}

/// A mid-turn control message injected by the owner (ADR-0052 shape). Delivered
/// over a channel the supervisor races against the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injection {
    /// Cancel the turn now and reap the agent.
    Interrupt,
}

/// How the supervisor bounds and reaps a turn.
#[derive(Debug, Clone, Copy)]
pub struct SupervisePolicy {
    /// Hard wall-clock ceiling for a turn (oversight's per-turn deadline). `None`
    /// disables the cap.
    pub turn_deadline: Option<std::time::Duration>,
    /// Grace between `SIGTERM` and the escalated `SIGKILL` when reaping.
    pub reap_grace: std::time::Duration,
}

impl Default for SupervisePolicy {
    fn default() -> Self {
        Self {
            turn_deadline: None,
            reap_grace: std::time::Duration::from_secs(5),
        }
    }
}

/// A transient ACP projection event. ACP `session/update` variants collapse onto
/// this small staging set before the executor produces neutral runtime messages;
/// this type is neither the domain event model nor a persistence contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpProjectedEvent {
    /// Assistant text. ACP's unstable message id is retained only as a transient
    /// stream boundary; it never becomes a durable domain identifier.
    Message {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    /// The agent surfaced a tool call to us (the inbound-tool path). `id` is the
    /// ACP `tool_call_id`, so a later [`ToolResult`](Self::ToolResult) correlates
    /// to this call. Later occurrences with the same nonempty id refine staged
    /// arguments; they do not represent another invocation or committed fact.
    ToolCall {
        #[serde(default)]
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// A tool call's result, surfaced when the agent reports the call reached a
    /// terminal status (completed or failed). `id` is the [`ToolCall`](Self::ToolCall)
    /// `tool_call_id` it answers; `content` is the result's text; `is_error` is set
    /// when the tool failed.
    ToolResult {
        #[serde(default)]
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// The turn's token usage, when the agent reported it (`unstable_session_usage`).
    /// Carries plain counts so this low wire crate needs no runtime-contract
    /// dependency; the executor projects it onto the neutral committed `ThreadUsage`.
    Usage {
        #[serde(default)]
        prompt_tokens: u64,
        #[serde(default)]
        completion_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        cache_creation_tokens: u64,
    },
    /// The turn ended with a reason.
    TurnEnd { reason: TerminationReason },
}

/// The prompt frame the bridge writes to the agent (bridge → agent).
#[derive(Serialize)]
struct PromptFrame<'a> {
    prompt: &'a str,
}

/// Why appending a projected event to the runtime store failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AppendError {
    /// Event sequence must be strictly increasing (ADR-0042 monotonicity).
    #[error("non-monotonic event seq: got {got}, last committed {last}")]
    NonMonotonic { got: u64, last: u64 },
    /// The backing store rejected the append.
    #[error("event append failed: {0}")]
    Append(String),
}

/// The binding seam (ADR-0041 amendment): the sole port through which projected
/// events reach runtime-core. Implemented by the one runtime-facing crate; the
/// bridge depends on this abstraction, never on the store.
///
/// Named an *appender*, not a "sink": each call durably commits one fact at a
/// strictly increasing `seq` and may fail (unlike this repo's ephemeral,
/// best-effort stream sinks). Appended facts land in the same fact log the native
/// and A2A executors commit through the one boundary (`commit_run`).
#[async_trait]
pub trait RunFactAppender: Send {
    /// Commit one projected event at `seq` (strictly increasing per run).
    async fn append(&mut self, seq: u64, event: &AcpProjectedEvent) -> Result<(), AppendError>;
}

/// Canonical no-op appender for protocol phases whose projected frames are not
/// new Run facts (capability probes and `session/load` history replay).
#[cfg(feature = "real-acp")]
#[derive(Default)]
pub(crate) struct DiscardRunFacts;

#[cfg(feature = "real-acp")]
#[async_trait]
impl RunFactAppender for DiscardRunFacts {
    async fn append(&mut self, _seq: u64, _event: &AcpProjectedEvent) -> Result<(), AppendError> {
        Ok(())
    }
}

/// A neutral projection of one agent→client permission request: the tool the
/// external CLI (its own brain) wants to run, surfaced when the agent asks the
/// client to authorize it mid-turn (`session/request_permission`).
#[derive(Debug, Clone)]
pub struct PermissionAsk {
    /// The tool's name/kind as the agent described it (best-effort; may be empty).
    pub tool: String,
    /// The ACP `tool_call_id` this decision authorizes.
    pub call_id: String,
    /// The tool's arguments (`rawInput`), or `Null` when the agent supplied none.
    pub arguments: serde_json::Value,
}

/// The authorization verdict for one [`PermissionAsk`]. Immediate allow/deny is
/// projected back onto the agent's offered option; `Await` tells the runtime
/// adapter to commit a durable permission ticket and end this process attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny,
    Await { correlation_id: String },
}

/// Correlation-free authorization lattice used when several existing policy
/// namespaces must agree on one ACP permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionConsensus {
    Allow,
    Await,
    Deny,
}

#[must_use]
pub fn permission_consensus_class(verdict: &PermissionVerdict) -> PermissionConsensus {
    match verdict {
        PermissionVerdict::Allow => PermissionConsensus::Allow,
        PermissionVerdict::Await { .. } => PermissionConsensus::Await,
        PermissionVerdict::Deny => PermissionConsensus::Deny,
    }
}

#[must_use]
pub const fn combine_permission_consensus(
    left: PermissionConsensus,
    right: PermissionConsensus,
) -> PermissionConsensus {
    match (left, right) {
        (PermissionConsensus::Deny, _) | (_, PermissionConsensus::Deny) => {
            PermissionConsensus::Deny
        }
        (PermissionConsensus::Await, _) | (_, PermissionConsensus::Await) => {
            PermissionConsensus::Await
        }
        (PermissionConsensus::Allow, PermissionConsensus::Allow) => PermissionConsensus::Allow,
    }
}

/// Resolves an agent permission request. Injected like [`RunFactAppender`]: this
/// low wire crate defines the narrow port; the executor supplies an adapter that
/// bridges it to the single neutral `ToolPermissionPolicy` authority (G21). This is a
/// projection seam, **not** a parallel policy — the driver never decides, it asks.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict;
}

/// The default resolver: allow. The sandbox is the enforcement boundary for an
/// external CLI, so a host that wires no policy defaults permissive; the newline
/// fixture wire has no permission concept and never consults it.
pub struct AllowAll;

#[async_trait]
impl PermissionResolver for AllowAll {
    async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
        PermissionVerdict::Allow
    }
}

/// One exact backend-owned model selection delivered through ACP after a
/// session is opened. It contains only the catalog-owned option id and published
/// value; provider credentials and endpoints cannot enter this contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfigOptionSelection {
    pub config_id: String,
    pub value: String,
}

/// The per-turn cross-cutting inputs the driver needs beyond the prompt, bundled
/// so a turn threads as one value rather than a widening parameter list. The Acp
/// codec reads/updates it across the handshake; the newline stand-in ignores all
/// but nothing (it has no session/permission concept).
pub struct TurnConfig<'a> {
    pub tool_aliases: std::collections::BTreeMap<String, String>,
    /// Project the Run's existing max_steps budget into adapters whose SDK owns
    /// an opaque provider loop (Claude Code supports this as maxTurns).
    pub max_turn_requests: Option<usize>,
    /// Adapter `_meta.systemPrompt` resent on every `session/new|load|resume`,
    /// so a relaunched adapter never reverts to its preset prompt.
    pub system_prompt: Option<serde_json::Value>,
    /// Authorizes the agent's mid-turn tool requests (the neutral `ToolPermissionPolicy`
    /// behind an executor adapter).
    pub resolver: &'a dyn PermissionResolver,
    /// MCP servers to hand the CLI at `session/new` (the `AcpSession` interface, for
    /// claude/gemini/opencode). Empty for the newline stand-in and for config-file CLIs
    /// (legacy config-file adapters receive no in-band servers).
    pub mcp_servers: Vec<SessionMcpServer>,
    /// Runtime correlation; only endpoints explicitly opting in receive it.
    pub mcp_execution_context: Option<awaken_acp_contract::McpExecutionContext>,
    /// In: an ACP session id to resume via `session/load` (a relaunched CLI reloads
    /// its own session, so context survives the per-turn relaunch). Out: the id the
    /// turn used — `session/new`'s fresh id when none was given — so the caller can
    /// resume it next turn. Left `None` by the newline stand-in.
    pub session_id: Option<String>,
    /// Exact model to select with ACP `session/set_model`. `None` leaves the
    /// Session's advertised current model unchanged.
    pub session_model: Option<String>,
    /// In: a session mode to pin via `session/set_mode` after the handshake (an
    /// adapter-local datum — `None` leaves the agent's default). Validated
    /// fail-closed against the modes the agent advertised for the session.
    pub session_mode: Option<String>,
    /// Exact backend-owned model option to set after session creation/loading and
    /// before the first prompt. `None` preserves the CLI's own default.
    pub session_config_options: Vec<SessionConfigOptionSelection>,
    /// In: the interior working directory the CLI runs the session under (the
    /// sandbox's fixed workspace path). For a CLI that keys sessions by cwd (Claude
    /// Code), holding this stable across relaunches/machines is what lets
    /// `session/load` find the session cross-directory. `None` → `/` (the default).
    pub session_cwd: Option<String>,
    /// Authentication method selected by the host's ACP adapter catalog. The
    /// credential itself stays in the launched process environment; the protocol
    /// carries only this advertised method id.
    pub auth_method_id: Option<String>,
    /// Exact publication-time profile that the live initialize/session
    /// handshake must reproduce before any configured option or prompt is sent.
    pub expected_capability: Option<AcpCapabilityExpectation>,
    /// Per-step wall-clock bound for the prompt-free ACP handshake. This is not
    /// the turn/job deadline: it fences `initialize` (including optional auth),
    /// `session/new|load`, and every selected Session configuration step so a live
    /// child that never answers cannot retain a Run claim forever.
    pub handshake_step_deadline: std::time::Duration,
    /// Bounded protocol cancellation drain after an Await boundary. The pending
    /// ticket remains authoritative even if the adapter does not acknowledge.
    pub permission_settle_deadline: std::time::Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpCapabilityExpectation {
    pub adapter_id: String,
    pub adapter_version: String,
    pub fingerprint: String,
}

impl<'a> TurnConfig<'a> {
    /// A config that only authorizes (no session resume, no mode pin) — the default
    /// a plain `run_turn`/`supervise` builds for fixtures.
    #[must_use]
    pub fn new(resolver: &'a dyn PermissionResolver) -> Self {
        Self {
            resolver,
            tool_aliases: Default::default(),
            max_turn_requests: None,
            system_prompt: None,
            mcp_servers: Vec::new(),
            mcp_execution_context: None,
            session_id: None,
            session_model: None,
            session_mode: None,
            session_config_options: Vec::new(),
            session_cwd: None,
            auth_method_id: None,
            expected_capability: None,
            handshake_step_deadline: std::time::Duration::from_secs(30),
            permission_settle_deadline: std::time::Duration::from_secs(5),
        }
    }
}

/// A scoped handle to a [`LaunchObserver`]: the observer plus the run/thread scope
/// its events belong to. Cheap to copy, so it threads through the driver stack
/// (executor → supervisor → wire) without cloning the `Arc`.
#[derive(Clone, Copy)]
pub struct LaunchSink<'a> {
    observer: &'a dyn LaunchObserver,
    scope: &'a str,
}

impl<'a> LaunchSink<'a> {
    #[must_use]
    pub fn new(observer: &'a dyn LaunchObserver, scope: &'a str) -> Self {
        Self { observer, scope }
    }

    /// Emit one lifecycle event to the scoped observer.
    pub fn emit(&self, event: AcpLaunchEvent) {
        self.observer.on_launch(self.scope, &event);
    }
}

/// Emit a lifecycle event to an optional sink (no-op when none is wired).
pub(crate) fn notify_launch(launch_sink: Option<LaunchSink<'_>>, event: AcpLaunchEvent) {
    if let Some(sink) = launch_sink {
        sink.emit(event);
    }
}

/// Why the bridge could not complete a turn.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// Channel read/write failed.
    #[error("agent channel io: {0}")]
    Io(String),
    /// A frame from the agent was not valid JSON / not an `AcpProjectedEvent`.
    #[error("malformed agent frame: {0}")]
    Frame(String),
    /// The fact appender rejected a projected event.
    #[error("append: {0}")]
    Append(#[from] AppendError),
    /// The agent stream ended before emitting a `TurnEnd`.
    #[error("agent stream ended before turn end")]
    Truncated,
    /// A requested session mode is not among those the agent advertised — pinned
    /// fail-closed rather than silently ignored.
    #[error("session mode not supported by the agent: {0}")]
    UnsupportedSessionMode(String),
    /// Policy requires an out-of-band decision. The JSON-RPC request was answered
    /// `cancelled` before this is raised, so the opaque agent is not left blocked;
    /// the executor persists the ask as its ordinary neutral `ResumeTicket`.
    #[error("agent tool permission awaits decision: {correlation_id}")]
    PermissionAwait {
        correlation_id: String,
        ask: Box<PermissionAsk>,
    },
    /// A provider HARD-quota banner arrived as assistant TEXT (`"You've hit your
    /// weekly limit · resets …"`) — the case where the CLI then hangs. The turn is
    /// failed closed carrying the classified [`AcpFailure`] (a `RateLimited`), rather
    /// than committing the banner as an ordinary assistant message.
    #[error("provider hard-limit banner: {}", .0.message)]
    HardLimit(AcpFailure),
}

/// The bridge: drive one turn of an opaque agent over a duplex channel.
pub struct AcpBridge;

impl AcpBridge {
    /// Drive one turn over `channel`, dispatching on the wire [`Codec`]: the
    /// newline stand-in (fixtures) or the official JSON-RPC ACP driver. Both
    /// project into the same [`RunFactAppender`] and return the same
    /// [`TerminationReason`], so nothing downstream depends on the wire.
    pub async fn run_turn(
        channel: &mut dyn AgentChannel,
        prompt: &str,
        sink: &mut dyn RunFactAppender,
        codec: Codec,
        launch_sink: Option<LaunchSink<'_>>,
    ) -> Result<TerminationReason, AcpError> {
        let mut config = TurnConfig::new(&AllowAll);
        Self::run_turn_with_config(channel, prompt, sink, codec, &mut config, launch_sink).await
    }

    /// Drive one turn with the [`TurnConfig`] — authorizing the agent's mid-turn
    /// permission requests and resuming/negotiating its ACP session (the Acp codec
    /// only; the newline stand-in ignores it). The real ACP path wires the
    /// executor's config; fixtures use [`run_turn`](Self::run_turn).
    #[cfg_attr(not(feature = "real-acp"), allow(unused_variables))]
    pub async fn run_turn_with_config(
        channel: &mut dyn AgentChannel,
        prompt: &str,
        sink: &mut dyn RunFactAppender,
        codec: Codec,
        config: &mut TurnConfig<'_>,
        launch_sink: Option<LaunchSink<'_>>,
    ) -> Result<TerminationReason, AcpError> {
        match codec {
            Codec::Newline => Self::run_turn_newline(channel, prompt, sink, launch_sink).await,
            #[cfg(feature = "real-acp")]
            Codec::Acp => {
                crate::jsonrpc::run_turn_with_config(channel, prompt, sink, config, launch_sink)
                    .await
            }
        }
    }

    /// Send `prompt` to the agent, then read its newline-JSON event frames until
    /// `TurnEnd`, projecting each into the [`RunFactAppender`] with a strictly
    /// increasing seq.
    ///
    /// Cancel-safe at frame boundaries: a dropped future may leave events already
    /// committed, which is correct (they happened) — the supervisor maps the miss.
    async fn run_turn_newline(
        channel: &mut dyn AgentChannel,
        prompt: &str,
        sink: &mut dyn RunFactAppender,
        launch_sink: Option<LaunchSink<'_>>,
    ) -> Result<TerminationReason, AcpError> {
        // The newline stand-in has no handshake; writing the prompt is the point the
        // agent becomes live for this turn.
        notify_launch(launch_sink, AcpLaunchEvent::stage(AcpLaunchStage::Ready));
        let frame =
            serde_json::to_string(&PromptFrame { prompt }).expect("PromptFrame always serializes");
        channel
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?;
        channel
            .write_all(b"\n")
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?;
        channel
            .flush()
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?;

        let mut reader = BufReader::new(&mut *channel);
        let mut seq = 0u64;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| AcpError::Io(e.to_string()))?;
            if n == 0 {
                return Err(AcpError::Truncated);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event: AcpProjectedEvent =
                serde_json::from_str(trimmed).map_err(|e| AcpError::Frame(e.to_string()))?;
            seq += 1;
            sink.append(seq, &event).await?;
            if let AcpProjectedEvent::TurnEnd { reason } = event {
                return Ok(reason);
            }
        }
    }
}

/// After the protocol outcome is fixed, drain opaque trailing bytes so a
/// bounded stdout pipe cannot prevent the transport's later exit-status frame.
/// EOF/error here is never process termination: only the original cleanup future
/// decides success, escalation and deadlines. No trailing fact is projected.
async fn drain_during_cleanup<T>(
    channel: &mut dyn AgentChannel,
    cleanup: impl std::future::Future<Output = T>,
) -> T {
    tokio::pin!(cleanup);
    let mut sink = tokio::io::sink();
    tokio::select! {
        biased;
        result = &mut cleanup => result,
        _ = tokio::io::copy(channel, &mut sink) => cleanup.await,
    }
}

/// The supervisor: a consumer policy over the process/channel primitives — the
/// session-runner shape. It drives a turn, bounds it with a hard deadline, honors
/// mid-turn interrupts, and reaps the agent `SIGTERM`→(grace)→`SIGKILL` on exit.
pub struct Supervisor;

const PROCESS_REAP_DEADLINE: &str = "process cleanup exceeded its total deadline";
const PROCESS_EXIT_UNOBSERVED: &str = "process did not report exit after SIGKILL";

/// The one cleanup ladder's typed failure, projected into the ACP or Sandbox
/// caller without reverse-engineering a string or weakening a provider error.
enum ReapFailure {
    Provider(pc::SandboxError),
    Deadline,
    ExitUnobserved,
}

impl ReapFailure {
    fn into_acp(self) -> AcpError {
        AcpError::Io(match self {
            Self::Provider(error) => error.to_string(),
            Self::Deadline => PROCESS_REAP_DEADLINE.into(),
            Self::ExitUnobserved => PROCESS_EXIT_UNOBSERVED.into(),
        })
    }

    fn into_sandbox(self) -> pc::SandboxError {
        match self {
            Self::Provider(error) => error,
            Self::Deadline => pc::SandboxError::timeout(PROCESS_REAP_DEADLINE),
            Self::ExitUnobserved => pc::SandboxError::timeout(PROCESS_EXIT_UNOBSERVED),
        }
    }
}

#[async_trait]
impl awaken_session_environment_contract::ProcessReaper for Supervisor {
    async fn reap(
        &self,
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, String> {
        // This is a projection of the existing Supervisor kernel, not a second
        // signal ladder. Keep Host MCP diagnostics byte-for-byte at this seam.
        Supervisor::reap(process, grace)
            .await
            .map_err(|error| error.to_string())
    }
}

impl Supervisor {
    /// Settle a process after the protocol has already produced a terminal fact.
    ///
    /// A well-behaved per-turn adapter normally exits immediately after writing its
    /// terminal frame. Give that natural exit a small bounded window before starting
    /// the TERM -> KILL ladder. Cancellation and deadline paths continue to call
    /// [`Self::reap`] directly, so this never delays an explicit stop request.
    pub async fn reap_after_terminal(
        channel: &mut dyn AgentChannel,
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, AcpError> {
        let budget = grace
            .saturating_mul(4)
            .max(std::time::Duration::from_millis(1));
        drain_during_cleanup(
            channel,
            tokio::time::timeout(budget, Self::reap_after_terminal_inner(process, grace)),
        )
        .await
        .map_err(|_| AcpError::Io("terminal process cleanup exceeded its total deadline".into()))?
    }

    async fn reap_after_terminal_inner(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, AcpError> {
        const SETTLE_STEPS: u32 = 5;
        let settle = grace.min(std::time::Duration::from_millis(10));
        let step = settle / SETTLE_STEPS;
        let io = |e: pc::SandboxError| AcpError::Io(e.to_string());
        for _ in 0..SETTLE_STEPS {
            if process.poll().await.map_err(io)?.is_some() {
                return Ok(false);
            }
            tokio::time::sleep(step).await;
        }
        Self::reap_inner(process, grace)
            .await
            .map_err(ReapFailure::into_acp)
    }

    /// Reap the agent: `SIGTERM`, wait up to `grace` for it to exit, else escalate
    /// to `SIGKILL`. Returns `true` if the kill escalation was needed. (Process-group
    /// semantics live in the provider; this is the neutral signal ladder.)
    pub async fn reap(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, AcpError> {
        Self::reap_kernel(process, grace)
            .await
            .map_err(ReapFailure::into_acp)
    }

    /// The same bounded process cleanup for an owner that must retain the
    /// provider's typed [`pc::SandboxError`] classification. Both this method
    /// and [`Self::reap`] enter one signal/observation kernel.
    pub async fn reap_process(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, pc::SandboxError> {
        Self::reap_kernel(process, grace)
            .await
            .map_err(ReapFailure::into_sandbox)
    }

    async fn reap_kernel(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, ReapFailure> {
        let budget = grace
            .saturating_mul(4)
            .max(std::time::Duration::from_millis(1));
        tokio::time::timeout(budget, Self::reap_inner(process, grace))
            .await
            .map_err(|_| ReapFailure::Deadline)?
    }

    async fn reap_inner(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, ReapFailure> {
        if process
            .poll()
            .await
            .map_err(ReapFailure::Provider)?
            .is_some()
        {
            return Ok(false);
        }
        // A provider may lose the signal-vs-exit race before its outer handle
        // observes the exit. Only that same handle can supply the evidence;
        // signal errors must not skip the existing bounded observation window.
        let term_error = process.signal(pc::Signal::Term).await.err();
        const STEPS: u32 = 5;
        let step = grace / STEPS;
        for _ in 0..STEPS {
            if process
                .poll()
                .await
                .map_err(ReapFailure::Provider)?
                .is_some()
            {
                return Ok(false);
            }
            tokio::time::sleep(step).await;
        }
        // A failed TERM request supplies neither an exit receipt nor a reason
        // to abandon the existing KILL stage. Both signals address this same
        // owned handle; success still requires its later exit observation.
        let kill_error = process.signal(pc::Signal::Kill).await.err();
        // Wait through the provider boundary so lifecycle hooks (notably durable
        // credential-file write-back) run before the turn is considered finished.
        // A broken provider must not retain the caller's lifecycle lock forever.
        tokio::time::timeout(
            grace.max(std::time::Duration::from_millis(1)),
            process.wait(),
        )
        .await
        .map_err(|_| {
            kill_error
                .or(term_error)
                .map(ReapFailure::Provider)
                .unwrap_or(ReapFailure::ExitUnobserved)
        })?
        .map_err(ReapFailure::Provider)?;
        Ok(true)
    }

    /// Drive a turn, racing it against `cancel`, mid-turn `injections`, and the
    /// policy's turn deadline. Any of the three reaps the agent and maps the reason.
    #[allow(clippy::too_many_arguments)]
    pub async fn supervise(
        channel: &mut dyn AgentChannel,
        process: &dyn pc::ProcessHandle,
        prompt: &str,
        sink: &mut dyn RunFactAppender,
        cancel: impl std::future::Future<Output = ()>,
        injections: &mut tokio::sync::mpsc::Receiver<Injection>,
        policy: SupervisePolicy,
        codec: Codec,
        launch_sink: Option<LaunchSink<'_>>,
    ) -> Result<TerminationReason, AcpError> {
        let mut config = TurnConfig::new(&AllowAll);
        Self::supervise_with_config(
            channel,
            process,
            prompt,
            sink,
            cancel,
            injections,
            policy,
            codec,
            &mut config,
            launch_sink,
        )
        .await
    }

    /// [`supervise`](Self::supervise) with the [`TurnConfig`]: the executor wires
    /// its neutral permission resolver and the ACP session to resume here, so an
    /// external CLI's tool requests are decided by the single `ToolPermissionPolicy`
    /// and its session survives the per-turn relaunch.
    #[allow(clippy::too_many_arguments)]
    pub async fn supervise_with_config(
        channel: &mut dyn AgentChannel,
        process: &dyn pc::ProcessHandle,
        prompt: &str,
        sink: &mut dyn RunFactAppender,
        cancel: impl std::future::Future<Output = ()>,
        injections: &mut tokio::sync::mpsc::Receiver<Injection>,
        policy: SupervisePolicy,
        codec: Codec,
        config: &mut TurnConfig<'_>,
        launch_sink: Option<LaunchSink<'_>>,
    ) -> Result<TerminationReason, AcpError> {
        let deadline = async {
            match policy.turn_deadline {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            outcome = AcpBridge::run_turn_with_config(channel, prompt, sink, codec, config, launch_sink) => outcome,
            Some(Injection::Interrupt) = injections.recv() => {
                drain_during_cleanup(channel, Self::reap(process, policy.reap_grace)).await?;
                Ok(TerminationReason::Cancelled)
            }
            () = cancel => {
                drain_during_cleanup(channel, Self::reap(process, policy.reap_grace)).await?;
                Ok(TerminationReason::Cancelled)
            }
            () = deadline => {
                drain_during_cleanup(channel, Self::reap(process, policy.reap_grace)).await?;
                Ok(TerminationReason::TimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    /// An in-memory sink that enforces monotonic seq and records the projected events.
    #[derive(Default)]
    pub(super) struct RecordingSink {
        last: u64,
        pub(super) events: Vec<(u64, AcpProjectedEvent)>,
        fail_at: Option<u64>,
    }

    #[async_trait]
    impl RunFactAppender for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AcpProjectedEvent) -> Result<(), AppendError> {
            if let Some(f) = self.fail_at
                && seq == f
            {
                return Err(AppendError::Append("store down".into()));
            }
            if seq <= self.last {
                return Err(AppendError::NonMonotonic {
                    got: seq,
                    last: self.last,
                });
            }
            self.last = seq;
            self.events.push((seq, event.clone()));
            Ok(())
        }
    }

    /// Records the launch lifecycle events emitted for a scope — the otherwise
    /// untested `LaunchObserver`/`LaunchSink`/`notify_launch` observability seam.
    #[derive(Default)]
    pub(crate) struct RecordingLaunch {
        pub(crate) events: Mutex<Vec<(String, AcpLaunchEvent)>>,
    }

    impl LaunchObserver for RecordingLaunch {
        fn on_launch(&self, scope: &str, event: &AcpLaunchEvent) {
            self.events
                .lock()
                .unwrap()
                .push((scope.to_string(), event.clone()));
        }
    }

    /// A fake agent: read one prompt line, emit the scripted frames, then hang or exit.
    async fn fake_agent(mut side: DuplexStream, frames: Vec<String>, hang: bool) {
        let mut reader = BufReader::new(&mut side);
        let mut prompt = String::new();
        let _ = reader.read_line(&mut prompt).await.unwrap();
        assert!(
            prompt.contains("\"prompt\""),
            "bridge must send a prompt frame"
        );
        for f in frames {
            side.write_all(f.as_bytes()).await.unwrap();
            side.write_all(b"\n").await.unwrap();
            side.flush().await.unwrap();
        }
        if hang {
            // Stay open so the turn never ends on its own (exercises cancel).
            std::future::pending::<()>().await;
        }
    }

    fn combined() -> (Box<dyn AgentChannel>, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(4096);
        (Box::new(ours), theirs)
    }

    #[tokio::test]
    async fn run_turn_projects_events_in_order_until_turn_end() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![
                r#"{"type":"message","text":"working"}"#.into(),
                r#"{"type":"tool_call","name":"read_file","input":{"path":"a"}}"#.into(),
                r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
            ],
            false,
        ));

        let mut sink = RecordingSink::default();
        let reason = AcpBridge::run_turn(ours.as_mut(), "do it", &mut sink, Codec::Newline, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert_eq!(sink.events.len(), 3);
        assert_eq!(sink.events[0].0, 1);
        assert_eq!(sink.events[2].0, 3);
        assert!(matches!(
            sink.events[0].1,
            AcpProjectedEvent::Message { .. }
        ));
        assert!(matches!(
            sink.events[1].1,
            AcpProjectedEvent::ToolCall { .. }
        ));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn refusal_and_error_reasons_propagate() {
        for (frame, want) in [
            (
                r#"{"type":"turn_end","reason":"refusal"}"#,
                TerminationReason::Refusal,
            ),
            (
                r#"{"type":"turn_end","reason":"error"}"#,
                TerminationReason::Error,
            ),
        ] {
            let (mut ours, theirs) = combined();
            let agent = tokio::spawn(fake_agent(theirs, vec![frame.into()], false));
            let mut sink = RecordingSink::default();
            let got = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline, None)
                .await
                .unwrap();
            assert_eq!(got, want);
            agent.await.unwrap();
        }
    }

    #[tokio::test]
    async fn malformed_frame_is_rejected() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(theirs, vec!["not json".into()], true));
        let mut sink = RecordingSink::default();
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Frame(_)));
        agent.abort();
    }

    #[tokio::test]
    async fn stream_truncated_before_turn_end_errors() {
        let (mut ours, theirs) = combined();
        // Agent emits one message then closes without a turn_end.
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"message","text":"partial"}"#.into()],
            false,
        ));
        let mut sink = RecordingSink::default();
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Truncated));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn sink_failure_aborts_the_turn() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"message","text":"x"}"#.into()],
            true,
        ));
        let mut sink = RecordingSink {
            fail_at: Some(1),
            ..Default::default()
        };
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Append(AppendError::Append(_))));
        agent.abort();
    }

    /// A write that fails mid-prompt surfaces as `AcpError::Io`, not a wrong terminal
    /// class: with the agent side dropped before the turn starts, the prompt
    /// `write_all` hits a broken pipe. This is the entire outbound-write error path,
    /// which the read-side `Frame`/`Truncated` tests never reach.
    #[tokio::test]
    async fn a_broken_pipe_on_the_prompt_write_surfaces_as_io() {
        let (mut ours, theirs) = combined();
        drop(theirs); // the agent is gone before we write the prompt
        let mut sink = RecordingSink::default();
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AcpError::Io(_)),
            "a write failure is Io, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_blank_line_between_frames_is_skipped_not_projected() {
        // A blank and a whitespace-only line arrive between two real frames: each
        // must be skipped (no event, no seq bump, no error), and the real frames
        // still project in order — the newline codec's `trimmed.is_empty()` branch.
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![
                r#"{"type":"message","text":"one"}"#.into(),
                String::new(),
                "   ".into(),
                r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
            ],
            false,
        ));
        let mut sink = RecordingSink::default();
        let reason = AcpBridge::run_turn(ours.as_mut(), "go", &mut sink, Codec::Newline, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert_eq!(
            sink.events.len(),
            2,
            "only the two non-blank frames project"
        );
        assert_eq!(sink.events[0].0, 1);
        assert_eq!(sink.events[1].0, 2, "seq skips the blank lines");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_newline_turn_emits_a_ready_launch_event_to_the_scoped_observer() {
        // The newline stand-in has no handshake, so writing the prompt is when the
        // agent becomes live: exactly one `Ready` lifecycle event, tagged with the
        // run scope, reaches the observer.
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"turn_end","reason":"natural_end"}"#.into()],
            false,
        ));
        let observer = RecordingLaunch::default();
        let mut sink = RecordingSink::default();
        AcpBridge::run_turn(
            ours.as_mut(),
            "go",
            &mut sink,
            Codec::Newline,
            Some(LaunchSink::new(&observer, "run-7")),
        )
        .await
        .unwrap();
        {
            let events = observer.events.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "the newline turn emits one lifecycle event"
            );
            assert_eq!(events[0].0, "run-7", "the event carries the run scope");
            assert_eq!(events[0].1.stage, AcpLaunchStage::Ready);
        }
        agent.await.unwrap();
    }

    // ── AcpProjectedEvent wire contract ─────────────────────────────────────────────
    //
    // AcpProjectedEvent is the projection contract the newline codec (and the store)
    // depend on: every variant must round-trip, its `#[serde(default)]` fields must
    // tolerate an agent that omits them, and an unknown tag must be a clean error.

    fn round_trip(ev: &AcpProjectedEvent) -> AcpProjectedEvent {
        let json = serde_json::to_string(ev).expect("serializes");
        serde_json::from_str(&json).expect("deserializes")
    }

    #[tokio::test]
    async fn every_agent_event_variant_round_trips() {
        let events = [
            AcpProjectedEvent::Message {
                text: "hi".into(),
                message_id: None,
            },
            AcpProjectedEvent::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a" }),
            },
            AcpProjectedEvent::ToolResult {
                id: "call_1".into(),
                content: "ok".into(),
                is_error: true,
            },
            AcpProjectedEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                cache_read_tokens: 3,
                cache_creation_tokens: 4,
            },
            AcpProjectedEvent::TurnEnd {
                reason: TerminationReason::NaturalEnd,
            },
        ];
        for ev in events {
            assert_eq!(round_trip(&ev), ev, "{ev:?} is lossless on the wire");
        }
    }

    #[tokio::test]
    async fn optional_fields_default_when_the_agent_omits_them() {
        // A tool_call with neither id nor input.
        let tc: AcpProjectedEvent =
            serde_json::from_str(r#"{"type":"tool_call","name":"ls"}"#).expect("tool_call parses");
        assert_eq!(
            tc,
            AcpProjectedEvent::ToolCall {
                id: String::new(),
                name: "ls".into(),
                input: serde_json::Value::Null,
            }
        );
        // A tool_result with neither id nor is_error.
        let tr: AcpProjectedEvent =
            serde_json::from_str(r#"{"type":"tool_result","content":"done"}"#)
                .expect("tool_result parses");
        assert_eq!(
            tr,
            AcpProjectedEvent::ToolResult {
                id: String::new(),
                content: "done".into(),
                is_error: false,
            }
        );
        // A usage frame reporting only some token axes; the rest default to 0.
        let us: AcpProjectedEvent = serde_json::from_str(r#"{"type":"usage","prompt_tokens":7}"#)
            .expect("partial usage parses");
        assert_eq!(
            us,
            AcpProjectedEvent::Usage {
                prompt_tokens: 7,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }
        );
    }

    #[tokio::test]
    async fn an_unknown_event_tag_is_a_clean_deserialize_error() {
        let err = serde_json::from_str::<AcpProjectedEvent>(r#"{"type":"nope","text":"x"}"#)
            .expect_err("an unknown tag must not parse");
        // The newline codec maps exactly this serde error into AcpError::Frame.
        assert!(err.to_string().contains("nope") || err.is_data());
    }

    // ── Supervisor ──────────────────────────────────────────────────────────

    struct FakeProcess {
        signalled: Arc<Mutex<Vec<pc::Signal>>>,
        /// When true, `poll` reports an exit once any signal has been delivered.
        exits_after_signal: bool,
    }

    impl FakeProcess {
        fn new(exits_after_signal: bool) -> (Self, Arc<Mutex<Vec<pc::Signal>>>) {
            let signalled = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    signalled: signalled.clone(),
                    exits_after_signal,
                },
                signalled,
            )
        }
    }

    #[async_trait]
    impl pc::ProcessHandle for FakeProcess {
        fn id(&self) -> &str {
            "fake"
        }
        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }
        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            let signalled = !self.signalled.lock().unwrap().is_empty();
            Ok(
                (self.exits_after_signal && signalled).then_some(pc::ExitStatus {
                    code: None,
                    signaled: true,
                }),
            )
        }
        async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
            self.signalled.lock().unwrap().push(signal);
            Ok(())
        }
    }

    fn fast_policy() -> SupervisePolicy {
        SupervisePolicy {
            turn_deadline: None,
            reap_grace: std::time::Duration::from_millis(20),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reap_escalates_to_kill_when_the_agent_ignores_term() {
        // C1 TERM is ignored; C2 the grace window is still open; C3 the grace
        // expires and wait observes exit. R1 C1+C2 -> TERM only, still pending;
        // R2 C1+C3 -> TERM then KILL, successful cleanup within the total budget.
        // The fake performs no host IO: virtual time keeps these policy boundaries
        // independent of Windows scheduling without changing production deadlines.
        let (process, signalled) = FakeProcess::new(false); // never exits on its own
        let grace = std::time::Duration::from_millis(20);
        let started = tokio::time::Instant::now();
        let cleanup = Supervisor::reap(&process, grace);
        tokio::pin!(cleanup);
        tokio::select! {
            biased;
            result = &mut cleanup => panic!("R1 cleanup completed before grace: {result:?}"),
            // awaken-allow(wait): the paused-clock half of the TERM grace is the boundary under test
            () = tokio::time::sleep(grace / 2) => {}
        }
        assert_eq!(signalled.lock().unwrap().as_slice(), &[pc::Signal::Term]);
        let killed = cleanup.await.unwrap();
        assert!(killed, "an unresponsive agent must be escalated to SIGKILL");
        assert!(started.elapsed() >= grace, "R2 must retain the TERM grace");
        assert!(
            started.elapsed() < grace * 4,
            "R2 must precede the total deadline"
        );
        assert_eq!(
            signalled.lock().unwrap().as_slice(),
            &[pc::Signal::Term, pc::Signal::Kill]
        );
    }

    #[tokio::test]
    async fn reap_stops_at_term_when_the_agent_exits() {
        // Causes: C1 the same process owner observes exit after TERM; C2 the
        // caller enters directly or through the Session Environment SPI.
        // Effects: E1 both report no KILL escalation and exactly one TERM.
        // Decision table: R1 C1+direct -> E1; R2 C1+port -> E1.
        let (process, signalled) = FakeProcess::new(true); // exits right after SIGTERM
        let killed = Supervisor::reap(&process, std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(!killed);
        assert_eq!(signalled.lock().unwrap().as_slice(), &[pc::Signal::Term]);
        let (port_process, port_signalled) = FakeProcess::new(true);
        let port_killed = awaken_session_environment_contract::ProcessReaper::reap(
            &Supervisor,
            &port_process,
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(port_killed, killed, "R2 outcome");
        assert_eq!(
            port_signalled.lock().unwrap().as_slice(),
            &[pc::Signal::Term],
            "R2 signals"
        );
    }

    #[tokio::test]
    async fn terminal_reap_prefers_a_bounded_natural_exit_over_signalling() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct NaturallyExiting {
            polls: AtomicUsize,
            signalled: Arc<Mutex<Vec<pc::Signal>>>,
        }

        #[async_trait]
        impl pc::ProcessHandle for NaturallyExiting {
            fn id(&self) -> &str {
                "natural"
            }

            async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
                Ok(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                })
            }

            async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
                Ok(
                    (self.polls.fetch_add(1, Ordering::SeqCst) >= 2).then_some(pc::ExitStatus {
                        code: Some(0),
                        signaled: false,
                    }),
                )
            }

            async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
                self.signalled.lock().unwrap().push(signal);
                Ok(())
            }
        }

        let signalled = Arc::new(Mutex::new(Vec::new()));
        let (mut channel, _peer) = tokio::io::duplex(64);
        let killed = Supervisor::reap_after_terminal(
            &mut channel,
            &NaturallyExiting {
                polls: AtomicUsize::new(0),
                signalled: signalled.clone(),
            },
            std::time::Duration::from_millis(20),
        )
        .await
        .expect("natural terminal cleanup succeeds");

        assert!(!killed);
        assert!(
            signalled.lock().unwrap().is_empty(),
            "a process that naturally settles after its terminal frame is not signalled"
        );
    }

    /// Reap liveness cause/effect rule R3: C1=TERM ignored, C2=KILL delivered,
    /// C3=provider wait never resolves; E1=the canonical signal ladder returns a
    /// bounded error after TERM+KILL instead of retaining an execution lock.
    /// The typed caller receives Timeout; the ACP caller retains its existing
    /// IO error. Both projections run the same kernel over one owned handle.
    /// K1: virtual time advances the ladder independently of host scheduling;
    /// the total deadline must not win solely because this test was preempted.
    #[tokio::test(start_paused = true)]
    async fn reap_bounds_a_provider_that_never_reports_exit_after_kill() {
        struct NeverExits {
            signalled: Arc<Mutex<Vec<pc::Signal>>>,
        }

        #[async_trait]
        impl pc::ProcessHandle for NeverExits {
            fn id(&self) -> &str {
                "never-exits"
            }

            async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
                std::future::pending().await
            }

            async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
                Ok(None)
            }

            async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
                self.signalled.lock().unwrap().push(signal);
                Ok(())
            }
        }

        let signalled = Arc::new(Mutex::new(Vec::new()));
        let error = Supervisor::reap(
            &NeverExits {
                signalled: signalled.clone(),
            },
            std::time::Duration::from_millis(5),
        )
        .await
        .expect_err("a provider wait cannot block forever after SIGKILL");
        assert!(error.to_string().contains("after SIGKILL"));
        assert_eq!(
            signalled.lock().unwrap().as_slice(),
            &[pc::Signal::Term, pc::Signal::Kill]
        );
        let typed_signalled = Arc::new(Mutex::new(Vec::new()));
        let typed_error = Supervisor::reap_process(
            &NeverExits {
                signalled: typed_signalled.clone(),
            },
            std::time::Duration::from_millis(5),
        )
        .await
        .expect_err("the typed caller cannot claim an unobserved exit");
        assert!(
            matches!(typed_error, pc::SandboxError::Timeout(ref detail) if detail == PROCESS_EXIT_UNOBSERVED)
        );
        assert_eq!(
            typed_signalled.lock().unwrap().as_slice(),
            &[pc::Signal::Term, pc::Signal::Kill]
        );
    }

    #[tokio::test]
    async fn reap_total_deadline_also_bounds_a_stuck_provider_poll() {
        // Causes: C1 provider poll never returns; C2 caller needs ACP, port, or typed
        // Sandbox classification; C3 provider poll returns Transport. Effects:
        // E1 all bound the same operation; E2 ACP and port keep identical IO wording; E3
        // Sandbox returns Timeout for C1; E4 C3 keeps the original Transport.
        // R1 C1+C2 enters one deadline and never reports physical exit;
        // R2 C3 returns the provider class before signalling.
        struct StuckPoll;

        #[async_trait]
        impl pc::ProcessHandle for StuckPoll {
            fn id(&self) -> &str {
                "stuck-poll"
            }

            async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
                std::future::pending().await
            }

            async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
                std::future::pending().await
            }

            async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
                Ok(())
            }
        }

        let error = Supervisor::reap(&StuckPoll, std::time::Duration::from_millis(5))
            .await
            .expect_err("the total cleanup deadline must cover provider polling");
        assert!(error.to_string().contains("total deadline"));
        let port_error = awaken_session_environment_contract::ProcessReaper::reap(
            &Supervisor,
            &StuckPoll,
            std::time::Duration::from_millis(5),
        )
        .await
        .expect_err("the injected port cannot claim an unobserved exit");
        assert_eq!(port_error, error.to_string(), "R1 original Host wording");
        let typed = Supervisor::reap_process(&StuckPoll, std::time::Duration::from_millis(5))
            .await
            .expect_err("the typed caller also has a bounded poll");
        assert!(
            matches!(typed, pc::SandboxError::Timeout(ref detail) if detail == PROCESS_REAP_DEADLINE)
        );

        struct ClassifiedPollFailure;

        #[async_trait]
        impl pc::ProcessHandle for ClassifiedPollFailure {
            fn id(&self) -> &str {
                "classified-poll-failure"
            }

            async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
                unreachable!("the first poll fails before waiting")
            }

            async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
                Err(pc::SandboxError::transport("provider channel lost"))
            }

            async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
                unreachable!("the first poll fails before signalling")
            }
        }

        let provider =
            Supervisor::reap_process(&ClassifiedPollFailure, std::time::Duration::from_millis(5))
                .await
                .expect_err("a provider class must not be flattened");
        assert_eq!(
            provider,
            pc::SandboxError::transport("provider channel lost")
        );
    }

    #[tokio::test]
    async fn supervisor_reaps_the_agent_on_cancel() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"message","text":"thinking"}"#.into()],
            true,
        ));
        let (process, signalled) = FakeProcess::new(true);
        let (_tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut sink = RecordingSink::default();

        let reason = Supervisor::supervise(
            ours.as_mut(),
            &process,
            "go",
            &mut sink,
            tokio::time::sleep(std::time::Duration::from_millis(30)),
            &mut rx,
            fast_policy(),
            Codec::Newline,
            None,
        )
        .await
        .unwrap();

        assert_eq!(reason, TerminationReason::Cancelled);
        assert_eq!(signalled.lock().unwrap()[0], pc::Signal::Term);
        assert!(
            !sink.events.is_empty(),
            "the pre-cancel message was committed"
        );
        agent.abort();
    }

    #[tokio::test]
    async fn supervisor_times_out_on_the_turn_deadline() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"message","text":"slow"}"#.into()],
            true,
        ));
        let (process, signalled) = FakeProcess::new(true);
        let (_tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut sink = RecordingSink::default();
        let policy = SupervisePolicy {
            turn_deadline: Some(std::time::Duration::from_millis(30)),
            reap_grace: std::time::Duration::from_millis(20),
        };

        let reason = Supervisor::supervise(
            ours.as_mut(),
            &process,
            "go",
            &mut sink,
            std::future::pending::<()>(),
            &mut rx,
            policy,
            Codec::Newline,
            None,
        )
        .await
        .unwrap();
        assert_eq!(reason, TerminationReason::TimedOut);
        assert_eq!(signalled.lock().unwrap()[0], pc::Signal::Term);
        agent.abort();
    }

    #[tokio::test]
    async fn supervisor_honors_a_mid_turn_interrupt_injection() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"message","text":"busy"}"#.into()],
            true,
        ));
        let (process, signalled) = FakeProcess::new(true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(Injection::Interrupt).await.unwrap();
        let mut sink = RecordingSink::default();

        let reason = Supervisor::supervise(
            ours.as_mut(),
            &process,
            "go",
            &mut sink,
            std::future::pending::<()>(),
            &mut rx,
            fast_policy(),
            Codec::Newline,
            None,
        )
        .await
        .unwrap();
        assert_eq!(reason, TerminationReason::Cancelled);
        assert!(!signalled.lock().unwrap().is_empty());
        agent.abort();
    }

    #[tokio::test]
    async fn supervisor_returns_agent_outcome_when_nothing_interrupts() {
        let (mut ours, theirs) = combined();
        let agent = tokio::spawn(fake_agent(
            theirs,
            vec![r#"{"type":"turn_end","reason":"natural_end"}"#.into()],
            false,
        ));
        let (process, _sig) = FakeProcess::new(false);
        let (_tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut sink = RecordingSink::default();
        let reason = Supervisor::supervise(
            ours.as_mut(),
            &process,
            "go",
            &mut sink,
            std::future::pending::<()>(),
            &mut rx,
            SupervisePolicy::default(),
            Codec::Newline,
            None,
        )
        .await
        .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();
    }
}

#[cfg(test)]
#[path = "tests/cleanup.rs"]
mod cleanup_tests;

#[cfg(test)]
mod exhaustive {
    use super::*;

    pub(crate) const EVERY_PERMISSION_CONSENSUS: [PermissionConsensus; 3] = [
        PermissionConsensus::Allow,
        PermissionConsensus::Await,
        PermissionConsensus::Deny,
    ];

    const fn listed_permission_consensus(consensus: PermissionConsensus) -> usize {
        match consensus {
            PermissionConsensus::Allow => 0,
            PermissionConsensus::Await => 1,
            PermissionConsensus::Deny => 2,
        }
    }

    #[test]
    fn every_permission_consensus_is_listed_once() {
        for (index, consensus) in EVERY_PERMISSION_CONSENSUS.into_iter().enumerate() {
            assert_eq!(listed_permission_consensus(consensus), index);
        }
    }

    /// @exhaustive PermissionConsensus(3) x PermissionConsensus(3) x PermissionConsensus(3) = 27
    /// @obligation acp.mcp_alias_permission_consensus_never_widens_authority
    #[test]
    fn permission_consensus_is_a_conservative_order_independent_semilattice() {
        for a in EVERY_PERMISSION_CONSENSUS {
            for b in EVERY_PERMISSION_CONSENSUS {
                for c in EVERY_PERMISSION_CONSENSUS {
                    assert_eq!(combine_permission_consensus(a, a), a);
                    assert_eq!(
                        combine_permission_consensus(a, b),
                        combine_permission_consensus(b, a)
                    );
                    assert_eq!(
                        combine_permission_consensus(combine_permission_consensus(a, b), c),
                        combine_permission_consensus(a, combine_permission_consensus(b, c))
                    );
                    assert_eq!(
                        combine_permission_consensus(a, PermissionConsensus::Allow),
                        a
                    );
                    assert_eq!(
                        combine_permission_consensus(a, PermissionConsensus::Deny),
                        PermissionConsensus::Deny
                    );
                }
            }
        }
    }
}
