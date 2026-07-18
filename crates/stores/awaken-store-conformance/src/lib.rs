//! Trait-generic store conformance suite (ADR-0039 slice 2.6).
//!
//! Every persistence backend that implements `Coordinator + CheckpointReader`
//! must pass the same behavioural checks — commit atomicity, fact-authority
//! reads, committed-event order, and cursor paging (G1/G13). Backends call these
//! from their own test crate, each with a freshly constructed store, so the suite
//! stays backend-agnostic and the media (inmem/fs/postgres/sqlite) cannot diverge.

use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};

fn checkpoint(thread: &ThreadId, run: &RunId, text: &str, state: RunState) -> ThreadCommit {
    let disposition = match state {
        RunState::Running => RunDisposition::running(run.clone()),
        RunState::Ended(cause) => RunDisposition::ended(run.clone(), cause),
        RunState::Awaiting => panic!("awaiting checkpoints must be constructed with a ticket"),
    };
    ThreadCommit {
        thread_id: thread.clone(),
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

fn ended_checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    checkpoint(thread, run, text, RunState::Ended(EndCause::NaturalEnd))
}

fn running_checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    checkpoint(thread, run, text, RunState::Running)
}

/// A awaiting ticket correlated to `thread`/`run` — the shape `ThreadCommit::validate`
/// accepts (matching run/thread ids), so awaiting never orphans a ticket.
fn ticket(thread: &ThreadId, run: &RunId) -> ResumeTicket {
    ResumeTicket {
        correlation_id: "conf-corr".to_string(),
        run_id: run.clone(),
        thread_id: thread.clone(),
        snapshot_id: "conf-snap".to_string(),
        catalog_fingerprint: "conf-fp".to_string(),
        reason: AwaitReason::ToolPermission,
        call_id: Some("conf-call".to_string()),
        pending_tool: None,
        deadline_ms: None,
    }
}

/// A `Awaiting` checkpoint that awaits the run with a correlated ticket, committed
/// atomically with the state transition.
fn awaiting_checkpoint(thread: &ThreadId, run: &RunId) -> ThreadCommit {
    ThreadCommit {
        thread_id: thread.clone(),
        run: RunDisposition::awaiting(ticket(thread, run)),
        messages: Vec::new(),
        state: Vec::new(),
        events: Vec::new(),
    }
}

