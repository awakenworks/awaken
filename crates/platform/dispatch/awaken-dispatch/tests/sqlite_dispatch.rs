// @formal evidence ingress.commit_epoch_atomic_local
// @formal evidence run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
//! The SQLite dispatch store and a fully-embedded durable loop. SQLite needs no
//! external server, so these always run: store-level claim/lease/idempotency
//! checks, plus an end-to-end durable submit -> await -> resume entirely on SQLite
//! (dispatch queue *and* commit boundary).

use awaken_dispatch::DispatchAttemptRecordReads as _;

mod harness;

#[tokio::test]
async fn admission_append_failure_rolls_back_and_cold_replay_retains_entry() {
    use awaken_dispatch::{
        AttemptAdmission, DispatchCursor, DispatchOperationalFeed, SqliteDispatchStore,
    };
    // Cause/effect rules: E1 log INSERT fails -> admission errors, empty slot,
    // no entry; E2 same claim after fault removal -> one entry and exact slot;
    // E3 cold reopen + response-loss retry -> AlreadyApplied, original entry
    // unchanged. Neither E2 nor E3 is evidence of process or effect closure.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("admission.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "entry-cold", &clock).await;
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER test_reject_admission_insert BEFORE INSERT ON runtime_dispatch_operation WHEN json_extract(NEW.operation, '$.type') = 'attempt_admitted' BEGIN SELECT RAISE(ABORT, 'test admission append failure'); END").execute(&connection).await.unwrap();
    assert!(
        store.begin_attempt(&claim, Some(&binding)).await.is_err(),
        "E1 append is part of admission"
    );
    assert_eq!(
        store.load_physical_attempt(&claim.run_id).await.unwrap(),
        None,
        "E1 rollback slot"
    );
    let before = store
        .events_after(DispatchCursor::default(), usize::MAX, Some(&claim.run_id))
        .await
        .unwrap();
    assert!(
        before.events.iter().all(
            |event| serde_json::to_value(&event.operation).unwrap()["type"] != "attempt_admitted"
        ),
        "E1 zero entry"
    );
    sqlx::query("DROP TRIGGER test_reject_admission_insert")
        .execute(&connection)
        .await
        .unwrap();
    assert_eq!(
        store.begin_attempt(&claim, Some(&binding)).await.unwrap(),
        AttemptAdmission::Applied,
        "E2"
    );
    let original = store
        .events_after(DispatchCursor::default(), usize::MAX, Some(&claim.run_id))
        .await
        .unwrap();
    assert_eq!(
        original.events.len(),
        before.events.len() + 1,
        "E2 exactly one entry"
    );
    let physical = store
        .load_physical_attempt(&claim.run_id)
        .await
        .unwrap()
        .unwrap();
    drop(store);
    let reopened = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .begin_attempt(&claim, Some(&binding))
            .await
            .unwrap(),
        AttemptAdmission::AlreadyApplied,
        "E3 retry"
    );
    assert_eq!(
        reopened.load_physical_attempt(&claim.run_id).await.unwrap(),
        Some(physical),
        "E3 exact frozen tuple"
    );
    assert_eq!(
        reopened
            .events_after(DispatchCursor::default(), usize::MAX, Some(&claim.run_id))
            .await
            .unwrap(),
        original,
        "E3 original cursor, time and entry"
    );
}

