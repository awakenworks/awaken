#![cfg(feature = "postgres")]

mod postgres_fixture;

use std::sync::Arc;

use awaken_server_contract::contract::commit_coordinator::{
    Checkpoint, CommitCoordinator, CommitError, StagedCanonicalEvent,
};
use awaken_server_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventReader, EventScope,
    EventVisibility, EventWriter,
};
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::staged_commit::{
    CheckpointStagedWrites, StagedCommitCoordinator,
};
use awaken_server_contract::contract::storage::{RunRecord, ThreadRunStore, ThreadStore};
use awaken_stores::{PgCommitCoordinator, PostgresStore};
use serde_json::json;

use postgres_fixture::PostgresFixture;

fn unique_prefix(name: &str) -> String {
    // Postgres caps identifier length at 63 chars; the longest suffix added
    // to a base prefix is `_event_scope_index` (18 chars), so the prefix
    // budget is ~45. Use the first 8 chars of a v7 uuid for uniqueness.
    let uuid_short = uuid::Uuid::now_v7().simple().to_string();
    format!("pgc_{}_{}", name, &uuid_short[..8])
}

fn run_record(thread_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "test-agent".to_string(),
        status: RunStatus::Done,
        ..Default::default()
    }
}

fn sample_draft(kind: &str, thread_id: &str, run_id: &str) -> CanonicalEventDraft {
    let mut draft = CanonicalEventDraft::new(
        vec![EventScope::thread(thread_id), EventScope::run(run_id)],
        CanonicalEventKind::new(kind).unwrap(),
        json!({"kind": kind}),
        "test",
    )
    .unwrap();
    draft.visibility = EventVisibility::Public;
    draft
}

