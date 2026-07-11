//! The shared terminal-commit path (G13).
//!
//! Every `RunExecutor` — the native loop's `finish`, the ACP bridge's projected
//! turn, the A2A task reply — ends by turning a `Vec<Message>` plus a final
//! `Phase` into durable truth through the one commit boundary. That construction
//! is identical regardless of how the run executed, so it lives here once and the
//! fact log stays uniform across execution sources instead of each executor
//! re-rolling the same `ThreadCommit`.

use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Phase};
use crate::agent::thread::Id as ThreadId;
use crate::commit::coordinator::{Coordinator, Error};
use crate::commit::staged::{CommitRecord, ThreadCommit};
use crate::fact::run::Fact as RunFact;

/// Commit a run's terminal facts — its produced messages and final phase —
/// through the single commit boundary (G1/G13). The one place a `Vec<Message>` +
/// `Phase` becomes committed truth, shared by the native, ACP, and A2A executors.
pub async fn commit_run(
    coordinator: &dyn Coordinator,
    thread_id: &ThreadId,
    run_id: &RunId,
    messages: Vec<Message>,
    phase: Phase,
) -> Result<CommitRecord, Error> {
    coordinator
        .commit(ThreadCommit {
            thread_id: thread_id.clone(),
            run_fact: RunFact {
                run_id: run_id.clone(),
                phase,
            },
            messages,
            state: Vec::new(),
            events: Vec::new(),
            outbox: Vec::new(),
            waiting: None,
        })
        .await
}
