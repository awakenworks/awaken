use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCommand {
    Cancel {
        run_id: awaken_agent_contract::agent::run::Id,
    },
    /// Cooperatively pause an in-flight run (ADR-0054): the runtime sets the run's
    /// `PauseSignal`; the run awaits durably at its next safe boundary. Resuming a
    /// paused run is a durable re-admission through the ingress, not a live command.
    Pause {
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
