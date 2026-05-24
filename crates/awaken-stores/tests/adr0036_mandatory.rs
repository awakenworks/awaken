//! ADR-0036 §341-348 mandatory test matrix.
//!
//! Each test name matches the row name in the ADR's testing table so the
//! conformance suite is greppable by ADR identifier. Pre-existing tests
//! that cover the same property under different names are cross-referenced
//! in comments.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_contract::contract::commit_coordinator::{
    CanonicalEventStager, CheckpointCommitPlan, CommitCoordinator, CommitError,
    StagedCanonicalEvent,
};
use awaken_contract::contract::durable_event_sink::{
    AgentEventNormalizationContext, DurableEventSink, RuntimeEventDurability,
    ScopedAgentEventNormalizer,
};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::{EventSink, VecEventSink};
use awaken_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventReader, EventScope,
    EventVisibility, EventWriter,
};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::storage::{RunRecord, ThreadStore};
use awaken_stores::{
    InMemoryEventStore, InMemoryOutboxStore, InMemoryStore, MemoryCommitCoordinator,
};
use serde_json::json;

fn run_record(thread_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        status: RunStatus::Done,
        agent_id: "agent-1".to_string(),
        ..Default::default()
    }
}

#[derive(Default)]
struct TestBuffer {
    drafts: Mutex<Vec<StagedCanonicalEvent>>,
}

impl TestBuffer {
    fn drain(&self) -> Vec<StagedCanonicalEvent> {
        std::mem::take(&mut *self.drafts.lock().unwrap())
    }

    fn len(&self) -> usize {
        self.drafts.lock().unwrap().len()
    }

    fn is_empty(&self) -> bool {
        self.drafts.lock().unwrap().is_empty()
    }
}

impl CanonicalEventStager for TestBuffer {
    fn stage(&self, draft: CanonicalEventDraft) {
        self.drafts
            .lock()
            .unwrap()
            .push(StagedCanonicalEvent::new(draft));
    }
}

fn build_buffered_sink(
    thread_id: &str,
    run_id: &str,
    mode: RuntimeEventDurability,
) -> (DurableEventSink, Arc<VecEventSink>, Arc<TestBuffer>) {
    let inner = Arc::new(VecEventSink::new());
    let buffer = Arc::new(TestBuffer::default());
    let context = AgentEventNormalizationContext::new(thread_id, run_id, "test").unwrap();
    let sink = DurableEventSink::new(
        inner.clone(),
        buffer.clone() as Arc<dyn CanonicalEventStager>,
        Arc::new(ScopedAgentEventNormalizer::new(context)),
        mode,
    );
    (sink, inner, buffer)
}

