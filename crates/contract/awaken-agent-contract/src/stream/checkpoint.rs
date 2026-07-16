//! Durable snapshot of an interrupted inference stream, for cross-process resume.
//!
//! A model response can be cut off mid-stream (a transient transport drop, an
//! idle stall). The runtime recovers *in-process* by continuing from the partial
//! rather than regenerating (R1–R3 recovery). This port makes that recovery
//! survive a *process* death: at an interruption boundary the runtime flushes a
//! [`StreamCheckpoint`] — the accumulated partial for the single in-flight step —
//! and a later process resumes from it instead of re-running the whole step.
//!
//! It is **not** a conversation log: committed messages live in the commit
//! coordinator. The checkpoint holds only the uncommitted in-flight tail, keyed
//! by `run_id` (per-step commit means at most one step is ever in flight). It is
//! deleted the moment recovery concludes, so it survives only the narrow window
//! between an interruption and its resolution — exactly the crash it guards.
//!
//! Writes are best-effort: a failing checkpoint store must never disrupt the
//! live stream. Implementations swallow their errors (a `put`/`delete` that
//! fails is a lost optimization, not a run failure; a failed `get` yields
//! `None`, i.e. "resume nothing").

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One tool call as it stood when the stream was interrupted. `raw_arguments` is
/// the provider's accumulated-so-far argument text — raw, unparsed JSON that may
/// be syntactically incomplete if the drop landed mid-arguments. Whether it
/// parses is the completion signal: parseable ⇒ the model finished emitting this
/// call (replay it); unparseable ⇒ it was still in flight (drop it and continue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialToolCall {
    pub call_id: String,
    pub tool_id: String,
    pub raw_arguments: String,
}

/// The accumulated partial of a single in-flight inference step, enough to resume
/// it in a fresh process without regenerating what already arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCheckpoint {
    /// Primary key. Per-step commit guarantees at most one in-flight step per run.
    pub run_id: String,
    /// The run's thread, for inspection and per-thread cleanup.
    pub thread_id: String,
    /// The model the interrupted attempt targeted, for staleness/routing checks.
    pub model: String,
    /// Assistant text accumulated across the step's attempts before interruption.
    pub partial_text: String,
    /// Tool calls seen when the stream dropped (completed and/or in flight).
    pub partial_tools: Vec<PartialToolCall>,
}

/// Where the runtime flushes and reads an interrupted step's partial. Keyed by
/// `run_id`; `delete` is idempotent. Every method is best-effort — see the module
/// docs: a store fault degrades to "no resume", never a run failure.
#[async_trait]
pub trait StreamCheckpointStore: Send + Sync {
    /// The checkpoint for `run_id`, or `None` if there is none (or on any error).
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint>;
    /// Persist (overwriting any prior) the in-flight partial for its `run_id`.
    async fn put(&self, checkpoint: StreamCheckpoint);
    /// Remove the checkpoint for `run_id`; a no-op if absent.
    async fn delete(&self, run_id: &str);
}
