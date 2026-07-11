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
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use serde_json::Value;

/// A tool a run parked on.
#[derive(Debug, Clone)]
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
    /// True when the *client* runs the tool and returns the result; false for a
    /// built-in tool awaiting a permission decision.
    pub client_executed: bool,
}

/// The result of one step (a turn or a resume).
#[derive(Debug, Clone)]
pub struct StepOutcome {
    /// Messages committed during this step, in order.
    pub new_messages: Vec<Message>,
    /// True when the run parked (awaiting a decision / client result).
    pub waiting: bool,
    /// True when the run stopped by exhausting its step budget.
    pub exhausted: bool,
    /// The tool the run parked on, when `waiting`.
    pub pending: Option<Pending>,
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
                waiting: false,
                exhausted: false,
                pending: None,
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
        assert!(!outcome.waiting);
        assert_eq!(rt.runs.load(Ordering::SeqCst), 1);
    }
}
