mod support;
use awaken_lease_testkit::ManualClock;
use awaken_session_contract::SessionConditionalPublication as _;
use awaken_session_contract::{
    IdempotencyRecord, LifecycleOutbox, PreparedParticipantIntentClaim, PreparedParticipantIntents,
    SessionCreateResult, SessionRepository,
};
use awaken_store_conformance::session_store;
use awaken_store_s3::{Limits, S3Storage};
use std::sync::Arc;

fn repository(engine: S3Storage) -> awaken_session_store::ObjectManagedSessionRepository {
    awaken_session_store::ObjectManagedSessionRepository::new(Arc::new(
        engine.component_store("managed_session").unwrap(),
    ))
}
#[tokio::test]
async fn object_repository_runs_the_postgres_session_suite() {
    session_store::run_suite(|| async {
        repository(
            S3Storage::new(
                support::object_transport(),
                "suite",
                Arc::new(ManualClock::new(1_786_000_000_000)),
                Limits::default(),
            )
            .unwrap(),
        )
    })
    .await;
}
#[tokio::test]
async fn object_repository_runs_the_postgres_reopen_suite() {
    let store = support::object_transport();
    let clock = Arc::new(ManualClock::new(1_786_000_000_000));
    let first = repository(
        S3Storage::new(store.clone(), "reopen", clock.clone(), Limits::default()).unwrap(),
    );
    session_store::reopen_preserves_authority(first, || async {
        repository(
            S3Storage::new(store.clone(), "reopen", clock.clone(), Limits::default()).unwrap(),
        )
    })
    .await;
}
#[tokio::test]
async fn creation_intent_abort_and_adoption_share_the_head_cas() {
    let (_, _, engine, _) = support::setup().await;
    let repository = awaken_session_store::ObjectManagedSessionRepository::new(Arc::new(
        engine.component_store("managed_session").unwrap(),
    ));
    repository
        .record_creation_intent("tenant", &"intent".into(), "resource", 100)
        .await
        .unwrap();
    let PreparedParticipantIntentClaim::Claimed(intent) = repository
        .claim_creation_abort(&"intent".into())
        .await
        .unwrap()
    else {
        panic!("intent missing")
    };
    assert_eq!(intent.repository_ids(), &["resource".to_string()]);
    repository
        .complete_creation_abort(&"intent".into())
        .await
        .unwrap();
    let root = session_store::session("intent", "aborted");
    let id = IdempotencyRecord {
        key: "create".into(),
        payload_hash: "hash".into(),
    };
    assert!(
        repository
            .create("tenant", root, id.clone(), vec![])
            .await
            .is_err()
    );
    repository
        .record_creation_intent("tenant", &"adopt".into(), "resource", 100)
        .await
        .unwrap();
    assert!(matches!(
        repository
            .create(
                "tenant",
                session_store::session("adopt", "adopt"),
                id,
                vec![]
            )
            .await
            .unwrap(),
        SessionCreateResult::Applied(_)
    ));
    assert!(matches!(
        repository
            .claim_creation_abort(&"adopt".into())
            .await
            .unwrap(),
        PreparedParticipantIntentClaim::Absent
    ));
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_takeover_fences_a_staged_session_root_mutation_at_the_same_cas() {
    use awaken_dispatch_contract::{
        ClaimAuthority, DispatchClaims as _, DispatchCommitAuthority as _, DispatchEnqueue as _,
    };
    use std::sync::atomic::Ordering;
    let (objects, clock, engine, _) = support::setup().await;
    let repository = Arc::new(repository(engine.clone()));
    let race = session_store::prepare_independent_race(repository.as_ref(), "managed").await;
    let queue = Arc::new(awaken_dispatch::ObjectDispatchPartition::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "ws_a".into(),
        "managed".into(),
        clock.clone(),
    ));
    queue
        .enqueue(support::dispatch("root-run", "managed", "managed"))
        .await
        .unwrap();
    let old = awaken_worker_contract::RunClaim::from(
        &queue
            .claim("worker", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let ClaimAuthority::Conditional(grant) =
        queue.acquire_commit_authority(&old).await.unwrap().unwrap()
    else {
        panic!("conditional authority required")
    };
    let mutation = race.commands[0].mutation.clone();
    let writing_repository = repository.clone();
    let writing_mutation = mutation.clone();
    objects.hold_next_head.store(true, Ordering::SeqCst);
    let write = tokio::spawn(async move {
        writing_repository
            .commit_conditionally("ws_a", writing_mutation, &grant.condition)
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        objects.head_entered.notified(),
    )
    .await
    .expect("prepared root mutation must reach the held head CAS");
    clock.advance(1000);
    let successor = awaken_worker_contract::RunClaim::from(
        &queue
            .claim("replacement", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    objects.head_resume.notify_one();
    assert!(write.await.unwrap().is_err());
    use awaken_session_contract::SessionRepositoryReads as _;
    assert_eq!(
        repository
            .get(&race.session_id)
            .await
            .unwrap()
            .title
            .as_deref(),
        Some("before")
    );
    assert!(
        repository
            .idempotency_receipt(&race.session_id, &race.commands[0].key)
            .await
            .unwrap()
            .is_none()
    );
    let ClaimAuthority::Conditional(grant) = queue
        .acquire_commit_authority(&successor)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("conditional authority required")
    };
    repository
        .commit_conditionally("ws_a", mutation, &grant.condition)
        .await
        .unwrap();
    assert_eq!(
        repository
            .get(&race.session_id)
            .await
            .unwrap()
            .title
            .as_deref(),
        Some(race.commands[0].title.as_str())
    );
}
