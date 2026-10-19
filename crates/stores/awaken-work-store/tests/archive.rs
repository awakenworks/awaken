//! Archive preserves the original Session Work authority on real adapters.
#![cfg(test)]
#![cfg(feature = "test-support")]
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkRevisionOperation as Operation, EnvironmentWorkRevisionOutcome as Outcome,
    HeartbeatResult, LeaseHeartbeat, SessionAccessAuthenticator as _, WorkSessionAccess,
};
use awaken_store_conformance::work_queue::WorkBackend;
use awaken_work_store::{
    InMemoryWorkQueue, PostgresWorkQueue, SqliteWorkQueue, test_session_access_digest,
};
#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

async fn archived_lease<Q: WorkBackend>(q: &Q) -> WorkSessionAccess {
    let session = q
        .enqueue_session("archive", &"existing".into())
        .await
        .unwrap();
    let digest = test_session_access_digest(907);
    q.claim_with_session_access("archive", "owner", "poller", 0, None, &digest)
        .await
        .unwrap()
        .unwrap();
    let proof = q
        .authenticate_session_access(&digest, 1)
        .await
        .unwrap()
        .unwrap();
    let before = q.get("archive", &session).await.unwrap().unwrap();
    let queued = q
        .enqueue_session("archive", &"waiting".into())
        .await
        .unwrap();
    q.apply_environment_revision(
        "archive",
        EnvironmentRevision(1),
        Operation::Register { healthcheck: true },
    )
    .await
    .unwrap();
    assert_eq!(q.list("archive").await.unwrap().len(), 3);
    assert_eq!(
        q.apply_environment_revision("archive", EnvironmentRevision(2), Operation::Withdraw)
            .await
            .unwrap(),
        Outcome::Applied
    );
    let current = q.get("archive", &session).await.unwrap();
    assert!(
        current.is_some(),
        "archive must retain the exact active Session Work"
    );
    let current = current.unwrap();
    assert_eq!(current.id, before.id);
    assert_eq!(current.environment_id, before.environment_id);
    assert_eq!(current.data, before.data);
    assert_eq!(current.metadata, before.metadata);
    assert_eq!(current.state, before.state);
    assert_eq!(current.created_at, before.created_at);
    assert_eq!(current.acknowledged_at, before.acknowledged_at);
    assert_eq!(current.latest_heartbeat_at, before.latest_heartbeat_at);
    assert_eq!(current.started_at, before.started_at);
    assert_eq!(current.stop_requested_at, before.stop_requested_at);
    assert_eq!(current.stopped_at, before.stopped_at);
    assert!(
        q.get("archive", &queued).await.unwrap().is_some(),
        "queued existing Session must survive"
    );
    assert_eq!(
        q.list("archive").await.unwrap().len(),
        2,
        "only the healthcheck is removed"
    );
    assert_eq!(
        q.authenticate_session_access(&digest, 1).await.unwrap(),
        Some(proof.clone()),
        "same digest must retain exact owner, epoch and expiry"
    );
    assert_eq!(
        q.apply_environment_revision("archive", EnvironmentRevision(2), Operation::Withdraw)
            .await
            .unwrap(),
        Outcome::AlreadyApplied
    );
    assert_eq!(
        q.apply_environment_revision(
            "archive",
            EnvironmentRevision(1),
            Operation::Register { healthcheck: true }
        )
        .await
        .unwrap(),
        Outcome::Superseded
    );
    assert!(q.ensure_healthcheck("archive").await.is_err());
    assert!(q.enqueue_healthcheck("archive").await.is_err());
    proof
}

async fn retained_mutations<Q: WorkBackend>(q: &Q, proof: &WorkSessionAccess) {
    assert!(matches!(
        q.heartbeat(
            "archive",
            &proof.work_id,
            "other-owner",
            2,
            LeaseHeartbeat::unconditional()
        )
        .await
        .unwrap(),
        HeartbeatResult::PreconditionFailed
    ));
    let mut foreign = proof.clone();
    foreign.lease_owner = "other-owner".into();
    assert!(
        !q.ack_with_session_access(&foreign)
            .await
            .unwrap()
            .is_accepted()
    );
    assert!(matches!(
        q.heartbeat_with_session_access(proof, 2, LeaseHeartbeat::unconditional())
            .await
            .unwrap(),
        HeartbeatResult::Accepted(_)
    ));
    assert!(
        q.ack("archive", &proof.work_id, "owner")
            .await
            .unwrap()
            .is_accepted()
    );
    assert!(
        q.ack_with_session_access(proof)
            .await
            .unwrap()
            .is_accepted()
    );
    assert!(
        q.stop_with_session_access(proof)
            .await
            .unwrap()
            .is_accepted()
    );
    assert!(
        q.authenticate_session_access(&test_session_access_digest(907), 3)
            .await
            .unwrap()
            .is_none(),
        "stop clears the original proof"
    );
    let next = q
        .claim_with_session_access(
            "archive",
            "next-owner",
            "next-poller",
            3,
            None,
            &test_session_access_digest(908),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.item.data,
        awaken_executable_environment_contract::WorkPayload::Session {
            id: "waiting".into()
        }
    );
    assert!(
        q.stop("archive", &next.item.id, "next-owner")
            .await
            .unwrap()
            .is_accepted()
    );
}

#[tokio::test]
async fn inmemory_archive_keeps_existing_session_authority() {
    let q = InMemoryWorkQueue::new(awaken_time::system_clock());
    let proof = archived_lease(&q).await;
    retained_mutations(&q, &proof).await;
}

#[tokio::test]
async fn sqlite_archive_keeps_existing_session_authority_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let q = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let proof = archived_lease(&q).await;
    drop(q);
    let q = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        q.authenticate_session_access(&test_session_access_digest(907), 1)
            .await
            .unwrap(),
        Some(proof.clone()),
        "reopen must recover the same persisted digest and lease"
    );
    retained_mutations(&q, &proof).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_archive_keeps_existing_session_authority_after_reconnect() {
    // A1 optional+absent is an explicit local skip; A2 required+absent or
    // invalid configuration fails; A3 live must preserve the exact lease and
    // digest through archive/reconnect and release only this owned schema.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 2).await;
    let q = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let proof = archived_lease(&q).await;
    drop(q);
    let q = PostgresWorkQueue::connect(&fixture.scoped_url, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        q.authenticate_session_access(&test_session_access_digest(907), 1)
            .await
            .unwrap(),
        Some(proof.clone()),
        "new pool must recover the same persisted digest and lease"
    );
    retained_mutations(&q, &proof).await;
    drop(q);
    fixture.cleanup().await;
}
