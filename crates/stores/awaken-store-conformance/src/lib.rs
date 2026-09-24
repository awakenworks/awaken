//! Trait-generic store conformance suite (ADR-0039 slice 2.6).
//!
//! Every persistence backend that implements `Coordinator + CheckpointReader + CommittedThreadView`
//! must pass the same behavioural checks — commit atomicity, fact-authority
//! reads, committed-event order, and cursor paging (G1/G13). Backends call these
//! from their own test crate, each with a freshly constructed store, so the suite
//! stays backend-agnostic and the media (inmem/fs/postgres/sqlite) cannot diverge.

pub mod dream_store;
pub mod env_registry;
pub mod resource_lifecycle;
pub mod resource_registry;
pub mod session_store;
pub mod stream_checkpoint;
pub mod work_queue;
pub mod worker_directory;

use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason,
};
use awaken_agent_contract::agent::delegation::{
    DelegationId, DelegationLimits, DelegationRegistry, DelegationStatus, RequestDelegation,
};
use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{
    Command as StateCommand, Key as StateKey, MergePolicy, Scope, Store,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleEventKind, decode_run_lifecycle_cursor,
};
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_agent_contract::thread::read::transcript::{
    TranscriptError, TranscriptRange, TranscriptSliceSpec, TranscriptView,
};

const RUN_DELEGATIONS_KEY: &str = "runtime.delegations.v1";
const ACTIVE_TOOL_BATCH_KEY: &str = "runtime.active_tool_batch.v1";

fn delegation_state(registry: &DelegationRegistry) -> StateCommand {
    StateCommand::set(
        Scope::Run,
        MergePolicy::Disjoint,
        RUN_DELEGATIONS_KEY,
        serde_json::to_value(Some(registry)).expect("delegation registry serializes"),
    )
}

fn tool_batch_state(run: &RunId, phase: &str) -> StateCommand {
    StateCommand::set(
        Scope::Run,
        MergePolicy::Disjoint,
        ACTIVE_TOOL_BATCH_KEY,
        serde_json::json!({
            "run_id": run,
            "call_id": "agent-call",
            "phase": phase,
        }),
    )
}

fn load_delegation_registry(store: &Store) -> DelegationRegistry {
    serde_json::from_value::<Option<DelegationRegistry>>(
        store
            .get(Scope::Run, &StateKey(RUN_DELEGATIONS_KEY.to_string()))
            .cloned()
            .expect("delegation registry cell is present"),
    )
    .expect("delegation registry shape")
    .expect("delegation registry value is present")
}

fn checkpoint(thread: &ThreadId, run: &RunId, text: &str, state: RunState) -> ThreadCommit {
    let disposition = match state {
        RunState::Running => RunDisposition::running(run.clone()),
        RunState::Ended(cause) => RunDisposition::ended(run.clone(), cause),
        RunState::Awaiting => panic!("awaiting checkpoints must be constructed with a ticket"),
    };
    // The production assembler supplies a typed lifecycle fact, rather than
    // a text-only placeholder that opaque persistence cannot validate.
    ThreadCommit::assemble(
        thread.clone(),
        disposition,
        true,
        vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        Vec::new(),
        Vec::new(),
    )
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
    ResumeTicket::new(
        "conf-corr",
        run.clone(),
        thread.clone(),
        "conf-snap",
        "conf-fp",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "conf-call".into(),
            tool: PendingTool {
                tool_id: "conf-tool".into(),
                arguments: serde_json::json!({}),
            },
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        },
    )
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
pub async fn commit_then_read<S: Coordinator + CheckpointReader + CommittedThreadView>(store: &S) {
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
        store
            .list_events(&EventScope::Run(run), None, 10)
            .await
            .expect("events")
            .len(),
        1,
        "one event"
    );
}

