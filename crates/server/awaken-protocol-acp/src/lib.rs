//! ACP bridge + supervisor (ADR-0041 Slice 3), agents plane.
//!
//! An **anti-corruption layer** over an opaque agent's protocol stream. It reads
//! the agent's events off an [`AgentChannel`], projects them into neutral
//! [`AgentEvent`]s, and commits them through the [`RunFactAppender`] binding seam —
//! the sole path by which projected truth reaches the runtime store (G13: the
//! bridge appends, it never owns the commit). The [`Supervisor`] drives one turn
//! and reaps the process on cancel, mapping the outcome to a [`TerminationReason`].
//!
//! The framing here is a minimal newline-delimited JSON stand-in for ACP's
//! `session/update`; substituting the official `agent-client-protocol` codec is a
//! change behind this same projection + sink, so nothing downstream moves. The
//! agent process runs OUTSIDE us on a `tool_transparent` tier — its own syscalls
//! are OS-jailed, not routed through our tool layer.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Official ACP codec projection (`real-acp` feature): the same ACL over the real
/// `agent-client-protocol` `SessionUpdate`/`StopReason` types.
#[cfg(feature = "real-acp")]
pub mod real_acp;

/// The official JSON-RPC 2.0 ACP client driver (`real-acp` feature).
#[cfg(feature = "real-acp")]
pub mod jsonrpc;

/// Which wire the bridge speaks to the agent. A per-session datum (each ACP CLI
/// row declares its own), not a build-time global: a test/fixture agent speaks the
/// newline stand-in; a real `claude --acp`/`codex acp` speaks official JSON-RPC.
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

/// A neutral, projected agent event. ACP `session/update` variants collapse onto
/// this small set; the store never sees ACP vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Assistant text.
    Message { text: String },
    /// The agent surfaced a tool call to us (the inbound-tool path). `id` is the
    /// ACP `tool_call_id`, so a later [`ToolResult`](Self::ToolResult) correlates
    /// to this call.
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
    async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), AppendError>;
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

/// The authorization verdict for one [`PermissionAsk`], projected back onto the
/// agent's own offered option (allow/reject).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny,
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

/// A neutral MCP server for the ACP `session/new` `mcpServers` param — the protocol-acp
/// shape the executor projects `McpServerConfig` into (keeping this crate free of the
/// executor's config vocab), mapped to `agent_client_protocol::McpServer` under
/// `real-acp`. A stdio child carries `command`+`args`; an HTTP endpoint carries `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMcpServer {
    pub name: String,
    /// Stdio transport: the child command + args (`url` is then `None`).
    pub command: Option<String>,
    pub args: Vec<String>,
    /// HTTP transport: the endpoint URL (`command` is then `None`).
    pub url: Option<String>,
    /// An `(name, value)` the CLI presents as an auth header (HTTP) or env var (stdio).
    /// α: a broker reference the gateway resolves; β: a raw secret on a trusted launch.
    /// `None` for an unauthenticated server.
    pub auth: Option<(String, String)>,
}

/// The per-turn cross-cutting inputs the driver needs beyond the prompt, bundled
/// so a turn threads as one value rather than a widening parameter list. The Acp
/// codec reads/updates it across the handshake; the newline stand-in ignores all
/// but nothing (it has no session/permission concept).
pub struct TurnConfig<'a> {
    /// Authorizes the agent's mid-turn tool requests (the neutral `ToolPermissionPolicy`
    /// behind an executor adapter).
    pub resolver: &'a dyn PermissionResolver,
    /// MCP servers to hand the CLI at `session/new` (the `AcpSession` interface, for
    /// claude/gemini/opencode). Empty for the newline stand-in and for config-file CLIs
    /// (codex gets a `config.toml` instead).
    pub mcp_servers: Vec<SessionMcpServer>,
    /// In: an ACP session id to resume via `session/load` (a relaunched CLI reloads
    /// its own session, so context survives the per-turn relaunch). Out: the id the
    /// turn used — `session/new`'s fresh id when none was given — so the caller can
    /// resume it next turn. Left `None` by the newline stand-in.
    pub session_id: Option<String>,
    /// In: a session mode to pin via `session/set_mode` after the handshake (an
    /// adapter-local datum — `None` leaves the agent's default). Validated
    /// fail-closed against the modes the agent advertised for the session.
    pub session_mode: Option<String>,
    /// In: the interior working directory the CLI runs the session under (the
    /// sandbox's fixed workspace path). For a CLI that keys sessions by cwd (Claude
    /// Code), holding this stable across relaunches/machines is what lets
    /// `session/load` find the session cross-directory. `None` → `/` (the default).
    pub session_cwd: Option<String>,
}

