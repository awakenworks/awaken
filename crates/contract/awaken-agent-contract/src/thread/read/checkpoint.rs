//! The single after-commit read repository for a thread's committed truth.
//!
//! `CheckpointReader` is the asynchronous event read side of the commit boundary
//! (G1/G13). Thread and Run recovery uses the existing `RunRecoverySource` atomic
//! snapshot; the synchronous `CommittedThreadView` is an execution view only.

use crate::agent::run::Id as RunId;
use crate::agent::thread::Id as ThreadId;
use crate::audit::record::Record as EventRecord;
use async_trait::async_trait;

/// The scope a committed-event read is addressed by. The event sequence (`u64`)
/// is the cursor; `from` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// Every event visible through this committed-truth partition.
    All,
    Thread(ThreadId),
    Run(RunId),
}

/// The durable after-commit event read port. Storage errors fail the read closed;
/// an empty event page is never used to conceal an unavailable authority.
#[async_trait]
pub trait CheckpointReader: Send + Sync {
    /// Committed events in a scope, after the exclusive cursor `from`, up to
    /// `limit`, in commit order.
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, String>;
}
