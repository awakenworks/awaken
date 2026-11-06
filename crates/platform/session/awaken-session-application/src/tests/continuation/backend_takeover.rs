//! T19-C03 (并发接管及保留), real backends: two continuation owners drive the
//! same suspend operation through independently opened stores (two file
//! SQLite handles plus a reopen; two PostgreSQL pools plus a fresh pool).
//!
//! Oracles (groups JSON order):
//! - O0 the old owner is refused;
//! - O1 a failed delete of an expired checkpoint keeps the reference and the
//!   original cleanup responsibility;
//! - O2 the real backend state is consistent across handles.
//!
//! Negative control: "treating a failed delete as cleaned" must fail. It is
//! executed: the production transition a delete-as-cleaned implementation
//! would commit (`discard_expired_checkpoint` despite the failed effect) is
//! applied to the retained root and the `reference_retained` checker rejects
//! it.
//!
//! | Step | Handle | Effect |
//! |---|---|---|
//! | K1 | A | starts the checkpoint effect of the Uploading root and is held inside it |
//! | K2 | B | completes the same operation: checkpoint, source release, Hibernated |
//! | K3 | A released | its CAS is refused; the root equals B's; A made no further effect |
//! | K4 | C (reopen) | the expired checkpoint's delete fails; the Session stays Hibernated with its reference |
//! | K5 | D (reopen) | the retry deletes exactly once and the Session is Unmaterialized |
use super::*;

type Store = Arc<dyn crate::SessionStore>;

const NOW: u64 = 2_000;
const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// The same committed Uploading step B017 uses: suspend intent plus recorded
/// quiescence, so the next decided effect is the checkpoint. Like the
/// production suspend intent, the operation freezes the root's current
/// realization lease, which later authorizes source-release preparation.
fn uploading(id: &str) -> PersistedSession {
    let mut session = generated_session(id);
    let realization = session.realization.clone();
    let operation = session
        .environment
        .begin_suspend_at(
            "workspace",
            &awaken_session_types::SessionId::from(id),
            0,
            realization,
            NOW,
        )
        .unwrap()
        .clone();
    let generation_id = session.environment.generation().unwrap().id().to_owned();
    session
        .environment
        .record_quiescence(
            &awaken_session_contract::QuiescenceReceipt {
                effect_id: operation.effect_id().to_owned(),
                generation_id,
                activity_epoch: 0,
                live_environment_effects: 0,
                mcp_generations: Vec::new(),
            },
            &[],
        )
        .unwrap();
    session
}

fn hibernated_checkpoint(
    root: &PersistedSession,
) -> Option<&awaken_provisioning_contract::SandboxCheckpointRef> {
    match &root.environment {
        awaken_session_contract::SessionEnvironmentState::Hibernated { checkpoint, .. } => {
            Some(checkpoint)
        }
        _ => None,
    }
}

/// O1 checker: the root still owns the exact checkpoint reference.
fn reference_retained(
    root: &PersistedSession,
    checkpoint: &awaken_provisioning_contract::SandboxCheckpointRef,
) -> bool {
    hibernated_checkpoint(root) == Some(checkpoint)
}

fn app(store: Store, runtime: Arc<ContinuationRuntime>) -> SessionApplication {
    application_with_runtime(
        runtime,
        store,
        Arc::new(RecordingEnvironmentSource::default()),
    )
}

