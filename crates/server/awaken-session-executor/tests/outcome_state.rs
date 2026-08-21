//! Store-adapter integration tests for the Outcome extension's Thread codec.

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_goal::outcome::{Definition, Grade, GradeDecision, Id, State};
use awaken_ext_goal::state::{
    Binding, Error, ThreadOutcomeState, acknowledgment_run_id, grader_run_id, grader_thread_id,
    worker_run_id,
};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_store_inmem::MemoryCommitCoordinator;

fn snapshot(id: &str) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(id)
        .model(ModelBinding::new("test", "model", "native"))
        .build()
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
async fn state_codec_round_trips_and_rejects_a_stale_transition() {
    let store = MemoryCommitCoordinator::new();
    let thread = ThreadId("worker-thread".into());
    let adapter = ThreadOutcomeState::new(&thread, &store, &store);
    let (definition, binding, mut state) = fixture();
    adapter.create(&definition, &binding, &state).await.unwrap();

    let restored = adapter.active().unwrap().unwrap();
    assert_eq!(restored.definition, definition);
    assert_eq!(restored.binding, binding);
    assert_eq!(restored.state, state);

    let expected = state.version;
    state.start(worker_run_id(&state.outcome_id, 0)).unwrap();
    adapter
        .commit_if_current(expected, &state, None)
        .await
        .unwrap();
    assert!(matches!(
        adapter.commit_if_current(expected, &state, None).await,
        Err(Error::Conflict {
            expected: 0,
            current: 1
        })
    ));
}

#[tokio::test]
async fn state_codec_recovers_append_only_evaluation_and_clears_terminal_pointer() {
    let store = MemoryCommitCoordinator::new();
    let thread = ThreadId("worker-thread".into());
    let adapter = ThreadOutcomeState::new(&thread, &store, &store);
    let (definition, binding, mut state) = fixture();
    adapter.create(&definition, &binding, &state).await.unwrap();
    let worker = worker_run_id(&state.outcome_id, 0);
    let grader = grader_run_id(&state.outcome_id, 0);
    state.start(worker.clone()).unwrap();
    state
        .worker_completed(&worker, grader.clone(), 4, 9)
        .unwrap();
    let evaluation = awaken_ext_goal::outcome::Evaluation {
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
        .commit_if_current(0, &state, Some(&evaluation))
        .await
        .unwrap();
    assert_eq!(
        ThreadOutcomeState::new(&thread, &store, &store)
            .load(&Id("o-1".into()))
            .unwrap()
            .evaluations,
        vec![evaluation]
    );

    let version = state.version;
    assert!(state.interrupt());
    adapter
        .commit_if_current(version, &state, None)
        .await
        .unwrap();
    assert!(adapter.active().unwrap().is_none());
}

#[tokio::test]
async fn sqlite_restart_recovers_the_extension_aggregate() {
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
            .commit_if_current(version, &state, None)
            .await
            .unwrap();
    }

    let reopened =
        awaken_store_sqlite::SqliteCommitCoordinator::open(path.to_str().expect("utf-8 test path"))
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