/// Committed events are returned in commit order and can be paged by cursor.
pub async fn events_ordered_and_paged<S: Coordinator + CheckpointReader + CommittedThreadView>(
    store: &S,
) {
    let thread = ThreadId("conf-events".to_string());
    let run = RunId("conf-events-r".to_string());
    let other_thread = ThreadId("conf-events-other".to_string());
    let other_run = RunId("conf-events-other-r".to_string());
    // A run's two events come from a mid-flight `Running` step and then the
    // terminal `Ended` commit — the order a real run produces them. (Committing
    // two terminal facts for one run would trip the terminal-is-final guard, which
    // fences a stale owner's duplicate post-terminal commit; a run ends once.)
    store
        .commit(running_checkpoint(&thread, &run, "one"))
        .await
        .expect("commit 1");
    store
        .commit(running_checkpoint(&other_thread, &other_run, "interleaved"))
        .await
        .expect("interleaved commit");
    store
        .commit(ended_checkpoint(&thread, &run, "two"))
        .await
        .expect("commit 2");

    let scope = EventScope::Run(run);
    let all = store.list_events(&scope, None, 10).await.expect("events");
    assert_eq!(all.len(), 2, "both events");
    assert!(all[0].sequence < all[1].sequence, "commit order");
    let partition = store
        .list_events(&EventScope::All, None, 10)
        .await
        .expect("events");
    assert_eq!(partition.len(), 3, "all interleaved partition events");
    assert!(
        partition
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence),
        "the partition-wide scope preserves durable order across Threads"
    );

    // Cursor paging: first page of one, then the remainder after that cursor.
    let first = store.list_events(&scope, None, 1).await.expect("events");
    assert_eq!(first.len(), 1, "first page");
    let rest = store
        .list_events(&scope, Some(first[0].sequence), 10)
        .await
        .expect("events");
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
pub async fn terminal_run_is_fenced<S: Coordinator + CheckpointReader + CommittedThreadView>(
    store: &S,
) {
    // Cause/effect graph: C1=one terminal commit is durable; C2=a complete,
    // separately identifiable awaiting shape for the same Run is valid in
    // isolation; C3=C2 is submitted after C1. Effects: E1=C1 remains the exact
    // message/run/event prefix; E2=C3 rejects; E3=no rejected message, state,
    // event, Run/latest projection, or resume ticket becomes visible.
    // Constraint: C3 is rejected by the durable terminal transition, not by
    // ThreadCommit shape validation. Decision table: R1=C1=>E1;
    // R2=C1+C2+C3=>E1+E2+E3. Every backend executes these same rules.
    let thread = ThreadId("conf-fence".to_string());
    let run = RunId("conf-fence-r".to_string());
    store
        .commit(ended_checkpoint(&thread, &run, "final"))
        .await
        .expect("first terminal commit lands");

    let baseline_messages = store.committed_messages(&thread);
    let baseline_state = store.committed_state(&thread);
    let baseline_events = store
        .list_events(&EventScope::Run(run.clone()), None, 100)
        .await
        .expect("events");
    let baseline_run = store.run(&run);
    let baseline_latest = store.latest_run(&thread);
    assert!(store.resume_ticket(&run).is_none(), "R1/E1");

    let rejected = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::awaiting(ticket(&thread, &run)),
        true,
        vec![Message::text(
            MsgId("m-rejected-shape".into()),
            Role::Assistant,
            "rejected-shape",
        )],
        vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "rejected.shape",
            serde_json::json!({"must": "stutter"}),
        )],
        vec![Draft {
            kind: EventKind::PermissionDecided,
            payload: serde_json::json!({"candidate": "rejected-shape"}),
        }],
    );
    rejected
        .validate()
        .expect("R2/C2 is a valid complete ThreadCommit shape");
    assert!(store.commit(rejected).await.is_err(), "R2/E2");

    assert_eq!(
        store.committed_messages(&thread),
        baseline_messages,
        "R2/E3 message region stutters"
    );
    assert_eq!(
        store.committed_state(&thread),
        baseline_state,
        "R2/E3 state region stutters"
    );
    assert_eq!(
        store
            .list_events(&EventScope::Run(run.clone()), None, 100)
            .await
            .expect("events"),
        baseline_events,
        "R2/E3 event region stutters"
    );
    assert_eq!(store.run(&run), baseline_run, "R2/E3 Run fact stutters");
    assert_eq!(
        store.latest_run(&thread),
        baseline_latest,
        "R2/E3 latest Run stutters"
    );
    assert!(
        store.resume_ticket(&run).is_none(),
        "R2/E3 awaiting ticket is absent"
    );
}

