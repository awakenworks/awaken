//! The single after-commit read repository for a thread's committed truth.
//!
//! `CheckpointReader` is the read side of the commit boundary (G1/G13): it never
//! mutates truth, and it reads from committed facts so a durable run resumes after
//! a process restart (ADR-0006 fact authority; ADR-0039 D4). It extends the
//! process-local execution view with durable event reads; there is no separate
//! Thread/Run persistence port (ADR-0039 D1).

use crate::agent::run::Id as RunId;
use crate::agent::thread::Id as ThreadId;
use crate::audit::record::Record as EventRecord;
use crate::thread::read::committed_thread_view::CommittedThreadView;

/// The scope a committed-event read is addressed by. The event sequence (`u64`)
/// is the cursor; `from` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// Every event visible through this committed-truth partition.
    All,
    Thread(ThreadId),
    Run(RunId),
}

/// The single durable after-commit repository for Thread truth.
pub trait CheckpointReader: CommittedThreadView {
    /// Committed events in a scope, after the exclusive cursor `from`, up to
    /// `limit`, in commit order.
    fn list_events(&self, scope: &EventScope, from: Option<u64>, limit: usize) -> Vec<EventRecord>;
}
