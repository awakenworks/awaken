//! T12-C01 (批次边界与拒绝), application-layer rows: every batch reaches the
//! production `admit_session_event_batch_idempotent` root-CAS admission.
//!
//! Oracles (groups-2.json order):
//! - O0 stable classification (typed `SessionEventBatchError` text, BadRequest);
//! - O1 an illegal target/batch has zero effect (root, outbox, Runtime counters);
//! - O2 a legal no-op (exact idempotent replay) appends no fact;
//! - O3 error precedence follows the entry table: replay > terminal > compile >
//!   tool-reply occupancy.
//!
//! Negative control: JSON/typed grammar alone must not replace business
//! admission. Every rejected payload is constructed as a typed, grammatically
//! valid `SessionEventInput`; a grammar-only acceptor accepts all of them,
//! while the production admission rejects each.
//!
//! | Row | Batch | Root | Expected |
//! |---|---|---|---|
//! | A1 | empty | idle | EmptyBatch |
//! | A2 | System before User | idle | SystemMessagePlacement |
//! | A3 | two System messages | idle | MultipleSystemMessages |
//! | A4 | empty User content | idle | EmptyMessageContent |
//! | A5 | Interrupt without Primary / duplicated target | idle | InvalidInterruptTargets |
//! | A6 | duplicate reply to an occupied pending call | idle | DuplicateToolReply |
//! | A7 | valid batch | terminal | terminal refusal |
//! | A8 | empty batch | terminal | terminal refusal (terminal before compile) |
//! | A9 | exact replay of an accepted key | idle | same receipt, no new fact |
use super::*;
use awaken_session_contract::LifecycleOutbox as _;
use awaken_session_contract::SessionEventBatchError;

fn user(text: &str) -> SessionEventInput {
    SessionEventInput::UserMessage {
        content: vec![ContentBlock::text(text)],
    }
}

fn system(text: &str) -> SessionEventInput {
    SessionEventInput::SystemMessage {
        content: vec![ContentBlock::text(text)],
    }
}

fn interrupt(
    requested_target: Option<SessionThreadTarget>,
    targets: Vec<SessionThreadTarget>,
) -> SessionEventInput {
    SessionEventInput::Interrupt(SessionEventInterrupt {
        requested_target,
        targets,
    })
}

/// A grammar-only acceptor: every value that is constructible as a typed
/// `SessionEventInput` with well-formed fields is "valid JSON input". This is
/// the oracle the negative control proves insufficient.
fn grammar_only_accepts(inputs: &[SessionEventInput]) -> bool {
    inputs.iter().all(|input| match input {
        SessionEventInput::UserMessage { content }
        | SessionEventInput::SystemMessage { content } => content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. })),
        SessionEventInput::DefineOutcome { .. } => true,
        SessionEventInput::ToolReply(reply) => !reply.runtime_tool_use_id.is_empty(),
        SessionEventInput::Interrupt(interrupt) => interrupt
            .targets
            .iter()
            .chain(interrupt.requested_target.iter())
            .all(|target| match target {
                SessionThreadTarget::Primary => true,
                SessionThreadTarget::Child(thread) => !thread.0.is_empty(),
            }),
    })
}

#[derive(Debug, PartialEq, Eq)]
struct EffectFrame {
    reserved: usize,
    activated: usize,
    tool_replies: usize,
    primary_interrupts: usize,
    child_interrupts: usize,
    trace: Vec<String>,
}

fn effect_frame(runtime: &EventBatchRuntime) -> EffectFrame {
    EffectFrame {
        reserved: runtime.reserved.lock().unwrap().len(),
        activated: runtime.activated.lock().unwrap().len(),
        tool_replies: runtime.tool_reply_calls.load(Ordering::SeqCst),
        primary_interrupts: runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        child_interrupts: runtime.child_interrupt_calls.load(Ordering::SeqCst),
        trace: runtime.trace.lock().unwrap().clone(),
    }
}

struct BoundaryFixture {
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    runtime: Arc<EventBatchRuntime>,
    app: SessionApplication,
}

async fn boundary_fixture(sessions: &[(&str, &str)]) -> BoundaryFixture {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("T12 Session store"),
    );
    for (id, status) in sessions {
        create(repository.as_ref(), persisted(id, false, status)).await;
    }
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    BoundaryFixture {
        repository,
        runtime,
        app,
    }
}

