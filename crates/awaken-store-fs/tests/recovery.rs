//! Invariant 2 for the durable backend (ADR-0039 D4): a fresh instance over the
//! same directory replays committed facts and resumes; a torn final line is
//! discarded so recovery keeps exactly the committed prefix.

use std::fs::OpenOptions;
use std::io::Write;

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

fn checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: thread.clone(),
        run_fact: RunFact {
            run_id: run.clone(),
            phase: Phase::Ended(EndCause::NaturalEnd),
        },
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: vec![Draft {
            kind: EventKind::MessageCommitted,
            payload: serde_json::Value::Null,
        }],
        outbox: Vec::new(),
        waiting: None,
    }
}

#[tokio::test]
async fn reopen_replays_committed_facts() {
    let dir = std::env::temp_dir().join("awaken_store_fs_reopen");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &run, "hello"))
            .await
            .expect("commit");
        // store dropped here — simulate a process restart
    }

    let store = FsCommitCoordinator::open(&dir).await.expect("reopen");
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert_eq!(
        store.run(&run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(store.list_events(&EventScope::Run(run), None, 10).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn torn_final_line_is_discarded() {
    let dir = std::env::temp_dir().join("awaken_store_fs_torn");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &run, "good"))
            .await
            .expect("commit");
    }
    // Simulate a crash mid-append: a partial, unparseable trailing record.
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join("commits.ndjson"))
            .expect("append");
        file.write_all(b"{\"thread_id\":\"t1\",\"run_fact\":")
            .expect("torn write");
    }

    let store = FsCommitCoordinator::open(&dir).await.expect("reopen");
    // Exactly the committed prefix survives; the torn tail is discarded.
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert!(store.run(&run).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
