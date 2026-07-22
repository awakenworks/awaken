//! Durable Outcome aggregate projection over the existing Worker Thread log.
//!
//! This is the Outcome extension's typed state codec, not a second persistence
//! abstraction. Values are ordinary Thread-scoped state commands and every write
//! crosses the existing `CommitCoordinator`; reads rebuild the same committed
//! Thread truth through `ThreadReader`.

use awaken_runtime_contract::{
    CommitCoordinator, EndCause, ExecutableAgentSnapshot, MergePolicy, RunDisposition, RunId,
    Scope, StateCommand, StateKey, Store, ThreadCommit, ThreadId, ThreadReader,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::outcome::{Definition, Evaluation, Id, State};

const ACTIVE_KEY: &str = "outcome/active";

/// Immutable execution inputs pinned when an Outcome is defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub worker: ExecutableAgentSnapshot,
    pub grader: ExecutableAgentSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Serialization(String),
    Commit(String),
    NotFound(String),
    AlreadyActive(String),
    Conflict { expected: u64, current: u64 },
    ActivePointerMismatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialization(message) => {
                write!(formatter, "Outcome state serialization failed: {message}")
            }
            Self::Commit(message) => write!(formatter, "Outcome state commit failed: {message}"),
            Self::NotFound(id) => write!(formatter, "Outcome {id} was not found"),
            Self::AlreadyActive(id) => {
                write!(formatter, "Worker Thread already has active Outcome {id}")
            }
            Self::Conflict { expected, current } => write!(
                formatter,
                "Outcome version conflict: expected version {expected}, current version {current}"
            ),
            Self::ActivePointerMismatch => {
                formatter.write_str("Outcome active pointer and aggregate id disagree")
            }
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    pub definition: Definition,
    pub binding: Binding,
    pub state: State,
    pub evaluations: Vec<Evaluation>,
}

/// The existing Thread reader/coordinator viewed as the Outcome consistency
/// boundary. The version check prevents stale transitions from being appended;
/// no external I/O occurs while this codec evaluates or commits a transition.
pub struct ThreadOutcomeState<'a> {
    thread_id: &'a ThreadId,
    reader: &'a dyn ThreadReader,
    coordinator: &'a dyn CommitCoordinator,
}

impl<'a> ThreadOutcomeState<'a> {
    #[must_use]
    pub fn new(
        thread_id: &'a ThreadId,
        reader: &'a dyn ThreadReader,
        coordinator: &'a dyn CommitCoordinator,
    ) -> Self {
        Self {
            thread_id,
            reader,
            coordinator,
        }
    }

    pub fn active(&self) -> Result<Option<Aggregate>, Error> {
        let store = self.store();
        let Some(id) = load_optional::<Id>(&store, ACTIVE_KEY)? else {
            return Ok(None);
        };
        self.load_from(&store, &id).map(Some)
    }

    pub fn load(&self, id: &Id) -> Result<Aggregate, Error> {
        self.load_from(&self.store(), id)
    }

    pub async fn create(
        &self,
        definition: &Definition,
        binding: &Binding,
        state: &State,
    ) -> Result<(), Error> {
        definition
            .validate()
            .map_err(|error| Error::Serialization(error.to_string()))?;
        if let Some(active) = self.active()? {
            return Err(Error::AlreadyActive(active.state.outcome_id.0));
        }
        if state.outcome_id.0.is_empty() {
            return Err(Error::Serialization(
                "Outcome id must not be empty".to_string(),
            ));
        }
        let id = &state.outcome_id;
        self.commit(
            id,
            false,
            vec![
                set(ACTIVE_KEY, id)?,
                set(&definition_key(id), definition)?,
                set(&binding_key(id), binding)?,
                set(&state_key(id), state)?,
            ],
        )
        .await
    }