#[tokio::test]
async fn ordinary_ack_append_failure_rolls_back_and_cold_replay_retains_original_subject() {
    use awaken_dispatch::{
        AttemptAdmission, AttemptPhysicalFact, AttemptReturnCause, DispatchCursor,
        DispatchSettlement, SettleOutcome, SqliteDispatchStore,
    };
    // R1 exact ordinary Some + INSERT failure -> rollback slot/retired epoch
    // and zero fact; R2 same original ACK after the fault is removed -> one
    // binding+cause event; R3 process reopen/response loss -> exact same event,
    // even after live Dispatch-row deletion. This native transaction test does
    // not turn a structured return into a Provider or Session stop assertion.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ordinary-ack.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "ordinary-cold", &clock).await;
    assert_eq!(
        store.begin_attempt(&claim, Some(&binding)).await.unwrap(),
        AttemptAdmission::Applied
    );
    let original = store
        .load_physical_attempt(&claim.run_id)
        .await
        .unwrap()
        .unwrap();
    let failure = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER test_reject_ack_insert BEFORE INSERT ON runtime_dispatch_operation WHEN json_extract(NEW.operation, '$.type') = 'attempt_acknowledgement' BEGIN SELECT RAISE(ABORT, 'test ACK append failure'); END").execute(&failure).await.unwrap();
    assert!(
        store
            .finish_attempt(&claim, Some(AttemptReturnCause::StructuredReturnObserved))
            .await
            .is_err(),
        "R1 append failure"
    );
    assert_eq!(
        store.load_physical_attempt(&claim.run_id).await.unwrap(),
        Some(original.clone()),
        "R1 original binding still visible"
    );
    assert!(
        store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        store.begin_attempt(&claim, Some(&binding)).await.unwrap(),
        AttemptAdmission::AlreadyApplied,
        "R1 epoch was not retired"
    );
    sqlx::query("DROP TRIGGER test_reject_ack_insert")
        .execute(&failure)
        .await
        .unwrap();
    failure.close().await;
    assert_eq!(
        store
            .finish_attempt(&claim, Some(AttemptReturnCause::StructuredReturnObserved))
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "R2"
    );
    let retained = store
        .attempt_closure_records_after(DispatchCursor::default(), usize::MAX, Some(&claim.run_id))
        .await
        .unwrap();
    assert_eq!(retained.events.len(), 1);
    let AttemptPhysicalFact::Acknowledgement { record } = &retained.events[0].record else {
        panic!("R2 local observation is not independent closure")
    };
    assert_eq!(record.attempt(), &original);
    drop(store);
    let reopened = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .finish_attempt(&claim, Some(AttemptReturnCause::StructuredReturnObserved))
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "R3 cold ACK replay"
    );
    assert_eq!(
        reopened
            .settle(
                &claim.run_id,
                claim.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    assert_eq!(
        reopened
            .finish_attempt(&claim, Some(AttemptReturnCause::StructuredReturnObserved))
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "R3 row tombstone"
    );
    assert_eq!(
        reopened
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap(),
        retained,
        "R3 original cursor/time, no duplicate"
    );
}

#[tokio::test]
async fn closed_entry_survives_process_reopen_and_rejects_late_worker_ack() {
    use awaken_dispatch::{SettleOutcome, SqliteDispatchStore};
    // C1 close is committed before its caller loses the response; C2 all
    // process-local handles vanish; C3 an old ACK arrives after cold reopen;
    // C4 independent exact closure becomes available. S1=C1+C2 => closing
    // survives with the original binding. S2=C3 => Fenced, no early release.
    // S3=C4 => one retirement; duplicate closure cannot resurrect the epoch.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("closing.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "cold-closing", &clock).await;
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    store.close_attempt(&claim).await.unwrap();
    drop(store);
    let reopened = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let stored = reopened
        .load_physical_attempt(&claim.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.closing);
    assert_eq!(stored.binding.as_ref(), Some(&binding));
    let closure = awaken_dispatch::AttemptClosureRecord::try_new(
        stored,
        awaken_dispatch::AttemptClosureKind::ControlledExecutionClosed,
        "cold-storage-original-closure",
    )
    .unwrap();
    assert_eq!(
        reopened.finish_attempt(&claim, None).await.unwrap(),
        SettleOutcome::Fenced
    );
    // S5 the original transaction's append fails after slot update: neither
    // retirement nor a record may become visible. This fixture adds/removes
    // only its own INSERT failure trigger, never a production constraint.
    let failure = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER test_reject_attempt_closure_insert BEFORE INSERT ON runtime_dispatch_operation WHEN json_extract(NEW.operation, '$.type') = 'attempt_closure' BEGIN SELECT RAISE(ABORT, 'test closure append failure'); END")
        .execute(&failure).await.unwrap();
    assert!(
        reopened.finish_attempt_closure(&closure).await.is_err(),
        "S5 append failure"
    );
    assert_eq!(
        reopened
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .as_ref(),
        Some(closure.attempt()),
        "S5 slot rollback preserves exact source"
    );
    assert!(
        reopened
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "S5 no partial record"
    );
    sqlx::query("DROP TRIGGER test_reject_attempt_closure_insert")
        .execute(&failure)
        .await
        .unwrap();
    drop(failure);
    assert_eq!(
        reopened.finish_attempt_closure(&closure).await.unwrap(),
        SettleOutcome::Applied
    );
    assert_eq!(
        reopened.finish_attempt_closure(&closure).await.unwrap(),
        SettleOutcome::Applied
    );
    assert!(
        reopened
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .is_none()
    );
    // S4 first independent retirement commits but its caller loses all handles:
    // cold reopen reads the same exact data/reference and one operation cursor,
    // rather than reconstructing a proof from the retired epoch or empty slot.
    drop(reopened);
    let reopened = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let retained = reopened
        .attempt_closure_records_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&claim.run_id),
        )
        .await
        .unwrap();
    assert_eq!(
        retained.events.len(),
        1,
        "S4 one retained fact across cold reopen"
    );
    assert_eq!(
        retained.events[0].record,
        awaken_dispatch::AttemptPhysicalFact::IndependentClosure {
            record: closure.clone()
        },
        "S4 exact original subject/kind/ref"
    );
    assert_eq!(
        reopened.finish_attempt_closure(&closure).await.unwrap(),
        SettleOutcome::Applied,
        "S4 replay after cold reopen"
    );
    assert_eq!(
        reopened
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap(),
        retained,
        "S4 no duplicate cursor or record"
    );
    // S6 corrupt private/public storage must fail both purpose projections;
    // filtering may not swallow an unknown old tag or malformed known private
    // record. A zero logical LIMIT performs no decoding and retains its cursor.
    // Each injected row is removed by its exact fixture-owned sequence only.
    let raw = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    // S7 the same real table can express negative recorded time and a Run
    // column/payload mismatch. Both purposes must reject before filtering;
    // negative sequence itself is covered by the canonical codec's W5 table.
    let valid_private =
        serde_json::json!({"type":"attempt_closure", "record": &closure}).to_string();
    let mut wrong_run = claim.clone();
    wrong_run.run_id = awaken_agent_contract::agent::run::Id("different-stored-run".into());
    let wrong_run =
        serde_json::to_string(&awaken_dispatch::DispatchOperation::Claimed { claim: wrong_run })
            .unwrap();
    for (encoded, recorded_at_ms) in [
        (r#"{"type":"attempt_closure"}"#.to_string(), 0),
        (r#"{"type":"unknown_old_operation"}"#.to_string(), 0),
        (valid_private, -1),
        (wrong_run, 0),
    ] {
        let inserted = sqlx::query("INSERT INTO runtime_dispatch_operation(run_id, operation, recorded_at_ms) VALUES (?1, ?2, ?3)")
            .bind(&claim.run_id.0).bind(encoded).bind(recorded_at_ms).execute(&raw).await.unwrap().last_insert_rowid();
        assert!(
            reopened
                .events_after(
                    awaken_dispatch::DispatchCursor::default(),
                    usize::MAX,
                    Some(&claim.run_id)
                )
                .await
                .is_err(),
            "S6/S7 diagnostics cannot swallow corrupt storage"
        );
        assert!(
            reopened
                .attempt_closure_records_after(
                    awaken_dispatch::DispatchCursor::default(),
                    usize::MAX,
                    Some(&claim.run_id)
                )
                .await
                .is_err(),
            "S6/S7 internal purpose cannot swallow corrupt storage"
        );
        let cursor = awaken_dispatch::DispatchCursor(retained.next_cursor.0);
        let public_zero = reopened
            .events_after(cursor, 0, Some(&claim.run_id))
            .await
            .unwrap();
        assert!(public_zero.events.is_empty());
        assert_eq!(
            public_zero.next_cursor, cursor,
            "S6 public zero-limit no decode/advance"
        );
        let internal_zero = reopened
            .attempt_closure_records_after(cursor, 0, Some(&claim.run_id))
            .await
            .unwrap();
        assert!(internal_zero.events.is_empty());
        assert_eq!(
            internal_zero.next_cursor, cursor,
            "S6 internal zero-limit no decode/advance"
        );
        sqlx::query("DELETE FROM runtime_dispatch_operation WHERE sequence = ?1")
            .bind(inserted)
            .execute(&raw)
            .await
            .unwrap();
    }
    assert_eq!(
        reopened
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap(),
        retained,
        "S6 original fact survives every fault/zero read"
    );
}

#[tokio::test]
async fn sqlite_execution_binding_survives_reopen_without_the_worker() {
    // B11 admitted binding -> all store handles dropped -> reopen => exact
    // provider/Worker/physical coordinates remain. B12 raw SQL cannot clear
    // only the slot; the canonical finish clears slot+binding atomically.
    // B13 persisted malformed binding => observation errors, never false empty.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bound-attempt.db");
    let path = path.to_str().unwrap();
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path, clock.clock())
        .await
        .unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "reopened-binding", &clock).await;
    clock.set(0);
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    drop(store);
    let reopened = SqliteDispatchStore::open(path, clock.clock())
        .await
        .unwrap();
    let expected = Some(awaken_dispatch::PhysicalAttempt {
        closing: false,
        claim: claim.clone(),
        binding: Some(binding.clone()),
        thread_id: Some(awaken_agent_contract::agent::thread::Id(
            "reopened-binding".into(),
        )),
        session_thread_id: None,
    });
    assert_eq!(
        reopened.load_physical_attempt(&claim.run_id).await.unwrap(),
        expected,
        "B11"
    );
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    assert!(
        sqlx::query(
            "UPDATE runtime_dispatch SET active_attempt_owner=NULL, active_attempt_epoch=NULL"
        )
        .execute(&connection)
        .await
        .is_err(),
        "B12 orphan binding"
    );
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding='null'")
        .execute(&connection)
        .await
        .unwrap();
    assert!(
        reopened.load_physical_attempt(&claim.run_id).await.is_err(),
        "B13 malformed is not absence"
    );
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding=?1")
        .bind(serde_json::to_string(&binding).unwrap())
        .execute(&connection)
        .await
        .unwrap();
    assert_eq!(
        reopened.finish_attempt(&claim, None).await.unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
    drop(reopened);
    assert_eq!(
        SqliteDispatchStore::open(path, clock.clock())
            .await
            .unwrap()
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap(),
        None,
        "B12 persisted release"
    );
    assert!(
        sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding='{}'")
            .execute(&connection)
            .await
            .is_err(),
        "B12 cannot leave orphan metadata"
    );
}

