//! Named, immutable projections of committed Run records.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::diagnostic::{RuntimeDiagnostic, RuntimeErrorKind};
use awaken_runtime_contract::execution::Error;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunOutcomeError {
    #[error("Run `{0}` has no committed record")]
    NotCommitted(String),
    #[error("Run `{0}` belongs to another identity or Thread")]
    IdentityConflict(String),
}

impl From<RunOutcomeError> for Error {
    fn from(error: RunOutcomeError) -> Self {
        let code = match &error {
            RunOutcomeError::NotCommitted(_) => "run_not_committed",
            RunOutcomeError::IdentityConflict(_) => "run_identity_conflict",
        };
        RuntimeDiagnostic::new(RuntimeErrorKind::Conflict, code, error).into()
    }
}

/// A point-in-time receipt, derived solely from the committed Run record.
/// Later execution may change an Awaiting/Running receipt; call `read` to refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    record: Record,
}

impl RunOutcome {
    pub fn read(
        reader: &dyn CommittedThreadView,
        run_id: &RunId,
        thread_id: &ThreadId,
    ) -> Result<Self, RunOutcomeError> {
        let record = reader
            .run(run_id)
            .ok_or_else(|| RunOutcomeError::NotCommitted(run_id.0.clone()))?;
        if record.id != *run_id || record.thread_id != *thread_id {
            return Err(RunOutcomeError::IdentityConflict(run_id.0.clone()));
        }
        Ok(Self { record })
    }
    pub fn run_id(&self) -> &RunId {
        &self.record.id
    }
    pub fn thread_id(&self) -> &ThreadId {
        &self.record.thread_id
    }
    pub fn state(&self) -> &RunState {
        &self.record.state
    }
    pub fn end_cause(&self) -> Option<&EndCause> {
        match self.state() {
            RunState::Ended(cause) => Some(cause),
            _ => None,
        }
    }

    /// Read the Thread's current committed conversation, including other Runs.
    /// This is not an output buffer or an authority for resuming execution.
    pub fn transcript(
        &self,
        reader: &dyn CommittedThreadView,
    ) -> Result<Vec<Message>, RunOutcomeError> {
        Self::read(reader, self.run_id(), self.thread_id())?;
        Ok(reader.committed_messages(self.thread_id()))
    }

    /// Read the current ticket, not a cached copy from this receipt.
    pub fn resume_ticket(
        &self,
        reader: &dyn CommittedThreadView,
    ) -> Result<Option<ResumeTicket>, RunOutcomeError> {
        Self::read(reader, self.run_id(), self.thread_id())?;
        let ticket = reader.resume_ticket(self.run_id());
        if ticket.as_ref().is_some_and(|ticket| {
            ticket.run_id != *self.run_id() || ticket.thread_id != *self.thread_id()
        }) {
            return Err(RunOutcomeError::IdentityConflict(self.run_id().0.clone()));
        }
        Ok(ticket)
    }
}