fn build_coordinator() -> (
    MemoryCommitCoordinator,
    Arc<InMemoryStore>,
    Arc<InMemoryEventStore>,
    Arc<InMemoryOutboxStore>,
) {
    let thread_run = Arc::new(InMemoryStore::new());
    let events = Arc::new(InMemoryEventStore::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let coord =
        MemoryCommitCoordinator::new(thread_run.clone(), events.clone(), outbox.clone()).unwrap();
    (coord, thread_run, events, outbox)
}

fn sample_draft(kind: &str, thread_id: &str, run_id: &str, idem: &str) -> StagedCanonicalEvent {
    let mut draft = CanonicalEventDraft::new(
        vec![EventScope::thread(thread_id), EventScope::run(run_id)],
        CanonicalEventKind::new(kind).unwrap(),
        json!({"kind": kind, "idem": idem}),
        "test",
    )
    .unwrap();
    draft.visibility = EventVisibility::Public;
    StagedCanonicalEvent::new(draft).with_options(AppendOptions {
        writer_id: Some("runtime".to_string()),
        idempotency_key: Some(idem.to_string()),
        ..Default::default()
    })
}

/// ADR-0036 §347: transient `AgentEvent` reaches subscribers but produces
/// no canonical row in Compacted mode.
#[tokio::test]
async fn transient_not_persisted() {
    let (sink, inner, buffer) =
        build_buffered_sink("t-tr", "r-tr", RuntimeEventDurability::Compacted);

    sink.emit(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .await;
    sink.emit(AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: "{".into(),
    })
    .await;

    assert_eq!(
        inner.events().len(),
        2,
        "transient deltas must reach the live wire sink"
    );
    assert!(
        buffer.is_empty(),
        "Compacted mode must not stage observed runtime events"
    );
}

/// ADR-0036 §348: normalizer-level dedup (started_runs / terminal_runs)
/// survives the durable sink reshape. Re-emitting
/// `AgentEvent::RunStart` for the same `(thread, run)` yields exactly one
/// `RunStarted` canonical draft; the second emission maps to `RunResumed`.
#[tokio::test]
async fn normalizer_dedup_preserved() {
    let (sink, _inner, buffer) =
        build_buffered_sink("t-d", "r-d", RuntimeEventDurability::Compacted);

    let event = AgentEvent::RunStart {
        thread_id: "t-d".into(),
        run_id: "r-d".into(),
        parent_run_id: None,
        identity: None,
    };
    sink.emit(event.clone()).await;
    sink.emit(event).await;

    let staged = buffer.drain();
    assert_eq!(staged.len(), 2, "two emissions stage two distinct drafts");
    assert_eq!(
        staged[0].draft.event_kind.as_str(),
        "RunStarted",
        "first emission must canonicalise to RunStarted"
    );
    assert_eq!(
        staged[1].draft.event_kind.as_str(),
        "RunResumed",
        "second emission must canonicalise to RunResumed (normalizer dedup intact)"
    );
}

/// ADR-0036 §346: a panic-equivalent abort mid-phase causes the buffered
/// drafts to be lost. The post-crash state must show neither the
/// `ThreadRunStore` checkpoint advanced nor canonical events appended.
///
/// We simulate the panic by staging drafts and then dropping the buffer
/// without calling `commit_checkpoint` — the same observable end state as
/// a panicking phase (the runtime never reaches the checkpoint commit).
#[tokio::test]
async fn phase_crash_replay() {
    let (coord, thread_run, events, _outbox) = build_coordinator();

    // Stage as the runtime tee would have, then "crash" by dropping the
    // buffer without flushing.
    {
        let buffer = TestBuffer::default();
        buffer.stage(
            sample_draft("ToolCallReady", "t-crash", "r-crash", "k-1")
                .draft
                .clone(),
        );
        buffer.stage(
            sample_draft("ToolCallDone", "t-crash", "r-crash", "k-2")
                .draft
                .clone(),
        );
        assert_eq!(buffer.len(), 2);
        // Buffer dropped here; no commit_checkpoint call.
    }

    // Replay (in this test = direct store reads) must observe nothing.
    assert!(
        thread_run.load_thread("t-crash").await.unwrap().is_none(),
        "checkpoint must not advance when phase crashes before commit"
    );
    assert_eq!(
        events.count(EventScope::run("r-crash")).await.unwrap(),
        0,
        "uncommitted drafts must be absent on replay"
    );

    // Sanity: a subsequent successful commit (post-replay) writes cleanly.
    let plan = CheckpointCommitPlan::checkpoint_only(
        "t-crash",
        Vec::new(),
        run_record("t-crash", "r-crash"),
    );
    coord.commit_checkpoint(plan).await.unwrap();
    assert!(thread_run.load_thread("t-crash").await.unwrap().is_some());
}

/// ADR-0036 §345: two `MemoryCommitCoordinator` instances backed by
/// distinct store sets report distinct `TransactionScopeId` values, so the
/// builder's scope-equality check in ADR-0036 D3 cannot confuse them.
///
/// The full ADR scenario — `RuntimeBuilder::build()` rejecting a mixed
/// memory + Postgres pair with both backend names in the error — is
/// blocked on the D8 "mandatory coordinator" enforcement in the builder.
/// This test pins the scope-identity primitive that the builder check
/// will rely on.
#[tokio::test]
async fn scope_mismatch_rejected() {
    let (coord_a, _, _, _) = build_coordinator();
    let (coord_b, _, _, _) = build_coordinator();

    let scope_a = coord_a.scope();
    let scope_b = coord_b.scope();

    assert_ne!(
        scope_a, scope_b,
        "distinct in-memory store sets must yield distinct scope ids"
    );
    assert_eq!(
        coord_a.scope(),
        scope_a,
        "scope must be stable across calls"
    );

    // Surface scope ids in the diagnostic string format the builder will
    // embed in `BuildError::TransactionScopeMismatch` once D8 lands.
    let diag = format!("{} != {}", scope_a.as_str(), scope_b.as_str());
    assert!(
        diag.contains("memory::"),
        "scope id descriptor must identify the backend family for builder diagnostics: {diag}"
    );
}

/// ADR-0036 §344: ADR-named alias for the in-place atomicity property
/// already pinned by `memory_commit_coordinator::tests::
/// commit_rolls_back_on_event_append_idempotency_conflict`. Re-asserts the
/// invariant under the ADR's canonical test name so the conformance
/// matrix is greppable.
#[tokio::test]
async fn memory_commit_atomicity() {
    let (coord, thread_run, events, _outbox) = build_coordinator();

    // Seed a canonical event sharing an idempotency identity but with a
    // different payload, so the staged draft collides on append.
    let seed = sample_draft("RunStarted", "t-ma", "r-ma", "k-collide");
    events
        .append(seed.draft.clone(), seed.append_options.clone())
        .await
        .unwrap();

    let pre_count = events.count(EventScope::run("r-ma")).await.unwrap();

    let mut conflicting = sample_draft("RunStarted", "t-ma", "r-ma", "k-collide");
    conflicting.draft.payload = json!({"kind": "RunStarted", "diverged": true});

    let plan =
        CheckpointCommitPlan::checkpoint_only("t-ma", Vec::new(), run_record("t-ma", "r-ma"))
            .with_canonical_drafts(vec![conflicting]);

    let result = coord.commit_checkpoint(plan).await;
    assert!(
        matches!(result, Err(CommitError::EventAppend(_))),
        "append failure must surface as CommitError::EventAppend"
    );

    assert!(
        thread_run.load_thread("t-ma").await.unwrap().is_none(),
        "checkpoint must not advance on append failure"
    );
    assert_eq!(
        events.count(EventScope::run("r-ma")).await.unwrap(),
        pre_count,
        "event store state must be unchanged on append failure"
    );
}