async fn takeover_table<R, RF>(prefix: &str, store_a: Store, store_b: Store, reopen: R)
where
    R: Fn() -> RF,
    RF: std::future::Future<Output = Store>,
{
    assert!(
        !std::ptr::addr_eq(Arc::as_ptr(&store_a), Arc::as_ptr(&store_b)),
        "independently opened stores"
    );
    let id = unique(prefix);
    let session_id = SessionId::from(id.as_str());
    create(store_a.as_ref(), uploading(&id)).await;
    let original = store_a.get(&session_id).await.unwrap();

    // K1: A's checkpoint effect starts and is held. A's upload is its own
    // independent observation (its own content digest): with a byte-identical
    // observation A's commit would be the very command B already committed and
    // the snapshot idempotency law replays it, so there would be no stale write
    // left to refuse.
    let runtime_a = Arc::new(ContinuationRuntime::default());
    let gate = CheckpointGate {
        digest: Some("digest-held-by-a".into()),
        ..CheckpointGate::default()
    };
    *runtime_a.checkpoint_gate.lock().unwrap() = Some(gate.clone());
    let app_a = Arc::new(app(store_a.clone(), runtime_a.clone()));
    let held = {
        let app_a = app_a.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            app_a
                .continuations
                .reconcile_environment_continuation(&session_id, NOW)
                .await
        })
    };
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("K1 A is inside its checkpoint effect");

    // K2: B completes the same operation through its own handle.
    let runtime_b = Arc::new(ContinuationRuntime::default());
    let app_b = app(store_b.clone(), runtime_b.clone());
    app_b
        .continuations
        .reconcile_environment_continuation(&session_id, NOW)
        .await
        .expect("K2 B completes the suspend");
    let committed_by_b = store_b.get(&session_id).await.unwrap();
    assert!(
        committed_by_b.revision > original.revision,
        "K2 B advanced the root"
    );
    let checkpoint = hibernated_checkpoint(&committed_by_b)
        .expect("K2 B hibernated the Session")
        .clone();
    assert_eq!(
        (
            runtime_b.checkpoints.load(Ordering::SeqCst),
            runtime_b.preparations.load(Ordering::SeqCst),
            runtime_b.disposals.load(Ordering::SeqCst),
        ),
        (1, 1, 1),
        "K2 B ran the complete suspend once"
    );

    // K3/O0: A is released; its stale CAS is refused with zero root change.
    gate.release.notify_one();
    let refused = tokio::time::timeout(WAIT, held)
        .await
        .expect("K3 A re-decides after B's commit")
        .expect("K3 A task");
    assert!(
        refused.is_err(),
        "K3/O0 the old owner is refused: {refused:?}"
    );
    assert_eq!(
        store_a.get(&session_id).await.unwrap(),
        committed_by_b,
        "K3/O0/O2 the root is exactly B's, seen through A's handle"
    );
    assert_ne!(
        checkpoint.digest, "digest-held-by-a",
        "K3/O0 A's stale checkpoint observation never reached the root"
    );
    assert_eq!(
        (
            runtime_a.checkpoints.load(Ordering::SeqCst),
            runtime_a.preparations.load(Ordering::SeqCst),
            runtime_a.disposals.load(Ordering::SeqCst),
        ),
        (1, 0, 0),
        "K3/O0 A made no effect after its refused checkpoint"
    );

    // K4/O1: the expired checkpoint's delete fails on a reopened handle.
    let expired_at = checkpoint.expires_at_unix_ms.max(100_000) + 1;
    let runtime_c = Arc::new(ContinuationRuntime::default());
    runtime_c.fail_delete_once.store(true, Ordering::SeqCst);
    let app_c = app(reopen().await, runtime_c.clone());
    assert!(
        app_c
            .continuations
            .ensure_environment_resident(&session_id, expired_at)
            .await
            .is_err(),
        "K4 the failed delete is reported"
    );
    assert_eq!(
        runtime_c.deletes.load(Ordering::SeqCst),
        1,
        "K4 one delete attempt"
    );
    let store_d = reopen().await;
    let retained = store_d.get(&session_id).await.unwrap();
    assert!(
        reference_retained(&retained, &checkpoint),
        "K4/O1/O2 the reopened root keeps the exact checkpoint reference"
    );
    assert_eq!(
        retained, committed_by_b,
        "K4/O1 the failure committed nothing"
    );

    // NC: the root a delete-as-cleaned implementation would commit.
    let mut cleaned = awaken_session_contract::SessionAggregate::try_from(retained.clone())
        .expect("valid retained root");
    cleaned
        .environment()
        .discard_expired_checkpoint(expired_at)
        .expect("the production expiry transition");
    assert!(
        !reference_retained(&cleaned.into_snapshot(), &checkpoint),
        "NC a delete failure treated as cleaned is detected"
    );

    // K5/O1: the retry deletes once, then and only then drops the reference.
    let runtime_d = Arc::new(ContinuationRuntime::default());
    let app_d = app(store_d.clone(), runtime_d.clone());
    app_d
        .continuations
        .ensure_environment_resident(&session_id, expired_at)
        .await
        .expect("K5 retried delete");
    assert_eq!(
        runtime_d.deletes.load(Ordering::SeqCst),
        1,
        "K5 one retried delete"
    );
    assert_eq!(
        runtime_d.restores.load(Ordering::SeqCst),
        0,
        "K5 never restored"
    );
    assert!(
        matches!(
            store_b.get(&session_id).await.unwrap().environment,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
        ),
        "K5/O2 every handle sees the reference dropped"
    );
}

#[tokio::test]
async fn sqlite_continuation_takeover_refuses_the_old_owner_and_retains_failed_deletes() {
    let directory = tempfile::tempdir().expect("T19 SQLite directory");
    let path = directory
        .path()
        .join("t19-takeover.db")
        .to_str()
        .unwrap()
        .to_string();
    let open = |path: String| async move {
        Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .await
                .expect("T19 SQLite handle"),
        ) as Store
    };
    let store_a = open(path.clone()).await;
    let store_b = open(path.clone()).await;
    Box::pin(takeover_table("t19-sqlite", store_a, store_b, || {
        open(path.clone())
    }))
    .await;
}

#[tokio::test]
async fn postgres_continuation_takeover_refuses_the_old_owner_and_retains_failed_deletes() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let connect = |url: String| async move {
        Arc::new(
            awaken_session_store::PostgresManagedSessionRepository::connect(&url)
                .await
                .expect("T19 PostgreSQL pool"),
        ) as Store
    };
    let store_a = connect(url.clone()).await;
    let store_b = connect(url.clone()).await;
    Box::pin(takeover_table("t19-postgres", store_a, store_b, || {
        connect(url.clone())
    }))
    .await;
}

#[test]
fn postgres_continuation_takeover_refuses_the_old_owner_and_retains_failed_deletes_fixture_admission()
 {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::continuation::backend_takeover::postgres_continuation_takeover_refuses_the_old_owner_and_retains_failed_deletes",
    );
}
