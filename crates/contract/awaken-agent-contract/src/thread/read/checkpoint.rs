//! The single after-commit read repository for a thread's committed truth.
//!
//! `CheckpointReader` is the read side of the commit boundary (G1/G13): it never
//! mutates truth, and it reads from committed facts so a durable run resumes after
//! a process restart (ADR-0006 fact authority; ADR-0039 D4). It is the merged read
//! repository over the older `ThreadReader` (transcript/state/waiting) and
//! `RunStore` (run record) split — one aggregate, one repository (ADR-0039 D1).

use crate::agent::run::{Id as RunId, Record as RunRecord};
use crate::agent::thread::Id as ThreadId;
use crate::audit::record::Record as EventRecord;
use crate::thread::read::thread_reader::ThreadReader;

/// The scope a committed-event read is addressed by. The event sequence (`u64`)
/// is the cursor; `from` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    Thread(ThreadId),
    Run(RunId),
}

/// The after-commit read repository. Extends [`ThreadReader`] (committed messages,
/// state, waiting ticket) with the run record and committed-event reads that
/// resume and projection need.
pub trait CheckpointReader: ThreadReader {
    /// The committed run record for a run id, if any.
    fn run(&self, id: &RunId) -> Option<RunRecord>;

    /// The latest committed run on a thread, if any.
    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord>;

    /// Committed events in a scope, after the exclusive cursor `from`, up to
    /// `limit`, in commit order.
    fn list_events(&self, scope: &EventScope, from: Option<u64>, limit: usize) -> Vec<EventRecord>;
}
