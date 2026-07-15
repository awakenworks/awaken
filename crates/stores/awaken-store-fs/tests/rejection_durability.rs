//! Rejections must leave the durable log intact (G13): a commit the read model
//! would reject must never reach the append-only log, or replaying it on the next
//! `open` would fail and the store would be permanently unopenable. These pin the
//! two rejection paths — plan validation and the terminal fence — against that
//! failure mode, mirroring `awaken-store-sqlite`'s `g13_failed_commit_*` tests so
//! the filesystem backend cannot diverge from the embedded one.

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_store_fs::FsCommitCoordinator;

fn ck(thread: &str, run: &str, text: &str, phase: Phase) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run_fact: RunFact {
            run_id: RunId(run.to_string()),
            phase,
        },
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: vec![Draft {
            kind: EventKind::RunPhaseChanged,
            payload: serde_json::Value::Null,
        }],
        waiting: None,
    }
}

async fn fresh(name: &str) -> (std::path::PathBuf, FsCommitCoordinator) {
    let dir = std::env::temp_dir().join(format!("awaken_store_fs_reject_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsCommitCoordinator::open(&dir).await.expect("open");
    (dir, store)
}

// G13: an invalid plan (empty thread_id) is rejected AND writes nothing durable —
// so the store still opens (empty) after the restart. Before the fix the invalid
// line was appended before the inner model validated, and the reopen replay failed.
#[tokio::test]
async fn invalid_commit_leaves_the_log_reopenable_and_empty() {
    let (dir, store) = fresh("invalid").await;

    let bad = ck("", "r1", "bad", Phase::Ended(EndCause::NaturalEnd));
    assert!(store.commit(bad).await.is_err(), "invalid plan rejected");
    drop(store); // simulate a restart

    let reopened = FsCommitCoordinator::open(&dir)
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
        .commit(ck("t1", "A", "a", Phase::Ended(EndCause::NaturalEnd)))
        .await
        .expect("run A ends");
    store
        .commit(ck("t1", "B", "b", Phase::Ended(EndCause::NaturalEnd)))
        .await
        .expect("run B ends and becomes latest");

    // Re-commit terminal run A (now non-latest): must be fenced.
    let fenced = store
        .commit(ck("t1", "A", "a-again", Phase::Ended(EndCause::NaturalEnd)))
        .await;
    assert!(fenced.is_err(), "post-terminal commit on run A is fenced");

    drop(store); // simulate a restart
    let reopened = FsCommitCoordinator::open(&dir)
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
        .commit(ck("t1", "r1", "one", Phase::Running))
        .await
        .expect("commit 1");
    store
        .commit(ck("t1", "r1", "two", Phase::Running))
        .await
        .expect("commit 2");
    store
        .commit(ck("t1", "r1", "three", Phase::Ended(EndCause::NaturalEnd)))
        .await
        .expect("commit 3");
    drop(store);

    let reopened = FsCommitCoordinator::open(&dir).await.expect("reopen");
    let scope = EventScope::Run(RunId("r1".to_string()));
    let all = reopened.list_events(&scope, None, 10);
    assert_eq!(all.len(), 3, "three events survive the reopen");
    assert!(
        all[0].sequence < all[1].sequence && all[1].sequence < all[2].sequence,
        "monotone commit order after replay"
    );
    // Cursor paging is exclusive of the cursor sequence.
    let page = reopened.list_events(&scope, Some(all[0].sequence), 10);
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
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
