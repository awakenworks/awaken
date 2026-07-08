//! ACP bridge + supervisor (ADR-0041 Slice 3), agents plane.
//!
//! An **anti-corruption layer** over an opaque agent's protocol stream. It reads
//! the agent's events off an [`AgentChannel`], projects them into neutral
//! [`AgentEvent`]s, and commits them through the [`RunEventSink`] binding seam —
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
    /// The agent surfaced a tool call to us (the inbound-tool path).
    ToolCall {
        name: String,
        #[serde(default)]
        input: serde_json::Value,
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
pub enum SinkError {
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
#[async_trait]
pub trait RunEventSink: Send {
    /// Commit one projected event at `seq` (strictly increasing per run).
    async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), SinkError>;
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
    /// The sink rejected an event.
    #[error("sink: {0}")]
    Sink(#[from] SinkError),
    /// The agent stream ended before emitting a `TurnEnd`.
    #[error("agent stream ended before turn end")]
    Truncated,
}

/// The bridge: drive one turn of an opaque agent over a duplex channel.
pub struct AcpBridge;

impl AcpBridge {
    /// Drive one turn over `channel`, dispatching on the wire [`Codec`]: the
    /// newline stand-in (fixtures) or the official JSON-RPC ACP driver. Both
    /// project into the same [`RunEventSink`] and return the same
    /// [`TerminationReason`], so nothing downstream depends on the wire.
    pub async fn run_turn(
        channel: &mut dyn AgentChannel,
        prompt: &str,
        sink: &mut dyn RunEventSink,
        codec: Codec,
    ) -> Result<TerminationReason, AcpError> {
        match codec {
            Codec::Newline => Self::run_turn_newline(channel, prompt, sink).await,
            #[cfg(feature = "real-acp")]
            Codec::Acp => crate::jsonrpc::run_turn(channel, prompt, sink).await,
        }
    }

    /// Send `prompt` to the agent, then read its newline-JSON event frames until
    /// `TurnEnd`, projecting each into the [`RunEventSink`] with a strictly
    /// increasing seq.
    ///
    /// Cancel-safe at frame boundaries: a dropped future may leave events already
    /// committed, which is correct (they happened) — the supervisor maps the miss.
    async fn run_turn_newline(
        channel: &mut dyn AgentChannel,
        prompt: &str,
        sink: &mut dyn RunEventSink,
    ) -> Result<TerminationReason, AcpError> {
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
    pub async fn supervise(
        channel: &mut dyn AgentChannel,
        process: &dyn pc::ProcessHandle,
        prompt: &str,
        sink: &mut dyn RunEventSink,
        cancel: impl std::future::Future<Output = ()>,
        injections: &mut tokio::sync::mpsc::Receiver<Injection>,
        policy: SupervisePolicy,
        codec: Codec,
    ) -> Result<TerminationReason, AcpError> {
        let deadline = async {
            match policy.turn_deadline {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            outcome = AcpBridge::run_turn(channel, prompt, sink, codec) => outcome,
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
    impl RunEventSink for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), SinkError> {
            if let Some(f) = self.fail_at
                && seq == f
            {
                return Err(SinkError::Append("store down".into()));
            }
            if seq <= self.last {
                return Err(SinkError::NonMonotonic {
                    got: seq,
                    last: self.last,
                });
            }
            self.last = seq;
            self.events.push((seq, event.clone()));
            Ok(())
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
        let reason = AcpBridge::run_turn(ours.as_mut(), "do it", &mut sink, Codec::Newline)
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
            let got = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline)
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
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline)
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
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline)
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
        let err = AcpBridge::run_turn(ours.as_mut(), "p", &mut sink, Codec::Newline)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Sink(SinkError::Append(_))));
        agent.abort();
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
        )
        .await
        .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();
    }
}
