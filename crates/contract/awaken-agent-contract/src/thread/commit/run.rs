//! The shared terminal-commit path (G13).
//!
//! Every `RunExecutor` — the native loop's `finish`, the ACP bridge's projected
//! turn, the A2A task reply — ends by turning a `Vec<Message>` plus a final
//! `RunState` into durable truth through the one commit boundary. That construction
//! is identical regardless of how the run executed, so it lives here once and the
//! fact log stays uniform across execution sources instead of each executor
//! re-rolling the same `ThreadCommit`.

use crate::agent::message::Message;
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::thread::commit::coordinator::{Coordinator, Error};
use crate::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};

/// Commit a run's terminal facts — its produced messages, final state, and any
/// committed state — through the single commit boundary (G1/G13). The one place a
/// `Vec<Message>` + `RunState` becomes committed truth, shared by the native, ACP,
/// and A2A executors.
///
/// A `RunState::Awaiting` await carries its resumable `ResumeTicket` (a `Await` at the
/// safe loop boundary, ADR-0054), so a paused ACP run persists the same durable
/// awaiting authority the native engine commits; a `RunAwaiting` event rides the
/// same commit. An end passes `None` and no awaiting event is emitted. `state`
/// carries committed thread state (e.g. the ACP turn's accumulated token usage);
/// a `StateChanged` event rides the commit when it is non-empty.
pub async fn commit_run(
    coordinator: &dyn Coordinator,
    thread_id: &ThreadId,
    run: RunDisposition,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<CommitRecord, Error> {
    // A projected turn always transitions state (it ended or is awaiting), so it emits
    // `RunStateChanged` like the native loop — one assembler, one fact trail.
    coordinator
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            run,
            true,
            messages,
            state,
            Vec::new(),
        ))
        .await
}
