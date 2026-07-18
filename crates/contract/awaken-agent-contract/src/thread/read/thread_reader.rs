//! Committed-thread read access needed to resume an awaiting run.
//!
//! Resume reconstructs the transcript from committed messages and validates the
//! resume against the committed [`ResumeTicket`]. This is an after-commit read
//! port (G1/G13) — it never creates or erases runtime truth.

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::Id as RunId;
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;

pub trait ThreadReader: Send + Sync {
    /// Committed messages for a thread, in commit order.
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message>;

    /// The active awaiting ticket for a run, if it is currently awaiting. A run
    /// that has reached a terminal or resumed state returns `None`, so a stale
    /// resume against an old correlation fails closed.
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket>;

    /// Committed state commands for a thread, in commit order. A run rebuilds the
    /// materialized `Store` from these to read accumulated state during
    /// execution (G1/G13); the default is empty for readers that hold no state.
    fn committed_state(&self, _thread_id: &ThreadId) -> Vec<StateCommand> {
        Vec::new()
    }
}