/// Successive commits accumulate transcript and retain historical Run identity;
/// `latest_run` independently tracks the Thread head.
pub async fn commits_accumulate<S: Coordinator + CheckpointReader + CommittedThreadView>(
    store: &S,
) {
    // Cause/effect graph: C1 the first Run is committed; C2 a distinct second
    // Run is committed on the same Thread; C3 an unknown Run is queried.
    // Effects: E1 transcript contains both commits in order; E2 both known Runs
    // remain addressable; E3 latest_run selects the second Run; E4 unknown Run
    // stays absent. Constraint: C2 follows C1. Decision table: R1 C1&&!C2 ->
    // first is historical+latest; R2 C1&&C2 -> E1+E2+E3; R3 C3 -> E4. Every
    // backend executes these rules through this canonical conformance case.
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
        Some(run2.clone()),
        "latest run wins"
    );
    assert_eq!(
        store.run(&run1).map(|record| record.id),
        Some(run1),
        "the first Run remains addressable after the Thread head advances"
    );
    assert_eq!(
        store.run(&run2).map(|record| record.id),
        Some(run2),
        "the latest Run is addressable through the same view"
    );
    assert!(
        store.run(&RunId("conf-acc-missing".to_string())).is_none(),
        "an unknown Run remains absent"
    );
}

/// Transcript snapshots freeze an append-only prefix. A later commit may extend
/// the Thread, but the old reference still reconstructs byte-identical messages;
/// a changed identity fails closed. This is the neutral window substrate used by
/// Memory, Compact, Outcome, and inference without teaching the store those
/// business consumers.
pub async fn transcript_snapshots_freeze_append_only_prefix<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
    let thread = ThreadId("conf-transcript-snapshot".to_string());
    let first_run = RunId("conf-transcript-snapshot-r1".to_string());
    let second_run = RunId("conf-transcript-snapshot-r2".to_string());

    store
        .commit(ended_checkpoint(&thread, &first_run, "first"))
        .await
        .expect("first commit");
    let frozen = store.transcript_snapshot(&thread, TranscriptView::RawCommitted);
    assert_eq!(frozen.reference().version, 1);
    assert_eq!(frozen.reference().end_seq, 1);

    store
        .commit(ended_checkpoint(&thread, &second_run, "second"))
        .await
        .expect("second commit");
    let latest = store.transcript_snapshot(&thread, TranscriptView::RawCommitted);
    assert_eq!(latest.reference().version, 2);

    let old_slice = store
        .transcript_slice(&TranscriptSliceSpec {
            snapshot: frozen.reference().clone(),
            ranges: vec![TranscriptRange::new(0, 1)],
        })
        .expect("old append-only prefix remains readable");
    assert_eq!(old_slice.messages.len(), 1);
    assert_eq!(old_slice.messages[0].text_content(), "first");

    let mut tampered = frozen.reference().clone();
    tampered.version += 1;
    let error = store
        .transcript_slice(&TranscriptSliceSpec {
            snapshot: tampered,
            ranges: vec![TranscriptRange::new(0, 1)],
        })
        .expect_err("a forged snapshot identity fails closed");
    assert_eq!(error, TranscriptError::SnapshotMismatch);
}