impl BoundaryFixture {
    /// Submit one batch and require a rejection with zero root, outbox, and
    /// Runtime effect. Returns the observed error.
    async fn reject_without_effect(
        &self,
        row: &str,
        session: &str,
        inputs: Vec<SessionEventInput>,
    ) -> RunError {
        let session_id = SessionId::from(session);
        let root = self.repository.get(&session_id).await.unwrap();
        let outbox =
            serde_json::to_value(self.repository.pending_lifecycle().await.unwrap()).unwrap();
        let effects = effect_frame(&self.runtime);
        let error = self
            .app
            .event_batches
            .admit_session_event_batch_idempotent(
                &session_id,
                inputs,
                None,
                None,
                Some((format!("t12-{row}"), format!("t12-{row}-fingerprint"))),
            )
            .await
            .expect_err(row);
        assert_eq!(
            self.repository.get(&session_id).await.unwrap(),
            root,
            "{row}/O1 root unchanged"
        );
        assert_eq!(
            serde_json::to_value(self.repository.pending_lifecycle().await.unwrap()).unwrap(),
            outbox,
            "{row}/O1 outbox unchanged"
        );
        assert_eq!(
            effect_frame(&self.runtime),
            effects,
            "{row}/O1 no Runtime effect"
        );
        error
    }
}

fn assert_classified(row: &str, error: &RunError, expected: &str) {
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "{row}/O0"
    );
    assert_eq!(error.message, expected, "{row}/O0 stable class text");
}

#[tokio::test]
async fn invalid_batches_are_classified_and_have_zero_effect() {
    let idle = "t12-boundaries";
    let terminal = "t12-terminal";
    let fixture = boundary_fixture(&[(idle, "idle"), (terminal, "terminated")]).await;
    assert!(
        fixture
            .repository
            .get(&SessionId::from(terminal))
            .await
            .unwrap()
            .is_terminal(),
        "A7 fixture is terminal"
    );

    let compile_rows: Vec<(&str, Vec<SessionEventInput>, SessionEventBatchError)> = vec![
        ("A1", Vec::new(), SessionEventBatchError::EmptyBatch),
        (
            "A2",
            vec![system("first"), user("after")],
            SessionEventBatchError::SystemMessagePlacement,
        ),
        (
            "A3",
            vec![user("one"), system("two"), system("three")],
            SessionEventBatchError::MultipleSystemMessages,
        ),
        (
            "A4",
            vec![SessionEventInput::UserMessage {
                content: Vec::new(),
            }],
            SessionEventBatchError::EmptyMessageContent,
        ),
        (
            "A5-no-primary",
            vec![interrupt(None, Vec::new())],
            SessionEventBatchError::InvalidInterruptTargets,
        ),
        (
            "A5-duplicate",
            vec![interrupt(
                None,
                vec![SessionThreadTarget::Primary, SessionThreadTarget::Primary],
            )],
            SessionEventBatchError::InvalidInterruptTargets,
        ),
        (
            "A5-requested-mismatch",
            vec![interrupt(
                Some(SessionThreadTarget::Primary),
                vec![SessionThreadTarget::Child(ThreadId("child".into()))],
            )],
            SessionEventBatchError::InvalidInterruptTargets,
        ),
    ];
    for (row, inputs, expected) in compile_rows {
        let error = fixture.reject_without_effect(row, idle, inputs).await;
        assert_classified(row, &error, &expected.to_string());
    }

    // A6: occupy one pending call, then answer it again under another key.
    let accepted = fixture
        .app
        .event_batches
        .admit_session_event_batch_idempotent(
            &SessionId::from(idle),
            vec![tool_reply_input("t12-call")],
            None,
            None,
            Some(("t12-reply".into(), "t12-reply-fingerprint".into())),
        )
        .await
        .expect("A6 first reply occupies the pending call")
        .into_batch()
        .expect("this fixture requires a retained Event batch");
    let duplicate = fixture
        .reject_without_effect("A6", idle, vec![tool_reply_input("t12-call")])
        .await;
    assert_classified(
        "A6",
        &duplicate,
        &SessionEventBatchError::DuplicateToolReply.to_string(),
    );

    // A7/A8 (O3): terminal outranks every compile rule.
    let terminal_valid = fixture
        .reject_without_effect("A7", terminal, vec![user("late")])
        .await;
    assert_classified(
        "A7",
        &terminal_valid,
        "Session no longer accepts Event batches",
    );
    let terminal_invalid = fixture
        .reject_without_effect("A8", terminal, Vec::new())
        .await;
    assert_eq!(
        (
            &terminal_invalid.message,
            terminal_invalid.kind,
            &terminal_invalid.code
        ),
        (
            &terminal_valid.message,
            terminal_valid.kind,
            &terminal_valid.code
        ),
        "A8/O3 terminal precedes compile: identical refusal for an empty batch"
    );

    // A9 (O2/O3): the exact replay of an accepted key outranks occupancy and
    // appends no fact.
    let session_id = SessionId::from(idle);
    let root = fixture.repository.get(&session_id).await.unwrap();
    let outbox =
        serde_json::to_value(fixture.repository.pending_lifecycle().await.unwrap()).unwrap();
    let effects = effect_frame(&fixture.runtime);
    let replay = fixture
        .app
        .event_batches
        .admit_session_event_batch_idempotent(
            &session_id,
            vec![tool_reply_input("t12-call")],
            None,
            None,
            Some(("t12-reply".into(), "t12-reply-fingerprint".into())),
        )
        .await
        .expect("A9 exact replay")
        .into_batch()
        .expect("this fixture requires a retained Event batch");
    assert_eq!(replay, accepted, "A9 original receipt");
    let after = fixture.repository.get(&session_id).await.unwrap();
    assert_eq!(after.revision, root.revision, "A9/O2 no new revision");
    assert_eq!(after.event_batches, root.event_batches, "A9/O2 no new fact");
    assert_eq!(
        serde_json::to_value(fixture.repository.pending_lifecycle().await.unwrap()).unwrap(),
        outbox,
        "A9/O2 no outbox fact"
    );
    assert_eq!(
        effect_frame(&fixture.runtime),
        effects,
        "A9/O2 no Runtime effect"
    );
}

