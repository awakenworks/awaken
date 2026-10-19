//! Direct Query reads reuse the original Runtime fixture, SQLite and slice law.
use super::*;
use awaken_agent_contract::thread::read::transcript::{
    TranscriptError, TranscriptRange, TranscriptSliceSpec, TranscriptSnapshot, TranscriptView,
};
use awaken_session_configuration_contract::SessionBaselineState;
use awaken_session_types::{RunErrorKind, SessionRuntimePlacement};

// Transform the existing creation input, rather than duplicating its defaults or
// minting another root/Store. The canonical finalizer owns the new fingerprint.
fn prefix_session(id: &str, prefix: Option<TranscriptSliceSpec>, worker: bool) -> PersistedSession {
    let mut command = super::super::creation::creation_command(id);
    command.intent.control.agent_id = "coord-root".into();
    command.intent.control.agent_revision = Some(1);
    command.intent.control.transcript_prefix = prefix;
    if worker {
        command.intent.control.runtime_placement = SessionRuntimePlacement::Worker;
    }
    let mut session = persisted(id, false, "idle");
    session.baseline = SessionBaselineState::Frozen(command.intent.finalize().unwrap().baseline);
    session
}

#[tokio::test]
async fn frozen_query_reads_the_original_prefix_without_a_current_source_session() {
    // Causes/effects: Q1 constructor borrows one selected executor and retains
    // only its read Arcs -> Query outlives that borrow with zero raw callbacks.
    // Q2 false materialization or absent reference -> no raw read, empty context.
    // Q3 true + stored prefix + later appends -> exact old ranges/ids, no target
    // message copy or root mutation. Q4 source has no current Session row, then
    // an archived row -> SAME retained raw prefix, no current-root requirement.
    // No public branch ingress/admission is invented: the original internal
    // finalized input carries the trusted ref. Worker/Runtime suites separately
    // own request-only installation/noncommit, not this reconstruction oracle.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let messages = ["zero", "one", "two", "later"]
        .into_iter()
        .enumerate()
        .map(|(i, text)| Message::text(MessageId(format!("source-{i}")), Role::User, text))
        .collect::<Vec<_>>();
    let snapshot = TranscriptSnapshot::new(
        ThreadId("retained-source".into()),
        TranscriptView::RawCommitted,
        messages[..3].to_vec(),
    );
    let spec = TranscriptSliceSpec {
        snapshot: snapshot.reference().clone(),
        ranges: vec![TranscriptRange::new(0, 1), TranscriptRange::new(2, 3)],
    };
    *runtime.committed.lock().unwrap() = messages.clone();
    let queries = {
        let selected = crate::SessionExecutor::from(runtime.clone());
        crate::SessionQueries::new(
            repository.clone(),
            &selected,
            Some(Arc::new(CoordinatedAgentSource)),
        )
    };
    assert!(
        runtime.committed_read_trace.lock().unwrap().is_empty(),
        "Q1"
    );
    let session = prefix_session("prefix-target", Some(spec), false);
    create(repository.as_ref(), session.clone()).await;
    let session = repository.get(&session.session_id).await.unwrap();
    let before = session.clone();
    let disabled = queries
        .frozen_session_projection("workspace".into(), &session, false)
        .await
        .unwrap();
    assert!(disabled.request_context.is_empty(), "Q2 disabled");
    let no_prefix = prefix_session("no-prefix", None, false);
    assert!(
        queries
            .frozen_session_projection("workspace".into(), &no_prefix, true)
            .await
            .unwrap()
            .request_context
            .is_empty(),
        "Q2 absent"
    );
    assert!(
        runtime.committed_read_trace.lock().unwrap().is_empty(),
        "Q2 no raw read"
    );
    assert!(
        matches!(
            repository.get(&SessionId::from("retained-source")).await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "Q4 no current source row"
    );
    let first = queries
        .frozen_session_projection("workspace".into(), &session, true)
        .await
        .unwrap();
    assert_eq!(
        first.request_context,
        [messages[0].clone(), messages[2].clone()],
        "Q3 exact ranges and ids"
    );
    let mut archived = persisted("retained-source", false, "idle");
    archived.archive("1970-01-01T00:00:01Z", 1_000).unwrap();
    create(repository.as_ref(), archived).await;
    runtime.committed.lock().unwrap().push(Message::text(
        MessageId("source-latest".into()),
        Role::Assistant,
        "future",
    ));
    let replay = queries
        .frozen_session_projection("workspace".into(), &session, true)
        .await
        .unwrap();
    assert_eq!(
        replay.request_context, first.request_context,
        "Q3/Q4 archived source plus append"
    );
    assert_eq!(
        *runtime.committed_read_trace.lock().unwrap(),
        ["committed:retained-source", "committed:retained-source"],
        "Q3 source coordinate, never target"
    );
    // Q4 declares the fixture's typed None reply for the SAME source that
    // supplied raw messages. This is a read-port result partition, not proof
    // of a production Run history, public branch admission or physical cleanup.
    let source_session_id = SessionId::from("retained-source");
    assert!(
        queries
            .projection_thread_snapshot(&source_session_id, source_session_id.as_str(), None)
            .await
            .unwrap()
            .is_none(),
        "Q4 declared typed None does not replace the original raw reader"
    );
    assert_eq!(
        repository.get(&session.session_id).await.unwrap(),
        before,
        "Q3 target unchanged"
    );
    assert!(
        runtime
            .dispatch_projection_installs
            .lock()
            .unwrap()
            .is_empty(),
        "Q3 no install"
    );
    assert!(
        runtime.reserved.lock().unwrap().is_empty() && runtime.activated.lock().unwrap().is_empty(),
        "Q3 no execution"
    );
}

#[tokio::test]
async fn frozen_query_refuses_source_faults_before_the_later_publication_refusal() {
    // Cause/effect decision table: E1 raw error -> preserve original kind/code/
    // message; E2 short read -> original unavailable end; E3 version mismatch,
    // E4 past-end range, E5 overlap -> original slice error projection. All rows
    // ALSO lack the required Worker publication: context failure must win first.
    // With materialization=false the SAME row performs no raw read and instead
    // reaches that later publication refusal (E6). These assertions observe the
    // first refusal and raw coordinates, not a profile callback count: None is
    // not a profile observer. Original body preservation separately owns call
    // sequence. This observes refusal order without
    // a second profile matcher/observer or accepting a copied model snapshot.
    // Damaged stored slice rows are decoder-boundary rejection controls, not
    // claims that a public branch-create ingress accepts those malformed values.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let selected = crate::SessionExecutor::from(runtime.clone());
    let queries = crate::SessionQueries::new(repository.clone(), &selected, None);
    let messages = (0..3)
        .map(|i| {
            Message::text(
                MessageId(format!("prefix-value-{i}")),
                Role::User,
                "retained",
            )
        })
        .collect::<Vec<_>>();
    let snapshot = TranscriptSnapshot::new(
        ThreadId("source-fault".into()),
        TranscriptView::RawCommitted,
        messages.clone(),
    );
    let valid = TranscriptSliceSpec {
        snapshot: snapshot.reference().clone(),
        ranges: vec![TranscriptRange::new(0, 3)],
    };
    let mut identity = valid.clone();
    identity.snapshot.version += 1;
    let mut range = valid.clone();
    range.ranges = vec![TranscriptRange::new(0, 4)];
    let mut overlap = valid.clone();
    overlap.ranges = vec![TranscriptRange::new(0, 2), TranscriptRange::new(1, 3)];
    for (rule, prefix, committed, raw_error, kind, code, message) in [
        (
            "E1",
            valid.clone(),
            messages.clone(),
            Some(RunError::conflict_classified(
                "raw-source-fault",
                "original raw failure",
            )),
            RunErrorKind::Conflict,
            "raw-source-fault",
            "original raw failure".into(),
        ),
        (
            "E2",
            valid,
            messages[..1].to_vec(),
            None,
            RunErrorKind::Unavailable,
            "unavailable",
            "transcript prefix ending at 3 is unavailable; committed end is 1".into(),
        ),
        (
            "E3",
            identity,
            messages.clone(),
            None,
            RunErrorKind::Unavailable,
            "unavailable",
            TranscriptError::SnapshotMismatch.to_string(),
        ),
        (
            "E4",
            range,
            messages.clone(),
            None,
            RunErrorKind::Unavailable,
            "unavailable",
            TranscriptError::InvalidRange {
                start: 0,
                end: 4,
                snapshot_end: 3,
            }
            .to_string(),
        ),
        (
            "E5",
            overlap,
            messages.clone(),
            None,
            RunErrorKind::Unavailable,
            "unavailable",
            TranscriptError::OverlappingRanges.to_string(),
        ),
    ] {
        let session = prefix_session(&format!("prefix-{rule}"), Some(prefix), true);
        create(repository.as_ref(), session.clone()).await;
        let session = repository.get(&session.session_id).await.unwrap();
        *runtime.committed.lock().unwrap() = committed;
        *runtime.committed_read_error.lock().unwrap() = raw_error;
        runtime.committed_read_trace.lock().unwrap().clear();
        let error = queries
            .frozen_session_projection("workspace".into(), &session, true)
            .await
            .unwrap_err();
        assert_eq!(
            (error.kind, error.code.as_str(), error.message),
            (kind, code, message),
            "{rule} first refusal"
        );
        assert_eq!(
            *runtime.committed_read_trace.lock().unwrap(),
            ["committed:source-fault"],
            "{rule} raw coordinate"
        );
        runtime.committed_read_trace.lock().unwrap().clear();
        let later = queries
            .frozen_session_projection("workspace".into(), &session, false)
            .await
            .unwrap_err();
        assert_eq!(
            later.message,
            "Agent 'coord-root' publication revision 1 is unavailable for Worker realization",
            "E6 later publication fault"
        );
        assert!(
            runtime.committed_read_trace.lock().unwrap().is_empty(),
            "E6 no raw read"
        );
        assert_eq!(
            repository.get(&session.session_id).await.unwrap(),
            session,
            "{rule} no root mutation"
        );
    }
}

#[tokio::test]
async fn scoped_transcript_query_keeps_root_refusals_before_raw_reads() {
    // S1 missing, foreign Workspace or hidden root -> original non-disclosing
    // BadRequest before raw messages. S2 unfinalized root -> original unavailable.
    // S3 existing injected repository fault -> unchanged unavailable before raw.
    // S4 legal current Scope -> original raw messages; S5 its raw fault keeps
    // exact kind/code/message. The original real SQLite root is unchanged for
    // every read. This current-root Scope gate must not be added to Q3/Q4's
    // already-frozen historical source. No branch authorization is tested here.
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let repository = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(EventBatchRuntime::default());
    let selected = crate::SessionExecutor::from(runtime.clone());
    let queries = crate::SessionQueries::new(repository.clone(), &selected, None);
    let visible = prefix_session("scoped-visible", None, false);
    create(durable.as_ref(), visible.clone()).await;
    let visible = durable.get(&visible.session_id).await.unwrap();
    let mut hidden = prefix_session("scoped-hidden", None, false);
    hidden.request_delete("workspace", 1).unwrap();
    create(durable.as_ref(), hidden.clone()).await;
    let hidden = durable.get(&hidden.session_id).await.unwrap();
    let mut preparing = prefix_session("scoped-preparing", None, false);
    preparing.baseline = SessionBaselineState::Preparing(
        super::super::creation::creation_command("scoped-preparing").intent,
    );
    preparing.execution = awaken_session_state_contract::SessionExecutionState::Preparing;
    create(durable.as_ref(), preparing.clone()).await;
    let preparing = durable.get(&preparing.session_id).await.unwrap();
    for (rule, owner, id, kind, message) in [
        (
            "S1 missing",
            "workspace",
            "missing",
            RunErrorKind::BadRequest,
            "Session was not found",
        ),
        (
            "S1 foreign",
            "foreign",
            "scoped-visible",
            RunErrorKind::BadRequest,
            "Session was not found",
        ),
        (
            "S1 hidden",
            "workspace",
            "scoped-hidden",
            RunErrorKind::BadRequest,
            "Session was not found",
        ),
        (
            "S2",
            "workspace",
            "scoped-preparing",
            RunErrorKind::Unavailable,
            "Session creation finalization is incomplete",
        ),
    ] {
        let error = queries.session_transcript(owner, id).await.unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (kind, message),
            "{rule}"
        );
        assert!(
            runtime.committed_read_trace.lock().unwrap().is_empty(),
            "{rule} no raw read"
        );
    }
    repository
        .failed_gets
        .lock()
        .unwrap()
        .insert("scoped-visible".into());
    let error = queries
        .session_transcript("workspace", "scoped-visible")
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind, error.code.as_str(), error.message.as_str()),
        (
            RunErrorKind::Unavailable,
            "unavailable",
            "injected Session read failure"
        ),
        "S3 original mapping"
    );
    assert!(
        runtime.committed_read_trace.lock().unwrap().is_empty(),
        "S3 no raw read"
    );
    repository.failed_gets.lock().unwrap().clear();
    let message = Message::text(
        MessageId("scoped-value".into()),
        Role::Assistant,
        "original committed output",
    );
    *runtime.committed.lock().unwrap() = vec![message.clone()];
    assert_eq!(
        queries
            .session_transcript("workspace", "scoped-visible")
            .await
            .unwrap(),
        [message],
        "S4"
    );
    *runtime.committed_read_error.lock().unwrap() = Some(RunError::unavailable_classified(
        "scoped-source-fault",
        "original scoped read failure",
    ));
    let error = queries
        .session_transcript("workspace", "scoped-visible")
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind, error.code.as_str(), error.message.as_str()),
        (
            RunErrorKind::Unavailable,
            "scoped-source-fault",
            "original scoped read failure"
        ),
        "S5 original raw fault"
    );
    assert_eq!(
        *runtime.committed_read_trace.lock().unwrap(),
        ["committed:scoped-visible", "committed:scoped-visible"],
        "S4/S5 exact raw target"
    );
    for original in [visible, hidden, preparing] {
        assert_eq!(
            durable.get(&original.session_id).await.unwrap(),
            original,
            "S1-S5 no root mutation"
        );
    }
    assert!(
        runtime
            .dispatch_projection_installs
            .lock()
            .unwrap()
            .is_empty(),
        "S1-S5 no install"
    );
    assert!(
        runtime.reserved.lock().unwrap().is_empty() && runtime.activated.lock().unwrap().is_empty(),
        "S1-S5 no execution"
    );
}

#[tokio::test]
async fn frozen_query_refuses_an_unconsumed_creation_intent_before_reading() {
    // E7: a complete original create intent is still Preparing. The finalized
    // baseline gate must refuse before raw reads or a missing publication; no
    // public private-field ingress or malformed partial intent is fabricated.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let selected = crate::SessionExecutor::from(runtime.clone());
    let queries = crate::SessionQueries::new(repository, &selected, None);
    let mut preparing = prefix_session("preparing-prefix", None, false);
    preparing.baseline = SessionBaselineState::Preparing(
        super::super::creation::creation_command("preparing-prefix").intent,
    );
    preparing.execution = awaken_session_state_contract::SessionExecutionState::Preparing;
    let error = queries
        .frozen_session_projection("workspace".into(), &preparing, true)
        .await
        .unwrap_err();
    assert_eq!(
        error.message, "Session creation intent was not consumed",
        "E7 baseline refusal before reads"
    );
    assert!(
        runtime.committed_read_trace.lock().unwrap().is_empty(),
        "E7"
    );
    assert!(
        runtime
            .dispatch_projection_installs
            .lock()
            .unwrap()
            .is_empty(),
        "E7 no install"
    );
}