/// Awaiting-ticket lifecycle: a `Awaiting` checkpoint with a correlated ticket awaits
/// the run (the ticket becomes readable), and the next non-awaiting commit
/// (resume/terminal) clears it so a stale resume against an old correlation finds
/// nothing and fails closed (G5). Every backend awaits/clears the ticket atomically
/// with the checkpoint, so a backend that leaked a cleared ticket — or dropped a
/// live one — diverges here.
pub async fn resume_ticket_awaits_then_clears<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
    let thread = ThreadId("conf-wait".to_string());
    let run = RunId("conf-wait-r".to_string());

    let expected = ticket(&thread, &run);
    store
        .commit(awaiting_checkpoint(&thread, &run))
        .await
        .expect("await");
    assert_eq!(
        store.resume_ticket(&run),
        Some(expected),
        "the complete closed tool target survives the storage boundary"
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

/// Thread-level wait selection is one atomic committed-view query. Historical
/// tickets remain addressable by Run for replay, but only the latest Awaiting
/// Run may be selected for new thread-addressed input.
pub async fn open_wait_selects_only_the_latest_run<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
    /* Cause/effect graph and decision table:
     * C1 an old Run is Awaiting with ticket T1; C2 a newer Run exists; C3 the
     * newer Run is terminal; C4 the newer Run is Awaiting with ticket T2.
     * E1 no open wait; E2 select exactly T1; E3 select exactly T2.
     * Constraints: C3 and C4 are mutually exclusive; historical T1 may remain
     * readable by Run but cannot determine the Thread head.
     * | Rule | C1 | C2 | C3 | C4 | Effect |
     * | R1   | 0  | 0  | 0  | 0  | E1     |
     * | R2   | 1  | 0  | 0  | 0  | E2     |
     * | R3   | 1  | 1  | 1  | 0  | E1     |
     * | R4   | 1  | 1  | 0  | 1  | E3     |
     */
    let thread = ThreadId("conf-open-wait".to_string());
    let old_run = RunId("conf-open-wait-old".to_string());
    let old_ticket = ticket(&thread, &old_run);
    assert_eq!(store.open_wait_for_thread(&thread), None, "R1/E1");

    store
        .commit(awaiting_checkpoint(&thread, &old_run))
        .await
        .expect("R2 old wait");
    assert_eq!(
        store.open_wait_for_thread(&thread),
        Some((old_run.clone(), old_ticket)),
        "R2/E2"
    );

    let terminal_run = RunId("conf-open-wait-terminal".to_string());
    store
        .commit(ended_checkpoint(&thread, &terminal_run, "terminal"))
        .await
        .expect("R3 newer terminal");
    assert!(
        store.resume_ticket(&old_run).is_some(),
        "R3 retains historical Run-addressed ticket"
    );
    assert_eq!(store.open_wait_for_thread(&thread), None, "R3/E1");

    let latest_run = RunId("conf-open-wait-latest".to_string());
    let latest_ticket = ticket(&thread, &latest_run);
    store
        .commit(awaiting_checkpoint(&thread, &latest_run))
        .await
        .expect("R4 latest wait");
    assert_eq!(
        store.open_wait_for_thread(&thread),
        Some((latest_run, latest_ticket)),
        "R4/E3"
    );
}

/// Concurrent append: two commits for distinct runs on one thread, issued
/// concurrently, both survive with distinct AND dense (consecutive) sequences —
/// neither lost the other's transcript and neither collided on the commit
/// sequence. The two commits are polled concurrently (`join!`), so a backend that
/// allocated the sequence non-atomically, or dropped an interleaved append, would
/// diverge here. (Postgres additionally proves the cross-process race with its own
/// dedicated test; this is the portable single-store invariant.)
pub async fn concurrent_appends_are_dense_and_distinct<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
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
pub async fn two_threads_in_one_store_are_isolated<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
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