impl<'a> TurnConfig<'a> {
    /// A config that only authorizes (no session resume, no mode pin) — the default
    /// a plain `run_turn`/`supervise` builds for fixtures.
    #[must_use]
    pub fn new(resolver: &'a dyn PermissionResolver) -> Self {
        Self {
            resolver,
            mcp_servers: Vec::new(),
            session_id: None,
            session_mode: None,
            session_cwd: None,
        }
    }
}

/// Which stage of bringing an ACP agent online a lifecycle notification marks.
/// Ordered install → launch → initialize → ready, aligning to oversight-next's
/// probe stages so a UI renders a consistent progress affordance. `Failed` is the
/// terminal error stage; the accompanying [`AcpLaunchEvent::detail`] says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpLaunchStage {
    /// Dynamically installing the adapter — an `npx` wrapper is pulling its pinned
    /// package into the npm cache. The slow step: seconds to minutes on a cold
    /// cache, near-instant when already cached. A native CLI never emits this.
    Installing,
    /// Spawning the agent process.
    Launching,
    /// ACP handshake in progress (`initialize` + `session/new`).
    Initializing,
    /// The agent is live: handshake done, about to accept the prompt turn.
    Ready,
    /// Bring-up failed at some stage (detail carries the cause).
    Failed,
}

/// One lifecycle notification for an ACP agent's bring-up — the neutral event a UI
/// renders as "installing… / launching… / ready". `detail` optionally carries
/// human-facing context (the adapter package id, a spawn error message).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpLaunchEvent {
    pub stage: AcpLaunchStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AcpLaunchEvent {
    #[must_use]
    pub fn stage(stage: AcpLaunchStage) -> Self {
        Self {
            stage,
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(stage: AcpLaunchStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: Some(detail.into()),
        }
    }
}

/// The seam a host wires to observe an ACP agent's bring-up (install → launch →
/// initialize → ready), so it can publish progress to a UI. Injected like
/// [`RunFactAppender`]; the driver depends on this abstraction, never on the
/// transport. `scope` identifies the run/thread the event belongs to (so a
/// per-thread UI channel can route it). Fire-and-forget (sync, non-blocking) so
/// emitting never stalls the launch — a slow observer must buffer internally.
pub trait LaunchObserver: Send + Sync {
    fn on_launch(&self, scope: &str, event: &AcpLaunchEvent);
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
    /// A frame from the agent was not valid JSON / not an `AgentEvent`.
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
            let event: AgentEvent =
                serde_json::from_str(trimmed).map_err(|e| AcpError::Frame(e.to_string()))?;
            seq += 1;
            sink.append(seq, &event).await?;
            if let AgentEvent::TurnEnd { reason } = event {
                return Ok(reason);
            }
        }
    }
}

/// The supervisor: a consumer policy over the process/channel primitives — the
/// session-runner shape. It drives a turn, bounds it with a hard deadline, honors
/// mid-turn interrupts, and reaps the agent `SIGTERM`→(grace)→`SIGKILL` on exit.
pub struct Supervisor;

impl Supervisor {
    /// Reap the agent: `SIGTERM`, wait up to `grace` for it to exit, else escalate
    /// to `SIGKILL`. Returns `true` if the kill escalation was needed. (Process-group
    /// semantics live in the provider; this is the neutral signal ladder.)
    pub async fn reap(
        process: &dyn pc::ProcessHandle,
        grace: std::time::Duration,
    ) -> Result<bool, AcpError> {
        let io = |e: pc::SandboxError| AcpError::Io(e.to_string());
        process.signal(pc::Signal::Term).await.map_err(io)?;
        const STEPS: u32 = 5;
        let step = grace / STEPS;
        for _ in 0..STEPS {
            if process.poll().await.map_err(io)?.is_some() {
                return Ok(false);
            }
            tokio::time::sleep(step).await;
        }
        process.signal(pc::Signal::Kill).await.map_err(io)?;
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
                Self::reap(process, policy.reap_grace).await?;
                Ok(TerminationReason::Cancelled)
            }
            () = cancel => {
                Self::reap(process, policy.reap_grace).await?;
                Ok(TerminationReason::Cancelled)
            }
            () = deadline => {
                Self::reap(process, policy.reap_grace).await?;
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
    struct RecordingSink {
        last: u64,
        events: Vec<(u64, AgentEvent)>,
        fail_at: Option<u64>,
    }

    #[async_trait]
    impl RunFactAppender for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), AppendError> {
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
    struct RecordingLaunch {
        events: Mutex<Vec<(String, AcpLaunchEvent)>>,
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
        assert!(matches!(sink.events[0].1, AgentEvent::Message { .. }));
        assert!(matches!(sink.events[1].1, AgentEvent::ToolCall { .. }));
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

