use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadCommit {
    pub thread_id: crate::agent::thread::Id,
    pub run_fact: crate::fact::run::Fact,
    pub messages: Vec<crate::agent::message::Message>,
    pub state: Vec<crate::agent::state::Command>,
    pub events: Vec<crate::event::draft::Draft>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sequence: u64,
}