#[tokio::test]
async fn sqlite_ack_retirement_survives_reopen_and_sql_bypass_is_rejected() {
    // R8 ACK commit then process/store loss => the same claim cannot reenter.
    // R9 bypassing admission SQL => CHECK rejects retired or future watermarks.
    // No fixture-supplied terminal truth or in-process registry survives reopen.
    use awaken_dispatch::{AttemptAdmission as A, SettleOutcome as S};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("retirement.db");
    let path = path.to_str().unwrap();
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path, clock.clock())
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(harness::activation("retired")))
        .await
        .unwrap();
    clock.set(0);
    let a = store
        .claim("a", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let a = RunClaim::from(&a.lease);
    clock.set(0);
    assert_eq!(store.begin_attempt(&a, None).await.unwrap(), A::Applied);
    assert_eq!(store.finish_attempt(&a, None).await.unwrap(), S::Applied);
    drop(store);
    let reopened = SqliteDispatchStore::open(path, clock.clock())
        .await
        .unwrap();
    clock.set(0);
    assert_eq!(
        reopened.begin_attempt(&a, None).await.unwrap(),
        A::Fenced,
        "R8"
    );
    assert_eq!(
        reopened.finish_attempt(&a, None).await.unwrap(),
        S::Applied,
        "lost ACK response"
    );
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    for invalid in [
        "UPDATE runtime_dispatch SET active_attempt_owner = 'a', active_attempt_epoch = retired_attempt_epoch",
        "UPDATE runtime_dispatch SET retired_attempt_epoch = lease_epoch + 1",
        "UPDATE runtime_dispatch SET retired_attempt_epoch = -1",
    ] {
        assert!(
            sqlx::query(invalid).execute(&connection).await.is_err(),
            "R9 {invalid}"
        );
    }
}

