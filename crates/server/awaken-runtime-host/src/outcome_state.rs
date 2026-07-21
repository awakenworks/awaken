//! Durable Outcome aggregate projection over the existing Worker Thread log.
//!
//! This is a concrete adapter, not another repository port: values are ordinary
//! thread-scoped `StateCommand`s and every write crosses the existing
//! `CommitCoordinator`. A stable control Run groups those state-only commits so
//! no second physical store or schema is introduced.

#![allow(dead_code)] // Consumed by the OutcomeController in ADR-0064 P5.

use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::state::{Command, Key, MergePolicy, Scope, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_goal::outcome::{Definition, Evaluation, Id, State};
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const ACTIVE_KEY: &str = "outcome/active";

/// Immutable execution inputs pinned when an Outcome is defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub(crate) worker: ExecutableAgentSnapshot,
    pub(crate) grader: ExecutableAgentSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Outcome state serialization failed: {0}")]
    Serialization(String),
    #[error("Outcome state commit failed: {0}")]
    Commit(String),
    #[error("Outcome {0} was not found")]
    NotFound(String),
    #[error("Worker Thread already has active Outcome {0}")]
    AlreadyActive(String),
    #[error("Outcome CAS conflict: expected version {expected}, current version {current}")]
    Conflict { expected: u64, current: u64 },
    #[error("Outcome active pointer and aggregate id disagree")]
    ActivePointerMismatch,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Aggregate {
    pub(crate) definition: Definition,
    pub(crate) binding: Binding,
    pub(crate) state: State,
    pub(crate) evaluations: Vec<Evaluation>,
}

/// The existing thread reader/coordinator viewed as one Outcome consistency
/// boundary. Callers serialize commands for a Worker Thread (the Session state
/// lock supplies the in-process critical section); the version check prevents a
/// stale transition from being appended.
pub(crate) struct ThreadOutcomeState<'a> {
    thread_id: &'a ThreadId,
    reader: &'a dyn ThreadReader,
    coordinator: &'a dyn Coordinator,
}

impl<'a> ThreadOutcomeState<'a> {
    pub(crate) fn new(
        thread_id: &'a ThreadId,
        reader: &'a dyn ThreadReader,
        coordinator: &'a dyn Coordinator,
    ) -> Self {
        Self {
            thread_id,
            reader,
            coordinator,
        }
    }

