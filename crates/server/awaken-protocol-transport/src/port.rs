//! The neutral runtime seam every protocol adapter drives (DDD port).
//!
//! Implemented by the server over the neutral shared host; an adapter never
//! constructs a runtime. The vocabulary is neutral — `Message`, a step outcome, a
//! pending tool, a resume command — with no wire types, so one host backs every
//! adapter (AG-UI, AI SDK, A2A) on the same thread, and a turn started through one
//! protocol is resumable and observable through another.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::project::{AgentEvent, terminal_waiting};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use serde_json::Value;

/// A tool a run parked on.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
    /// True when the *client* runs the tool and returns the result; false for a
    /// built-in tool awaiting a permission decision.
    pub client_executed: bool,
}

/// A run that ended in a terminal fault, classified (code + message) — the neutral
/// twin of the Managed `TurnFailure`. Carried so every wire adapter can surface a
/// failed run instead of projecting a silent, empty finish.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepFailure {
    /// The fault code (e.g. `inference_failed`), stable across wires.
    pub code: String,
    /// A human-readable message describing the fault.
    pub message: String,
}

/// How a step ended — the single terminal-state authority. A step reaches exactly
/// one of these, and the parked tool (if any) lives *inside* [`Terminal::Waiting`],
/// so both "waiting *and* failed" and "a pending tool on a finished run" are
/// unrepresentable.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Terminal {
    /// The run reached its natural end.
    #[default]
    Finished,
    /// The run stopped by exhausting its step budget.
    Exhausted,
    /// The run parked awaiting a decision / client result. `pending` names the tool
    /// it parked on, or `None` when it awaits non-tool input (e.g. a user message).
    Waiting { pending: Option<Pending> },
    /// The run ended in a terminal fault (`EndCause::Error`). Adapters render it as
    /// their error frame (AI-SDK `error`, AG-UI `RUN_ERROR`, A2A a `failed` Task)
    /// rather than a silent, empty finish.
    Failed(StepFailure),
}

/// The result of one step (a turn or a resume): the messages it committed and how
/// it ended. `new_messages` is common to every ending, so it stays on the struct;
/// the variant-specific terminal data lives in [`Terminal`].
#[derive(Debug, Clone, Default)]
pub struct StepOutcome {
    /// Messages committed during this step, in order.
    pub new_messages: Vec<Message>,
    /// How the step ended.
    pub terminal: Terminal,
}

impl StepOutcome {
    /// The tool the run parked on, if it parked on one. `Some` only when
    /// `terminal` is [`Terminal::Waiting`] with a tool — the type makes a pending
    /// tool on any other ending impossible. Also drives the last tool call's
    /// disposition in the message projection.
    pub fn pending(&self) -> Option<&Pending> {
        match &self.terminal {
            Terminal::Waiting { pending } => pending.as_ref(),
            _ => None,
        }
    }

    /// The neutral terminal event this step closes with — the single owner of the
    /// failed / parked / finished distinction. Event-stream adapters (AI-SDK,
    /// AG-UI) transcode it; the request/response A2A adapter maps the same states
    /// onto a `Task` state directly.
    pub fn terminal_event(&self) -> AgentEvent {
        match &self.terminal {
            Terminal::Failed(failure) => AgentEvent::RunFailed {
                code: failure.code.clone(),
                message: failure.message.clone(),
            },
            Terminal::Waiting { pending } => {
                terminal_waiting(pending.as_ref().map(|p| p.tool_use_id.as_str()))
            }
            Terminal::Exhausted => AgentEvent::RunFinished { exhausted: true },
            Terminal::Finished => AgentEvent::RunFinished { exhausted: false },
        }
    }
}

/// A resume command targeting the pending tool.
#[derive(Debug, Clone)]
pub enum Resume {
    /// Answer a built-in tool's permission gate.
    Confirm { allow: bool, note: Option<String> },
    /// Deliver a client-executed tool's result.
    ClientResult { content: String, is_error: bool },
}

/// A driver failure classified by fault.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
}

/// The neutral runtime seam. Implemented by the server over the shared host.
#[async_trait]
pub trait ProtocolRuntime: Send + Sync {
    /// Run `thread` once with the (already converted) new `messages`,
    /// optionally naming the agent. Runs to the first park or the natural end.
    async fn run(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError>;

    /// Run `thread` once, forwarding the engine's best-effort live progress to `sink`
    /// as it happens, and still returning the committed [`StepOutcome`]. The
    /// default ignores `sink` and delegates to [`ProtocolRuntime::run`] — a
    /// runtime with no live channel degrades to the committed projection only,
    /// which is correct because the live stream is never the source of truth
    /// (G10/G13). Adapters that want a chunked stream call this and drain `sink`.
    async fn run_streaming(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, DriverError> {
        let _ = sink;
        self.run(thread, agent, messages).await
    }

    /// Resume the run parked on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and the resume
    /// variant matches its binding.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: Resume,
    ) -> Result<StepOutcome, DriverError>;

    /// The tool a run on `thread` is parked on, if any.
    async fn pending(&self, thread: &str) -> Option<Pending>;

    /// All committed messages on `thread` (history), oldest first.
    async fn history(&self, thread: &str) -> Vec<Message>;

    /// The model id echoed in adapter metadata (AI SDK / AG-UI) or the A2A card.
    fn model(&self) -> String;

    /// The thread's accumulated token usage `(input_tokens, output_tokens)` across all
    /// turns, for an adapter that surfaces usage in its wire (e.g. the AI SDK `finish`
    /// part). Default `(0, 0)` — a runtime whose provider reports no usage.
    async fn usage(&self, _thread: &str) -> (u64, u64) {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A runtime that only counts `run` calls, to prove the default
    /// `run_streaming` degrades to `run` (best-effort: no live channel).
    #[derive(Default)]
    struct CountingRuntime {
        runs: AtomicUsize,
    }

    #[async_trait]
    impl ProtocolRuntime for CountingRuntime {
        async fn run(
            &self,
            _thread: &str,
            _agent: Option<String>,
            _messages: Vec<Message>,
        ) -> Result<StepOutcome, DriverError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(StepOutcome {
                new_messages: Vec::new(),
                terminal: Terminal::Finished,
            })
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _resume: Resume,
        ) -> Result<StepOutcome, DriverError> {
            unreachable!()
        }

        async fn pending(&self, _thread: &str) -> Option<Pending> {
            None
        }

        async fn history(&self, _thread: &str) -> Vec<Message> {
            Vec::new()
        }

        fn model(&self) -> String {
            "test".into()
        }
    }

    struct NoopSink;

    #[async_trait]
    impl StreamSink for NoopSink {
        async fn send(
            &self,
            _event: awaken_agent_contract::stream::event::Event,
        ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_streaming_delegates_to_run() {
        let rt = CountingRuntime::default();
        let outcome = rt
            .run_streaming("t1", None, Vec::new(), Arc::new(NoopSink))
            .await
            .unwrap();
        assert_eq!(outcome.terminal, Terminal::Finished);
        assert_eq!(rt.runs.load(Ordering::SeqCst), 1);
    }
}