use awaken_time::system_clock;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{
    ClaimedCommitCoordinator, DispatchCursor, DispatchOperation, DispatchOperationalFeed,
    DispatchTestHarness, GuardedRunCommit, Inbox, PendingInput, RunClaim, RunDispatch,
    SqliteDispatchStore,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_dispatch_testkit::ConformanceClock as _;
use harness::{TICKET, activation, tool_runtime};

struct BlockingCommit {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl CommitCoordinator for BlockingCommit {
    async fn commit(&self, _commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(CommitRecord { sequence: 1 })
    }
}

fn running_commit(run_id: &str) -> ThreadCommit {
    ThreadCommit::assemble(
        ThreadId(harness::THREAD.to_string()),
        RunDisposition::running(RunId(run_id.to_string())),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

fn pending(message_id: &str, correlation: &str, allow: bool) -> PendingInput {
    harness::pending(
        message_id,
        "run-1",
        correlation,
        if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(None)
        },
    )
}

#[tokio::test]
async fn enqueue_is_idempotent_and_expired_lease_is_reclaimed() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Re-enqueue is a no-op.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // A held lease blocks a second claim; an expired lease is reclaimed.
    clock.set(0);
    assert!(
        store
            .claim("a", 1_000, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    clock.set(500);
    assert!(
        store
            .claim("b", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    clock.set(1_001);
    let recovered = store.claim("b", 1_000, &Default::default()).await.unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner().to_owned()),
        Some("b".to_string())
    );
}

#[tokio::test]
async fn sqlite_epoch_guard_blocks_reclaim_until_commit_returns() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(
        SqliteDispatchStore::open_in_memory(clock.clock())
            .await
            .expect("dispatch"),
    );
    store
        .enqueue(RunDispatch::new(activation("guarded")))
        .await
        .unwrap();
    clock.set(0);
    let lease = store
        .claim("owner-a", 100, &Default::default())
        .await
        .unwrap()
        .expect("claim")
        .lease;
    let inner = Arc::new(BlockingCommit {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let service = Arc::new(GuardedRunCommit::from_plain(inner.clone(), store.clone()));
    let fenced = ClaimedCommitCoordinator::embedded(service, RunClaim::from(&lease));

    let committing = tokio::spawn(async move { fenced.commit(running_commit("guarded")).await });
    inner.entered.notified().await;

    let reclaim_store = store.clone();
    let mut reclaiming = tokio::spawn(async move {
        clock.set(200);
        reclaim_store
            .claim("owner-b", 100, &Default::default())
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut reclaiming)
            .await
            .is_err(),
        "reclaim must wait while the exact commit epoch is held"
    );

    inner.release.notify_one();
    committing.await.unwrap().expect("commit");
    let reclaimed = reclaiming.await.unwrap().unwrap().expect("reclaim");
    assert_eq!(reclaimed.lease.epoch(), lease.epoch() + 1);
}

#[tokio::test]
async fn append_is_idempotent() {
    let store = SqliteDispatchStore::open_in_memory(system_clock())
        .await
        .expect("open");
    let input = pending("msg-1", TICKET, true);
    assert!(
        store.append(input.clone()).await.unwrap(),
        "first append stores"
    );
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op"
    );
}

#[tokio::test]
async fn durable_loop_runs_entirely_on_sqlite() {
    // Test design. Causes: C1 SQLite owns dispatch and commit stores; C2 the Run
    // awaits; C3 stale then matching input arrives. Effects: E1 C2 persists
    // Awaiting; E2 stale input does not resume; E3 matching input runs once and
    // settles. Constraint/Invariant: no in-memory authority participates.
    // Decision rule: cover Awaiting, stale-correlation, and exact-correlation
    // partitions through the SQLite adapters.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(
        SqliteDispatchStore::open_in_memory(system_clock())
            .await
            .expect("dispatch"),
    );
    let commit = Arc::new(
        SqliteCommitCoordinator::open_in_memory()
            .await
            .expect("commit"),
    );
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone(), system_clock());

    // Durable submit awaits on the gate.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Stale input (wrong correlation) does not resume.
    let state = ingress
        .deliver_resume(pending("stale", "old-ticket", true), harness::clock(0))
        .await
        .expect("stale");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // The correctly-correlated input resumes the run to completion.
    let state = ingress
        .deliver_resume(pending("good", TICKET, true), harness::clock(0))
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the pending tool ran once");

    let record =
        CommittedThreadView::run(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn pending_revision_cas_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory(system_clock())
        .await
        .expect("open");
    harness::assert_pending_revision_cas(&store).await;
}

#[tokio::test]
async fn cross_thread_outbox_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory(system_clock())
        .await
        .expect("open");
    harness::assert_cross_thread_outbox(&store).await;
}

#[tokio::test]
async fn message_idempotency_conflicts_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .unwrap();
    harness::assert_message_idempotency_conflicts(&store, &clock).await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_scheduled_due(&store, &clock).await;
}

#[tokio::test]
async fn millis_boundaries_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .unwrap();
    harness::assert_millis_boundaries(&store, &clock).await;
}