    /// Append one transition only when the committed head still has the version
    /// the caller evaluated. An Evaluation is immutable and written once with
    /// the head that consumes its Grade.
    pub async fn commit_if_current(
        &self,
        expected_version: u64,
        state: &State,
        evaluation: Option<&Evaluation>,
    ) -> Result<(), Error> {
        let committed = self
            .active()?
            .ok_or_else(|| Error::NotFound(state.outcome_id.0.clone()))?;
        if committed.state.outcome_id != state.outcome_id {
            return Err(Error::ActivePointerMismatch);
        }
        if committed.state.version != expected_version {
            return Err(Error::Conflict {
                expected: expected_version,
                current: committed.state.version,
            });
        }
        let mut commands = vec![set(&state_key(&state.outcome_id), state)?];
        if let Some(evaluation) = evaluation {
            if evaluation.iteration >= committed.definition.max_iterations {
                return Err(Error::Serialization(
                    "evaluation iteration exceeds Outcome budget".to_string(),
                ));
            }
            commands.push(set(
                &evaluation_key(&state.outcome_id, evaluation.iteration),
                evaluation,
            )?);
        }
        let terminal = state.phase.is_terminal();
        if terminal {
            commands.push(remove(ACTIVE_KEY));
        }
        self.commit(&state.outcome_id, terminal, commands).await
    }

    fn store(&self) -> Store {
        Store::rebuild(&self.reader.committed_state(self.thread_id))
    }

    fn load_from(&self, store: &Store, id: &Id) -> Result<Aggregate, Error> {
        let definition: Definition = load_required(store, &definition_key(id), id)?;
        let binding: Binding = load_required(store, &binding_key(id), id)?;
        let state: State = load_required(store, &state_key(id), id)?;
        if state.outcome_id != *id {
            return Err(Error::ActivePointerMismatch);
        }
        let evaluations = (0..definition.max_iterations)
            .filter_map(|iteration| {
                load_optional(store, &evaluation_key(id, iteration)).transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Aggregate {
            definition,
            binding,
            state,
            evaluations,
        })
    }

    async fn commit(
        &self,
        id: &Id,
        terminal: bool,
        commands: Vec<StateCommand>,
    ) -> Result<(), Error> {
        let run_id = control_run_id(id);
        let disposition = if terminal {
            RunDisposition::ended(run_id, EndCause::NaturalEnd)
        } else {
            RunDisposition::running(run_id)
        };
        self.coordinator
            .commit(ThreadCommit::assemble(
                self.thread_id.clone(),
                disposition,
                false,
                Vec::new(),
                commands,
                Vec::new(),
            ))
            .await
            .map(|_| ())
            .map_err(|error| Error::Commit(error.to_string()))
    }
}

#[must_use]
pub fn worker_run_id(id: &Id, iteration: u32) -> RunId {
    RunId(format!("outcome/{}/worker/{iteration}", id.0))
}

#[must_use]
pub fn grader_thread_id(id: &Id, iteration: u32) -> ThreadId {
    ThreadId(format!("outcome/{}/grader/{iteration}", id.0))
}

#[must_use]
pub fn grader_run_id(id: &Id, iteration: u32) -> RunId {
    RunId(format!("outcome/{}/grader/{iteration}/run", id.0))
}

#[must_use]
pub fn acknowledgment_run_id(id: &Id) -> RunId {
    RunId(format!("outcome/{}/ack", id.0))
}

fn control_run_id(id: &Id) -> RunId {
    RunId(format!("outcome/{}/state", id.0))
}

fn definition_key(id: &Id) -> String {
    format!("outcome/{}/definition", id.0)
}

fn binding_key(id: &Id) -> String {
    format!("outcome/{}/binding", id.0)
}

fn state_key(id: &Id) -> String {
    format!("outcome/{}/state", id.0)
}

fn evaluation_key(id: &Id, iteration: u32) -> String {
    format!("outcome/{}/evaluation/{iteration}", id.0)
}

fn set(key: &str, value: &impl Serialize) -> Result<StateCommand, Error> {
    serde_json::to_value(value)
        .map(|value| StateCommand::set(Scope::Thread, MergePolicy::Disjoint, key, value))
        .map_err(|error| Error::Serialization(error.to_string()))
}

fn remove(key: &str) -> StateCommand {
    StateCommand::remove(Scope::Thread, MergePolicy::Disjoint, key)
}

fn load_optional<T: DeserializeOwned>(store: &Store, key: &str) -> Result<Option<T>, Error> {
    let Some(value) = store.get(Scope::Thread, &StateKey(key.to_string())) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| Error::Serialization(format!("{key}: {error}")))
}

fn load_required<T: DeserializeOwned>(store: &Store, key: &str, id: &Id) -> Result<T, Error> {
    load_optional(store, key)?.ok_or_else(|| Error::NotFound(id.0.clone()))
}
