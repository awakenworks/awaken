//! Invariant 2 (ADR-0039 D4): a reader reconstructs a run from committed facts,
//! not from ephemeral live run state. For the in-memory backend this proves the
//! `CheckpointReader` read model derives from the committed log; the durable
//! cross-restart form of this test lives in the fs/sqlite backends.

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
use awaken_store_inmem::MemoryCommitCoordinator;

#[tokio::test]
async fn fresh_reader_resumes_from_committed_facts() {
    let store = MemoryCommitCoordinator::new();
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run_fact: RunFact {
                run_id: run.clone(),
                phase: Phase::Ended(EndCause::NaturalEnd),
            },
            messages: vec![Message::text(
                MsgId("m1".to_string()),
                Role::Assistant,
                "hello",
            )],
            state: Vec::new(),
            events: vec![Draft {
                kind: EventKind::MessageCommitted,
                payload: serde_json::Value::Null,
            }],
            outbox: Vec::new(),
            waiting: None,
        })
        .await
        .expect("commit");

    // A separate reader handle over the same committed log — the read model is
    // rebuilt from committed facts, never from a live run object.
    let reader = store.clone();
    let reader: &dyn CheckpointReader = &reader;

    assert_eq!(reader.committed_messages(&thread).len(), 1);
    assert_eq!(
        reader.run(&run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(
        reader.latest_run(&thread).map(|record| record.id),
        Some(run.clone())
    );
    assert_eq!(reader.list_events(&EventScope::Run(run), None, 10).len(), 1);
    assert_eq!(
        reader
            .list_events(&EventScope::Thread(thread), None, 10)
            .len(),
        1
    );
}
