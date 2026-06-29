use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCommand {
    Cancel {
        run_id: awaken_agent_contract::agent::run::Id,
    },
    Wake {
        run_id: awaken_agent_contract::agent::run::Id,
        reason: String,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("run is not active")]
    NotActive,
    #[error("live command rejected: {0}")]
    Rejected(String),
}

pub trait LiveRunControl {
    fn deliver(&self, command: LiveCommand) -> Result<(), Error>;
}