/// A recovery read returns exactly one committed Thread prefix. Its per-Thread
/// version excludes unrelated commits while the backend cursor includes them,
/// and the claimed Run ordinal counts only that Run's commits.
pub async fn recovery_snapshot_is_consistent<S: Coordinator + RunRecoverySource>(store: &S) {
    // Cause/effect graph: C1=commit one valid awaiting shape containing its Run
    // fact/ticket, message, Run-scoped state and generated+extra events; C2=a
    // second commit belongs to the same Thread; C3=an unrelated Thread commits
    // later. Effects: E1=every C1 region appears exactly once and its message,
    // state and events carry C1's one commit cursor; E2=the snapshot includes C2
    // but excludes C3; E3=Thread version excludes C3 while store cursor includes
    // it; E4=every event lies within that same snapshot fence. Constraints: the
    // Run ticket and Run-scoped state are bound by ThreadCommit::assemble, and
    // RunRecoverySource reads one backend consistency boundary. Decision table:
    // R1=C1=>E1; R2=C1+C2=>E1+E2; R3=C1+C2+C3=>E1+E2+E3+E4. This generic table
    // runs against memory, filesystem, SQLite, and PostgreSQL.
    let thread = ThreadId("conf-recovery".to_string());
    let awaiting_run = RunId("conf-recovery-awaiting".to_string());
    let claimed_run = RunId("conf-recovery-claimed".to_string());
    let other_thread = ThreadId("conf-recovery-other".to_string());
    let other_run = RunId("conf-recovery-other-run".to_string());

    let shape = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::awaiting(ticket(&thread, &awaiting_run)),
        true,
        vec![Message::text(
            MsgId("m-complete-shape".into()),
            Role::Assistant,
            "complete-shape",
        )],
        vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "complete.shape",
            serde_json::json!({"atomic": true}),
        )],
        vec![Draft {
            kind: EventKind::PermissionDecided,
            payload: serde_json::json!({"shape": "complete"}),
        }],
    );
    let shape_message = shape.messages[0].clone();
    let shape_state = shape.state.clone();
    let shape_events = shape.events.clone();
    let shape_ticket = shape
        .resume_ticket()
        .expect("complete awaiting shape owns its ticket")
        .clone();
    let shape_record = store.commit(shape).await.expect("awaiting commit");
    let claimed_record = store
        .commit(running_checkpoint(&thread, &claimed_run, "claimed"))
        .await
        .expect("claimed run commit");
    store
        .commit(running_checkpoint(&other_thread, &other_run, "other"))
        .await
        .expect("other thread commit");

    let snapshot = store
        .recovery_snapshot(&thread, &claimed_run)
        .await
        .expect("recovery snapshot");
    assert_eq!(snapshot.thread_id, thread);
    assert_eq!(snapshot.claimed_run_id, claimed_run);
    assert_eq!(snapshot.runs.len(), 2, "latest record for both Thread Runs");
    assert_eq!(
        snapshot
            .runs
            .iter()
            .find(|record| record.id == awaiting_run)
            .map(|record| record.state.clone()),
        Some(RunState::Awaiting),
        "R1/E1 awaiting Run fact"
    );
    assert_eq!(
        snapshot.messages,
        vec![
            shape_message,
            Message::text(MsgId("m-claimed".into()), Role::Assistant, "claimed")
        ],
        "R2/E2 exact local messages; foreign transcript excluded"
    );
    assert_eq!(
        snapshot.message_commit_cursors,
        vec![shape_record.sequence, claimed_record.sequence],
        "R1/E1 and R2/E2 message cursors bind their source commits"
    );
    assert_eq!(snapshot.state, shape_state, "R1/E1 exact state region");
    assert_eq!(
        snapshot.state_commit_cursors,
        vec![shape_record.sequence],
        "R1/E1 state shares the shape commit cursor"
    );
    assert_eq!(
        snapshot.events.len(),
        shape_events.len() + 1,
        "R2/E2 exact local events; foreign audit excluded"
    );
    for (offset, (record, draft)) in snapshot.events.iter().zip(shape_events.iter()).enumerate() {
        assert_eq!(record.run_id, awaiting_run, "R1/E1 shape event owner");
        assert_eq!(record.kind, draft.kind, "R1/E1 shape event kind");
        assert_eq!(record.payload, draft.payload, "R1/E1 shape event payload");
        let (source_commit, source_offset) =
            decode_run_lifecycle_cursor(RunLifecycleCursor(record.sequence));
        assert_eq!(source_commit, shape_record.sequence, "R1/E1 event cursor");
        assert_eq!(source_offset, offset, "R1/E1 event offset");
    }
    let claimed_event = snapshot.events.last().expect("R2 claimed event");
    assert_eq!(claimed_event.run_id, claimed_run, "R2/E2");
    let (claimed_source, claimed_offset) =
        decode_run_lifecycle_cursor(RunLifecycleCursor(claimed_event.sequence));
    assert_eq!(claimed_source, claimed_record.sequence, "R2/E2");
    assert_eq!(claimed_offset, 0, "R2/E2");
    assert!(
        snapshot.events.iter().all(|event| {
            decode_run_lifecycle_cursor(RunLifecycleCursor(event.sequence)).0
                <= snapshot.store_cursor
        }),
        "R3/E4"
    );
    assert_eq!(snapshot.resume_tickets.len(), 1, "R1/E1 active ticket");
    assert_eq!(snapshot.resume_tickets[0].run_id, awaiting_run);
    assert_eq!(snapshot.resume_tickets[0].ticket, shape_ticket, "R1/E1");
    assert_eq!(snapshot.thread_version, 2, "R3/E3 per-Thread version");
    assert_eq!(snapshot.store_cursor, 3, "R3/E3 backend-wide prefix cursor");
    assert_eq!(snapshot.next_commit_ordinal, 1, "claimed Run ordinal");
    // L1 the atomic prefix contains two local lifecycle facts and excludes the
    // later foreign Thread. L2 an exclusive page must still use pre-cursor
    // history for classification. Every physical backend executes these same
    // assertions; this is not a second lifecycle reader or fixture projection.
    let lifecycle = snapshot
        .lifecycle_events_after(RunLifecycleCursor::default(), usize::MAX)
        .unwrap();
    assert_eq!(
        lifecycle
            .events
            .iter()
            .map(|event| (event.run_id.clone(), event.kind, event.source_commit_cursor,))
            .collect::<Vec<_>>(),
        vec![
            (
                awaiting_run.clone(),
                RunLifecycleEventKind::Awaiting,
                shape_record.sequence
            ),
            (
                claimed_run.clone(),
                RunLifecycleEventKind::Running,
                claimed_record.sequence
            ),
        ],
        "L1 scoped facts and exact coordinates"
    );
    assert!(
        lifecycle
            .events
            .iter()
            .all(|event| event.thread_id == thread),
        "L1 owner"
    );
    // T1: two local commits + one foreign commit => exactly the local time
    // observations, within the snapshot fence; new writes never use a fixture date.
    assert_eq!(
        snapshot
            .commit_timestamps
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        snapshot
            .commit_timestamps
            .values()
            .all(|at| *at > 1_767_225_600_000)
    );
    // R4 an earlier Run commits again after a distinct Run: latest follows the
    // last COMMIT while the records retain first appearance order. R5 removing
    // that Run's wait changes only its ticket; the exact claimed Run and its
    // ordinal stay unchanged. This exercises the same shared recovery contract
    // in memory/fs and both SQL adapters, not a backend-specific order oracle.
    store
        .commit(running_checkpoint(
            &thread,
            &awaiting_run,
            "resumed-earlier",
        ))
        .await
        .expect("earlier Run resumes");
    let updated = store
        .recovery_snapshot(&thread, &claimed_run)
        .await
        .expect("R4 snapshot");
    assert_eq!(
        updated
            .runs
            .iter()
            .map(|run| run.id.clone())
            .collect::<Vec<_>>(),
        vec![awaiting_run.clone(), claimed_run.clone()],
        "R4 first appearance order"
    );
    assert_eq!(
        updated.latest_run_id,
        Some(awaiting_run.clone()),
        "R4 last commit owner"
    );
    assert!(
        updated
            .runs
            .iter()
            .all(|run| run.state == RunState::Running),
        "R4 latest states"
    );
    assert!(
        updated.resume_tickets.is_empty(),
        "R5 prior wait is consumed"
    );
    assert_eq!(
        updated.claimed_run_id, claimed_run,
        "R5 exact claim stays fixed"
    );
    assert_eq!(
        updated.next_commit_ordinal, 1,
        "R5 ordinal belongs to claimed Run"
    );
    assert_eq!(updated.thread_version, 3);
    assert_eq!(updated.store_cursor, 4);
    let resumed = updated
        .lifecycle_events_after(lifecycle.next_cursor, usize::MAX)
        .unwrap();
    assert_eq!(resumed.events.len(), 1, "L2 no foreign or replayed fact");
    assert_eq!(
        resumed.events[0].run_id, awaiting_run,
        "L2 exact earlier Run"
    );
    assert_eq!(
        resumed.events[0].kind,
        RunLifecycleEventKind::Resumed,
        "L2 retained Awaiting predecessor"
    );
    assert_eq!(resumed.events[0].source_commit_cursor, 4, "L2 source fence");
}

