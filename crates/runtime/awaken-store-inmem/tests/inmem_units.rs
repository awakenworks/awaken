//! White-box decision-table coverage for the in-memory reference backend, filling
//! the rows the shared conformance suite does not reach: commit validation and
//! terminal-is-final rejection, waiting-ticket park/clear, event-id density across
//! commits, empty-store and non-matching reads, the `RunStore` vs `CheckpointReader`
//! latest-only-vs-history split, the live `MemoryStreamSink`, the pure replay
//! helpers, and the `MemoryStreamCheckpointStore` overwrite/idempotency contract.

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error};
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::stream_checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_store_inmem::{
    MemoryCommitCoordinator, MemoryStreamCheckpointStore, MemoryStreamSink, replay_latest_phase,
    replay_state,
};

// ---- builders -------------------------------------------------------------

fn commit_with(
    thread: &str,
    run: &str,
    text: &str,
    phase: Phase,
    events: Vec<Draft>,
    state: Vec<Command>,
    waiting: Option<WaitingTicket>,
) -> ThreadCommit {
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
        state,
        events,
        waiting,
    }
}

fn one_event(kind: EventKind) -> Vec<Draft> {
    vec![Draft {
        kind,
        payload: serde_json::Value::Null,
    }]
}

fn ticket(correlation: &str, run: &str, thread: &str) -> WaitingTicket {
    WaitingTicket {
        correlation_id: correlation.to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(thread.to_string()),
        snapshot_id: "snap".to_string(),
        catalog_fingerprint: "fp".to_string(),
        reason: WaitingReason::ToolPermission,
        call_id: None,
        pending_tool: None,
        deadline_ms: None,
    }
}

// ---- commit: validation rejection (decision-table rows R5/R6/R7) ----------

#[tokio::test]
async fn commit_rejects_empty_run_id_before_any_write() {
    let store = MemoryCommitCoordinator::new();
    let err = store
        .commit(commit_with(
            "t",
            "",
            "x",
            Phase::Running,
            vec![],
            vec![],
            None,
        ))
        .await
        .expect_err("empty run_id must be rejected");
    match err {
        Error::Rejected(msg) => assert!(msg.contains("run_id"), "message names run_id: {msg}"),
    }
    // Fail-closed: nothing was written.
    assert_eq!(store.commit_count(), 0);
    assert!(store.committed().messages.is_empty());
}

#[tokio::test]
async fn commit_rejects_empty_thread_id() {
    let store = MemoryCommitCoordinator::new();
    let err = store
        .commit(commit_with(
            "",
            "r",
            "x",
            Phase::Running,
            vec![],
            vec![],
            None,
        ))
        .await
        .expect_err("empty thread_id must be rejected");
    let Error::Rejected(msg) = err;
    assert!(msg.contains("thread_id"), "message names thread_id: {msg}");
    assert_eq!(store.commit_count(), 0);
}

#[tokio::test]
async fn commit_rejects_ticket_for_a_different_run() {
    let store = MemoryCommitCoordinator::new();
    let err = store
        .commit(commit_with(
            "t",
            "r",
            "x",
            Phase::Waiting,
            vec![],
            vec![],
            Some(ticket("c", "other-run", "t")),
        ))
        .await
        .expect_err("cross-run ticket must be rejected");
    let Error::Rejected(msg) = err;
    assert!(
        msg.contains("run_id"),
        "message flags the run_id mismatch: {msg}"
    );
    assert_eq!(store.commit_count(), 0);
    assert!(store.waiting_for(&RunId("r".to_string())).is_none());
}

// ---- commit: terminal-is-final fence (row R4) -----------------------------

