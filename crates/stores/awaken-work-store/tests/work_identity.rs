//! Work identity is independent from reusable ordering ordinals.
#![cfg(feature = "test-support")]

use std::sync::Arc;

use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkRevisionOperation as Operation, HeartbeatResult, LeaseHeartbeat,
    WorkMutationResult, WorkPayload,
};
use awaken_store_conformance::work_queue::WorkBackend;
use awaken_work_store::{InMemoryWorkQueue, PostgresWorkQueue, SqliteWorkQueue};

#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

async fn old_worker_requests_cannot_change_replacement<Q: WorkBackend>(
    queue: &Q,
    environment: &str,
    retired_id: &str,
    replacement_id: &str,
) {
    assert_ne!(
        retired_id, replacement_id,
        "ordering ordinal reuse must not recycle public Work identity"
    );
    let replacement = queue
        .get(environment, replacement_id)
        .await
        .unwrap()
        .unwrap();
    let before = replacement;
    assert!(matches!(
        queue
            .ack(environment, retired_id, "same-worker")
            .await
            .unwrap(),
        WorkMutationResult::NotFound
    ));
    assert!(matches!(
        queue
            .stop(environment, retired_id, "same-worker")
            .await
            .unwrap(),
        WorkMutationResult::NotFound
    ));
    assert!(matches!(
        queue
            .heartbeat(
                environment,
                retired_id,
                "same-worker",
                2,
                LeaseHeartbeat::unconditional()
            )
            .await
            .unwrap(),
        HeartbeatResult::NotFound
    ));
    let after = queue
        .get(environment, replacement_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.environment_id, before.environment_id);
    assert_eq!(after.data, before.data);
    assert_eq!(after.metadata, before.metadata);
    assert_eq!(after.state, before.state);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.acknowledged_at, before.acknowledged_at);
    assert_eq!(after.latest_heartbeat_at, before.latest_heartbeat_at);
    assert_eq!(after.started_at, before.started_at);
    assert_eq!(after.stop_requested_at, before.stop_requested_at);
    assert_eq!(after.stopped_at, before.stopped_at);
    assert!(
        queue
            .ack(environment, replacement_id, "same-worker")
            .await
            .unwrap()
            .is_accepted()
    );
}

