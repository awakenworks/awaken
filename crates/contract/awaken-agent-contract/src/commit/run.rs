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
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::agent::waiting::WaitingTicket;
use crate::commit::coordinator::{Coordinator, Error};
use crate::commit::staged::{CommitRecord, ThreadCommit};

/// Commit a run's terminal facts — its produced messages, final phase, and any
/// committed state — through the single commit boundary (G1/G13). The one place a
/// `Vec<Message>` + `Phase` becomes committed truth, shared by the native, ACP,
/// and A2A executors.
///
/// A `Phase::Waiting` park carries its resumable `WaitingTicket` (a `Park` at the
/// safe loop boundary, ADR-0054), so a paused ACP run persists the same durable
/// waiting authority the native engine commits; a `RunWaiting` event rides the
/// same commit. A terminus passes `None` and no waiting event is emitted. `state`
/// carries committed thread state (e.g. the ACP turn's accumulated token usage);
/// a `StateChanged` event rides the commit when it is non-empty.
pub async fn commit_run(
    coordinator: &dyn Coordinator,
    thread_id: &ThreadId,
    run_id: &RunId,
    messages: Vec<Message>,
    phase: Phase,
    waiting: Option<WaitingTicket>,
    state: Vec<StateCommand>,
) -> Result<CommitRecord, Error> {
    // A projected turn always transitions phase (it ended or parked), so it emits
    // `RunPhaseChanged` like the native loop — one assembler, one fact trail.
    coordinator
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            run_id.clone(),
            phase,
            true,
            messages,
            state,
            waiting,
            Vec::new(),
        ))
        .await
}