#[tokio::test]
async fn first_ended_commit_is_allowed_then_post_terminal_is_fenced() {
    let store = MemoryCommitCoordinator::new();
    // The first Ended commit lands (the run is not yet terminal when it arrives).
    store
        .commit(commit_with(
            "t",
            "r",
            "first",
            Phase::Ended(EndCause::NaturalEnd),
            one_event(EventKind::RunPhaseChanged),
            vec![],
            None,
        ))
        .await
        .expect("first ended commit lands");

    // A stale owner re-driving the same run past its terminus is fenced.
    let err = store
        .commit(commit_with(
            "t",
            "r",
            "dup",
            Phase::Ended(EndCause::NaturalEnd),
            one_event(EventKind::RunPhaseChanged),
            vec![],
            None,
        ))
        .await
        .expect_err("post-terminal commit must be fenced");
    let Error::Rejected(msg) = err;
    assert!(msg.contains("already terminal"), "fence message: {msg}");

    // Exactly-once: the duplicate transcript/event never landed.
    assert_eq!(
        store.commit_count(),
        1,
        "sequence not bumped by a fenced commit"
    );
    assert_eq!(store.committed().messages.len(), 1, "no duplicate message");
    assert_eq!(store.committed().events.len(), 1, "no duplicate event");
}

// ---- commit: waiting park / clear (rows R1/R2/R3, G5) ---------------------

#[tokio::test]
async fn waiting_phase_with_ticket_parks_then_ended_clears_it() {
    let store = MemoryCommitCoordinator::new();
    let run = RunId("r".to_string());

    // Park: a Some ticket + Waiting phase records the ticket.
    store
        .commit(commit_with(
            "t",
            "r",
            "park",
            Phase::Waiting,
            vec![],
            vec![],
            Some(ticket("corr-1", "r", "t")),
        ))
        .await
        .expect("park commit");
    let parked = store.waiting_for(&run).expect("ticket parked");
    assert_eq!(parked.correlation_id, "corr-1");
    // Same value is visible through the ThreadReader read port.
    assert_eq!(
        (&store as &dyn ThreadReader)
            .waiting_ticket(&run)
            .map(|t| t.correlation_id),
        Some("corr-1".to_string())
    );

    // Resume to a terminal phase clears the ticket (G5): a terminal run cannot be
    // resumed again.
    store
        .commit(commit_with(
            "t",
            "r",
            "end",
            Phase::Ended(EndCause::NaturalEnd),
            vec![],
            vec![],
            None,
        ))
        .await
        .expect("terminal commit");
    assert!(
        store.waiting_for(&run).is_none(),
        "ticket cleared on terminal"
    );
}

#[tokio::test]
async fn ticket_is_only_honored_on_the_waiting_phase() {
    // A Some ticket riding a non-Waiting phase must NOT park the run: only the
    // `(Some, Waiting)` arm parks; every other pairing clears. This keeps a
    // Running/Ended commit from leaving a resumable ticket behind.
    let store = MemoryCommitCoordinator::new();
    let run = RunId("r".to_string());
    store
        .commit(commit_with(
            "t",
            "r",
            "run",
            Phase::Running,
            vec![],
            vec![],
            Some(ticket("corr", "r", "t")),
        ))
        .await
        .expect("running commit with stray ticket");
    assert!(
        store.waiting_for(&run).is_none(),
        "a ticket on a Running phase must not park the run"
    );
}

// ---- event id density & monotonicity --------------------------------------

#[tokio::test]
async fn event_ids_are_dense_within_a_commit_and_ascending_across_commits() {
    let store = MemoryCommitCoordinator::new();
    // Two events in commit #1 → sequences 1*1000+0, 1*1000+1 (dense by offset).
    store
        .commit(commit_with(
            "t",
            "r",
            "c1",
            Phase::Running,
            vec![
                Draft {
                    kind: EventKind::RunPhaseChanged,
                    payload: serde_json::Value::Null,
                },
                Draft {
                    kind: EventKind::RunPhaseChanged,
                    payload: serde_json::Value::Null,
                },
            ],
            vec![],
            None,
        ))
        .await
        .expect("commit 1");
    // One event in commit #2 → sequence 2*1000+0, strictly above commit #1's.
    store
        .commit(commit_with(
            "t",
            "r",
            "c2",
            Phase::Ended(EndCause::NaturalEnd),
            one_event(EventKind::RunPhaseChanged),
            vec![],
            None,
        ))
        .await
        .expect("commit 2");

    let events = store.list_events(&EventScope::Run(RunId("r".to_string())), None, 10);
    let seqs: Vec<u64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(
        seqs,
        vec![1000, 1001, 2000],
        "dense offsets, commit-partitioned"
    );
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
}