    pub(crate) fn active(&self) -> Result<Option<Aggregate>, Error> {
        let store = self.store();
        let Some(id) = load_optional::<Id>(&store, ACTIVE_KEY)? else {
            return Ok(None);
        };
        self.load_from(&store, &id).map(Some)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn load(&self, id: &Id) -> Result<Aggregate, Error> {
        self.load_from(&self.store(), id)
    }

    pub(crate) async fn create(
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
    pub(crate) async fn compare_and_set(
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

    async fn commit(&self, id: &Id, terminal: bool, commands: Vec<Command>) -> Result<(), Error> {
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

pub(crate) fn worker_run_id(id: &Id, iteration: u32) -> RunId {
    RunId(format!("outcome/{}/worker/{iteration}", id.0))
}

pub(crate) fn grader_thread_id(id: &Id, iteration: u32) -> ThreadId {
    ThreadId(format!("outcome/{}/grader/{iteration}", id.0))
}

pub(crate) fn grader_run_id(id: &Id, iteration: u32) -> RunId {
    RunId(format!("outcome/{}/grader/{iteration}/run", id.0))
}

pub(crate) fn acknowledgment_run_id(id: &Id) -> RunId {
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

fn set(key: &str, value: &impl Serialize) -> Result<Command, Error> {
    serde_json::to_value(value)
        .map(|value| Command::set(Scope::Thread, MergePolicy::Disjoint, key, value))
        .map_err(|error| Error::Serialization(error.to_string()))
}

fn remove(key: &str) -> Command {
    Command::remove(Scope::Thread, MergePolicy::Disjoint, key)
}

fn load_optional<T: DeserializeOwned>(store: &Store, key: &str) -> Result<Option<T>, Error> {
    let Some(value) = store.get(Scope::Thread, &Key(key.to_string())) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| Error::Serialization(format!("{key}: {error}")))
}

fn load_required<T: DeserializeOwned>(store: &Store, key: &str, id: &Id) -> Result<T, Error> {
    load_optional(store, key)?.ok_or_else(|| Error::NotFound(id.0.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_goal::outcome::{Grade, GradeDecision};
    use awaken_runtime::memory::MemoryCommitCoordinator;

    fn snapshot(id: &str) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(id).build()
    }

    fn fixture() -> (Definition, Binding, State) {
        let definition = Definition::new("ship", "tests pass", 3).unwrap();
        let binding = Binding {
            worker: snapshot("worker"),
            grader: snapshot("grader"),
        };
        let state = State::new(Id("o-1".into()), 4);
        (definition, binding, state)
    }

    #[tokio::test]
    async fn create_round_trip_pins_definition_binding_and_head() {
        let store = MemoryCommitCoordinator::new();
        let thread = ThreadId("worker-thread".into());
        let adapter = ThreadOutcomeState::new(&thread, &store, &store);
        let (definition, binding, state) = fixture();
        adapter.create(&definition, &binding, &state).await.unwrap();

        let restored = adapter.active().unwrap().unwrap();
        assert_eq!(restored.definition, definition);
        assert_eq!(restored.binding, binding);
        assert_eq!(restored.state, state);
        assert!(restored.evaluations.is_empty());
    }

    #[tokio::test]
    async fn one_thread_rejects_a_second_active_outcome() {
        let store = MemoryCommitCoordinator::new();
        let thread = ThreadId("worker-thread".into());
        let adapter = ThreadOutcomeState::new(&thread, &store, &store);
        let (definition, binding, state) = fixture();
        adapter.create(&definition, &binding, &state).await.unwrap();
        assert!(matches!(
            adapter.create(&definition, &binding, &state).await,
            Err(Error::AlreadyActive(_))
        ));
    }

    #[tokio::test]
    async fn stale_version_cannot_append_an_evaluation() {
        let store = MemoryCommitCoordinator::new();
        let thread = ThreadId("worker-thread".into());
        let adapter = ThreadOutcomeState::new(&thread, &store, &store);
        let (definition, binding, mut state) = fixture();
        adapter.create(&definition, &binding, &state).await.unwrap();
        let expected = state.version;
        state.start(worker_run_id(&state.outcome_id, 0)).unwrap();
        adapter
            .compare_and_set(expected, &state, None)
            .await
            .unwrap();

        assert!(matches!(
            adapter.compare_and_set(expected, &state, None).await,
            Err(Error::Conflict {
                expected: 0,
                current: 1
            })
        ));
    }

    #[tokio::test]
    async fn restart_rebuilds_head_and_append_only_evaluation() {
        let store = MemoryCommitCoordinator::new();
        let thread = ThreadId("worker-thread".into());
        let (definition, binding, mut state) = fixture();
        let adapter = ThreadOutcomeState::new(&thread, &store, &store);
        adapter.create(&definition, &binding, &state).await.unwrap();
        state.start(worker_run_id(&state.outcome_id, 0)).unwrap();
        let worker = worker_run_id(&state.outcome_id, 0);
        let grader = grader_run_id(&state.outcome_id, 0);
        state.worker_completed(&worker, grader.clone(), 9).unwrap();
        let evaluation = Evaluation {
            iteration: 0,
            worker_run_id: worker,
            grader_run_id: grader,
            message_start: 4,
            message_end: 9,
            grade: Grade {
                decision: GradeDecision::NeedsRevision,
                explanation: "missing proof".into(),
            },
        };
        adapter
            .compare_and_set(0, &state, Some(&evaluation))
            .await
            .unwrap();

        // A newly constructed adapter has no live memory of the aggregate; all
        // fields are recovered by replaying the Worker Thread's committed state.
        let restarted = ThreadOutcomeState::new(&thread, &store, &store);
        let restored = restarted.load(&Id("o-1".into())).unwrap();
        assert_eq!(restored.state, state);
        assert_eq!(restored.evaluations, vec![evaluation]);
    }

    #[tokio::test]
    async fn terminal_transition_clears_active_but_remains_addressable() {
        let store = MemoryCommitCoordinator::new();
        let thread = ThreadId("worker-thread".into());
        let adapter = ThreadOutcomeState::new(&thread, &store, &store);
        let (definition, binding, mut state) = fixture();
        adapter.create(&definition, &binding, &state).await.unwrap();
        let version = state.version;
        assert!(state.interrupt());
        adapter
            .compare_and_set(version, &state, None)
            .await
            .unwrap();

        assert!(adapter.active().unwrap().is_none());
        assert_eq!(adapter.load(&state.outcome_id).unwrap().state, state);
    }

    #[tokio::test]
    async fn sqlite_restart_recovers_the_active_aggregate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("thread.db");
        let thread = ThreadId("durable-worker".into());
        let (definition, binding, mut state) = fixture();
        {
            let store = awaken_store_sqlite::SqliteCommitCoordinator::open(
                path.to_str().expect("utf-8 test path"),
            )
            .unwrap();
            let adapter = ThreadOutcomeState::new(&thread, &store, &store);
            adapter.create(&definition, &binding, &state).await.unwrap();
            let version = state.version;
            state.start(worker_run_id(&state.outcome_id, 0)).unwrap();
            adapter
                .compare_and_set(version, &state, None)
                .await
                .unwrap();
        }

        let reopened = awaken_store_sqlite::SqliteCommitCoordinator::open(
            path.to_str().expect("utf-8 test path"),
        )
        .unwrap();
        let recovered = ThreadOutcomeState::new(&thread, &reopened, &reopened)
            .active()
            .unwrap()
            .unwrap();
        assert_eq!(recovered.definition, definition);
        assert_eq!(recovered.binding, binding);
        assert_eq!(recovered.state, state);
    }

    #[test]
    fn identities_are_deterministic_and_role_separated() {
        let id = Id("abc".into());
        assert_eq!(worker_run_id(&id, 2).0, "outcome/abc/worker/2");
        assert_eq!(grader_thread_id(&id, 2).0, "outcome/abc/grader/2");
        assert_eq!(grader_run_id(&id, 2).0, "outcome/abc/grader/2/run");
        assert_eq!(acknowledgment_run_id(&id).0, "outcome/abc/ack");
    }
}
