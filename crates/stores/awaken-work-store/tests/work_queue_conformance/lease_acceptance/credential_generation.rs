//! A credential principal and a process incarnation are different identities.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_credential_session_reclaim_fences_the_exact_generation() {
    gap_backends(
        "same credential crossed an exact generation",
        async |pair: &Pair| {
            let env = "exact-same-credential";
            let (work, old) = claimed_work(pair, env, true, 0).await;
            let old = old.unwrap();
            let fresh_digest = session_access_digest(902);
            pair.clock.set(LEASE_TTL_MS);
            assert!(claim_as(&pair.b, env, "owner", fresh_digest.clone()).await);
            let current = access(&pair.b, &fresh_digest).await.unwrap();
            assert_eq!(current.lease_owner, old.lease_owner);
            assert_eq!(current.work_id, old.work_id);
            assert!(current.lease_epoch > old.lease_epoch);
            assert!(access(&pair.a, &session_access_digest(901)).await.is_none());
            let before = pair.a.get(env, &work).await.unwrap().unwrap();
            pair.clock.set(LEASE_TTL_MS + 1);
            let reads = pair.clock.observations();
            let mut correct = matches!(
                pair.a.ack_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= matches!(
                pair.a
                    .heartbeat_with_session_access(&old, LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::PreconditionFailed
            );
            correct &= matches!(
                pair.a.stop_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= pair.clock.observations() == reads;
            correct &= same_item(&before, &pair.a.get(env, &work).await.unwrap().unwrap());
            correct &= access(&pair.a, &fresh_digest).await.as_ref() == Some(&current);

            // Positive controls are essential: refusing every request is not fencing.
            let acknowledged = pair
                .b
                .ack_with_session_access(&current)
                .await
                .unwrap()
                .into_item()
                .unwrap();
            correct &= acknowledged.acknowledged_at
                == Some(awaken_time::utc::epoch_millis_to_rfc3339(LEASE_TTL_MS + 1));
            correct &= matches!(
                pair.a.ack_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            pair.clock.set(LEASE_TTL_MS + 2);
            correct &= matches!(
                pair.b
                    .heartbeat_with_session_access(&current, LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::Accepted(_)
            );
            let renewed = access(&pair.a, &fresh_digest).await.unwrap();
            correct &= renewed.lease_epoch == current.lease_epoch;
            correct &= renewed.expires_at_unix_ms > current.expires_at_unix_ms;
            pair.clock.set(LEASE_TTL_MS + 3);
            let stopped = pair
                .b
                .stop_with_session_access(&renewed)
                .await
                .unwrap()
                .into_item()
                .unwrap();
            correct &= stopped.state == WorkState::Stopped;
            correct &= matches!(
                pair.a.stop_with_session_access(&old).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= pair
                .b
                .stop_with_session_access(&renewed)
                .await
                .unwrap()
                .into_item()
                .is_some_and(|retry| same_item(&retry, &stopped));
            correct &= access(&pair.a, &fresh_digest).await.is_none();
            correct &= !pair.a.has_owner_claims("owner").await.unwrap();
            correct
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_only_reclaim_preserves_the_credential_principal_boundary() {
    gap_backends(
        "credential boundary or current holder control failed",
        async |pair: &Pair| {
            let env = "owner-only-credential";
            let work = pair.a.enqueue_healthcheck(env).await.unwrap();
            pair.clock.set(0);
            assert_eq!(
                pair.a.claim(env, "principal").await.unwrap().unwrap().id,
                work
            );
            pair.clock.set(LEASE_TTL_MS);
            assert_eq!(
                pair.b.claim(env, "principal").await.unwrap().unwrap().id,
                work
            );
            pair.clock.set(LEASE_TTL_MS + 1);
            let before = pair.a.get(env, &work).await.unwrap().unwrap();
            let mut correct = matches!(
                pair.a.ack(env, &work, "foreign").await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= matches!(
                pair.a
                    .heartbeat(env, &work, "foreign", LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::PreconditionFailed
            );
            correct &= matches!(
                pair.a.stop(env, &work, "foreign").await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= same_item(&before, &pair.a.get(env, &work).await.unwrap().unwrap());
            // The supported current holder presents this tuple. An old process
            // holding the same credential can present the identical tuple: this
            // compatibility port cannot distinguish those two requests.
            correct &= pair
                .b
                .ack(env, &work, "principal")
                .await
                .unwrap()
                .is_accepted();
            pair.clock.set(LEASE_TTL_MS + 2);
            correct &= matches!(
                pair.b
                    .heartbeat(env, &work, "principal", LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::Accepted(_)
            );
            correct &= pair
                .b
                .stop(env, &work, "principal")
                .await
                .unwrap()
                .is_accepted();
            correct &= pair
                .a
                .stop(env, &work, "principal")
                .await
                .unwrap()
                .is_accepted();
            correct &= matches!(
                pair.a.stop(env, &work, "foreign").await.unwrap(),
                WorkMutationResult::PreconditionFailed
            );
            correct &= !pair.a.has_owner_claims("principal").await.unwrap();
            correct
        },
    )
    .await;
}