#[tokio::test]
async fn sqlite_dispatch_opens_a_file_and_persists() {
    let path =
        std::env::temp_dir().join(format!("awaken_sqlite_dispatch_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    {
        let store = SqliteDispatchStore::open(&path, clock.clock())
            .await
            .expect("open a");
        store
            .enqueue(RunDispatch::new(activation("run-1")))
            .await
            .unwrap();
    }
    // A fresh handle on the same file still has the enqueued run.
    let restarted = SqliteDispatchStore::open(&path, clock.clock())
        .await
        .expect("open b");
    clock.set(0);
    let claimed = restarted
        .claim("w", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("survived");
    assert_eq!(claimed.request.run_id().0, "run-1");
    drop(restarted);

    let feed = SqliteDispatchStore::open(&path, clock.clock())
        .await
        .expect("open feed reader");
    let page = feed
        .events_after(DispatchCursor(0), 10, None)
        .await
        .expect("backfill persisted operations");
    assert!(matches!(
        page.events.as_slice(),
        [event]
            if matches!(
                &event.operation,
                DispatchOperation::Claimed { claim } if claim.run_id.0 == "run-1"
            )
    ));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn legacy_completion_without_dispatch_identity_fails_closed() {
    // Legacy-tombstone decision rule L1: C1 a completed Run id exists, C2 its
    // pre-V0024 fingerprint is NULL, C3 a caller submits an otherwise plausible
    // dispatch under that id => E1 reject the replay and E2 create no live row.
    // Absence of the compacted payload cannot become authorization after upgrade.
    let path = std::env::temp_dir().join(format!(
        "awaken_sqlite_legacy_completion_{}.db",
        std::process::id()
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    drop(
        SqliteDispatchStore::open(&path, system_clock())
            .await
            .expect("migrate"),
    );
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .expect("open legacy fixture");
    sqlx::query(
        "INSERT INTO runtime_dispatch_completion (run_id, request_fingerprint) \
         VALUES (?1, NULL)",
    )
    .bind("legacy-completed")
    .execute(&connection)
    .await
    .expect("insert historical tombstone");
    connection.close().await;

    let store = SqliteDispatchStore::open(&path, system_clock())
        .await
        .expect("reopen migrated store");
    assert!(
        store
            .enqueue(RunDispatch::new(activation("legacy-completed")))
            .await
            .is_err(),
        "L1/E1"
    );
    assert!(store.list_dispatches().await.unwrap().is_empty(), "L1/E2");
    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cancellation_intent_survives_restart_and_reconciles_to_terminal() {
    // Test design. Causes: C1 SQLite persists cancellation intent; C2 the process
    // drops and reopens both stores; C3 recovery drains the intent. Effects: E1
    // C2 retains cancellation; E2 C3 commits Cancelled and removes the dispatch.
    // Constraint/Invariant: restart cannot lose or execute past durable cancel.
    // Decision rule: persist, reopen, recover, and assert terminal truth plus an
    // empty operational queue.
    let base = std::env::temp_dir().join(format!(
        "awaken_sqlite_cancel_recovery_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let dispatch_path = base.with_extension("dispatch.db");
    let commit_path = base.with_extension("commit.db");
    let dispatch_path = dispatch_path.to_string_lossy().into_owned();
    let commit_path = commit_path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&dispatch_path);
    let _ = std::fs::remove_file(&commit_path);
    let run = RunId("cancel-after-restart".to_string());

    {
        let store = SqliteDispatchStore::open(&dispatch_path, system_clock())
            .await
            .expect("first process store");
        store
            .enqueue(RunDispatch::new(activation("cancel-after-restart")))
            .await
            .unwrap();
        assert!(store.cancel(&run).await.unwrap().is_some());
        // Process crashes here: no terminal commit and no settlement.
    }

    let store = Arc::new(
        SqliteDispatchStore::open(&dispatch_path, system_clock())
            .await
            .expect("restarted store"),
    );
    let commit = Arc::new(
        SqliteCommitCoordinator::open(&commit_path)
            .await
            .expect("commit store"),
    );
    let ingress = DispatchTestHarness::new(
        harness::text_runtime(),
        store.clone(),
        commit.clone(),
        system_clock(),
    );
    assert_eq!(
        ingress
            .recover(harness::clock(0))
            .await
            .expect("reconcile cancellation"),
        vec![(run.clone(), RunState::Ended(EndCause::Cancelled))]
    );
    assert!(store.list_dispatches().await.unwrap().is_empty());
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .expect("terminal record")
            .state,
        RunState::Ended(EndCause::Cancelled)
    );
    drop(ingress);
    drop(store);
    drop(commit);
    let _ = std::fs::remove_file(&dispatch_path);
    let _ = std::fs::remove_file(&commit_path);
}

#[tokio::test]
async fn latest_schema_removes_the_obsolete_delegation_table() {
    let path = std::env::temp_dir().join(format!(
        "awaken_sqlite_no_delegation_store_{}.db",
        std::process::id()
    ));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);
    let store = SqliteDispatchStore::open(&path, system_clock())
        .await
        .expect("migrate");
    drop(store);

    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .expect("inspect");
    let table: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'runtime_delegation_group'",
    )
    .fetch_optional(&connection)
    .await
    .expect("query schema");
    assert!(table.is_none(), "delegation state belongs to ThreadCommit");
    connection.close().await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn dead_letter_budget_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_dead_letter(&store, &clock).await;
}

#[tokio::test]
async fn cancel_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_cancel(&store, &clock).await;
}

#[tokio::test]
async fn priority_dedupe_gc_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_priority_dedupe_gc(&store, &clock).await;
}

#[tokio::test]
async fn lease_renewal_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_lease_renewal(&store, &clock).await;
}

#[tokio::test]
async fn idle_thread_inbox_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_idle_thread_inbox(&store, &clock).await;
}

