//! Scope isolation, event-scope filtering, and migration idempotency for the
//! SQLite backend. Unlike the single-thread in-memory reference, the SQLite
//! projection keys messages and run records by thread, so two threads in one
//! store stay isolated — these tests pin that (a divergence the shared
//! conformance suite does not exercise, because it uses one thread per store).

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_store_sqlite::SqliteCommitCoordinator;

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
            payload: serde_json::json!({ "text": text }),
        }],
    }
}

// Two threads committed to one store stay isolated: each thread sees only its own
// transcript, and its own run is its latest — the other thread's rows are
// invisible. (The in-memory reference flattens to a single thread; SQLite keys by
// thread, so this is a real backend capability the shared suite does not cover.)
#[tokio::test]
async fn two_threads_in_one_store_do_not_leak_transcripts() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let ta = ThreadId("t-a".to_string());
    let tb = ThreadId("t-b".to_string());

    store
        .commit(ck(
            "t-a",
            "r-a",
            "alpha",
            RunState::Ended(EndCause::NaturalEnd),
        ))
        .await
        .expect("A");
    store
        .commit(ck(
            "t-b",
            "r-b",
            "beta",
            RunState::Ended(EndCause::NaturalEnd),
        ))
        .await
        .expect("B");

    let a = store.committed_messages(&ta);
    let b = store.committed_messages(&tb);
    assert_eq!(a.len(), 1, "thread A sees only its own message");
    assert_eq!(a[0].text_content(), "alpha");
    assert_eq!(b.len(), 1, "thread B sees only its own message");
    assert_eq!(b[0].text_content(), "beta");

    assert_eq!(
        store.latest_run(&ta).map(|r| r.id),
        Some(RunId("r-a".to_string())),
        "thread A's latest run is its own, not B's"
    );
    assert_eq!(
        store.latest_run(&tb).map(|r| r.id),
        Some(RunId("r-b".to_string()))
    );
}

// EventScope::Thread returns exactly the events of the runs on that thread; a
// second thread's events are excluded. (Conformance only covers EventScope::Run.)
#[tokio::test]
async fn event_scope_thread_isolates_across_threads() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");

    store
        .commit(ck("t-a", "r-a", "a1", RunState::Running))
        .await
        .expect("a1");
    store
        .commit(ck("t-b", "r-b", "b1", RunState::Running))
        .await
        .expect("b1");
    store
        .commit(ck(
            "t-a",
            "r-a",
            "a2",
            RunState::Ended(EndCause::NaturalEnd),
        ))
        .await
        .expect("a2");

    let a_events = store.list_events(&EventScope::Thread(ThreadId("t-a".to_string())), None, 10);
    assert_eq!(a_events.len(), 2, "thread A has two events");
    assert!(
        a_events
            .iter()
            .all(|e| e.run_id == RunId("r-a".to_string())),
        "no thread-B events leak into thread A's scope"
    );
    assert!(
        a_events[0].sequence < a_events[1].sequence,
        "thread-scoped events keep commit order"
    );

    let b_events = store.list_events(&EventScope::Thread(ThreadId("t-b".to_string())), None, 10);
    assert_eq!(b_events.len(), 1, "thread B has one event");
    assert_eq!(b_events[0].run_id, RunId("r-b".to_string()));
}

// Migration idempotency: running the schema bundle again on an existing DB file
// (every reopen re-runs it) is a no-op — the store opens repeatedly and the
// committed facts survive each time.
#[tokio::test]
async fn reopening_reruns_migrations_idempotently() {
    let path = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_idempotent_migrate_{}.db",
        std::process::id()
    ));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    {
        let store = SqliteCommitCoordinator::open(&path).expect("open 1");
        store
            .commit(ck("t1", "r1", "hi", RunState::Ended(EndCause::NaturalEnd)))
            .await
            .expect("commit");
    }
    // Re-open twice more: each open re-applies the bundle against an already
    // migrated schema. It must succeed and preserve the committed fence.
    for attempt in 2..=3 {
        let store =
            SqliteCommitCoordinator::open(&path).unwrap_or_else(|e| panic!("open {attempt}: {e}"));
        assert_eq!(
            store.commit_count(),
            1,
            "the fence survives an idempotent re-migration"
        );
        assert_eq!(
            store.committed_messages(&ThreadId("t1".to_string())).len(),
            1
        );
    }
    let _ = std::fs::remove_file(&path);
}

// Empty-store event reads are absent (complements g13_projection_absent_before_commit,
// which pins run reads and the fence but not list_events).
#[tokio::test]
async fn empty_store_event_reads_are_absent() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    assert!(
        store
            .list_events(&EventScope::Run(RunId("r1".to_string())), None, 10)
            .is_empty()
    );
    assert!(
        store
            .list_events(&EventScope::Thread(ThreadId("t1".to_string())), None, 10)
            .is_empty()
    );
    assert!(store.latest_run(&ThreadId("t1".to_string())).is_none());
}