    #[test]
    fn an_acp_launch_event_omits_absent_detail_and_round_trips() {
        // A stageless bring-up event omits `detail` on the wire; one with detail
        // round-trips losslessly (the UI progress affordance's contract).
        let bare = AcpLaunchEvent::stage(AcpLaunchStage::Installing);
        let value = serde_json::to_value(&bare).unwrap();
        assert!(
            value.get("detail").is_none(),
            "absent detail omitted: {value}"
        );
        assert_eq!(value["stage"], "installing");

        let detailed = AcpLaunchEvent::with_detail(AcpLaunchStage::Failed, "spawn failed");
        let back: AcpLaunchEvent =
            serde_json::from_value(serde_json::to_value(&detailed).unwrap()).unwrap();
        assert_eq!(back, detailed);
        assert_eq!(back.detail.as_deref(), Some("spawn failed"));
    }

    // ── AgentEvent wire contract ─────────────────────────────────────────────
    //
    // AgentEvent is the projection contract the newline codec (and the store)
    // depend on: every variant must round-trip, its `#[serde(default)]` fields must
    // tolerate an agent that omits them, and an unknown tag must be a clean error.

    fn round_trip(ev: &AgentEvent) -> AgentEvent {
        let json = serde_json::to_string(ev).expect("serializes");
        serde_json::from_str(&json).expect("deserializes")
    }

    #[tokio::test]
    async fn every_agent_event_variant_round_trips() {
        let events = [
            AgentEvent::Message { text: "hi".into() },
            AgentEvent::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a" }),
            },
            AgentEvent::ToolResult {
                id: "call_1".into(),
                content: "ok".into(),
                is_error: true,
            },
            AgentEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                cache_read_tokens: 3,
                cache_creation_tokens: 4,
            },
            AgentEvent::TurnEnd {
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
        let tc: AgentEvent =
            serde_json::from_str(r#"{"type":"tool_call","name":"ls"}"#).expect("tool_call parses");
        assert_eq!(
            tc,
            AgentEvent::ToolCall {
                id: String::new(),
                name: "ls".into(),
                input: serde_json::Value::Null,
            }
        );
        // A tool_result with neither id nor is_error.
        let tr: AgentEvent = serde_json::from_str(r#"{"type":"tool_result","content":"done"}"#)
            .expect("tool_result parses");
        assert_eq!(
            tr,
            AgentEvent::ToolResult {
                id: String::new(),
                content: "done".into(),
                is_error: false,
            }
        );
        // A usage frame reporting only some token axes; the rest default to 0.
        let us: AgentEvent = serde_json::from_str(r#"{"type":"usage","prompt_tokens":7}"#)
            .expect("partial usage parses");
        assert_eq!(
            us,
            AgentEvent::Usage {
                prompt_tokens: 7,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }
        );
    }

    #[tokio::test]
    async fn an_unknown_event_tag_is_a_clean_deserialize_error() {
        let err = serde_json::from_str::<AgentEvent>(r#"{"type":"nope","text":"x"}"#)
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

    #[tokio::test]
    async fn reap_escalates_to_kill_when_the_agent_ignores_term() {
        let (process, signalled) = FakeProcess::new(false); // never exits on its own
        let killed = Supervisor::reap(&process, std::time::Duration::from_millis(20))
            .await
            .unwrap();
        assert!(killed, "an unresponsive agent must be escalated to SIGKILL");
        assert_eq!(
            signalled.lock().unwrap().as_slice(),
            &[pc::Signal::Term, pc::Signal::Kill]
        );
    }

    #[tokio::test]
    async fn reap_stops_at_term_when_the_agent_exits() {
        let (process, signalled) = FakeProcess::new(true); // exits right after SIGTERM
        let killed = Supervisor::reap(&process, std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(!killed);
        assert_eq!(signalled.lock().unwrap().as_slice(), &[pc::Signal::Term]);
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
