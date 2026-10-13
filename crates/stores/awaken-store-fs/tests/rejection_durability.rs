//! Rejections must leave the durable log intact (G13): a commit the read model
//! would reject must never reach the append-only log, or replaying it on the next
//! `open` would fail and the store would be permanently unopenable. These pin the
//! reference-owned shape, terminal, sequence, and event-cursor admission against
//! that failure mode. Reachable event-capacity rejects exercise real fsync/reopen;
//! private in-memory boundary tests cover unreachably large global cursors.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitReceipt, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_store_fs::FsCommitCoordinator;

fn ck(thread: &str, run: &str, text: &str, state: RunState) -> ThreadCommit {
    let run_id = RunId(run.to_string());
    let disposition = match state {
        RunState::Running => RunDisposition::running(run_id),
        RunState::Ended(cause) => RunDisposition::ended(run_id, cause),
        RunState::Awaiting => panic!("test checkpoint requires an awaiting ticket"),
    };
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run: disposition,
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: vec![Draft {
            kind: EventKind::RunStateChanged,
            payload: serde_json::Value::Null,
        }],
    }
}

async fn fresh(name: &str) -> (std::path::PathBuf, FsCommitCoordinator) {
    let dir = std::env::temp_dir().join(format!("awaken_store_fs_reject_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
        .await
        .expect("open");
    (dir, store)
}

fn operation(commit: ThreadCommit, ordinal: u64) -> CommitOperation {
    CommitOperation {
        operation_id: CommitOperationId::new(commit.run_id().clone(), ordinal),
        expected_thread_version: ordinal,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    }
}

async fn submit(
    store: &FsCommitCoordinator,
    commit: ThreadCommit,
    operation_path: bool,
    ordinal: u64,
) -> Result<CommitRecord, Error> {
    if operation_path {
        store
            .commit_operation(operation(commit, ordinal))
            .await
            .map(|receipt| receipt.commit_record())
    } else {
        store.commit(commit).await
    }
}

fn oversized_candidate(first: &ThreadCommit) -> ThreadCommit {
    let mut rejected = first.clone();
    rejected.events.push(rejected.events[0].clone());
    rejected.messages = ck("t1", "r1", "rejected", RunState::Running).messages;
    rejected
        .state
        .push(awaken_agent_contract::agent::state::Command::set(
            awaken_agent_contract::agent::state::Scope::Thread,
            awaken_agent_contract::agent::state::MergePolicy::Exclusive,
            "rejected.candidate",
            serde_json::json!(true),
        ));
    rejected.run = RunDisposition::awaiting(ResumeTicket::new(
        "rejected",
        RunId("r1".into()),
        ThreadId("t1".into()),
        "snapshot",
        "fingerprint",
        AwaitTarget::Pause(PauseReason::Manual),
    ));
    rejected
        .validate()
        .expect("E2 candidate has a valid commit shape");
    rejected
}

async fn exact_replay(
    store: &FsCommitCoordinator,
    first: ThreadCommit,
    dir: &std::path::Path,
    bytes: &[u8],
) -> CommitReceipt {
    let original = operation(first, 0);
    let receipt = store.commit_operation(original.clone()).await.unwrap();
    assert!(receipt.duplicate, "E4");
    assert_eq!(receipt.payload_hash, original.payload_hash, "E4 hash");
    assert_eq!(receipt.commit_sequence, 1, "E4 original sequence");
    assert_eq!(receipt.thread_version, 1, "E4 original version");
    assert_eq!(
        std::fs::read(dir.join("commits.ndjson")).unwrap(),
        bytes,
        "E4 no reappend"
    );
    receipt
}

#[tokio::test]
async fn oversized_event_batch_is_rejected_before_fsync_for_commit_and_operation() {
    // Cause/effect table: E1 event count=1000 => exact legal dense append;
    // E2 event count=1001 => reject before fsync, warm prefix and raw log bytes
    // unchanged; E3 cold reopen => exact same messages/state/events/Run/tickets/
    // versions/ordinals/times; E4 exact operation replay => original receipt/hash,
    // duplicate=true and no appended byte. Both ordinary and operation paths
    // execute E1-E3; E4 applies only to the existing idempotent operation path.
    // The in-memory owner tests MAX sequence and multiplication/addition overflow;
    // this test proves the real filesystem calls that complete owner before append.
    for operation_path in [false, true] {
        let (dir, store) = fresh(if operation_path {
            "event-op"
        } else {
            "event-plain"
        })
        .await;
        let mut first = ck("t1", "r1", "legal", RunState::Running);
        first.events = vec![first.events[0].clone(); 1000];
        assert_eq!(
            submit(&store, first.clone(), operation_path, 0)
                .await
                .unwrap()
                .sequence,
            1,
            "E1"
        );
        let baseline = store
            .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
            .await
            .unwrap();
        assert_eq!(baseline.events.len(), 1000, "E1 exact event count");
        assert_eq!(baseline.events[0].sequence, 1000, "E1 original encoding");
        assert_eq!(baseline.events[999].sequence, 1999, "E1 last legal offset");
        let bytes = std::fs::read(dir.join("commits.ndjson")).unwrap();
        let receipt = if operation_path {
            Some(exact_replay(&store, first.clone(), &dir, &bytes).await)
        } else {
            None
        };
        assert!(
            submit(&store, oversized_candidate(&first), operation_path, 1)
                .await
                .is_err(),
            "E2"
        );
        assert_eq!(
            std::fs::read(dir.join("commits.ndjson")).unwrap(),
            bytes,
            "E2 zero durable bytes"
        );
        assert_eq!(
            store
                .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
                .await
                .unwrap(),
            baseline,
            "E2 warm prefix"
        );
        drop(store);
        let reopened = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
                .await
                .unwrap(),
            baseline,
            "E3 cold prefix"
        );
        if let Some(receipt) = receipt {
            assert_eq!(
                exact_replay(&reopened, first, &dir, &bytes).await,
                receipt,
                "E4 complete warm/cold receipt"
            );
        }
        assert_eq!(
            submit(
                &reopened,
                ck("t1", "r1", "after", RunState::Running),
                operation_path,
                1
            )
            .await
            .unwrap()
            .sequence,
            2,
            "E2 rejected append consumed no sequence"
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// G13: an invalid plan (empty thread_id) is rejected AND writes nothing durable —
// so the store still opens (empty) after the restart. Before the fix the invalid
// line was appended before the inner model validated, and the reopen replay failed.
#[tokio::test]
async fn invalid_commit_leaves_the_log_reopenable_and_empty() {
    let (dir, store) = fresh("invalid").await;

    let bad = ck("", "r1", "bad", RunState::Ended(EndCause::NaturalEnd));
    assert!(store.commit(bad).await.is_err(), "invalid plan rejected");
    drop(store); // simulate a restart

    let reopened = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
        .await
        .expect("store must still open after a rejected commit");
    assert!(
        reopened
            .committed_messages(&ThreadId("".to_string()))
            .is_empty(),
        "no partial state after a rejected commit"
    );
    assert!(reopened.run(&RunId("r1".to_string())).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// G13 + terminal fence: a post-terminal commit for a run that already ended is
// fenced even when a DIFFERENT run committed afterward and became the thread's
// latest — and it leaves nothing durable, so the store reopens with exactly the
// committed prefix. Before the fix fs's pre-check used the latest-run cache and
// missed a non-latest terminal run, appending a line the reopen replay rejected.
#[tokio::test]
async fn post_terminal_commit_on_a_non_latest_run_is_fenced_without_corrupting_the_log() {
    let (dir, store) = fresh("nonlatest").await;
    let thread = ThreadId("t1".to_string());

    store
        .commit(ck("t1", "A", "a", RunState::Ended(EndCause::NaturalEnd)))
        .await
        .expect("run A ends");
    store
        .commit(ck("t1", "B", "b", RunState::Ended(EndCause::NaturalEnd)))
        .await
        .expect("run B ends and becomes latest");

    // Re-commit terminal run A (now non-latest): must be fenced.
    let fenced = store
        .commit(ck(
            "t1",
            "A",
            "a-again",
            RunState::Ended(EndCause::NaturalEnd),
        ))
        .await;
    assert!(fenced.is_err(), "post-terminal commit on run A is fenced");

    drop(store); // simulate a restart
    let reopened = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
        .await
        .expect("store must still open after a fenced commit");
    // Exactly the two committed messages survive; the fenced duplicate is absent.
    assert_eq!(reopened.committed_messages(&thread).len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// Event ordering across multiple commits: sequences are monotone in commit order
// and paging is cursor-exclusive, read back after a reopen from the durable log.
#[tokio::test]
async fn events_keep_commit_order_across_a_reopen() {
    let (dir, store) = fresh("order").await;

    store
        .commit(ck("t1", "r1", "one", RunState::Running))
        .await
        .expect("commit 1");
    store
        .commit(ck("t1", "r1", "two", RunState::Running))
        .await
        .expect("commit 2");
    store
        .commit(ck(
            "t1",
            "r1",
            "three",
            RunState::Ended(EndCause::NaturalEnd),
        ))
        .await
        .expect("commit 3");
    drop(store);

    let reopened = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
        .await
        .expect("reopen");
    let scope = EventScope::Run(RunId("r1".to_string()));
    let all = reopened
        .list_events(&scope, None, 10)
        .await
        .expect("events");
    assert_eq!(all.len(), 3, "three events survive the reopen");
    assert!(
        all[0].sequence < all[1].sequence && all[1].sequence < all[2].sequence,
        "monotone commit order after replay"
    );
    // Cursor paging is exclusive of the cursor sequence.
    let page = reopened
        .list_events(&scope, Some(all[0].sequence), 10)
        .await
        .expect("events");
    assert_eq!(page.len(), 2, "remainder after the first cursor");
    assert_eq!(page[0].sequence, all[1].sequence);
    let _ = std::fs::remove_dir_all(&dir);
}

// Empty-store reads: a freshly opened store has no truth.
#[tokio::test]
async fn empty_store_reads_are_absent() {
    let (dir, store) = fresh("empty").await;
    assert!(
        store
            .committed_messages(&ThreadId("nope".to_string()))
            .is_empty()
    );
    assert!(store.run(&RunId("nope".to_string())).is_none());
    assert!(store.latest_run(&ThreadId("nope".to_string())).is_none());
    assert!(
        store
            .list_events(&EventScope::Run(RunId("nope".to_string())), None, 10)
            .await
            .expect("events")
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mismatched_fresh_operation_hash_rejects_before_fsync_and_cold_recovery() {
    // Causes: F1 valid body A carries canonical hash(body B), B != A; F2 live
    // filesystem append is requested; F3 the store is reopened. Rules F1+F2
    // =>reject before any durable byte or complete prefix change; F3=>same
    // empty prefix. A corrected canonical operation then appends normally.
    // This uses the existing reference-owned pre-fsync admission, not a second
    // filesystem hash validator or a new historical decoding path.
    let (dir, store) = fresh("canonical-hash").await;
    let candidate = ck("t1", "r1", "body-a", RunState::Running);
    let other = ck("t1", "r1", "body-b", RunState::Running);
    let mut request = operation(candidate.clone(), 0);
    request.payload_hash = commit_payload_hash(&other).unwrap();
    let before = store
        .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
        .await
        .unwrap();
    let bytes = std::fs::read(dir.join("commits.ndjson")).unwrap();
    assert!(store.commit_operation(request).await.is_err(), "F1/F2");
    assert_eq!(
        std::fs::read(dir.join("commits.ndjson")).unwrap(),
        bytes,
        "F2 before fsync"
    );
    assert_eq!(
        store
            .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
            .await
            .unwrap(),
        before,
        "F2 warm prefix"
    );
    drop(store);
    let reopened = FsCommitCoordinator::open(&dir, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .recovery_snapshot(&ThreadId("t1".into()), &RunId("r1".into()))
            .await
            .unwrap(),
        before,
        "F3 cold prefix"
    );
    assert_eq!(
        std::fs::read(dir.join("commits.ndjson")).unwrap(),
        bytes,
        "F3 bytes"
    );
    let canonical = reopened
        .commit_operation(operation(candidate, 0))
        .await
        .unwrap();
    assert_eq!(canonical.commit_sequence, 1, "corrected original append");
    assert!(!canonical.duplicate);
    drop(reopened);
    let _ = std::fs::remove_dir_all(dir);
}
