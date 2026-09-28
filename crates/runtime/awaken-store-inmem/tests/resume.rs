//! Invariant 2 (ADR-0039 D4): a reader reconstructs a run from committed facts,
//! not from ephemeral live run state. For the in-memory backend this proves the
//! `CheckpointReader` read model derives from the committed log; the durable
//! cross-restart form of this test lives in the fs/sqlite backends.

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_store_inmem::MemoryCommitCoordinator;

#[tokio::test]
async fn fresh_reader_resumes_from_committed_facts() {
    let store = MemoryCommitCoordinator::new(awaken_lease_contract::system_clock());
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            messages: vec![Message::text(
                MsgId("m1".to_string()),
                Role::Assistant,
                "hello",
            )],
            state: Vec::new(),
            events: vec![Draft {
                kind: EventKind::RunStateChanged,
                payload: serde_json::Value::Null,
            }],
        })
        .await
        .expect("commit");

    // A separate reader handle over the same committed log — the read model is
    // rebuilt from committed facts, never from a live run object.
    let reader = store.clone();
    let events: &dyn CheckpointReader = &reader;
    let reader: &dyn CommittedThreadView = &reader;

    assert_eq!(reader.committed_messages(&thread).len(), 1);
    assert_eq!(
        reader.run(&run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(
        reader.latest_run(&thread).map(|record| record.id),
        Some(run.clone())
    );
    assert_eq!(
        events
            .list_events(&EventScope::Run(run), None, 10)
            .await
            .expect("events")
            .len(),
        1
    );
    assert_eq!(
        events
            .list_events(&EventScope::Thread(thread), None, 10)
            .await
            .expect("events")
            .len(),
        1
    );
}
