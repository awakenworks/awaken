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
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};

fn checkpoint(thread: &ThreadId, run: &RunId, text: &str, phase: Phase) -> ThreadCommit {
    ThreadCommit {
        thread_id: thread.clone(),
        run_fact: RunFact {
            run_id: run.clone(),
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
            payload: serde_json::json!({ "text": text }),
        }],
        waiting: None,
    }
}

fn ended_checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    checkpoint(thread, run, text, Phase::Ended(EndCause::NaturalEnd))
}

fn running_checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    checkpoint(thread, run, text, Phase::Running)
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
    // A run's two events come from a mid-flight `Running` step and then the
    // terminal `Ended` commit — the order a real run produces them. (Committing
    // two terminal facts for one run would trip the terminal-is-final guard, which
    // fences a stale owner's duplicate post-terminal commit; a run ends once.)
    store
        .commit(running_checkpoint(&thread, &run, "one"))
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

/// Terminal-is-final (exactly-once committed log): once a run's committed phase
/// is terminal, any later commit for that same run is rejected and changes
/// nothing — the transcript stays exactly-once even under a stale owner's
/// duplicate post-terminal commit. Every backend enforces this identically
/// (in-memory scan / SQLite projection / fs pre-check), so it belongs in the
/// shared suite: a backend that let a second terminal commit through, or mutated
/// state while rejecting, would diverge here.
pub async fn terminal_run_is_fenced<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-fence".to_string());
    let run = RunId("conf-fence-r".to_string());
    store
        .commit(ended_checkpoint(&thread, &run, "final"))
        .await
        .expect("first terminal commit lands");

    // A second commit for the already-terminal run is fenced.
    let fenced = store
        .commit(ended_checkpoint(&thread, &run, "duplicate"))
        .await;
    assert!(fenced.is_err(), "post-terminal commit is rejected");

    // The rejection left no partial state: exactly the one committed message, and
    // the run's phase is still the original terminal fact.
    assert_eq!(
        store.committed_messages(&thread).len(),
        1,
        "no duplicate transcript after a fenced commit"
    );
    assert_eq!(
        store.run(&run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
        "phase unchanged by the fenced commit"
    );
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
