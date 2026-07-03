//! Trait-generic store conformance suite (ADR-0039 slice 2.6).
//!
//! Every persistence backend that implements `Coordinator + CheckpointReader`
//! must pass the same behavioural checks — commit atomicity, fact-authority
//! reads, committed-event order, and cursor paging (G1/G13). Backends call these
//! from their own test crate, each with a freshly constructed store, so the suite
//! stays backend-agnostic and the media (inmem/fs/postgres/sqlite) cannot diverge.

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

fn ended_checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
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
            payload: serde_json::json!({ "text": text }),
        }],
        outbox: Vec::new(),
        waiting: None,
    }
}

/// A committed checkpoint is readable as facts: transcript, run record, latest
/// run, and the committed event (ADR-0039 D1/D4).
pub async fn commit_then_read<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-read".to_string());
    let run = RunId("conf-read-r".to_string());
    store
        .commit(ended_checkpoint(&thread, &run, "hello"))
        .await
        .expect("commit");

    assert_eq!(store.committed_messages(&thread).len(), 1, "transcript");
    assert_eq!(
        store.run(&run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
        "run record",
    );
    assert_eq!(
        store.latest_run(&thread).map(|record| record.id),
        Some(run.clone()),
        "latest run"
    );
    assert_eq!(
        store.list_events(&EventScope::Run(run), None, 10).len(),
        1,
        "one event"
    );
}

/// Committed events are returned in commit order and can be paged by cursor.
pub async fn events_ordered_and_paged<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-events".to_string());
    let run = RunId("conf-events-r".to_string());
    store
        .commit(ended_checkpoint(&thread, &run, "one"))
        .await
        .expect("commit 1");
    store
        .commit(ended_checkpoint(&thread, &run, "two"))
        .await
        .expect("commit 2");

    let scope = EventScope::Run(run);
    let all = store.list_events(&scope, None, 10);
    assert_eq!(all.len(), 2, "both events");
    assert!(all[0].sequence < all[1].sequence, "commit order");

    // Cursor paging: first page of one, then the remainder after that cursor.
    let first = store.list_events(&scope, None, 1);
    assert_eq!(first.len(), 1, "first page");
    let rest = store.list_events(&scope, Some(first[0].sequence), 10);
    assert_eq!(rest.len(), 1, "remainder after cursor");
    assert_eq!(rest[0].sequence, all[1].sequence, "cursor is exclusive");
}

/// Successive commits on a thread accumulate transcript; the latest run wins.
pub async fn commits_accumulate<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-acc".to_string());
    let run1 = RunId("conf-acc-r1".to_string());
    let run2 = RunId("conf-acc-r2".to_string());
    store
        .commit(ended_checkpoint(&thread, &run1, "first"))
        .await
        .expect("commit 1");
    store
        .commit(ended_checkpoint(&thread, &run2, "second"))
        .await
        .expect("commit 2");

    assert_eq!(
        store.committed_messages(&thread).len(),
        2,
        "accumulated transcript"
    );
    assert_eq!(
        store.latest_run(&thread).map(|record| record.id),
        Some(run2),
        "latest run wins"
    );
}
