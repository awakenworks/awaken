//! Historical stop decisions carry their original subject, never live authority.
use super::*;

async fn stop(
    queue: &Handle,
    env: &str,
    work: &str,
    proof: Option<&WorkSessionAccess>,
) -> WorkMutationResult {
    if let Some(proof) = proof {
        queue.stop_with_session_access(proof).await.unwrap()
    } else {
        queue.stop(env, work, "owner").await.unwrap()
    }
}

pub(super) async fn retry_matrix() {
    gap_backends(
        "stop lost its original subject receipt",
        async |pair: &Pair| {
            let mut correct = true;
            for (index, bound) in [false, true].into_iter().enumerate() {
                let base = index as u64 * LEASE_TTL_MS * 4;
                let env = format!("stop-retry-{index}");
                let (work, proof) = claimed_work(pair, &env, bound, base).await;
                pair.clock.set(base + 100);
                let reads = pair.clock.observations();
                let Some(first) = stop(&pair.a, &env, &work, proof.as_ref()).await.into_item()
                else {
                    return false;
                };
                correct &= pair.clock.observations() == reads + 1;
                correct &= first.state == WorkState::Stopped
                    && first.stopped_at
                        == Some(awaken_time::utc::epoch_millis_to_rfc3339(base + 100))
                    && first.stop_requested_at == first.stopped_at;
                for offset in [200, LEASE_TTL_MS + 1] {
                    pair.clock.set(base + offset);
                    let reads = pair.clock.observations();
                    correct &= stop(&pair.b, &env, &work, proof.as_ref())
                        .await
                        .into_item()
                        .is_some_and(|reply| same_item(&reply, &first));
                    correct &= pair.clock.observations() == reads;
                }
                let reads = pair.clock.observations();
                correct &= matches!(
                    pair.b.stop(&env, &work, "foreign").await.unwrap(),
                    WorkMutationResult::PreconditionFailed
                );
                correct &= matches!(
                    pair.b.stop("foreign-env", &work, "owner").await.unwrap(),
                    WorkMutationResult::NotFound
                );
                correct &= matches!(
                    pair.b.stop(&env, "missing", "owner").await.unwrap(),
                    WorkMutationResult::NotFound
                );
                correct &= matches!(
                    pair.b.ack(&env, &work, "owner").await.unwrap(),
                    WorkMutationResult::PreconditionFailed
                );
                if let Some(proof) = proof {
                    let mut wrong = proof.clone();
                    wrong.lease_epoch = wrong.lease_epoch.next().unwrap();
                    correct &= matches!(
                        pair.b.stop_with_session_access(&wrong).await.unwrap(),
                        WorkMutationResult::PreconditionFailed
                    );
                    correct &= matches!(
                        pair.b
                            .heartbeat_with_session_access(
                                &proof,
                                base + 200,
                                LeaseHeartbeat::unconditional()
                            )
                            .await
                            .unwrap(),
                        HeartbeatResult::PreconditionFailed
                    );
                    correct &= pair
                        .b
                        .authenticate_session_access(&session_access_digest(base + 901), base + 100)
                        .await
                        .unwrap()
                        .is_none();
                }
                correct &= pair.clock.observations() == reads;
                correct &= !pair.b.has_owner_claims("owner").await.unwrap();
                correct &= pair
                    .a
                    .get(&env, &work)
                    .await
                    .unwrap()
                    .is_some_and(|stored| same_item(&stored, &first));
            }
            correct
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stops_share_one_original_receipt() {
    gap_backends(
        "concurrent stop lost the original receipt",
        async |pair: &Pair| {
            let (work, proof) = claimed_work(pair, "concurrent-stop", true, 0).await;
            let proof = proof.unwrap();
            pair.clock.set(100);
            let reads = pair.clock.observations();
            let (left, right, concurrent) = race(
                false,
                {
                    let queue = pair.a.clone();
                    let proof = proof.clone();
                    async move { queue.stop_with_session_access(&proof).await.unwrap() }
                },
                {
                    let queue = pair.b.clone();
                    async move { queue.stop_with_session_access(&proof).await.unwrap() }
                },
            )
            .await;
            let (Some(left), Some(right)) = (left.into_item(), right.into_item()) else {
                return false;
            };
            concurrent == 2
                && same_item(&left, &right)
                && pair.clock.observations() == reads + 1
                && pair
                    .a
                    .get("concurrent-stop", &work)
                    .await
                    .unwrap()
                    .is_some_and(|stored| same_item(&stored, &left))
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_stop_receipts_never_authorize_a_woken_generation() {
    gap_backends(
        "stop receipt crossed a lease generation",
        async |pair: &Pair| {
            let env = "stop-generation";
            let (work, old) = claimed_work(pair, env, true, 0).await;
            let old = old.unwrap();
            pair.clock.set(100);
            assert!(
                pair.a
                    .stop_with_session_access(&old)
                    .await
                    .unwrap()
                    .is_accepted()
            );
            assert_eq!(
                pair.b
                    .wake_session(env, &SessionId::from(env))
                    .await
                    .unwrap(),
                work
            );
            let digest = session_access_digest(902);
            assert!(claim_as(&pair.b, env, "owner", 200, digest.clone()).await);
            let current = access(&pair.b, &digest, 200).await.unwrap();
            assert!(current.lease_epoch > old.lease_epoch);
            let before = pair.a.get(env, &work).await.unwrap().unwrap();
            let reads = pair.clock.observations();
            let mut correct = matches!(
                pair.a.stop_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= pair.clock.observations() == reads;
            correct &= pair
                .a
                .get(env, &work)
                .await
                .unwrap()
                .is_some_and(|stored| same_item(&stored, &before));
            pair.clock.set(300);
            let Some(first) = pair
                .b
                .stop_with_session_access(&current)
                .await
                .unwrap()
                .into_item()
            else {
                return false;
            };
            correct &= first.stopped_at == Some(awaken_time::utc::epoch_millis_to_rfc3339(300));
            let reads = pair.clock.observations();
            correct &= matches!(
                pair.a.stop_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= pair
                .a
                .stop_with_session_access(&current)
                .await
                .unwrap()
                .into_item()
                .is_some_and(|retry| same_item(&retry, &first));
            correct && pair.clock.observations() == reads
        },
    )
    .await;
}

async fn first_stop(
    queue: &Handle,
    clock: &ObservedClock,
) -> (
    String,
    WorkSessionAccess,
    awaken_executable_environment_contract::WorkItem,
) {
    let work = queue
        .enqueue_session("stop-reopen", &SessionId::from("stop-reopen"))
        .await
        .unwrap();
    let digest = session_access_digest(991);
    assert!(claim_as(queue, "stop-reopen", "owner", 0, digest.clone()).await);
    let proof = access(queue, &digest, 0).await.unwrap();
    clock.set(100);
    let first = queue
        .stop_with_session_access(&proof)
        .await
        .unwrap()
        .into_item()
        .unwrap();
    (work, proof, first)
}

async fn reopened_receipt(
    queue: &Handle,
    clock: &ObservedClock,
    work: &str,
    proof: &WorkSessionAccess,
    first: &awaken_executable_environment_contract::WorkItem,
) -> bool {
    clock.set(LEASE_TTL_MS + 1);
    let reads = clock.observations();
    let result = queue
        .stop_with_session_access(proof)
        .await
        .unwrap()
        .into_item();
    result.is_some_and(|retry| same_item(&retry, first))
        && clock.observations() == reads
        && queue
            .authenticate_session_access(&session_access_digest(991), 200)
            .await
            .unwrap()
            .is_none()
        && !queue.has_owner_claims("owner").await.unwrap()
        && queue
            .get("stop-reopen", work)
            .await
            .unwrap()
            .is_some_and(|stored| same_item(&stored, first))
}

#[tokio::test]
async fn sqlite_stop_receipt_survives_reopening_without_restoring_authority() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stop.db");
    let clock = Arc::new(ObservedClock::new());
    let queue: Handle = Arc::new(
        SqliteWorkQueue::open(path.to_str().unwrap(), clock.clone())
            .await
            .unwrap(),
    );
    let (work, proof, first) = first_stop(&queue, &clock).await;
    drop(queue);
    let reopened: Handle = Arc::new(
        SqliteWorkQueue::open(path.to_str().unwrap(), clock.clone())
            .await
            .unwrap(),
    );
    let correct = reopened_receipt(&reopened, &clock, &work, &proof, &first).await;
    drop(reopened);
    assert!(
        correct,
        "SQLite must recover only the original stop receipt"
    );
}

#[tokio::test]
async fn postgres_stop_receipt_survives_a_new_pool_without_restoring_authority() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = WorkPostgresFixture::new(&url, 2).await;
    let clock = Arc::new(ObservedClock::new());
    let queue: Handle = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), clock.clone())
            .await
            .unwrap(),
    );
    let (work, proof, first) = first_stop(&queue, &clock).await;
    drop(queue);
    let reopened: Handle = Arc::new(
        PostgresWorkQueue::connect_existing(&fixture.scoped_url, clock.clone())
            .await
            .unwrap(),
    );
    let correct = reopened_receipt(&reopened, &clock, &work, &proof, &first).await;
    drop(reopened);
    fixture.cleanup().await;
    assert!(
        correct,
        "PostgreSQL must recover only the original stop receipt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_stop_reply_belongs_to_its_committed_lease_snapshot() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    assert!(postgres_mutation_snapshot_matches(&url, SnapshotMutation::Stop, false).await);
    assert!(
        !postgres_mutation_snapshot_matches(&url, SnapshotMutation::Stop, true).await,
        "late-reload mutant must expose the woken successor"
    );
}
