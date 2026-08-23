//! Durable Outcome aggregate projection over the existing Worker Thread log.
//!
//! This is the Outcome extension's typed state codec, not a second persistence
//! abstraction. Values are ordinary Thread-scoped state commands and every write
//! crosses the existing idempotent commit-operation boundary; reads rebuild one
//! internally consistent committed Thread prefix through `RunRecoverySource`.

use awaken_runtime_contract::{
    CommitOperation, CommitOperationCoordinator, CommitOperationId, EndCause,
    ExecutableAgentSnapshot, MergePolicy, Message, RunDisposition, RunId, RunRecoverySnapshot,
    RunRecoverySource, Scope, StateCell, StateCommand, Store, ThreadCommit, ThreadId,
    commit_payload_hash,
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
    Conflict {
        expected: u64,
        current: u64,
    },
    ConcurrentCommit {
        expected_thread_version: u64,
        current_thread_version: u64,
    },
    ActivePointerMismatch,
    Recovery(String),
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
            Self::ConcurrentCommit {
                expected_thread_version,
                current_thread_version,
            } => write!(
                formatter,
                "Outcome Thread commit conflict: expected version {expected_thread_version}, current version {current_thread_version}"
            ),
            Self::ActivePointerMismatch => {
                formatter.write_str("Outcome active pointer and aggregate id disagree")
            }
            Self::Recovery(message) => write!(formatter, "Outcome recovery failed: {message}"),
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

/// One exact committed Thread prefix for public Outcome projection. The
/// aggregate and transcript are materialized from the same recovery snapshot;
/// callers must not combine [`ThreadOutcomeState::load`] with a separate
/// transcript read.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub aggregate: Aggregate,
    pub messages: Vec<Message>,
}

/// The existing Thread recovery/commit-operation ports viewed as the Outcome
/// consistency boundary. The durable Thread-version CAS prevents stale
/// transitions from being appended across replicas; no external I/O occurs
/// while this codec evaluates or commits a transition.
pub struct ThreadOutcomeState<'a> {
    thread_id: &'a ThreadId,
    recovery: &'a dyn RunRecoverySource,
    coordinator: &'a dyn CommitOperationCoordinator,
}

impl<'a> ThreadOutcomeState<'a> {
    #[must_use]
    pub fn new(
        thread_id: &'a ThreadId,
        recovery: &'a dyn RunRecoverySource,
        coordinator: &'a dyn CommitOperationCoordinator,
    ) -> Self {
        Self {
            thread_id,
            recovery,
            coordinator,
        }
    }

    pub async fn active(&self) -> Result<Option<Aggregate>, Error> {
        let snapshot = self.snapshot(&projection_run_id()).await?;
        let store = Store::rebuild(&snapshot.state);
        let Some(id) = load_optional::<Id>(&store, ACTIVE_KEY)? else {
            return Ok(None);
        };
        self.load_from(&store, &id).map(Some)
    }

    pub async fn load(&self, id: &Id) -> Result<Aggregate, Error> {
        self.projection(id)
            .await
            .map(|projection| projection.aggregate)
    }

