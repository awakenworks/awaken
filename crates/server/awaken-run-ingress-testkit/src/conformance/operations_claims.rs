// Operational-feed and exact/parent-mediated claim conformance rules. Included
// at crate root so no parallel conformance surface is introduced.
/// Verify the durable operational feed independently from Run lifecycle truth.
///
/// The backend under test must be fresh enough that the supplied namespace does
/// not collide with live rows; pre-existing feed events are handled by taking an
/// initial cursor.
pub async fn assert_dispatch_operational_feed_conformance<S>(store: &S, namespace: &str)
where
    S: DispatchQueue + DispatchOperationalFeed + ?Sized,
{
    assert_dispatch_operational_feed_conformance_with_clock(store, namespace, &DirectCommandClock)
        .await;
}

/// Clock-explicit variant used by deterministic reference backends.
pub async fn assert_dispatch_operational_feed_conformance_with_clock<S>(
    store: &S,
    namespace: &str,
    clock: &dyn ConformanceClock,
) where
    S: DispatchQueue + DispatchOperationalFeed + ?Sized,
{
    let baseline = store
        .events_after(DispatchCursor(0), usize::MAX)
        .await
        .expect("read operational baseline");
    let cursor = baseline.next_cursor;

    let settled_run = run_id(namespace, "operations-settled");
    store
        .enqueue(dispatch(
            namespace,
            "operations-settled",
            "operations-settled-thread",
        ))
        .await
        .expect("enqueue settled operational run");
    let first = store
        .claim_run(
            &settled_run,
            "operations-a",
            LEASE_MS,
            0,
            &Default::default(),
        )
        .await
        .expect("first operational claim")
        .expect("settled run is runnable");
    clock.advance_past(first.lease.expires_ms).await;
    let recovered = store
        .claim_run(
            &settled_run,
            "operations-b",
            LEASE_MS,
            first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("operational recovery")
        .expect("expired run is recoverable");
    assert_eq!(
        store
            .settle(&settled_run, first.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("fenced settle verdict"),
        SettleOutcome::Fenced
    );
    assert_eq!(
        store
            .settle(
                &settled_run,
                recovered.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("current settle"),
        SettleOutcome::Applied
    );

    let dead_run = run_id(namespace, "operations-dead-letter");
    store
        .enqueue(dispatch(
            namespace,
            "operations-dead-letter",
            "operations-dead-letter-thread",
        ))
        .await
        .expect("enqueue dead-letter operational run");
    let dead_first = store
        .claim_run(
            &dead_run,
            "operations-c",
            LEASE_MS,
            2_000,
            &Default::default(),
        )
        .await
        .expect("dead-letter first claim")
        .expect("dead-letter run is runnable");
    clock.advance_past(dead_first.lease.expires_ms).await;
    let dead_recovered = store
        .claim_run(
            &dead_run,
            "operations-d",
            LEASE_MS,
            dead_first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("dead-letter recovery")
        .expect("dead-letter run is recoverable");
    clock.advance_past(dead_recovered.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(1, dead_recovered.lease.expires_ms.saturating_add(1),)
            .await
            .expect("manually quarantine exhausted run"),
        1
    );

    let cancelled_run = run_id(namespace, "operations-cancelled");
    store
        .enqueue(dispatch(
            namespace,
            "operations-cancelled",
            "operations-cancelled-thread",
        ))
        .await
        .expect("enqueue cancellation operational run");
    store
        .claim_run(
            &cancelled_run,
            "operations-e",
            LEASE_MS,
            5_000,
            &Default::default(),
        )
        .await
        .expect("cancellation claim")
        .expect("cancellation run is runnable");
    assert_eq!(
        store
            .cancel(&cancelled_run)
            .await
            .expect("cancel leased run"),
        Some(thread_id(namespace, "operations-cancelled-thread"))
    );

    let page = store
        .events_after(cursor, 100)
        .await
        .expect("read operational transitions");
    // Cause-effect graph / decision table:
    // C1=an authority mutation is applied; C2=the mutation is fenced/rejected.
    // R1 (C1,!C2) => one event with a store timestamp; R2 (!C1,C2) => no event.
    // For a sequence of R1 mutations, cursors and store timestamps must both be
    // monotonic. The operation assertions below cover claim, recovery, settle,
    // dead-letter, cancellation, and the fenced-settle R2 case for every store.
    assert!(
        page.events
            .windows(2)
            .all(|pair| pair[0].cursor < pair[1].cursor),
        "dispatch cursors are strictly increasing"
    );
    assert!(
        page.events
            .iter()
            .all(|event| event.recorded_at_ms.is_some()),
        "new durable authority facts always carry store time"
    );
    assert!(
        page.events.windows(2).all(|pair| {
            pair[0].recorded_at_ms.expect("checked above")
                <= pair[1].recorded_at_ms.expect("checked above")
        }),
        "store timestamps are nondecreasing in cursor order"
    );
    let operations = page
        .events
        .iter()
        .map(|event| &event.operation)
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 11, "only applied mutations emit facts");
    assert!(
        matches!(operations[0], DispatchOperation::Claimed { claim } if claim.run_id == settled_run)
    );
    assert!(matches!(
        operations[1],
        DispatchOperation::LeaseLost {
            claim,
            reason: LeaseLossReason::Expired
        } if claim.owner == "operations-a"
    ));
    assert!(matches!(
        operations[2],
        DispatchOperation::Reclaimed { previous, claim }
            if previous.owner == "operations-a" && claim.owner == "operations-b"
    ));
    assert!(matches!(
        operations[3],
        DispatchOperation::Settled {
            claim,
            outcome: DispatchOutcome::Awaiting
        } if claim.owner == "operations-b"
    ));
    assert!(
        matches!(operations[4], DispatchOperation::Claimed { claim } if claim.run_id == dead_run)
    );
    assert!(matches!(
        operations[5],
        DispatchOperation::LeaseLost {
            reason: LeaseLossReason::Expired,
            ..
        }
    ));
    assert!(matches!(operations[6], DispatchOperation::Reclaimed { .. }));
    assert!(matches!(
        operations[7],
        DispatchOperation::LeaseLost {
            reason: LeaseLossReason::RetryExhausted,
            ..
        }
    ));
    assert!(matches!(
        operations[8],
        DispatchOperation::DeadLettered {
            attempt_count: 1,
            ..
        }
    ));
    assert!(matches!(
        operations[9],
        DispatchOperation::Claimed { claim } if claim.run_id == cancelled_run
    ));
    assert!(matches!(
        operations[10],
        DispatchOperation::LeaseLost {
            claim,
            reason: LeaseLossReason::Cancelled
        } if claim.owner == "operations-e"
    ));

    let first_page = store
        .events_after(cursor, 2)
        .await
        .expect("first operational page");
    let second_page = store
        .events_after(first_page.next_cursor, 2)
        .await
        .expect("second operational page");
    assert_eq!(first_page.events.len(), 2);
    assert_eq!(second_page.events.len(), 2);
    assert_eq!(second_page.events[0], page.events[2]);
    let empty = store
        .events_after(page.next_cursor, 0)
        .await
        .expect("zero-sized operational page");
    assert!(empty.events.is_empty());
    assert_eq!(empty.next_cursor, page.next_cursor);
}

async fn local_claims_skip_remote_only_work(store: &dyn DispatchQueue, ns: &str) {
    let required_credential = WorkerCredentialRevision {
        id: format!("{ns}-worker-credential"),
        revision: 7,
    };
    let mut remote_placement = PlacementRequirements::remote_required();
    remote_placement
        .required_capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    remote_placement
        .required_credentials
        .insert(required_credential.clone());

    let remote_id = run_id(ns, "worker-private");
    let local_id = run_id(ns, "local-fallback");
    store
        .enqueue_with(
            dispatch(ns, "worker-private", "worker-private-thread")
                .with_placement(remote_placement),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("enqueue worker-private run");
    store
        .enqueue(dispatch(ns, "local-fallback", "local-fallback-thread"))
        .await
        .expect("enqueue local fallback run");

    let local = store
        .claim("conformance-local", LEASE_MS, 0, &Default::default())
        .await
        .expect("local claim succeeds")
        .expect("local-compatible work is available");
    assert_eq!(
        local.request.run_id(),
        &local_id,
        "a local executor must skip higher-priority remote-only work"
    );
    assert_eq!(
        store
            .settle(&local_id, local.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle local fallback"),
        SettleOutcome::Applied
    );

    let mut manifest = WorkerManifest::default();
    manifest
        .capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-worker"), "boot", 1),
        capability_fingerprint: manifest
            .fingerprint()
            .expect("worker-local manifest fingerprints"),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: [WorkerCredentialObservation::available(
            required_credential,
            0,
            // The matrix asserts exact credential matching, not expiry. Keep
            // the evidence valid under both the logical clocks used by the
            // reference stores and PostgreSQL's authoritative database clock.
            u64::MAX,
        )]
        .into_iter()
        .collect(),
        acp_capability_observations: Default::default(),
        expires_at_ms: u64::MAX,
    };
    let remote = store
        .claim_compatible(&worker, LEASE_MS, 0)
        .await
        .expect("worker-compatible claim succeeds")
        .expect("the exact worker credential revision is available");
    assert_eq!(remote.request.run_id(), &remote_id);
    assert_eq!(
        store
            .settle(&remote_id, remote.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle worker-private run"),
        SettleOutcome::Applied
    );
}

async fn exact_claim_recovery_and_fencing(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    clock.set(0);
    let target = run_id(ns, "target");
    let unrelated = run_id(ns, "unrelated");
    store
        .enqueue(dispatch(ns, "unrelated", "unrelated-thread"))
        .await
        .expect("enqueue unrelated conformance run");
    store
        .enqueue(dispatch(ns, "target", "target-thread"))
        .await
        .expect("enqueue exact-claim target");
    if let Some(depth) = store.runnable_depth(0).await.expect("query runnable depth") {
        assert_eq!(depth, 2, "both freshly enqueued runs are claimable");
    }

    let first = store
        .claim_run(&target, "conformance-a", LEASE_MS, 0, &Default::default())
        .await
        .expect("exact claim succeeds")
        .expect("target is runnable");
    assert_eq!(first.request.run_id(), &target);
    assert_eq!(first.lease.owner, "conformance-a");
    assert!(!first.recovered, "a fresh exact claim is not recovery");
    assert!(first.lease.epoch > 0, "a claimed lease has a fence epoch");
    assert!(
        first.credential_bindings.is_empty(),
        "A1 a credential-free publication carries no attempt binding"
    );

    let other = store
        .claim("conformance-pool", LEASE_MS, 0, &Default::default())
        .await
        .expect("general claim succeeds")
        .expect("exact claim leaves unrelated work available");
    assert_eq!(other.request.run_id(), &unrelated);
    assert_eq!(
        store
            .settle(&unrelated, other.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle unrelated"),
        SettleOutcome::Applied
    );

    if clock.exact_boundary_is_controllable() {
        clock.set(first.lease.expires_ms);
        assert!(
            store
                .claim_run(
                    &target,
                    "conformance-b",
                    LEASE_MS,
                    first.lease.expires_ms,
                    &Default::default(),
                )
                .await
                .expect("live-boundary claim")
                .is_none(),
            "a lease remains live at its exact expiry boundary"
        );
    }
    clock.advance_past(first.lease.expires_ms).await;
    let recovered = store
        .claim_run(
            &target,
            "conformance-b",
            LEASE_MS,
            first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("recovery claim succeeds")
        .expect("expired target is recoverable");
    assert!(recovered.recovered, "an expired running lease is recovery");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(
        store
            .settle(&target, first.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale settle returns a verdict"),
        SettleOutcome::Fenced,
        "the old epoch cannot settle a recovered run"
    );
    assert_eq!(
        store
            .settle(&target, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("current settle"),
        SettleOutcome::Applied
    );
}

async fn parent_mediated_commands_are_atomic(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    clock.set(10_000);
    let child = run_id(ns, "child");
    let claimed = store
        .claim_new_run(
            dispatch(ns, "child", "child-thread"),
            "conformance-parent",
            LEASE_MS,
            10_000,
            &Default::default(),
        )
        .await
        .expect("claim_new_run succeeds")
        .expect("new child is returned already claimed");
    assert_eq!(claimed.request.run_id(), &child);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_000, &Default::default(),)
            .await
            .expect("pool claim after atomic admission")
            .is_none(),
        "the pool cannot interleave between child enqueue and exact claim"
    );
    assert_eq!(
        store
            .settle(&child, claimed.lease.epoch, DispatchOutcome::Awaiting, &[],)
            .await
            .expect("child enters awaiting"),
        SettleOutcome::Applied
    );

    clock.set(10_001);
    let message_id = format!("{ns}-child-answer");
    let resumed = store
        .deliver_and_claim(
            PendingInput {
                message_id: message_id.clone(),
                run_id: child.clone(),
                thread_id: thread_id(ns, "child-thread"),
                correlation_id: format!("{ns}-approval"),
                available_at_ms: None,
                context_messages: Vec::new(),
                result: ResumeResult::Input("approved".to_string()),
            },
            "conformance-parent",
            LEASE_MS,
            10_001,
            &Default::default(),
        )
        .await
        .expect("deliver_and_claim succeeds")
        .expect("awaiting child is returned already claimed");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, message_id);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_001, &Default::default(),)
            .await
            .expect("pool claim after atomic delivery")
            .is_none(),
        "the pool cannot interleave between input delivery and exact claim"
    );
    assert_eq!(
        store
            .settle(
                &child,
                resumed.lease.epoch,
                DispatchOutcome::Done,
                &[message_id],
            )
            .await
            .expect("finish child"),
        SettleOutcome::Applied
    );
}
