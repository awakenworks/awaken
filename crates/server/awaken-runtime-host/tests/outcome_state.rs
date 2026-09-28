// @formal evidence outcome.stable_run_identity_and_exactly_once_ack
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
    // Test design. Causes: C1 an Outcome aggregate is created and advanced at
    // version V; C2 a transition retries with stale V. Effects: E1 current state
    // round-trips exactly; E2 C2 is fenced without overwrite. Constraint/
    // Invariant: Thread version is the sole Outcome CAS authority. Decision rule:
    // cover current commit and stale replay partitions.
    let store = MemoryCommitCoordinator::new();
    let thread = ThreadId("worker-thread".into());
    let adapter = ThreadOutcomeState::new(&thread, &store, &store);
    let (definition, binding, mut state) = fixture();
    adapter.create(&definition, &binding, &state).await.unwrap();

    let restored = adapter.active().await.unwrap().unwrap();
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
async fn active_active_create_uses_one_thread_version_fence() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 two replicas prepare the same stable id/payload;
    // C2 two replicas prepare different ids from the same empty Thread prefix.
    // Effects: E1 C1 returns idempotent success to both but appends one create;
    // E2 C2 allows one create and returns retryable AlreadyActive to the loser;
    // E3 committed active truth names exactly the winner. Decision table:
    // A1=C1=>E1+E3; A2=C2=>E2+E3. The fence is the store's existing atomic
    // expected_thread_version operation, never either process's Outcome mutex.
    let thread = ThreadId("same-create".into());
    let store = MemoryCommitCoordinator::new();
    let left = ThreadOutcomeState::new(&thread, &store, &store);
    let right = ThreadOutcomeState::new(&thread, &store, &store);
    let (definition, binding, state) = fixture();
    let (left_result, right_result) = tokio::join!(
        left.create(&definition, &binding, &state),
        right.create(&definition, &binding, &state),
    );
    assert!(
        left_result.is_ok() && right_result.is_ok(),
        "A1/E1: left={left_result:?}, right={right_result:?}"
    );
    assert_eq!(store.commit_count(), 1, "A1/E1");
    assert_eq!(
        left.active()
            .await
            .unwrap()
            .expect("A1 active")
            .state
            .outcome_id,
        state.outcome_id,
        "A1/E3"
    );

    let thread = ThreadId("competing-create".into());
    let store = MemoryCommitCoordinator::new();
    let left = ThreadOutcomeState::new(&thread, &store, &store);
    let right = ThreadOutcomeState::new(&thread, &store, &store);
    let left_state = State::new(Id("left".into()), 0);
    let right_state = State::new(Id("right".into()), 0);
    let (left_result, right_result) = tokio::join!(
        left.create(&definition, &binding, &left_state),
        right.create(&definition, &binding, &right_state),
    );
    assert_eq!(
        usize::from(left_result.is_ok()) + usize::from(right_result.is_ok()),
        1,
        "A2/E2"
    );
    let loser = if left_result.is_ok() {
        right_result
    } else {
        left_result
    };
    assert!(matches!(loser, Err(Error::AlreadyActive(_))), "A2/E2");
    assert_eq!(store.commit_count(), 1, "A2/E2");
    let winner = left
        .active()
        .await
        .unwrap()
        .expect("A2 active")
        .state
        .outcome_id;
    assert!(
        winner == left_state.outcome_id || winner == right_state.outcome_id,
        "A2/E3"
    );
}

#[tokio::test]
async fn state_codec_recovers_append_only_evaluation_and_clears_terminal_pointer() {
    // Test design. Causes: C1 an iteration appends one evaluation; C2 the Outcome
    // reaches terminal state. Effects: E1 evaluation history rehydrates intact;
    // E2 C2 clears the active pointer without deleting history. Constraint/
    // Invariant: evaluations are append-only while active selection is mutable.
    // Decision rule: commit C1 then C2 and verify both historical and active views.
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
            // The Judge Run's observed cost is part of the committed evidence,
            // so the durable round trip below proves it survives a restart
            // rather than being recomputed from a live Run that is gone.
            usage: Some(awaken_agent_contract::audit::model_request::TokenUsage {
                prompt_tokens: 88,
                completion_tokens: 12,
                cache_read_tokens: 3,
                cache_creation_tokens: 1,
            }),
        },
    };
    adapter
        .commit_if_current(0, &state, Some(&evaluation))
        .await
        .unwrap();
    assert_eq!(
        ThreadOutcomeState::new(&thread, &store, &store)
            .load(&Id("o-1".into()))
            .await
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
    assert!(adapter.active().await.unwrap().is_none());
}

#[tokio::test]
async fn sqlite_restart_recovers_the_extension_aggregate() {
    // Test design. Causes: C1 SQLite persists an active Outcome transition; C2
    // process-local adapters are dropped and reopened. Effects: E1 C2 recovers
    // definition, binding, version, and state exactly. Constraint/Invariant:
    // committed Thread state, not process memory, owns the extension aggregate.
    // Decision rule: persist, reopen, and compare the complete aggregate.
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
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.definition, definition);
    assert_eq!(recovered.binding, binding);
    assert_eq!(recovered.state, state);
}

#[test]
fn identities_are_deterministic_and_role_separated() {
    // Cause/effect graph: C1 the same Outcome/role/iteration is derived again;
    // C2 Outcome id changes; C3 iteration changes; C4 role changes among Worker,
    // Grader Thread, Grader Run and final acknowledgment. Effects: E1 C1 returns
    // the byte-identical Run/Thread id; E2 each C2/C3/C4 partition stays
    // distinct. Constraints: these are the extension's production identity
    // functions and acknowledgment has no iteration because it occurs once after
    // the capped final Grade. Decision rules I1=C1=>E1 and
    // I2=C2 or C3 or C4=>E2.
    let id = Id("abc".into());
    let worker = worker_run_id(&id, 2);
    let grader_thread = grader_thread_id(&id, 2);
    let grader_run = grader_run_id(&id, 2);
    let acknowledgment = acknowledgment_run_id(&id);
    assert_eq!(worker.0, "outcome/abc/worker/2", "I1/E1");
    assert_eq!(worker_run_id(&id, 2), worker, "I1/E1");
    assert_eq!(grader_thread.0, "outcome/abc/grader/2", "I1/E1");
    assert_eq!(grader_thread_id(&id, 2), grader_thread, "I1/E1");
    assert_eq!(grader_run.0, "outcome/abc/grader/2/run", "I1/E1");
    assert_eq!(grader_run_id(&id, 2), grader_run, "I1/E1");
    assert_eq!(acknowledgment.0, "outcome/abc/ack", "I1/E1");
    assert_eq!(acknowledgment_run_id(&id), acknowledgment, "I1/E1");

    let different_outcome = Id("def".into());
    let identities = [
        worker.0,
        grader_thread.0,
        grader_run.0,
        acknowledgment.0,
        worker_run_id(&id, 1).0,
        worker_run_id(&different_outcome, 2).0,
        grader_thread_id(&different_outcome, 2).0,
        grader_run_id(&different_outcome, 2).0,
        acknowledgment_run_id(&different_outcome).0,
    ];
    let unique = identities.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), identities.len(), "I2/E2");
}
