use std::fmt;

use serde::{Deserialize, Serialize};

/// A commit plan was structurally invalid (G1). Returned by
/// [`ThreadCommit::validate`] before any store write.
#[derive(Debug)]
pub struct ValidationError(String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid commit plan: {}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadCommit {
    pub thread_id: crate::agent::thread::Id,
    pub run_fact: crate::fact::run::Fact,
    pub messages: Vec<crate::agent::message::Message>,
    pub state: Vec<crate::agent::state::Command>,
    pub events: Vec<crate::event::draft::Draft>,
    /// A same-run pause committed atomically with this checkpoint. `Some` parks
    /// the run; `None` clears any prior ticket (resume/terminal).
    #[serde(default)]
    pub waiting: Option<crate::agent::waiting::WaitingTicket>,
}

impl ThreadCommit {
    /// Validate the commit plan before it reaches the store (G1).
    ///
    /// Both `thread_id` and `run_id` must be non-empty. A waiting ticket, when
    /// present, must reference the same `run_id` and `thread_id` as the commit
    /// itself so that parking can never create an orphaned or cross-run ticket.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.thread_id.0.is_empty() {
            return Err(ValidationError("thread_id must not be empty".to_string()));
        }
        if self.run_fact.run_id.0.is_empty() {
            return Err(ValidationError("run_id must not be empty".to_string()));
        }
        if let Some(ticket) = &self.waiting {
            if ticket.run_id != self.run_fact.run_id {
                return Err(ValidationError(format!(
                    "waiting ticket run_id {:?} does not match commit run_id {:?}",
                    ticket.run_id.0, self.run_fact.run_id.0
                )));
            }
            if ticket.thread_id != self.thread_id {
                return Err(ValidationError(format!(
                    "waiting ticket thread_id {:?} does not match commit thread_id {:?}",
                    ticket.thread_id.0, self.thread_id.0
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sequence: u64,
}
