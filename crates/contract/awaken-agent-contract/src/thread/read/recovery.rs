//! Claim-scoped recovery reads for a database-independent Worker.
//!
//! A remote Worker cannot reconstruct a Run by calling the synchronous read
//! ports independently: a commit between those calls could combine values from
//! different committed prefixes. This port returns the complete execution view
//! under one backend consistency boundary. The snapshot is read-only; commit
//! authority remains with the coordinator.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Record as RunRecord};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;

/// One active resume ticket paired with the Run whose disposition owns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResumeTicket {
    pub run_id: RunId,
    pub ticket: ResumeTicket,
}

/// One internally consistent prefix of committed Thread truth.
///
/// `thread_version` is the count of commits on this Thread and is therefore the
/// optimistic-concurrency version for the aggregate. `store_cursor` is the
/// backend-wide committed sequence used only for diagnostics/feed backfill.
/// They are intentionally distinct: another Thread advancing `store_cursor`
/// must not conflict this Thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecoverySnapshot {
    pub thread_id: ThreadId,
    pub claimed_run_id: RunId,
    pub runs: Vec<RunRecord>,
    pub latest_run_id: Option<RunId>,
    pub messages: Vec<Message>,
    pub state: Vec<StateCommand>,
    pub resume_tickets: Vec<RunResumeTicket>,
    pub thread_version: u64,
    pub store_cursor: u64,
    /// The stable ordinal a recovered Worker uses for the claimed Run's next
    /// logical commit. It is the number of that Run's committed operations.
    pub next_commit_ordinal: u64,
}

/// Failure to materialize an authoritative recovery prefix.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("recovery source rejected: {0}")]
    Rejected(String),
}

/// Asynchronous because a distributed store must read the authoritative
/// database in one repeatable-read (or stronger) transaction.
#[async_trait]
pub trait RunRecoverySource: Send + Sync {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError>;
}
