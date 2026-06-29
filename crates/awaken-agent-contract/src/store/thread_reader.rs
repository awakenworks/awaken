//! Committed-thread read access needed to resume a parked run.
//!
//! Resume reconstructs the transcript from committed messages and validates the
//! resume against the committed [`WaitingTicket`]. This is an after-commit read
//! port (G1/G13) — it never creates or erases runtime truth.

use crate::agent::message::Message;
use crate::agent::run::Id as RunId;
use crate::agent::thread::Id as ThreadId;
use crate::agent::waiting::WaitingTicket;

pub trait ThreadReader: Send + Sync {
    /// Committed messages for a thread, in commit order.
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message>;

    /// The active waiting ticket for a run, if it is currently parked. A run
    /// that has reached a terminal or resumed state returns `None`, so a stale
    /// resume against an old correlation fails closed.
    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket>;
}
