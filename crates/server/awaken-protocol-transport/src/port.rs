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
use awaken_agent_contract::event::{Fact, terminal_waiting};
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
/// twin of the Managed `StepFailure`. Carried so every wire adapter can surface a
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
    pub fn terminal_event(&self) -> Fact {
        match &self.terminal {
            Terminal::Failed(failure) => Fact::RunFailed {
                code: failure.code.clone(),
                message: failure.message.clone(),
            },
            Terminal::Waiting { pending } => {
                terminal_waiting(pending.as_ref().map(|p| p.tool_use_id.as_str()))
            }
            Terminal::Exhausted => Fact::RunFinished { exhausted: true },
            Terminal::Finished => Fact::RunFinished { exhausted: false },
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

    /// Interrupt the run in flight on `thread`, if any: cancel it so an in-progress
    /// turn ends promptly instead of running to completion. A no-op when nothing is
    /// running. The default is a no-op — a transport with no cancel path (e.g. a
    /// pure request/response adapter) keeps its prior behavior — but every adapter
    /// that can observe a client going away (a dropped SSE stream, a closed socket)
    /// should call this so an abandoned turn stops burning tokens. This is the
    /// protocol-neutral cancel verb; the managed `user.interrupt` and an ai-sdk /
    /// ag-ui client disconnect all converge here.
    async fn interrupt(&self, thread: &str) -> Result<(), DriverError> {
        let _ = thread;
        Ok(())
    }

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

    #[tokio::test]
    async fn default_usage_is_zero_zero() {
        // A runtime whose provider reports no usage degrades to (0, 0) rather than
        // fabricating a count.
        let rt = CountingRuntime::default();
        assert_eq!(rt.usage("t1").await, (0, 0));
    }

    #[tokio::test]
    async fn default_interrupt_is_a_noop_ok() {
        // A transport with no cancel path (CountingRuntime does not override it) gets
        // the default no-op — calling the neutral cancel verb never errors, so an
        // adapter can always wire a client disconnect to it safely.
        let rt = CountingRuntime::default();
        assert!(rt.interrupt("t1").await.is_ok());
    }

    fn outcome(terminal: Terminal) -> StepOutcome {
        StepOutcome {
            new_messages: Vec::new(),
            terminal,
        }
    }

    fn a_pending() -> Pending {
        Pending {
            tool_use_id: "call-7".into(),
            name: "bash".into(),
            input: serde_json::json!({ "cmd": "ls" }),
            client_executed: false,
        }
    }

    #[test]
    fn default_step_outcome_finishes_with_no_messages() {
        let o = StepOutcome::default();
        assert!(o.new_messages.is_empty());
        assert_eq!(o.terminal, Terminal::Finished);
        assert_eq!(Terminal::default(), Terminal::Finished);
    }

    #[test]
    fn finished_projects_to_run_finished_not_exhausted() {
        assert_eq!(
            outcome(Terminal::Finished).terminal_event(),
            Fact::RunFinished { exhausted: false }
        );
    }

    #[test]
    fn exhausted_projects_to_run_finished_exhausted() {
        // The budget-exhausted terminus is still a *finish*, flagged exhausted — not
        // a failure. Adapters must not render it as an error frame.
        assert_eq!(
            outcome(Terminal::Exhausted).terminal_event(),
            Fact::RunFinished { exhausted: true }
        );
    }

    #[test]
    fn failed_projects_to_run_failed_carrying_code_and_message() {
        // The failed terminal must reach the wire as a distinct RunFailed event, not
        // collapse into a silent empty finish (the dropped-error-channel hazard).
        let ev = outcome(Terminal::Failed(StepFailure {
            code: "inference_failed".into(),
            message: "upstream 503".into(),
        }))
        .terminal_event();
        assert_eq!(
            ev,
            Fact::RunFailed {
                code: "inference_failed".into(),
                message: "upstream 503".into(),
            }
        );
    }

    #[test]
    fn waiting_on_a_tool_names_it_in_the_terminal_event() {
        let ev = outcome(Terminal::Waiting {
            pending: Some(a_pending()),
        })
        .terminal_event();
        assert_eq!(
            ev,
            Fact::Waiting {
                pending_tool_use_id: Some("call-7".into()),
            }
        );
    }

    #[test]
    fn waiting_without_a_tool_carries_no_tool_id() {
        let ev = outcome(Terminal::Waiting { pending: None }).terminal_event();
        assert_eq!(
            ev,
            Fact::Waiting {
                pending_tool_use_id: None,
            }
        );
    }

    #[test]
    fn pending_is_some_only_when_waiting_on_a_tool() {
        assert_eq!(
            outcome(Terminal::Waiting {
                pending: Some(a_pending()),
            })
            .pending(),
            Some(&a_pending())
        );
        // Waiting on non-tool input, and every non-waiting terminal, has no pending.
        assert!(
            outcome(Terminal::Waiting { pending: None })
                .pending()
                .is_none()
        );
        assert!(outcome(Terminal::Finished).pending().is_none());
        assert!(outcome(Terminal::Exhausted).pending().is_none());
        assert!(
            outcome(Terminal::Failed(StepFailure::default()))
                .pending()
                .is_none()
        );
    }
}