// ---- list_events: empty store, scope filtering, limit ---------------------

#[tokio::test]
async fn list_events_on_empty_store_is_empty() {
    let store = MemoryCommitCoordinator::new();
    assert!(
        store
            .list_events(&EventScope::Run(RunId("nope".to_string())), None, 10)
            .is_empty()
    );
    assert!(
        store
            .list_events(&EventScope::Thread(ThreadId("nope".to_string())), None, 10)
            .is_empty()
    );
}

#[tokio::test]
async fn list_events_run_scope_filters_by_run_thread_scope_spans_runs() {
    let store = MemoryCommitCoordinator::new();
    for run in ["r1", "r2"] {
        store
            .commit(commit_with(
                "t",
                run,
                run,
                Phase::Ended(EndCause::NaturalEnd),
                one_event(EventKind::RunPhaseChanged),
                vec![],
                None,
            ))
            .await
            .expect("commit");
    }
    // Run scope isolates one run.
    assert_eq!(
        store
            .list_events(&EventScope::Run(RunId("r1".to_string())), None, 10)
            .len(),
        1
    );
    // A run that never committed → empty.
    assert!(
        store
            .list_events(&EventScope::Run(RunId("ghost".to_string())), None, 10)
            .is_empty()
    );
    // Thread scope spans both runs on the thread.
    assert_eq!(
        store
            .list_events(&EventScope::Thread(ThreadId("t".to_string())), None, 10)
            .len(),
        2
    );
    // A non-matching thread → empty.
    assert!(
        store
            .list_events(&EventScope::Thread(ThreadId("other".to_string())), None, 10)
            .is_empty()
    );
    // Limit truncates.
    assert_eq!(
        store
            .list_events(&EventScope::Thread(ThreadId("t".to_string())), None, 1)
            .len(),
        1
    );
}

// ---- RunStore::get (latest only) vs CheckpointReader::run (full history) ---

#[tokio::test]
async fn run_store_get_sees_only_latest_while_checkpoint_reader_finds_history() {
    let store = MemoryCommitCoordinator::new();
    for run in ["r1", "r2"] {
        store
            .commit(commit_with(
                "t",
                run,
                run,
                Phase::Ended(EndCause::NaturalEnd),
                vec![],
                vec![],
                None,
            ))
            .await
            .expect("commit");
    }
    let r1 = RunId("r1".to_string());
    // RunStore::get is a latest-run projection: the superseded run is invisible.
    assert!(
        (&store as &dyn RunStore).get(&r1).is_none(),
        "RunStore::get returns only the latest run"
    );
    assert_eq!(
        (&store as &dyn RunStore)
            .get(&RunId("r2".to_string()))
            .map(|r| r.id),
        Some(RunId("r2".to_string()))
    );
    // CheckpointReader::run reconstructs any run from the committed fact log.
    assert_eq!(
        store.run(&r1).map(|r| r.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
        "history reader still finds the earlier run"
    );
    // Unknown run → None on both.
    assert!(
        (&store as &dyn RunStore)
            .get(&RunId("ghost".to_string()))
            .is_none()
    );
    assert!(store.run(&RunId("ghost".to_string())).is_none());
}

// ---- reads over a non-matching / empty thread -----------------------------

#[tokio::test]
async fn reads_over_a_wrong_or_empty_thread_return_empty() {
    let store = MemoryCommitCoordinator::new();
    // Empty store: no messages, no latest run.
    assert!(
        store
            .committed_messages(&ThreadId("t".to_string()))
            .is_empty()
    );
    assert!(store.latest_run(&ThreadId("t".to_string())).is_none());

    store
        .commit(commit_with(
            "t",
            "r",
            "hi",
            Phase::Ended(EndCause::NaturalEnd),
            vec![],
            vec![Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "k",
                serde_json::json!(1),
            )],
            None,
        ))
        .await
        .expect("commit");

    // Matching thread returns the transcript and committed state.
    assert_eq!(
        store.committed_messages(&ThreadId("t".to_string())).len(),
        1
    );
    assert_eq!(store.committed_state(&ThreadId("t".to_string())).len(), 1);
    assert_eq!(
        store.latest_run(&ThreadId("t".to_string())).map(|r| r.id),
        Some(RunId("r".to_string()))
    );
    // A different thread id sees nothing (no cross-thread leakage).
    assert!(
        store
            .committed_messages(&ThreadId("other".to_string()))
            .is_empty()
    );
    assert!(
        store
            .committed_state(&ThreadId("other".to_string()))
            .is_empty()
    );
    assert!(store.latest_run(&ThreadId("other".to_string())).is_none());
}

