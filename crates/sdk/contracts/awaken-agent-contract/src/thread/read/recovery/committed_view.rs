// @formal surface requirement=runtime.run_tool_lifecycle.lifecycle
// @formal reason: Read-only projection of one supplied prefix through the existing Run, head and
// @formal reason: ticket selectors. It introduces no persistence, execution or commit authority.
//! The synchronous committed view of one immutable recovery prefix.

use super::RunRecoverySnapshot;
use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Record as RunRecord};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::thread::read::committed_thread_view::{CommittedThreadView, select_open_wait};

impl CommittedThreadView for RunRecoverySnapshot {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        if &self.thread_id == thread_id {
            self.messages.clone()
        } else {
            Vec::new()
        }
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.run_record(run_id).ok().flatten().cloned()
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        if &self.thread_id != thread_id {
            return None;
        }
        self.current_run().ok().flatten().cloned()
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.answerable_ticket(run_id).ok().flatten().cloned()
    }

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let latest = self.latest_run(thread_id)?;
        let ticket = self.resume_ticket(&latest.id);
        select_open_wait(thread_id, Some(&latest), ticket.as_ref())
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        if &self.thread_id == thread_id {
            self.state.clone()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests;