#[tokio::test]
async fn supersession_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_supersession(&store, &clock).await;
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = std::sync::Arc::new(
        SqliteDispatchStore::open_in_memory(clock.clock())
            .await
            .expect("open"),
    );
    harness::assert_concurrent_recovery_yields_one_winner(store, &clock).await;
}

#[tokio::test]
async fn dead_letter_ttl_gc_on_sqlite() {
    use awaken_dispatch::DispatchMaintenance as _;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("purge.db");
    let path = path.to_str().unwrap();
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path, clock.clock())
        .await
        .expect("open");
    harness::assert_dead_letter_ttl_gc(&store, &clock).await;
    harness::attempt_retirement::assert_purge_retains_occupied(&store, &clock).await;

    // SQLite cause/effect table. Q1 half owner/epoch input is refused by the
    // original BEFORE UPDATE pair trigger (1811), leaving the genuinely empty
    // row unchanged and collectible. Q2 an orphan binding is a CHECK-only
    // storage fault, injected on this fixture connection; Q3 closing-only and
    // Q4 malformed binding on a complete slot are representable raw facts.
    // Q2..Q4 -> direct/exact/domain read error, never false, and both GC paths
    // retain the exact original tuple. These are storage fault injections,
    // not claims that historical production rows have been recovered.
    //
    // | Rule | cause | modern schema | observation | GC |
    // | Q1 | owner-only/epoch-only | trigger refuses, no write | true empty | allowed |
    // | Q2 | orphan binding | fixture-local CHECK fault | unavailable | retain |
    // | Q3 | closing-only | raw update allowed | unavailable | retain |
    // | Q4 | malformed complete binding | raw update allowed | unavailable | retain |
    // The pure 16-row retained-fact table separately covers historical half
    // pairs -> error; no trigger is dropped to fabricate their persisted form.
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    let raw = "SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, active_attempt_closing FROM runtime_dispatch WHERE run_id = ?1";
    for (name, mutation) in [
        (
            "purge-owner-only",
            "active_attempt_owner = 'retained-original-owner'",
        ),
        ("purge-epoch-only", "active_attempt_epoch = 1"),
    ] {
        let (run, _) = harness::attempt_retirement::dead_letter_empty(&store, name, &clock).await;
        let before: (Option<String>, Option<i64>, Option<String>, bool) = sqlx::query_as(raw)
            .bind(&run.0)
            .fetch_one(&connection)
            .await
            .unwrap();
        let error = sqlx::query(&format!(
            "UPDATE runtime_dispatch SET {mutation} WHERE run_id = ?1"
        ))
        .bind(&run.0)
        .execute(&connection)
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("1811"),
            "Q1 current pair trigger refuses the half slot"
        );
        let after: (Option<String>, Option<i64>, Option<String>, bool) = sqlx::query_as(raw)
            .bind(&run.0)
            .fetch_one(&connection)
            .await
            .unwrap();
        assert_eq!(after, before, "Q1 rejection writes no physical facts");
        assert!(
            store.load_physical_attempt(&run).await.unwrap().is_none(),
            "Q1 truly empty original"
        );
        assert!(
            !store
                .dispatch_summary(&run)
                .await
                .unwrap()
                .unwrap()
                .physical_attempt_active,
            "Q1 summary agrees with exact empty read"
        );
    }
    assert_eq!(
        store.purge_dead_letters().await.unwrap(),
        2,
        "Q1 only the two never-occupied rows are collectible"
    );

    let mut snapshots = Vec::new();
    for (name, mutation) in [
        ("purge-orphan-binding", "active_attempt_binding = '{}'"),
        ("purge-closing-only", "active_attempt_closing = TRUE"),
        (
            "purge-malformed-binding",
            "active_attempt_owner = 'retained-original-owner', active_attempt_epoch = 1, active_attempt_binding = '{}'",
        ),
    ] {
        let (run, _) = harness::attempt_retirement::dead_letter_empty(&store, name, &clock).await;
        if name == "purge-orphan-binding" {
            // Isolate the existing CHECK fault injection. This does not affect
            // the pair triggers or the Store's own connection/constraints.
            sqlx::query("PRAGMA ignore_check_constraints = ON")
                .execute(&connection)
                .await
                .unwrap();
        }
        let write = sqlx::query(&format!(
            "UPDATE runtime_dispatch SET {mutation} WHERE run_id = ?1"
        ))
        .bind(&run.0)
        .execute(&connection)
        .await;
        if name == "purge-orphan-binding" {
            sqlx::query("PRAGMA ignore_check_constraints = OFF")
                .execute(&connection)
                .await
                .unwrap();
        }
        write.unwrap();
        let snapshot: (Option<String>, Option<i64>, Option<String>, bool) = sqlx::query_as(raw)
            .bind(&run.0)
            .fetch_one(&connection)
            .await
            .unwrap();
        assert!(
            store.load_physical_attempt(&run).await.is_err(),
            "Q2..Q4 retained facts are not absence"
        );
        assert!(
            store.dispatch_summary(&run).await.is_err(),
            "Q2..Q4 exact summary cannot erase residue"
        );
        assert!(
            awaken_dispatch::has_active_physical_attempt_for_owner(
                &store,
                "retained-original-owner"
            )
            .await
            .is_err(),
            "Q2..Q4 original Coordinator domain query is unavailable, never false"
        );
        snapshots.push((run, snapshot));
    }
    assert_eq!(
        snapshots.len(),
        3,
        "Q2..Q4 only representable storage faults were injected"
    );
    assert_eq!(
        store.purge_dead_letters().await.unwrap(),
        0,
        "Q2..Q4 unlimited"
    );
    assert_eq!(
        store.purge_dead_letters_before(u64::MAX).await.unwrap(),
        0,
        "Q2..Q4 expired"
    );
    for (run, before) in snapshots {
        let after: (Option<String>, Option<i64>, Option<String>, bool) = sqlx::query_as(raw)
            .bind(&run.0)
            .fetch_one(&connection)
            .await
            .unwrap();
        assert_eq!(
            after, before,
            "Q2..Q4 observation and GC preserve physical residue"
        );
    }
}

