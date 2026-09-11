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
//! Storage failures are explicit. A failed or interrupted read is not evidence
//! that no checkpoint exists. Runtime validates the recovered Run/Thread identity
//! and independent invocation evidence before using the partial; neither a
//! backend nor its caller may manufacture successful absence from a read error.
//!
//! The read is typed [`Observed<StreamCheckpoint>`] rather than
//! `Result<Option<StreamCheckpoint>, _>` so that manufacturing absence is not
//! merely forbidden by this comment but unrepresentable: there is no `Result`
//! to discard and no `Option` to flatten into.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::observation::{Observed, UnknownCause};

/// One tool call as it stood when the stream was interrupted. `raw_arguments` is
/// the provider's accumulated-so-far argument text — raw, unparsed JSON that may
/// be syntactically incomplete if the drop landed mid-arguments. Whether it
/// parses is only a syntax signal. Parseable arguments may be salvaged only
/// after Runtime separately establishes model termination and tool recovery
/// permission; neither valid JSON nor this checkpoint proves quiescence.
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
    /// Transparent provider retries already scheduled for this logical request.
    /// Preserved so a fresh process emits the same completed observation.
    #[serde(default)]
    pub retry_count: u32,
}

/// A write (`put`/`delete`) that did not take effect. Writes keep a `Result`:
/// unlike a read, a failed write has no third outcome to confuse — it simply did
/// not happen, and the caller is told so.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamCheckpointError {
    #[error("stream checkpoint storage unavailable: {0}")]
    Storage(String),
    #[error("stream checkpoint write was fenced: {0}")]
    Fenced(String),
}

impl From<StreamCheckpointError> for UnknownCause {
    /// A failed checkpoint operation leaves the reader ignorant, never informed
    /// of absence. Every error variant therefore becomes an `Unknown` cause and
    /// carries its own diagnostic across the boundary.
    fn from(error: StreamCheckpointError) -> Self {
        match error {
            StreamCheckpointError::Storage(detail) => UnknownCause::Storage(detail),
            StreamCheckpointError::Fenced(detail) => UnknownCause::Fenced(detail),
        }
    }
}

/// Where the runtime flushes and reads an interrupted step's partial. Keyed by
/// `run_id`; `delete` is idempotent. Every method reports storage/fencing errors;
/// callers must not treat an unread checkpoint as permission for fresh execution.
#[async_trait]
pub trait StreamCheckpointStore: Send + Sync {
    /// Observe the checkpoint for `run_id`.
    ///
    /// [`Observed::Absent`] is a *finding*: the backend answered and there is no
    /// checkpoint. A backend that could not answer must return
    /// [`Observed::Unknown`] — returning `Absent` on a storage failure or a
    /// fenced read is the fail-open defect this signature exists to prevent.
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint>;
    /// Persist (overwriting any prior) the in-flight partial for its `run_id`.
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError>;
    /// Remove the checkpoint for `run_id`; a no-op if absent.
    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_checkpoint_without_retry_count_defaults_to_zero() {
        // Cause C1: an older checkpoint omits retry_count. Effect E1: decode
        // preserves its partial and initializes the neutral counter to zero.
        // Decision rule R1=C1=>E1.
        // Constraints/invariants: compatibility supplies only the absent field;
        // existing Run, Thread, model, text, and tool partials remain unchanged.
        let checkpoint: StreamCheckpoint = serde_json::from_value(serde_json::json!({
            "run_id": "run",
            "thread_id": "thread",
            "model": "model",
            "partial_text": "partial",
            "partial_tools": []
        }))
        .expect("R1/E1");
        assert_eq!(checkpoint.retry_count, 0, "R1/E1");
    }

    #[test]
    fn every_checkpoint_error_becomes_an_unknown_cause_not_absence() {
        // Cause C2: a checkpoint operation fails with storage or fencing.
        // Effect E2: converting it for a reader yields the matching
        // UnknownCause with its detail intact. Decision rule R2=C2=>E2.
        // Constraints/invariants: no error variant maps to Absent; the
        // conversion is total, so a new error variant cannot silently acquire
        // "nothing is there" semantics.
        assert_eq!(
            UnknownCause::from(StreamCheckpointError::Storage("disk".into())),
            UnknownCause::Storage("disk".into()),
            "R2/E2"
        );
        assert_eq!(
            UnknownCause::from(StreamCheckpointError::Fenced("stale".into())),
            UnknownCause::Fenced("stale".into()),
            "R2/E2"
        );
    }
}