/// Operation receipts make an ambiguous acknowledgement retry exactly-once at
/// the committed-fact boundary, while Thread CAS rejects stale recovery context.
pub async fn commit_operation_is_idempotent_and_cas<
    S: OperationCoordinator + CheckpointReader + CommittedThreadView + RunRecoverySource,
>(
    store: &S,
) {
    let thread = ThreadId("conf-operation".to_string());
    let run = RunId("conf-operation-run".to_string());
    let first = CommitOperation {
        operation_id: CommitOperationId::new(run.clone(), 0),
        expected_thread_version: 0,
        payload_hash: CommitPayloadHash("sha256:first".to_string()),
        commit: running_checkpoint(&thread, &run, "first"),
    };
    let applied = store
        .commit_operation(first.clone())
        .await
        .expect("first operation");
    assert!(!applied.duplicate);
    assert_eq!(applied.thread_version, 1);

    // T2 exact receipt replay after clock advances => original observation;
    // T3 conflicting payload / stale CAS => no new observation or partial fact.
    let original_times = store
        .recovery_snapshot(&thread, &run)
        .await
        .unwrap()
        .commit_timestamps;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let duplicate = store
        .commit_operation(first.clone())
        .await
        .expect("same operation retry");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.commit_sequence, applied.commit_sequence);
    assert_eq!(duplicate.thread_version, applied.thread_version);
    assert_eq!(
        store.committed_messages(&thread).len(),
        1,
        "a duplicate receipt does not append facts"
    );

    let mut hash_conflict = first;
    hash_conflict.payload_hash = CommitPayloadHash("sha256:different".to_string());
    assert!(
        store.commit_operation(hash_conflict).await.is_err(),
        "one operation id cannot identify two payloads"
    );

    let stale = CommitOperation {
        operation_id: CommitOperationId::new(run.clone(), 1),
        expected_thread_version: 0,
        payload_hash: CommitPayloadHash("sha256:second".to_string()),
        commit: ended_checkpoint(&thread, &run, "second"),
    };
    assert!(
        store.commit_operation(stale.clone()).await.is_err(),
        "stale Thread context is rejected"
    );
    assert_eq!(
        store.committed_messages(&thread).len(),
        1,
        "CAS rejection is atomic"
    );

    assert_eq!(
        store
            .recovery_snapshot(&thread, &run)
            .await
            .unwrap()
            .commit_timestamps,
        original_times,
        "T2+T3"
    );
    let mut current = stale;
    current.expected_thread_version = 1;
    let second = store
        .commit_operation(current)
        .await
        .expect("current Thread version");
    assert_eq!(second.thread_version, 2);
    assert_eq!(store.committed_messages(&thread).len(), 2);

    let snapshot = store
        .recovery_snapshot(&thread, &run)
        .await
        .expect("post-operation recovery");
    assert_eq!(snapshot.thread_version, 2);
    assert_eq!(snapshot.next_commit_ordinal, 2);
    assert_eq!(snapshot.commit_timestamps.len(), 2);
    assert_eq!(
        snapshot.commit_timestamps.get(&applied.commit_sequence),
        original_times.get(&applied.commit_sequence)
    );
}