async fn build_coord(
    fixture: &PostgresFixture,
    prefix: &str,
) -> (PgCommitCoordinator, Arc<PostgresStore>) {
    let store = Arc::new(PostgresStore::with_prefix(fixture.pool.clone(), prefix));
    let coord = PgCommitCoordinator::new(Arc::clone(&store)).unwrap();
    (coord, store)
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_commit_atomicity_persists_checkpoint_and_events() {
    let fixture = PostgresFixture::start().await;
    let (coord, store) = build_coord(&fixture, &unique_prefix("happy")).await;

    let plan = Checkpoint::checkpoint_only("t-1", run_record("t-1", "r-1"));
    let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
        StagedCanonicalEvent::new(sample_draft("RunStarted", "t-1", "r-1")),
        StagedCanonicalEvent::new(sample_draft("RunCompleted", "t-1", "r-1")),
    ]);

    let outcome = coord.commit_checkpoint_staged(plan, staged).await.unwrap();
    assert_eq!(outcome.canonical_event_ids.len(), 2);
    assert!(store.load_thread("t-1").await.unwrap().is_some());

    let count = store.count(EventScope::run("r-1")).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_commit_rolls_back_on_idempotency_conflict() {
    let fixture = PostgresFixture::start().await;
    let (coord, store) = build_coord(&fixture, &unique_prefix("conflict")).await;
    store.ensure_schema().await.unwrap();

    // Seed an idempotent canonical event so a colliding draft below
    // triggers IdempotencyConflict inside the coordinator transaction.
    let seed_opts = AppendOptions {
        writer_id: Some("runtime".to_string()),
        idempotency_key: Some("k-collide".to_string()),
        ..Default::default()
    };
    store
        .append(sample_draft("RunStarted", "t-2", "r-2"), seed_opts.clone())
        .await
        .unwrap();

    let pre_count = store.count(EventScope::run("r-2")).await.unwrap();
    let pre_thread = store.load_thread("t-2").await.unwrap();

    let mut conflicting_draft = sample_draft("RunStarted", "t-2", "r-2");
    conflicting_draft.payload = json!({"kind": "RunStarted", "different": true});

    let plan = Checkpoint::checkpoint_only("t-2", run_record("t-2", "r-2"));
    let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
        StagedCanonicalEvent::new(conflicting_draft).with_options(seed_opts),
    ]);

    let result = coord.commit_checkpoint_staged(plan, staged).await;
    assert!(matches!(result, Err(CommitError::EventAppend(_))));

    // Transaction rollback: counts and thread state unchanged.
    let post_count = store.count(EventScope::run("r-2")).await.unwrap();
    assert_eq!(post_count, pre_count);
    let post_thread = store.load_thread("t-2").await.unwrap();
    assert_eq!(post_thread.is_some(), pre_thread.is_some());
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_commit_rolls_back_partial_appends_when_later_draft_fails() {
    let fixture = PostgresFixture::start().await;
    let (coord, store) = build_coord(&fixture, &unique_prefix("partial")).await;
    store.ensure_schema().await.unwrap();

    let collide_opts = AppendOptions {
        writer_id: Some("runtime".to_string()),
        idempotency_key: Some("k-second".to_string()),
        ..Default::default()
    };
    store
        .append(
            sample_draft("ToolCallReady", "t-3", "r-3"),
            collide_opts.clone(),
        )
        .await
        .unwrap();

    let mut conflicting_second = sample_draft("ToolCallReady", "t-3", "r-3");
    conflicting_second.payload = json!({"kind": "ToolCallReady", "diff": true});

    let plan = Checkpoint::checkpoint_only("t-3", run_record("t-3", "r-3"));
    let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
        // First draft would succeed in isolation.
        StagedCanonicalEvent::new(sample_draft("RunStarted", "t-3", "r-3")),
        // Second draft conflicts via idempotency.
        StagedCanonicalEvent::new(conflicting_second).with_options(collide_opts),
    ]);

    let pre_count = store.count(EventScope::run("r-3")).await.unwrap();

    let result = coord.commit_checkpoint_staged(plan, staged).await;
    assert!(matches!(result, Err(CommitError::EventAppend(_))));

    let post_count = store.count(EventScope::run("r-3")).await.unwrap();
    assert_eq!(
        post_count, pre_count,
        "first append should be rolled back when second fails"
    );
    assert!(store.load_thread("t-3").await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_scope_stable_per_store_instance() {
    let fixture = PostgresFixture::start().await;
    let (coord_a, _store_a) = build_coord(&fixture, &unique_prefix("scope_a")).await;
    let (coord_b, _store_b) = build_coord(&fixture, &unique_prefix("scope_b")).await;
    assert_eq!(coord_a.scope(), coord_a.scope());
    assert_ne!(coord_a.scope(), coord_b.scope());
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_commit_persists_thread_state_in_same_transaction() {
    let fixture = PostgresFixture::start().await;
    let (coord, store) = build_coord(&fixture, &unique_prefix("tstate")).await;

    let state = awaken_server_contract::PersistedState {
        revision: 11,
        extensions: Default::default(),
    };
    let plan = Checkpoint::checkpoint_only("t-ts", run_record("t-ts", "r-ts"))
        .with_thread_state(state.clone());
    coord
        .commit_checkpoint_staged(plan, CheckpointStagedWrites::default())
        .await
        .unwrap();
    assert_eq!(
        store.load_thread_state("t-ts").await.unwrap(),
        Some(state.clone())
    );
    assert!(store.load_thread_state("absent").await.unwrap().is_none());

    // A later commit without thread_state must not clear the stored value.
    coord
        .commit_checkpoint_staged(
            Checkpoint::checkpoint_only("t-ts", run_record("t-ts", "r-ts2")),
            CheckpointStagedWrites::default(),
        )
        .await
        .unwrap();
    assert_eq!(store.load_thread_state("t-ts").await.unwrap(), Some(state));
}

#[tokio::test]
#[ignore = "requires docker for testcontainers"]
async fn pg_load_checkpoint_reads_messages_run_and_thread_state_together() {
    use awaken_server_contract::contract::message::Message;

    let fixture = PostgresFixture::start().await;
    let (coord, store) = build_coord(&fixture, &unique_prefix("loadck")).await;

    let state = awaken_server_contract::PersistedState {
        revision: 3,
        extensions: Default::default(),
    };
    let plan = Checkpoint::append(
        "t-ck",
        vec![
            Message::user("hi").with_id("m-1".to_string()),
            Message::assistant("there").with_id("m-2".to_string()),
        ],
        Some(0),
        run_record("t-ck", "r-ck"),
    )
    .with_thread_state(state.clone());
    coord
        .commit_checkpoint_staged(plan, CheckpointStagedWrites::default())
        .await
        .unwrap();

    let snapshot = store
        .load_checkpoint("t-ck")
        .await
        .unwrap()
        .expect("snapshot present");
    assert_eq!(snapshot.message_version, 2);
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(snapshot.latest_run.unwrap().run_id, "r-ck");
    assert_eq!(snapshot.thread_state, Some(state));

    // Unknown thread yields no snapshot.
    assert!(store.load_checkpoint("t-absent").await.unwrap().is_none());
}
