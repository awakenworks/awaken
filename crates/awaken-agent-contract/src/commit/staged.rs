use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sequence: u64,
}