/// Two operations prepared from the same Thread prefix race: the Thread CAS
/// admits exactly one, even when they target different Runs.
pub async fn concurrent_operations_cas_one_winner<
    S: OperationCoordinator + CheckpointReader + CommittedThreadView + RunRecoverySource + Sync,
>(
    store: &S,
) {
    let thread = ThreadId("conf-operation-race".to_string());
    let operation = |run: &str| {
        let run_id = RunId(run.to_string());
        CommitOperation {
            operation_id: CommitOperationId::new(run_id.clone(), 0),
            expected_thread_version: 0,
            payload_hash: CommitPayloadHash(format!("sha256:{run}")),
            commit: running_checkpoint(&thread, &run_id, run),
        }
    };
    let (left, right) = tokio::join!(
        store.commit_operation(operation("left")),
        store.commit_operation(operation("right")),
    );
    assert_eq!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()),
        1,
        "one stale-prefix operation wins and one conflicts"
    );
    assert_eq!(
        store.committed_messages(&thread).len(),
        1,
        "the losing CAS appends no partial facts"
    );
    // T10 the failed writer leaves no observation. Read the same fenced
    // snapshot contract on all backends, not a separately cached timestamp map.
    let snapshot = store
        .recovery_snapshot(&thread, &RunId("left".into()))
        .await
        .unwrap();
    assert_eq!(snapshot.thread_version, 1);
    assert_eq!(snapshot.commit_timestamps.len(), 1, "T10 winner time only");
}

