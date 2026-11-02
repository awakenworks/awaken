//! The existing creation-intent authority must retain a completed abort's
//! Session-identity fence. This is a local Store law, not an external
//! Registry/Vault close receipt.

use std::sync::Arc;

use awaken_session_application::SessionStore;
use awaken_session_contract::{
    IdempotencyRecord, PersistedSession, PreparedParticipantIntentClaim,
    PreparedParticipantIntentPhase, SessionCreateResult, SessionMutationPayload,
    SessionRepositoryConflict, SessionRepositoryError,
};
use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
use awaken_session_types::{SessionId, SessionRevision};
use awaken_store_conformance::session_store::{record, session};

struct OriginalCreate {
    root: PersistedSession,
    record: IdempotencyRecord,
}

async fn complete_original_abort(store: &dyn SessionStore) -> OriginalCreate {
    let id = SessionId::from("closed-creation-abort");
    let mut root = session(id.as_str(), "original candidate");
    root.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(root.clone());
    let record = record("original-create", &payload);
    store
        .record_creation_intent(
            "ws_a",
            &id,
            "managed:closed-creation-abort:repository:r",
            100,
        )
        .await
        .unwrap();
    let claimed = store.claim_creation_abort(&id).await.unwrap();
    assert!(matches!(
        claimed,
        PreparedParticipantIntentClaim::Claimed(ref intent)
            if intent.phase() == PreparedParticipantIntentPhase::Aborting
    ));
    store.complete_creation_abort(&id).await.unwrap();
    store.complete_creation_abort(&id).await.unwrap();
    OriginalCreate { root, record }
}

async fn assert_closed(store: &dyn SessionStore, original: &OriginalCreate) {
    // DT-A: C1 the original prepare was durably claimed and completed as an
    // abort; C2 its delayed Root insert/re-prepare resumes; C3 same process or
    // cold independent pool. A1 C1+C2 (both C3 rows) => Tombstoned, no Root,
    // owner, receipt or outbox effect. The original identity never reopens.
    // A2 replay completion => the same closure, excluded from pending scans.
    // A3 another never-aborted identity remains creatable (sibling control).
    // Remote registration/material closure is deliberately outside this law;
    // absence of those participants cannot stand in for this retained fence.
    let id = &original.root.session_id;
    let facts = store.pending_lifecycle().await.unwrap();
    let late = store
        .create(
            "ws_a",
            original.root.clone(),
            original.record.clone(),
            Vec::new(),
        )
        .await;
    assert!(
        matches!(
            late,
            Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned
            ))
        ),
        "A1 completed abort must reject the delayed original create: {late:?}"
    );
    assert!(matches!(
        store.get(id).await,
        Err(SessionRepositoryError::NotFound)
    ));
    assert!(matches!(
        store.owner(id).await,
        Err(SessionRepositoryError::NotFound)
    ));
    assert!(
        store
            .idempotency_receipt(id, &original.record.key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.pending_lifecycle().await.unwrap(), facts);
    assert!(matches!(
        store
            .record_creation_intent(
                "ws_a",
                id,
                "managed:closed-creation-abort:repository:r",
                200
            )
            .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::Tombstoned
        ))
    ));
    assert!(
        store
            .recoverable_creation_intents(1_000, 50)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn assert_fresh_control(store: &dyn SessionStore) {
    let mut root = session("fresh-creation-control", "fresh candidate");
    root.revision = SessionRevision(0);
    let record = record(
        "fresh-create",
        &SessionMutationPayload::Replace(root.clone()),
    );
    assert!(matches!(
        store
            .create("ws_a", root, record, Vec::new())
            .await
            .unwrap(),
        SessionCreateResult::Applied(_)
    ));
}

#[tokio::test]
async fn sqlite_completed_abort_survives_cold_reopen_and_refuses_late_creation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("abort.db");
    let store = SqliteManagedSessionRepository::open(path.to_str().unwrap())
        .await
        .unwrap();
    let original = complete_original_abort(&store).await;
    assert_closed(&store, &original).await;
    drop(store);
    let reopened = SqliteManagedSessionRepository::open(path.to_str().unwrap())
        .await
        .unwrap();
    assert_closed(&reopened, &original).await;
    assert_fresh_control(&reopened).await;
}

#[tokio::test]
async fn postgres_completed_abort_survives_new_pool_and_refuses_late_creation() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let store = Arc::new(
        PostgresManagedSessionRepository::connect(&url)
            .await
            .unwrap(),
    );
    let original = complete_original_abort(store.as_ref()).await;
    assert_closed(store.as_ref(), &original).await;
    drop(store);
    let reopened = PostgresManagedSessionRepository::connect(&url)
        .await
        .unwrap();
    assert_closed(&reopened, &original).await;
    assert_fresh_control(&reopened).await;
}

#[test]
fn postgres_abort_fence_fixture_is_required_when_configured() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_completed_abort_survives_new_pool_and_refuses_late_creation",
    );
}
