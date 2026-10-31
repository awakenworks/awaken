//! T12-C02 (批次原回执): an idempotent Event batch replay returns the ORIGINAL
//! root receipt even after later root writes and a process restart.
//!
//! Oracles (groups JSON order):
//! - O0 the exact original root receipt is replayed;
//! - O1 a changed fingerprint conflicts;
//! - O2 nothing is appended or executed twice.
//!
//! Negative control: replacing the original receipt with the current root
//! revision must fail. It is executed: the root is advanced by a second batch
//! BEFORE every replay, so the current revision differs from the admitting
//! revision, and the same `is_original_receipt` checker is run against a
//! receipt rebuilt from the current revision and must reject it.
//!
//! | Row | Request | Instance | Root since K1 | Expected |
//! |---|---|---|---|---|
//! | R1 | K1/F1 | warm | - | admitted at revision r1 |
//! | R2 | K2/F1 | warm | advanced | distinct batch, current revision > r1 |
//! | R3 | K1/F1 | warm and restarted | advanced | the K1 receipt (r1), no append |
//! | R4 | K1/F2 | restarted | advanced | idempotency_conflict, no append |
//! | R5 | K/F drive twice | warm | - | one reservation, one activation, one committed input |
//! | NC | receipt rebuilt from the current revision | - | - | rejected by the R3 checker |
use super::super::*;
use awaken_session_contract::SessionEventBatch;

const SESSION: &str = "event-receipt-identity";

fn input(text: &str) -> Vec<SessionEventInput> {
    vec![SessionEventInput::UserMessage {
        content: vec![ContentBlock::text(text)],
    }]
}

/// O0 checker over the receipt identity. Processing flags may change after
/// admission; the admitting coordinate and retry identity never do.
fn is_original_receipt(replay: &SessionEventBatch, original: &SessionEventBatch) -> bool {
    replay.batch_id == original.batch_id
        && replay.admitted_revision == original.admitted_revision
        && replay.admission_projection_anchor == original.admission_projection_anchor
        && replay.idempotency_key == original.idempotency_key
        && replay.request_fingerprint == original.request_fingerprint
        && replay.events.len() == original.events.len()
}

async fn append(
    app: &SessionApplication,
    text: &str,
    key: &str,
    fingerprint: &str,
) -> Result<SessionEventBatch, RunError> {
    app.event_batches
        .append_session_event_batch_idempotent(
            &SessionId::from(SESSION),
            input(text),
            None,
            None,
            Some((key.into(), fingerprint.into())),
        )
        .await
}