#[tokio::test]
async fn grammar_valid_payloads_still_fail_business_admission() {
    // NC: each payload passes the grammar-only acceptor yet is refused.
    let idle = "t12-grammar";
    let terminal = "t12-grammar-terminal";
    let fixture = boundary_fixture(&[(idle, "idle"), (terminal, "terminated")]).await;
    let payloads: Vec<(&str, &str, Vec<SessionEventInput>)> = vec![
        ("G1", idle, vec![system("first"), user("after")]),
        (
            "G2",
            idle,
            vec![user("one"), system("two"), system("three")],
        ),
        (
            "G3",
            idle,
            vec![interrupt(
                None,
                vec![SessionThreadTarget::Primary, SessionThreadTarget::Primary],
            )],
        ),
        ("G4", terminal, vec![user("well formed but late")]),
    ];
    let mut refused = 0;
    for (row, session, inputs) in payloads {
        assert!(
            grammar_only_accepts(&inputs),
            "{row} NC the payload is grammatically valid"
        );
        fixture.reject_without_effect(row, session, inputs).await;
        refused += 1;
    }
    assert_eq!(
        refused, 4,
        "NC every grammar-valid payload was refused by admission"
    );
}

#[tokio::test]
async fn compile_rejections_carry_distinct_stable_codes() {
    let idle = "t12-codes";
    let fixture = boundary_fixture(&[(idle, "idle")]).await;
    let mut codes = Vec::new();
    for (row, inputs) in [
        ("C1", Vec::new()),
        ("C2", vec![system("first"), user("after")]),
        ("C3", vec![user("one"), system("two"), system("three")]),
        (
            "C4",
            vec![SessionEventInput::UserMessage {
                content: Vec::new(),
            }],
        ),
        ("C5", vec![interrupt(None, Vec::new())]),
    ] {
        codes.push(fixture.reject_without_effect(row, idle, inputs).await.code);
    }
    let distinct = codes.iter().collect::<BTreeSet<_>>();
    assert_eq!(
        distinct.len(),
        codes.len(),
        "each rejection class needs its own stable code: {codes:?}"
    );
    assert!(
        codes.iter().all(|code| code != "invalid_request"),
        "classes must not collapse into the generic code: {codes:?}"
    );
}

#[tokio::test]
async fn interrupt_of_unknown_child_target_is_rejected_without_effect() {
    let idle = "t12-ghost";
    let fixture = boundary_fixture(&[(idle, "idle")]).await;
    let session_id = SessionId::from(idle);
    let root = fixture.repository.get(&session_id).await.unwrap();
    let result = fixture
        .app
        .event_batches
        .admit_session_event_batch_idempotent(
            &session_id,
            vec![interrupt(
                None,
                vec![
                    SessionThreadTarget::Primary,
                    SessionThreadTarget::Child(ThreadId("ghost".into())),
                ],
            )],
            None,
            None,
            Some(("t12-ghost".into(), "t12-ghost-fingerprint".into())),
        )
        .await;
    assert!(
        result.is_err(),
        "an invalid (unknown) Child target must be refused: {result:?}"
    );
    assert_eq!(
        fixture.repository.get(&session_id).await.unwrap(),
        root,
        "an invalid target appends no fact"
    );
}

mod receipt_identity;