// ---- pure replay helpers --------------------------------------------------

#[tokio::test]
async fn replay_helpers_derive_from_committed_truth() {
    let store = MemoryCommitCoordinator::new();
    store
        .commit(commit_with(
            "t",
            "r",
            "s",
            Phase::Running,
            vec![],
            vec![Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "k",
                serde_json::json!("v"),
            )],
            None,
        ))
        .await
        .expect("running commit");
    store
        .commit(commit_with(
            "t",
            "r",
            "e",
            Phase::Ended(EndCause::MaxSteps),
            vec![],
            vec![],
            None,
        ))
        .await
        .expect("ended commit");

    let committed = store.committed();
    // replay_state rebuilds the materialized state store from committed commands.
    let state = replay_state(&committed);
    assert_eq!(
        state.get(
            Scope::Thread,
            &awaken_agent_contract::agent::state::Key("k".to_string())
        ),
        Some(&serde_json::json!("v"))
    );
    // replay_latest_phase returns the most-recent fact for the run.
    assert_eq!(
        replay_latest_phase(&committed, &RunId("r".to_string())),
        Some(Phase::Ended(EndCause::MaxSteps))
    );
    // An unknown run reconstructs to nothing.
    assert_eq!(
        replay_latest_phase(&committed, &RunId("ghost".to_string())),
        None
    );
}

// ---- MemoryStreamSink: order, independence from committed truth ------------

#[tokio::test]
async fn stream_sink_records_events_in_send_order() {
    let sink = MemoryStreamSink::new();
    assert!(sink.events().is_empty(), "fresh sink is empty");
    for kind in [
        AgentEvent::Fact(Fact::RunStarted),
        AgentEvent::Delta(Delta::TextDelta {
            delta: "hi".to_string(),
        }),
        AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
    ] {
        sink.send(StreamEvent {
            run_id: RunId("r".to_string()),
            kind,
        })
        .await
        .expect("send");
    }
    let events = sink.events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].kind, AgentEvent::Fact(Fact::RunStarted));
    assert_eq!(
        events[2].kind,
        AgentEvent::Fact(Fact::RunFinished { exhausted: false })
    );
}

// ---- MemoryStreamCheckpointStore: overwrite + delete idempotency ----------

fn checkpoint(run: &str, text: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: run.to_string(),
        thread_id: "t".to_string(),
        model: "m".to_string(),
        partial_text: text.to_string(),
        partial_tools: vec![PartialToolCall {
            call_id: "c".to_string(),
            tool_id: "tool".to_string(),
            raw_arguments: "{".to_string(),
        }],
    }
}

#[tokio::test]
async fn checkpoint_store_get_put_overwrite_delete() {
    let store = MemoryStreamCheckpointStore::new();
    // Empty read → None (resume nothing).
    assert!(store.get("r").await.is_none());

    store.put(checkpoint("r", "first")).await;
    assert_eq!(
        store.get("r").await.map(|c| c.partial_text),
        Some("first".to_string())
    );

    // put overwrites the prior partial for the same run_id (last write wins).
    store.put(checkpoint("r", "second")).await;
    assert_eq!(
        store.get("r").await.map(|c| c.partial_text),
        Some("second".to_string())
    );

    // A distinct run_id is kept independently.
    store.put(checkpoint("r2", "other")).await;
    assert_eq!(
        store.get("r2").await.map(|c| c.partial_text),
        Some("other".to_string())
    );

    // delete removes only the targeted key.
    store.delete("r").await;
    assert!(store.get("r").await.is_none());
    assert!(store.get("r2").await.is_some(), "unrelated key untouched");

    // delete is idempotent: deleting an absent key is a no-op, not an error.
    store.delete("r").await;
    store.delete("never-existed").await;
    assert!(store.get("r").await.is_none());
}