#[tokio::test]
async fn event_batch_replay_keeps_the_admitting_receipt_after_later_root_writes() {
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("receipt identity repository"),
    );
    create(repository.as_ref(), persisted(SESSION, false, "idle")).await;
    let app = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    // R1: the original admission.
    let first = append(&app, "first intent", "key-1", "fingerprint-1")
        .await
        .expect("R1 admitted");
    let admitted_root = repository.get(&SessionId::from(SESSION)).await.unwrap();
    assert_eq!(
        first.admitted_revision, admitted_root.revision,
        "R1 receipt is the admitting root revision"
    );

    // R2: a later, distinct command advances the root.
    let second = append(&app, "second intent", "key-2", "fingerprint-1")
        .await
        .expect("R2 distinct batch");
    assert_ne!(second.batch_id, first.batch_id, "R2 distinct intent");
    let current = repository.get(&SessionId::from(SESSION)).await.unwrap();
    assert!(
        current.revision > first.admitted_revision,
        "R2 the current root is a distinct, newer coordinate"
    );
    assert_eq!(current.event_batches.len(), 2, "R2 two batches");

    // R3/O0: warm replay of K1 after the later write.
    let warm = append(&app, "first intent", "key-1", "fingerprint-1")
        .await
        .expect("R3 warm replay");
    assert!(is_original_receipt(&warm, &first), "R3/O0 warm {warm:?}");
    assert_ne!(
        warm.admitted_revision, current.revision,
        "R3/O0 not the current revision"
    );
    assert_eq!(
        repository.get(&SessionId::from(SESSION)).await.unwrap(),
        current,
        "R3/O2 replay mutates nothing"
    );

    // R3/O0: a reconstructed application over the same repository.
    let restarted = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let observed = restarted
        .event_batches
        .session_event_batch_idempotency(&SessionId::from(SESSION), "key-1", "fingerprint-1")
        .await
        .expect("R3 restart read");
    let observed = match observed {
        SessionEventBatchIdempotency::Exact(batch) => batch,
        other => panic!("R3 restart read must find the exact receipt: {other:?}"),
    };
    assert!(is_original_receipt(&observed, &first), "R3/O0 restart read");
    let cold = append(&restarted, "first intent", "key-1", "fingerprint-1")
        .await
        .expect("R3 cold replay");
    assert!(is_original_receipt(&cold, &first), "R3/O0 cold {cold:?}");

    // NC: the receipt a current-root implementation would return.
    let rebuilt = SessionEventBatch {
        admitted_revision: current.revision,
        ..cold.clone()
    };
    assert!(
        !is_original_receipt(&rebuilt, &first),
        "NC a receipt carrying the current root revision is detected"
    );

    // R4/O1: a changed fingerprint conflicts and appends nothing.
    let conflict = append(&restarted, "first intent", "key-1", "fingerprint-2")
        .await
        .expect_err("R4/O1 changed fingerprint");
    assert_eq!(conflict.code, "idempotency_conflict", "R4/O1");
    let after = repository.get(&SessionId::from(SESSION)).await.unwrap();
    assert_eq!(after.revision, current.revision, "R4/O1 no root mutation");
    assert_eq!(after.event_batches.len(), 2, "R3/R4 O2 nothing appended");
}

#[tokio::test]
async fn driven_event_batch_replay_executes_its_run_once() {
    // R5/O2: the same keyed request driven twice reserves, activates and
    // commits its input exactly once.
    let session = "event-receipt-drive";
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("drive repository"),
    );
    create(repository.as_ref(), persisted(session, false, "idle")).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let session_id = SessionId::from(session);
    let drive = || {
        app.event_batches
            .append_and_drive_session_event_batch_idempotent(
                &session_id,
                input("drive once"),
                None,
                None,
                Some(("drive-key".into(), "drive-fingerprint".into())),
            )
    };
    // The fake's maps deduplicate by Run id, so every call is also counted
    // from its trace: a re-driven replay would add a reserve/activate call.
    let calls = |prefix: &str| {
        runtime
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with(prefix))
            .count()
    };
    let (first, first_error) = drive()
        .await
        .expect("R5 first drive")
        .expect("R5 an input batch is not a root no-op");
    assert!(first_error.is_none(), "R5 first drive completes");
    let reserved = runtime.reserved.lock().unwrap().len();
    let activated = runtime.activated.lock().unwrap().len();
    let committed = runtime.committed.lock().unwrap().len();
    assert_eq!((reserved, activated), (1, 1), "R5 the Run executed once");
    let (reserve_calls, activate_calls) = (calls("reserve:"), calls("activate:"));
    assert_eq!(activate_calls, 1, "R5 one activation call");
    let root = repository.get(&session_id).await.unwrap();

    let (replay, _) = drive()
        .await
        .expect("R5 replay drive")
        .expect("R5 the replay is not a root no-op");
    assert!(is_original_receipt(&replay, &first), "R5/O0 driven replay");
    assert_eq!(
        runtime.reserved.lock().unwrap().len(),
        reserved,
        "R5/O2 reservation"
    );
    assert_eq!(
        runtime.activated.lock().unwrap().len(),
        activated,
        "R5/O2 activation"
    );
    assert_eq!(
        calls("reserve:"),
        reserve_calls,
        "R5/O2 no second reserve call"
    );
    assert_eq!(
        calls("activate:"),
        activate_calls,
        "R5/O2 no second activate call"
    );
    assert_eq!(
        runtime.committed.lock().unwrap().len(),
        committed,
        "R5/O2 the input is committed once"
    );
    let after = repository.get(&session_id).await.unwrap();
    assert_eq!(
        after.event_batches.len(),
        root.event_batches.len(),
        "R5/O2 no append"
    );
}
