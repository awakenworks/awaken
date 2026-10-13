//! Private reference-state boundaries that a filesystem cannot reach by
//! appending u64::MAX records. No production counter setter or test-only writer
//! is exposed; public inspect/apply paths consume the original state below.

use super::*;
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::agent::state::{MergePolicy, Scope};
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind;
use awaken_agent_contract::thread::commit::operation::commit_payload_hash;
use awaken_agent_contract::thread::commit::staged::RunDisposition;

fn commit(events: usize) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId("admission-thread".into()),
        run: RunDisposition::running(RunId("admission-run".into())),
        messages: vec![Message::text(
            MessageId("candidate".into()),
            Role::User,
            "candidate",
        )],
        state: vec![StateCommand::set(
            Scope::Thread,
            MergePolicy::Exclusive,
            "admission.candidate",
            serde_json::json!(true),
        )],
        events: (0..events)
            .map(|_| Draft {
                kind: Kind::PermissionDecided,
                payload: serde_json::Value::Null,
            })
            .collect(),
    }
}

fn operation(commit: ThreadCommit) -> CommitOperation {
    CommitOperation {
        operation_id: CommitOperationId::new(commit.run_id().clone(), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    }
}

fn at_sequence(sequence: u64) -> MemoryCommitCoordinator {
    MemoryCommitCoordinator {
        state: Arc::new(Mutex::new(CommitState {
            sequence,
            ..CommitState::default()
        })),
        clock: awaken_time::system_clock(),
    }
}

async fn prefix(
    store: &MemoryCommitCoordinator,
) -> (
    RunRecoverySnapshot,
    HashMap<CommitOperationId, CommitReceipt>,
) {
    let snapshot = store
        .recovery_snapshot(
            &ThreadId("admission-thread".into()),
            &RunId("admission-run".into()),
        )
        .await
        .unwrap();
    let receipts = store.state.lock().unwrap().receipts.clone();
    (snapshot, receipts)
}

async fn check_capacity(sequence: u64, events: usize, accepted: bool) {
    for operation_path in [false, true] {
        let store = at_sequence(sequence);
        let candidate = commit(events);
        let operation = operation(candidate.clone());
        let before = prefix(&store).await;
        assert_eq!(store.inspect_commit(&candidate).is_ok(), accepted);
        assert_eq!(store.inspect_operation(&operation).is_ok(), accepted);
        assert_eq!(
            prefix(&store).await,
            before,
            "inspection must not reserve or write"
        );
        let applied = if operation_path {
            store
                .commit_operation_at(operation, Some(41))
                .await
                .map(|receipt| receipt.commit_record())
        } else {
            store.commit_at(candidate.clone(), Some(41)).await
        };
        assert_eq!(applied.is_ok(), accepted);
        if let Ok(record) = applied {
            assert_eq!(record.sequence, sequence + 1);
            let (after, receipts) = prefix(&store).await;
            assert_eq!(after.messages, candidate.messages);
            assert_eq!(after.state, candidate.state);
            assert_eq!(after.events.len(), events);
            for (offset, event) in after.events.iter().enumerate() {
                assert_eq!(
                    event.sequence,
                    encode_run_lifecycle_cursor(record.sequence, offset)
                        .unwrap()
                        .0,
                    "accepted append preserves the canonical event encoding"
                );
            }
            assert_eq!(after.commit_timestamps.get(&record.sequence), Some(&41));
            assert_eq!(receipts.len(), usize::from(operation_path));
        } else {
            assert_eq!(
                prefix(&store).await,
                before,
                "all rejected prefix/receipt regions stutter"
            );
        }
    }
}

#[tokio::test]
async fn inspection_and_apply_share_the_complete_event_slot_boundary() {
    // Causes: C1 event count is 0/1000/1001; C2 ordinary/operation entry;
    // C3 inspection precedes apply. Effects: E1 <=1000 admits unchanged IDs;
    // E2 1001 rejects before any message/state/event/receipt/time/sequence write;
    // E3 every inspection is read-only. Rules S1=(0 or 1000)=>E1+E3,
    // S2=1001=>E2+E3, crossed with both C2 paths by check_capacity. The actual
    // encode_run_lifecycle_cursor law, not a new adapter event limit, owns this.
    for (events, accepted) in [(0, true), (1000, true), (1001, false)] {
        check_capacity(0, events, accepted).await;
    }
}

#[tokio::test]
async fn inspection_and_apply_reject_global_sequence_overflow_before_mutation() {
    // G1 sequence=MAX-1 + no events => exact MAX append; G2 sequence=MAX =>
    // checked_add rejects, including with no events. Both inspect paths and
    // both application paths must preserve the complete old prefix on G2.
    // A record with no events needs no lifecycle cursor and remains legal at
    // MAX; do not accidentally impose event-codec capacity on an empty batch.
    check_capacity(u64::MAX - 1, 0, true).await;
    check_capacity(u64::MAX, 0, false).await;
}

#[tokio::test]
async fn lifecycle_cursor_multiplication_and_addition_are_pre_append_checks() {
    // The persisted codec is commit*1000+offset. C1 next=MAX/1000+1 => multiply
    // overflow at offset 0; C2 next=MAX/1000 and offset<=615 => valid; C3 same
    // next and offset=616 => addition overflow. Rules C1=>reject/stutter,
    // C2=>admit (last cursor exactly MAX), C3=>reject/stutter. Each rule crosses
    // ordinary/new-operation inspection and apply through check_capacity.
    check_capacity(u64::MAX / 1000, 1, false).await;
    check_capacity(u64::MAX / 1000 - 1, 616, true).await;
    check_capacity(u64::MAX / 1000 - 1, 617, false).await;
}

#[tokio::test]
async fn exact_operation_replay_never_reenters_future_append_admission() {
    // R1 an exact terminal operation is durable at time 41; R2 after global
    // sequence reaches MAX, ordinary admission must reject; R3 exact operation
    // retry (despite its stale CAS/Ended/current time) returns the old receipt.
    // Effects: inspection/apply are stutters on R2/R3; R3 preserves identity,
    // original sequence/hash/thread version/time and marks duplicate only.
    let store = at_sequence(0);
    let mut candidate = commit(1);
    candidate.run = RunDisposition::ended(candidate.run_id().clone(), EndCause::NaturalEnd);
    let operation = operation(candidate.clone());
    let original = store
        .commit_operation_at(operation.clone(), Some(41))
        .await
        .unwrap();
    store.state.lock().unwrap().sequence = u64::MAX;
    let before = prefix(&store).await;
    assert!(
        store.inspect_commit(&candidate).is_err(),
        "R2 ordinary terminal fence"
    );
    let inspected = store.inspect_operation(&operation).unwrap().unwrap();
    let replay = store
        .commit_operation_at(operation, Some(99))
        .await
        .unwrap();
    let mut expected = original;
    expected.duplicate = true;
    assert_eq!(
        inspected, expected,
        "R3 inspection returns original receipt"
    );
    assert_eq!(replay, expected, "R3 apply returns original receipt");
    assert_eq!(
        prefix(&store).await,
        before,
        "R2/R3 do not mutate any region"
    );
}

#[tokio::test]
async fn ordinary_inspection_is_not_a_sequence_reservation() {
    // A1 inspect a valid append => next=1, no mutation; A2 another real append
    // consumes sequence 1; A3 applying the earlier inspected value rechecks the
    // current reference state => sequence 2. Inspection is never a grant/token.
    let store = at_sequence(0);
    let candidate = commit(1);
    let before = prefix(&store).await;
    assert_eq!(store.inspect_commit(&candidate).unwrap().sequence, 1, "A1");
    assert_eq!(prefix(&store).await, before, "A1 read-only");
    assert_eq!(
        store
            .commit_at(candidate.clone(), Some(41))
            .await
            .unwrap()
            .sequence,
        1,
        "A2"
    );
    assert_eq!(
        store.commit_at(candidate, Some(42)).await.unwrap().sequence,
        2,
        "A3"
    );
}

#[tokio::test]
async fn shape_and_non_latest_terminal_rejections_share_read_only_admission() {
    // T1 invalid shape rejects before inspection/application mutate; T2 a Run
    // ended and a different Run became latest => the exact old Run still rejects
    // a fresh commit/operation. A valid unrelated latest Run is not a grant to
    // reopen old truth. The complete recovery prefix and receipt map stutter.
    let store = at_sequence(0);
    let mut invalid = commit(1);
    invalid.thread_id.0.clear();
    let before = prefix(&store).await;
    assert!(store.inspect_commit(&invalid).is_err(), "T1");
    assert!(
        store
            .inspect_operation(&operation(invalid.clone()))
            .is_err(),
        "T1"
    );
    assert!(
        store.commit_at(invalid.clone(), Some(40)).await.is_err(),
        "T1"
    );
    assert!(
        store
            .commit_operation_at(operation(invalid), Some(40))
            .await
            .is_err(),
        "T1"
    );
    assert_eq!(prefix(&store).await, before, "T1 complete prefix stutter");
    let mut ended = commit(1);
    ended.run = RunDisposition::ended(ended.run_id().clone(), EndCause::NaturalEnd);
    store.commit_at(ended.clone(), Some(41)).await.unwrap();
    let mut latest = commit(1);
    latest.run = RunDisposition::running(RunId("latest".into()));
    store.commit_at(latest, Some(42)).await.unwrap();
    let before = prefix(&store).await;
    let mut operation = operation(ended.clone());
    operation.expected_thread_version = 2;
    operation.operation_id.ordinal = 1;
    assert!(store.inspect_commit(&ended).is_err(), "T2");
    assert!(store.inspect_operation(&operation).is_err(), "T2");
    assert!(store.commit_at(ended, Some(43)).await.is_err(), "T2");
    assert!(
        store
            .commit_operation_at(operation, Some(43))
            .await
            .is_err(),
        "T2"
    );
    assert_eq!(prefix(&store).await, before, "T2 complete prefix stutter");
}

#[tokio::test]
async fn poisoned_reference_state_refuses_inspection_and_apply_without_recovery() {
    // Causes: P1 an exact operation and its complete prefix are durable; P2
    // the authority mutex is poisoned; P3 ordinary/operation inspection or
    // application is requested, including exact replay. Effects: all P2+P3
    // combinations reject without appending or recovering an untrusted lock;
    // every original state/receipt region remains unchanged. Existing receipt
    // identity does not make an unavailable authority readable. This uses the
    // private original mutex, with no production poison setter or alternate
    // write path. FS consumes the same inspection before any durable append.
    let store = at_sequence(0);
    let candidate = commit(1);
    let operation = operation(candidate.clone());
    store
        .commit_operation_at(operation.clone(), Some(41))
        .await
        .unwrap();
    let before = format!("{:?}", *store.state.lock().unwrap());
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = store.state.lock().unwrap();
        panic!("exercise the existing poisoned-authority rejection");
    }));
    assert!(poisoned.is_err(), "P2");
    assert!(
        store.inspect_commit(&candidate).is_err(),
        "P3 ordinary inspect"
    );
    assert!(
        store.inspect_operation(&operation).is_err(),
        "P3 replay inspect"
    );
    assert!(
        store.commit_at(candidate, Some(99)).await.is_err(),
        "P3 ordinary apply"
    );
    assert!(
        store
            .commit_operation_at(operation, Some(99))
            .await
            .is_err(),
        "P3 replay apply"
    );
    let unavailable = store.state.lock().unwrap_err();
    assert_eq!(
        format!("{:?}", *unavailable.into_inner()),
        before,
        "P2+P3 complete prefix stutter"
    );
}