async fn work_identity_is_not_reused<Q: WorkBackend>(queue: &Q) {
    // I1: Register healthcheck -> Cloud removes the last row -> Register again.
    // I2: hard-remove an old Session projection -> enqueue a different Session.
    // Keep the same Worker owner and an unexpired claim, so an epoch/owner
    // mismatch cannot accidentally hide reuse of the actual public Work id.
    queue
        .apply_environment_revision(
            "identity-healthcheck",
            EnvironmentRevision(1),
            Operation::Register { healthcheck: true },
        )
        .await
        .unwrap();
    let old = queue
        .claim("identity-healthcheck", "same-worker", 0)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(old.data, WorkPayload::HealthCheck { .. }));
    queue
        .apply_environment_revision(
            "identity-healthcheck",
            EnvironmentRevision(2),
            Operation::Register { healthcheck: false },
        )
        .await
        .unwrap();
    assert!(
        queue
            .get("identity-healthcheck", &old.id)
            .await
            .unwrap()
            .is_none()
    );
    queue
        .apply_environment_revision(
            "identity-healthcheck",
            EnvironmentRevision(3),
            Operation::Register { healthcheck: true },
        )
        .await
        .unwrap();
    let fresh = queue
        .claim("identity-healthcheck", "same-worker", 1)
        .await
        .unwrap()
        .unwrap();
    old_worker_requests_cannot_change_replacement(
        queue,
        "identity-healthcheck",
        &old.id,
        &fresh.id,
    )
    .await;

    let old_id = queue
        .enqueue_session("identity-session", &"old-session".into())
        .await
        .unwrap();
    let old = queue
        .claim("identity-session", "same-worker", 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.id, old_id);
    queue.remove_env("identity-session").await.unwrap();
    assert!(
        queue
            .get("identity-session", &old_id)
            .await
            .unwrap()
            .is_none()
    );
    let fresh_id = queue
        .enqueue_session("identity-session", &"new-session".into())
        .await
        .unwrap();
    let fresh = queue
        .claim("identity-session", "same-worker", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.id, fresh_id);
    old_worker_requests_cannot_change_replacement(queue, "identity-session", &old.id, &fresh.id)
        .await;

    // Same logical Session, owner and epoch on a replacement row: only its
    // non-recycled identity can distinguish the two full lease coordinates.
    let environment = "identity-access";
    let session = "same-session".into();
    queue.enqueue_session(environment, &session).await.unwrap();
    let old_digest = awaken_work_store::test_session_access_digest(9201);
    queue
        .claim_with_session_access(environment, "same-worker", "poller", 0, None, &old_digest)
        .await
        .unwrap()
        .unwrap();
    let old_access = queue
        .authenticate_session_access(&old_digest, 0)
        .await
        .unwrap()
        .unwrap();
    let old_lease = queue
        .acquire_session(environment, &session, "same-worker", 0)
        .await
        .unwrap()
        .unwrap();
    queue.remove_env(environment).await.unwrap();
    queue.enqueue_session(environment, &session).await.unwrap();
    let new_digest = awaken_work_store::test_session_access_digest(9202);
    queue
        .claim_with_session_access(environment, "same-worker", "poller", 0, None, &new_digest)
        .await
        .unwrap()
        .unwrap();
    let new_access = queue
        .authenticate_session_access(&new_digest, 0)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old_access.work_id, new_access.work_id);
    assert_eq!(old_access.lease_epoch, new_access.lease_epoch);
    assert_eq!(old_access.lease_owner, new_access.lease_owner);
    assert_eq!(old_access.expires_at_unix_ms, new_access.expires_at_unix_ms);
    assert!(
        queue
            .authenticate_session_access(&old_digest, 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        queue
            .ack_with_session_access(&old_access)
            .await
            .unwrap()
            .is_not_found()
    );
    assert!(
        queue
            .stop_with_session_access(&old_access)
            .await
            .unwrap()
            .is_not_found()
    );
    assert!(
        queue
            .heartbeat_with_session_access(&old_access, 1, LeaseHeartbeat::unconditional())
            .await
            .unwrap()
            .is_not_found()
    );
    assert!(!queue.release_session(&old_lease).await.unwrap());
    assert_eq!(
        queue
            .authenticate_session_access(&new_digest, 0)
            .await
            .unwrap(),
        Some(new_access.clone())
    );
    assert!(
        queue
            .ack_with_session_access(&new_access)
            .await
            .unwrap()
            .is_accepted()
    );
    let new_lease = queue
        .acquire_session(environment, &session, "same-worker", 0)
        .await
        .unwrap()
        .unwrap();
    assert!(queue.release_session(&new_lease).await.unwrap());
}

#[tokio::test]
async fn inmemory_work_identity_is_not_reused_after_lifecycle_removal() {
    let queue = InMemoryWorkQueue::new(Arc::new(awaken_lease_testkit::ManualClock::new(0)));
    work_identity_is_not_reused(&queue).await;
}

#[tokio::test]
async fn sqlite_work_identity_is_not_reused_after_lifecycle_removal() {
    let directory = tempfile::tempdir().unwrap();
    let queue = SqliteWorkQueue::open(
        directory.path().join("identity.db").to_str().unwrap(),
        Arc::new(awaken_lease_testkit::ManualClock::new(0)),
    )
    .await
    .unwrap();
    work_identity_is_not_reused(&queue).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_work_identity_is_not_reused_after_lifecycle_removal() {
    // A1 optional+absent reports a local skip; A2 required+absent or invalid
    // configuration fails; A3 live exercises every removal/replay identity
    // rule through the original backend and retains its schema cleanup.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 2).await;
    let queue = PostgresWorkQueue::with_pool(
        fixture.pool.clone(),
        Arc::new(awaken_lease_testkit::ManualClock::new(0)),
    )
    .await
    .unwrap();
    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(local.spawn_local(async move {
            work_identity_is_not_reused(&queue).await;
        }))
        .await;
    // Expected-red evidence must also release only this owned schema.
    fixture.cleanup().await;
    result.expect("Work identity assertions must pass");
}

#[path = "support/work_identity_durable.rs"]
mod durable;
#[path = "support/work_identity_restart.rs"]
mod restart;