/// A `Running` checkpoint carrying committed state commands (no message), for the
/// state-replay read.
fn state_checkpoint(thread: &ThreadId, run: &RunId, state: Vec<StateCommand>) -> ThreadCommit {
    ThreadCommit {
        thread_id: thread.clone(),
        run: RunDisposition::running(run.clone()),
        messages: Vec::new(),
        state,
        events: Vec::new(),
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
        store.run(&run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
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

/// Terminal-is-final (exactly-once committed log): once a run's committed state
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
    // the run's state is still the original terminal fact.
    assert_eq!(
        store.committed_messages(&thread).len(),
        1,
        "no duplicate transcript after a fenced commit"
    );
    assert_eq!(
        store.run(&run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
        "state unchanged by the fenced commit"
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

/// Awaiting-ticket lifecycle: a `Awaiting` checkpoint with a correlated ticket awaits
/// the run (the ticket becomes readable), and the next non-awaiting commit
/// (resume/terminal) clears it so a stale resume against an old correlation finds
/// nothing and fails closed (G5). Every backend awaits/clears the ticket atomically
/// with the checkpoint, so a backend that leaked a cleared ticket — or dropped a
/// live one — diverges here.
pub async fn resume_ticket_awaits_then_clears<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-wait".to_string());
    let run = RunId("conf-wait-r".to_string());

    store
        .commit(awaiting_checkpoint(&thread, &run))
        .await
        .expect("await");
    assert!(
        store.resume_ticket(&run).is_some(),
        "an awaiting run exposes its ticket"
    );

    // Resume to a terminal fact clears the ticket.
    store
        .commit(ended_checkpoint(&thread, &run, "resumed"))
        .await
        .expect("resume to terminal");
    assert!(
        store.resume_ticket(&run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );
}

/// Concurrent append: two commits for distinct runs on one thread, issued
/// concurrently, both survive with distinct AND dense (consecutive) sequences —
/// neither lost the other's transcript and neither collided on the commit
/// sequence. The two commits are polled concurrently (`join!`), so a backend that
/// allocated the sequence non-atomically, or dropped an interleaved append, would
/// diverge here. (Postgres additionally proves the cross-process race with its own
/// dedicated test; this is the portable single-store invariant.)
pub async fn concurrent_appends_are_dense_and_distinct<S: Coordinator + CheckpointReader>(
    store: &S,
) {
    let thread = ThreadId("conf-concurrent".to_string());
    let run_a = RunId("conf-cc-a".to_string());
    let run_b = RunId("conf-cc-b".to_string());

    let (a, b) = tokio::join!(
        store.commit(running_checkpoint(&thread, &run_a, "a")),
        store.commit(running_checkpoint(&thread, &run_b, "b")),
    );
    let sa = a.expect("commit a").sequence;
    let sb = b.expect("commit b").sequence;

    assert_ne!(sa, sb, "concurrent commits get distinct sequences");
    let (lo, hi) = (sa.min(sb), sa.max(sb));
    assert_eq!(hi - lo, 1, "the two sequences are dense (consecutive)");
    assert_eq!(
        store.committed_messages(&thread).len(),
        2,
        "both concurrent appends survived on the thread"
    );
}

/// Multi-thread isolation: two threads committed to ONE store do not leak
/// transcripts — each thread reads only its own messages, and its own run is its
/// latest. This is the property every backend must hold and the reason a fleet can
/// host many sessions in one store. All backends key committed truth by thread (the
/// in-memory reference, and thus the filesystem store that reuses it as its read
/// model, are thread-keyed too), so every backend runs this case.
pub async fn two_threads_in_one_store_are_isolated<S: Coordinator + CheckpointReader>(store: &S) {
    let ta = ThreadId("conf-iso-a".to_string());
    let tb = ThreadId("conf-iso-b".to_string());
    let ra = RunId("conf-iso-ra".to_string());
    let rb = RunId("conf-iso-rb".to_string());

    store
        .commit(ended_checkpoint(&ta, &ra, "alpha"))
        .await
        .expect("A");
    store
        .commit(ended_checkpoint(&tb, &rb, "beta"))
        .await
        .expect("B");

    let a = store.committed_messages(&ta);
    let b = store.committed_messages(&tb);
    assert_eq!(a.len(), 1, "thread A sees only its own message");
    assert_eq!(a[0].text_content(), "alpha");
    assert_eq!(b.len(), 1, "thread B sees only its own message");
    assert_eq!(b[0].text_content(), "beta");

    assert_eq!(
        store.latest_run(&ta).map(|record| record.id),
        Some(ra),
        "thread A's latest run is its own, not B's"
    );
    assert_eq!(store.latest_run(&tb).map(|record| record.id), Some(rb));
}

/// Empty-store reads: before any commit, every read port is absent — no messages,
/// no run record, no latest run, no events (by run or thread scope), no awaiting
/// ticket, no committed state. A backend that pre-materialized a partial or
/// default row would diverge here (G13).
pub async fn empty_store_reads_are_absent<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-empty".to_string());
    let run = RunId("conf-empty-r".to_string());

    assert!(
        store.committed_messages(&thread).is_empty(),
        "no transcript"
    );
    assert!(store.run(&run).is_none(), "no run record");
    assert!(store.latest_run(&thread).is_none(), "no latest run");
    assert!(
        store
            .list_events(&EventScope::Run(run.clone()), None, 10)
            .is_empty(),
        "no run-scoped events"
    );
    assert!(
        store
            .list_events(&EventScope::Thread(thread.clone()), None, 10)
            .is_empty(),
        "no thread-scoped events"
    );
    assert!(store.resume_ticket(&run).is_none(), "no awaiting ticket");
    assert!(
        store.committed_state(&thread).is_empty(),
        "no committed state"
    );
}

/// State-command replay read: committed state commands are readable back through
/// `committed_state`, in commit order, so a resumed run rebuilds its materialized
/// state from durable truth (and usage / compaction accounting, which read the
/// same port, stay correct). A backend that persisted state commands but did not
/// serve them back would diverge here.
///
/// NOTE: this exercises the `committed_state` read port; a backend that stores the
/// state-command rows durably but leaves `committed_state` as the trait default
/// (empty) would FAIL — that is the divergence this case exists to catch. Every
/// backend now projects the port (inmem/fs from their read model, SQLite/Postgres
/// from the durable `state_command` rows), so all of them run this case.
pub async fn committed_state_replays<S: Coordinator + CheckpointReader>(store: &S) {
    let thread = ThreadId("conf-state".to_string());
    let run = RunId("conf-state-r".to_string());
    let commands = vec![
        StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "k1",
            serde_json::json!("v1"),
        ),
        StateCommand::set(
            Scope::Run,
            MergePolicy::Commutative,
            "k2",
            serde_json::json!(2),
        ),
    ];

    store
        .commit(state_checkpoint(&thread, &run, commands.clone()))
        .await
        .expect("commit state");

    assert_eq!(
        store.committed_state(&thread),
        commands,
        "committed state replays in commit order"
    );
}