#[tokio::test]
async fn relinquish_claim_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_relinquish_claim(&store, &clock).await;
}

#[tokio::test]
async fn defer_claim_is_request_local_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_defer_claim_is_request_local(&store, &clock).await;
}

#[tokio::test]
async fn blocked_reclaims_spend_no_budget_within_the_window_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::blocked_reclaim::assert_blocked_reclaims_spend_no_budget_within_the_window(
        &store, &clock,
    )
    .await;
}

#[tokio::test]
async fn capacity_deferrals_hand_off_without_spending_budget_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::capacity_deferral::assert_capacity_deferrals_hand_off_without_spending_budget(
        &store, &clock,
    )
    .await;
}

#[tokio::test]
async fn defer_claim_uses_the_store_clock_and_exact_deadline_on_sqlite() {
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let store = SqliteDispatchStore::open_in_memory(clock.clone())
        .await
        .expect("open");
    harness::assert_defer_claim_deadline_edge(&store, &clock).await;
}

#[tokio::test]
async fn physical_attempt_admission_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_physical_attempt_admission(&store, &clock).await;
}

#[tokio::test]
async fn list_dispatches_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_list_dispatches(&store, &clock).await;
}

#[tokio::test]
async fn dedupe_ignores_dead_lettered_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_dedupe_ignores_dead_lettered(&store, &clock).await;
}