/// Empty-store reads: before any commit, every read port is absent — no messages,
/// no run record, no latest run, no events (by run or thread scope), no awaiting
/// ticket, no committed state. A backend that pre-materialized a partial or
/// default row would diverge here (G13).
pub async fn empty_store_reads_are_absent<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
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
            .await
            .expect("events")
            .is_empty(),
        "no run-scoped events"
    );
    assert!(
        store
            .list_events(&EventScope::Thread(thread.clone()), None, 10)
            .await
            .expect("events")
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
pub async fn committed_state_replays<S: Coordinator + CheckpointReader + CommittedThreadView>(
    store: &S,
) {
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

/// Delegation relationship state and the initiating tool call are one atomic
/// ThreadCommit state transition. This is the persistence-level refinement
/// check for the production design: no independent delegation repository may
/// advance (or survive a fenced terminal retry) apart from the ToolBatch cell.
pub async fn delegation_and_tool_state_commit_atomically<
    S: Coordinator + CheckpointReader + CommittedThreadView,
>(
    store: &S,
) {
    let thread = ThreadId("conf-delegation-atomic".to_string());
    let run = RunId("conf-delegation-parent".to_string());
    let child = RunId("conf-delegation-child".to_string());
    let delegation_id = DelegationId::for_parent_call(&run, "agent-call");

    let mut registry = DelegationRegistry::new(
        run.clone(),
        "coordinator",
        Vec::new(),
        0,
        DelegationLimits::new(3, 2, 4),
    );
    registry
        .request(RequestDelegation {
            id: delegation_id.clone(),
            parent_call_id: "agent-call".to_string(),
            target_agent_id: "researcher".to_string(),
            recursive_self: false,
            child_run_id: child,
        })
        .expect("register child");

    let start = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::running(run.clone()),
        true,
        Vec::new(),
        vec![
            delegation_state(&registry),
            tool_batch_state(&run, "executing"),
        ],
        Vec::new(),
    );
    store.commit(start).await.expect("commit start");

    let durable = store.committed_state(&thread);
    assert_eq!(durable.len(), 2, "both cells land in the same commit");
    assert!(
        durable
            .iter()
            .all(|command| command.run_id.as_ref() == Some(&run)),
        "every Run-scoped cell is bound to the parent Run"
    );
    let materialized = Store::rebuild(&durable);
    let mut registry = load_delegation_registry(&materialized);
    assert_eq!(
        registry.get(&delegation_id).map(|entry| entry.status),
        Some(DelegationStatus::Open)
    );
    assert_eq!(
        materialized
            .get(Scope::Run, &StateKey(ACTIVE_TOOL_BATCH_KEY.to_string()))
            .and_then(|value| value.get("phase")),
        Some(&serde_json::json!("executing"))
    );

    registry.end_parent();
    let terminal = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::ended(run.clone(), EndCause::Cancelled),
        true,
        Vec::new(),
        vec![
            delegation_state(&registry),
            tool_batch_state(&run, "indeterminate"),
        ],
        Vec::new(),
    );
    store
        .commit(terminal.clone())
        .await
        .expect("commit terminal");

    let terminal_state = store.committed_state(&thread);
    assert_eq!(
        terminal_state.len(),
        4,
        "two complete atomic state revisions"
    );
    let materialized = Store::rebuild(&terminal_state);
    let registry = load_delegation_registry(&materialized);
    assert_eq!(
        registry.get(&delegation_id).map(|entry| entry.status),
        Some(DelegationStatus::CancelRequested)
    );
    assert_eq!(
        materialized
            .get(Scope::Run, &StateKey(ACTIVE_TOOL_BATCH_KEY.to_string()))
            .and_then(|value| value.get("phase")),
        Some(&serde_json::json!("indeterminate"))
    );

    assert!(
        store.commit(terminal).await.is_err(),
        "a stale post-terminal retry is fenced"
    );
    assert_eq!(
        store.committed_state(&thread),
        terminal_state,
        "the fenced retry writes neither cell"
    );
}