    /// Rebuild one Outcome and its transcript from one durable recovery read.
    /// This is a query only: it neither resumes the aggregate nor reconstructs
    /// state from an in-process execution result.
    pub async fn projection(&self, id: &Id) -> Result<Projection, Error> {
        let snapshot = self.snapshot(&control_run_id(id)).await?;
        let aggregate = self.load_from(&Store::rebuild(&snapshot.state), id)?;
        Ok(Projection {
            aggregate,
            messages: snapshot.messages,
        })
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
        let run_id = control_run_id(&state.outcome_id);
        let snapshot = self.snapshot(&run_id).await?;
        let store = Store::rebuild(&snapshot.state);
        if let Some(active_id) = load_optional::<Id>(&store, ACTIVE_KEY)? {
            if active_id == state.outcome_id {
                let active = self.load_from(&store, &active_id)?;
                if active.definition == *definition {
                    return Ok(());
                }
            }
            return Err(Error::AlreadyActive(active_id.0));
        }
        if state.outcome_id.0.is_empty() {
            return Err(Error::Serialization(
                "Outcome id must not be empty".to_string(),
            ));
        }
        let id = &state.outcome_id;
        let result = self
            .commit(
                id,
                false,
                snapshot.thread_version,
                snapshot.next_commit_ordinal,
                vec![
                    set(ACTIVE_KEY, id)?,
                    set(&definition_key(id), definition)?,
                    set(&binding_key(id), binding)?,
                    set(&state_key(id), state)?,
                ],
            )
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(Error::ConcurrentCommit { .. }) => {
                // Another replica may have applied this exact create or won
                // with a different Outcome. Rebuild from committed truth before
                // classifying the retry; never trust the failed call's receipt.
                match self.active().await? {
                    Some(active)
                        if active.state.outcome_id == *id && active.definition == *definition =>
                    {
                        Ok(())
                    }
                    Some(active) => Err(Error::AlreadyActive(active.state.outcome_id.0)),
                    None => match self.load(id).await {
                        Ok(existing) if existing.definition == *definition => Ok(()),
                        Ok(_) => Err(Error::AlreadyActive(id.0.clone())),
                        Err(Error::NotFound(_)) => result,
                        Err(error) => Err(error),
                    },
                }
            }
            Err(error) => Err(error),
        }
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
        let run_id = control_run_id(&state.outcome_id);
        let snapshot = self.snapshot(&run_id).await?;
        let store = Store::rebuild(&snapshot.state);
        let active_id = load_optional::<Id>(&store, ACTIVE_KEY)?
            .ok_or_else(|| Error::NotFound(state.outcome_id.0.clone()))?;
        let committed = self.load_from(&store, &active_id)?;
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
        self.commit(
            &state.outcome_id,
            terminal,
            snapshot.thread_version,
            snapshot.next_commit_ordinal,
            commands,
        )
        .await
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
        expected_thread_version: u64,
        operation_ordinal: u64,
        commands: Vec<StateCommand>,
    ) -> Result<(), Error> {
        let run_id = control_run_id(id);
        let disposition = if terminal {
            RunDisposition::ended(run_id, EndCause::NaturalEnd)
        } else {
            RunDisposition::running(run_id)
        };
        let commit = ThreadCommit::assemble(
            self.thread_id.clone(),
            disposition,
            false,
            Vec::new(),
            commands,
            Vec::new(),
        );
        let payload_hash = commit_payload_hash(&commit)
            .map_err(|error| Error::Serialization(error.to_string()))?;
        let operation = CommitOperation {
            operation_id: CommitOperationId::new(control_run_id(id), operation_ordinal),
            expected_thread_version,
            payload_hash,
            commit,
        };
        match self.coordinator.commit_operation(operation).await {
            Ok(_) => Ok(()),
            Err(error) => {
                let current = self.snapshot(&control_run_id(id)).await?;
                if current.thread_version != expected_thread_version {
                    return Err(Error::ConcurrentCommit {
                        expected_thread_version,
                        current_thread_version: current.thread_version,
                    });
                }
                Err(Error::Commit(error.to_string()))
            }
        }
    }

    async fn snapshot(&self, claimed_run_id: &RunId) -> Result<RunRecoverySnapshot, Error> {
        self.recovery
            .recovery_snapshot(self.thread_id, claimed_run_id)
            .await
            .map_err(|error| Error::Recovery(error.to_string()))
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

fn projection_run_id() -> RunId {
    RunId("outcome/state/projection".into())
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
    StateCell::new(Scope::Thread, MergePolicy::Disjoint, key)
        .write(value)
        .map_err(|error| Error::Serialization(error.to_string()))
}

fn remove(key: &str) -> StateCommand {
    StateCell::<()>::new(Scope::Thread, MergePolicy::Disjoint, key).remove()
}

fn load_optional<T: DeserializeOwned>(store: &Store, key: &str) -> Result<Option<T>, Error> {
    StateCell::new(Scope::Thread, MergePolicy::Disjoint, key)
        .load(store)
        .map_err(|error| Error::Serialization(format!("{key}: {error}")))
}

fn load_required<T: DeserializeOwned>(store: &Store, key: &str, id: &Id) -> Result<T, Error> {
    load_optional(store, key)?.ok_or_else(|| Error::NotFound(id.0.clone()))
}