#[tokio::test]
async fn wake_suppressed_while_thread_running_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock())
        .await
        .expect("open");
    harness::assert_wake_suppressed_while_thread_running(&store, &clock).await;
}

use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchAttemptControl as _, DispatchClaims as _,
    DispatchControlReads as _, DispatchEnqueue as _, DispatchExecution as _, DispatchReads as _,
};

/// Writes an untrustworthy generation straight into the SQLite row, as the
/// corruption, bad migration or tampering the refusal exists for would.
struct SqlitePoison(awaken_sqlite_pool::EmbeddedPool);

impl harness::untrustworthy_generation::Poison for SqlitePoison {
    async fn poison(&self, run_id: &str, expired_attempts: Option<u64>) {
        let sql = match expired_attempts {
            Some(_) => {
                "UPDATE runtime_dispatch SET lease_epoch = ?1, attempt_count = ?2, \
                 lease_until = 0 WHERE run_id = ?3 AND status = 'running'"
            }
            None => {
                "UPDATE runtime_dispatch SET lease_epoch = ?1 WHERE run_id = ?3 \
                 AND status = 'pending' AND ?2 IS NULL"
            }
        };
        let changed = sqlx::query(sql)
            .bind(i64::MAX)
            .bind(expired_attempts.map(|attempts| i64::try_from(attempts).unwrap()))
            .bind(run_id)
            .execute(&self.0)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(changed, 1, "poisoned exactly {run_id}");
    }

    async fn poison_generation(&self, run_id: &str, epoch: i64, reserved: bool) {
        let mut connection = self.0.sqlx().acquire().await.unwrap();
        if epoch < 0 {
            // The schema refuses a negative counter; the corruption this row
            // stands for (a damaged file, a hand edit) is not bound by it.
            sqlx::raw_sql(
                "PRAGMA ignore_check_constraints = ON; \
                 DROP TRIGGER runtime_dispatch_nonnegative_authority_update;",
            )
            .execute(&mut *connection)
            .await
            .unwrap();
        }
        let sql = if reserved {
            "UPDATE runtime_dispatch SET lease_epoch = ?1, status = 'reserved', \
             lease_until = 0 WHERE run_id = ?2 AND status = 'pending'"
        } else {
            "UPDATE runtime_dispatch SET lease_epoch = ?1 WHERE run_id = ?2"
        };
        let changed = sqlx::query(sql)
            .bind(epoch)
            .bind(run_id)
            .execute(&mut *connection)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(changed, 1, "poisoned exactly {run_id}");
    }
}

#[tokio::test]
async fn sqlite_claim_scans_dead_letter_an_untrustworthy_generation_and_continue() {
    // Decision table U1-U11: tests/harness/untrustworthy_generation.rs.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("integrity.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(10);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    harness::untrustworthy_generation::assert_untrustworthy_generation_rows(
        &store,
        &SqlitePoison(connection),
        i64::MAX as u64,
        i64::MAX,
    )
    .await;
}

#[tokio::test]
async fn sqlite_claim_scans_skip_a_negative_generation_and_report_it_once() {
    // Decision table U12: tests/harness/untrustworthy_generation.rs.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("negative.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(10);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock())
        .await
        .unwrap();
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    harness::untrustworthy_generation::assert_negative_generation_rows(
        &store,
        &SqlitePoison(connection),
    )
    .await;
}
