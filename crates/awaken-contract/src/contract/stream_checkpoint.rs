//! Cross-process stream resume: persist mid-stream accumulated state so a
//! new process (or a retry after a hard crash) can pick up where the
//! previous attempt left off, instead of re-inferring the whole turn.
//!
//! ## Semantics
//!
//! A `StreamCheckpoint` is a *snapshot of accumulated deltas* scoped to a
//! single `run_id`. Writers (the loop runner's `drive_one_stream`) flush
//! the snapshot periodically as deltas arrive. Readers (the start of a
//! new `execute_streaming` call) look up any stored checkpoint for the
//! active `run_id` and, if present, mutate the request to include the
//! previously-accumulated text as an assistant prefix + a continuation
//! prompt — mechanically identical to the in-process R1 recovery path.
//!
//! ## Non-goals
//!
//! - The checkpoint is **not** a full conversation log. Committed
//!   messages are still owned by `ThreadRunStore`. The checkpoint only
//!   captures the in-flight delta accumulator.
//! - The contract does **not** prescribe a retention policy. Production
//!   implementations may bound the checkpoint TTL; the in-memory store
//!   keeps everything until explicitly deleted.
//! - The contract is provider-agnostic. A NATS JetStream-backed impl,
//!   a filesystem impl, or a Redis impl all satisfy this trait.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::executor::InFlightTool;
use super::message::ToolCall;

/// A persisted snapshot of in-flight stream accumulator state for a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamCheckpoint {
    /// Identifier of the run this checkpoint belongs to. Uniquely keys
    /// the checkpoint in storage.
    pub run_id: String,
    /// Identifier of the containing thread. Carried for operator
    /// inspection and for scoped bulk deletion (per-thread cleanup).
    pub thread_id: String,
    /// Upstream model the interrupted attempt targeted.
    pub upstream_model: String,
    /// Text content accumulated before interruption.
    pub partial_text: String,
    /// Tool calls whose arguments parsed cleanly before interruption.
    /// On resume these are replayed as completed; the model does not
    /// re-emit them.
    pub completed_tool_calls: Vec<ToolCall>,
    /// The open tool whose arguments had not finished arriving. Used to
    /// surface a cancelled-tool hint on resume, same as R2.
    pub in_flight_tool: Option<InFlightTool>,
    /// Wall-clock millis when the writer last updated the checkpoint.
    /// Used to bound staleness on read.
    pub updated_at_ms: u64,
}

/// Backend-level failure (IO, network, serialization). The contract is a
/// newtype rather than a multi-variant enum because every checkpoint
/// backend collapses its native error to a string at this boundary —
/// callers never branch on the cause.
#[derive(Debug, Error)]
#[error("stream checkpoint backend error: {0}")]
pub struct StreamCheckpointError(pub String);

/// Persistence for stream checkpoints.
///
/// The trait is deliberately small: implementations are free to batch,
/// buffer, or shard under the hood. Callers assume sub-millisecond
/// latency for the in-memory path and bounded (single-digit ms) latency
/// for production backends.
#[async_trait]
pub trait StreamCheckpointStore: Send + Sync {
    /// Upsert a checkpoint for `checkpoint.run_id`.
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError>;

    /// Look up the most recent checkpoint for `run_id`, if any.
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError>;

    /// Remove the checkpoint for `run_id`. Idempotent: removing a
    /// nonexistent key is not an error.
    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError>;
}

/// Reference in-memory implementation. Suitable for tests and for
/// single-process operation where cross-process resume is not needed
/// but the in-process retry loop still benefits from the same
/// checkpoint interface.
pub struct InMemoryStreamCheckpointStore {
    data: Mutex<HashMap<String, StreamCheckpoint>>,
}

impl Default for InMemoryStreamCheckpointStore {
    fn default() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

impl InMemoryStreamCheckpointStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count stored checkpoints (test-only helper).
    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    /// True when no checkpoints are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl StreamCheckpointStore for InMemoryStreamCheckpointStore {
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        let mut guard = self.data.lock().unwrap();
        guard.insert(checkpoint.run_id.clone(), checkpoint);
        Ok(())
    }

    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        Ok(self.data.lock().unwrap().get(run_id).cloned())
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        self.data.lock().unwrap().remove(run_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(run_id: &str) -> StreamCheckpoint {
        StreamCheckpoint {
            run_id: run_id.into(),
            thread_id: "thread-1".into(),
            upstream_model: "test-model".into(),
            partial_text: "hello".into(),
            completed_tool_calls: vec![],
            in_flight_tool: None,
            updated_at_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn in_memory_put_get_delete_roundtrip() {
        let store = InMemoryStreamCheckpointStore::new();
        assert!(store.is_empty());

        store.put(sample("run-a")).await.unwrap();
        store.put(sample("run-b")).await.unwrap();
        assert_eq!(store.len(), 2);

        let got = store.get("run-a").await.unwrap().unwrap();
        assert_eq!(got.run_id, "run-a");
        assert_eq!(got.partial_text, "hello");

        store.delete("run-a").await.unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("run-a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_not_an_error() {
        let store = InMemoryStreamCheckpointStore::new();
        store.delete("no-such-run").await.unwrap();
    }

    #[tokio::test]
    async fn put_overwrites_existing_entry() {
        let store = InMemoryStreamCheckpointStore::new();
        store.put(sample("run-a")).await.unwrap();

        let updated = StreamCheckpoint {
            partial_text: "updated text".into(),
            updated_at_ms: 2_000,
            ..sample("run-a")
        };
        store.put(updated).await.unwrap();

        let got = store.get("run-a").await.unwrap().unwrap();
        assert_eq!(got.partial_text, "updated text");
        assert_eq!(got.updated_at_ms, 2_000);
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let checkpoint = sample("run-1");
        let json = serde_json::to_string(&checkpoint).unwrap();
        let parsed: StreamCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.partial_text, "hello");
    }
}
