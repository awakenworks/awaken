// Operational-feed and exact/parent-mediated claim conformance rules. Included
// at crate root so no parallel conformance surface is introduced.
/// Verify the durable operational feed independently from Run lifecycle truth.
///
/// The backend under test must be fresh enough that the supplied namespace does
/// not collide with live rows; pre-existing feed events are handled by taking an
/// initial cursor.
pub async fn assert_dispatch_operational_feed_conformance<S>(store: &S, namespace: &str)
where
    S: ControlDispatchQueue + DispatchOperationalFeed + ?Sized,
{
    assert_dispatch_operational_feed_conformance_with_clock(
        store,
        namespace,
        &AuthoritativeWallClock,
    )
    .await;
}

/// Prove that an operator can quarantine a dispatch whose physical predecessor
/// vanished without acknowledging quiescence, and only then.
///
/// This reproduces the exact durable shape observed in a deployed cluster: the
/// Run is committed terminal, cancellation intent is recorded, and the physical
/// attempt slot still names a Worker incarnation that will never return. Before
/// the shared kernel gained `quarantine_admits_occupation`, such a row could not
/// be settled (fenced by the slot), could not admit a successor (blocked by the
/// slot), and could not be quarantined (filtered out) — so it re-leased forever
/// while holding execution capacity it could never use.
///
/// Causes: C1 the physical attempt slot is occupied by a predecessor;
/// C2 cancellation intent is recorded; C3 the lease is expired at/above the
/// retry budget.
/// Effects: E1 C1+C2+C3 quarantines exactly that row; E2 C1+C3 without C2 is
/// untouched, because an uncancelled predecessor may still legitimately finish;
/// E3 quarantine never clears the slot, so a returning predecessor can still
/// acknowledge its own quiescence exactly.
///
/// R3..R6 cover the neighbouring dispositions that share this slot guard, so a
/// later change cannot widen one of them by accident:
/// E4 execution stays blocked, E5 automatic terminalization stays blocked
/// (it would fabricate Run outcome truth), E6 the sweep is idempotent, and
/// E7 a quarantined row never re-enters execution.
///
/// | Rule | C1 slot | C2 cancel | C3 expired+exhausted | Effect |
/// |---|---|---|---|---|
/// | R1 | occupied | yes | yes | E1 dead-lettered |
/// | R2 | occupied | no  | yes | E2 retained as running |
/// | R3 | occupied | yes | yes | E4 successor execution still Blocked |
/// | R4 | occupied | yes | yes | E5 `claim_retry_exhausted` still finds nothing |
/// | R5 | occupied | yes | already swept | E6 second sweep is a no-op |
/// | R6 | occupied | yes | dead-lettered | E7 requeue and claim both refused |
pub async fn assert_occupied_slot_quarantine_conformance_with_clock<S>(
    store: &S,
    namespace: &str,
    clock: &dyn ConformanceClock,
) where
    S: DispatchQueue + ?Sized,
{
    // Both cases need a row that has actually burned its retry budget, so each
    // one claims, loses the lease, and is recovered by a successor. Only the
    // recovery re-claim spends retry budget, exactly as the deployed row did.

    // R2: an occupied slot without cancellation intent stays untouched.
    let retained = run_id(namespace, "occupied-retained");
    store
        .enqueue(dispatch(
            namespace,
            "occupied-retained",
            "occupied-retained-thread",
        ))
        .await
        .expect("enqueue retained run");
    let retained_first = store
        .claim_run(
            &retained,
            "occupied-a",
            LEASE_MS,
            1_000,
            &Default::default(),
        )
        .await
        .expect("retained claim")
        .expect("retained run is runnable");
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&retained_first.lease), 1_000, None)
            .await
            .expect("install retained physical attempt"),
        AttemptAdmission::Applied,
        "R2 predecessor occupies the slot"
    );
    clock.advance_past(retained_first.lease.expires_ms).await;
    let retained_recovered = store
        .claim_run(
            &retained,
            "occupied-a2",
            LEASE_MS,
            retained_first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("retained recovery")
        .expect("retained run is recoverable");
    clock
        .advance_past(retained_recovered.lease.expires_ms)
        .await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(1, retained_recovered.lease.expires_ms.saturating_add(1))
            .await
            .expect("quarantine sweep without cancellation intent"),
        0,
        "R2/E2 an uncancelled occupied row is never quarantined"
    );

    // R1: the same shape with recorded cancellation intent is quarantinable.
    // Cancelling before the first claim records the intent without revoking a
    // lease, so the successor's recovery re-claim reproduces the deployed row:
    // running, cancellation recorded, slot naming an incarnation that never
    // returned.
    let stuck = run_id(namespace, "occupied-stuck");
    store
        .enqueue(dispatch(
            namespace,
            "occupied-stuck",
            "occupied-stuck-thread",
        ))
        .await
        .expect("enqueue stuck run");
    store.cancel(&stuck).await.expect("record cancel intent");
    let stuck_first = store
        .claim_run(&stuck, "occupied-b", LEASE_MS, 5_000, &Default::default())
        .await
        .expect("stuck claim")
        .expect("a cancelled row is still claimable");
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&stuck_first.lease), 5_000, None)
            .await
            .expect("install stuck physical attempt"),
        AttemptAdmission::Applied,
        "R1 predecessor occupies the slot"
    );
    clock.advance_past(stuck_first.lease.expires_ms).await;
    let stuck_successor = store
        .claim_run(
            &stuck,
            "occupied-c",
            LEASE_MS,
            stuck_first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("successor recovery")
        .expect("the stuck run is recoverable");
    clock.advance_past(stuck_successor.lease.expires_ms).await;
    let sweep_now = stuck_successor.lease.expires_ms.saturating_add(1);

    // R3/E4: the successor is still refused physical execution. Quarantine is
    // reachable precisely because it grants nobody the executor.
    // The refusal reason depends on whether the successor's own lease is still
    // live (Blocked) or has since expired (Fenced). The invariant under test is
    // that neither ever grants the executor.
    let successor_admission = store
        .begin_attempt(&RunClaim::from(&stuck_successor.lease), sweep_now, None)
        .await
        .expect("successor admission is decidable");
    assert!(
        matches!(
            successor_admission,
            AttemptAdmission::Blocked | AttemptAdmission::Fenced
        ),
        "R3/E4 an occupied slot never grants execution, got {successor_admission:?}"
    );

    // R4/E5: automatic maintenance must stay blocked. `claim_retry_exhausted`
    // commits `Ended(Indeterminate)` through the Worker/`Done` path, so it would
    // fabricate Run outcome truth while an opaque predecessor may still be
    // running (ADR-0065). Only the explicit operator hatch may proceed. This is
    // the asymmetry the fix depends on; widening it here would be a regression.
    assert!(
        store
            .claim_retry_exhausted("occupied-auto", LEASE_MS, sweep_now, 1)
            .await
            .expect("automatic retry exhaustion is decidable")
            .is_none(),
        "R4/E5 automatic terminalization never reaches an occupied slot"
    );

    assert_eq!(
        store
            .quarantine_retry_exhausted(1, sweep_now)
            .await
            .expect("quarantine the stuck run"),
        1,
        "R1/E1 the operator escape hatch reaches a vanished predecessor"
    );

    // R5/E6: the sweep is idempotent — a dead-lettered row is not swept twice.
    assert_eq!(
        store
            .quarantine_retry_exhausted(1, sweep_now.saturating_add(1))
            .await
            .expect("second quarantine sweep"),
        0,
        "R5/E6 quarantine never dead-letters the same row twice"
    );

    // R6/E7: the quarantined row can never re-enter execution. Cancellation is
    // terminal intent, so requeue is refused and the row cannot be handed back
    // to a claimant while its predecessor remains unaccounted for.
    assert!(
        !store.requeue(&stuck).await.expect("requeue is decidable"),
        "R6/E7 a cancelled dead letter is never requeued into execution"
    );
    assert!(
        store
            .claim_run(
                &stuck,
                "occupied-d",
                LEASE_MS,
                sweep_now.saturating_add(2),
                &Default::default(),
            )
            .await
            .expect("claiming a dead letter is decidable")
            .is_none(),
        "R6/E7 a quarantined row leaves the claim pool for good"
    );
    // E3: the slot survives quarantine, so physical truth is never fabricated.
    assert_eq!(
        store
            .finish_attempt(&RunClaim::from(&stuck_first.lease))
            .await
            .expect("a returning predecessor still settles its own slot"),
        SettleOutcome::Applied,
        "R1/E3 quarantine never cleared the physical slot"
    );
}

/// Clock-explicit variant used by deterministic reference backends.
pub async fn assert_dispatch_operational_feed_conformance_with_clock<S>(
    store: &S,
    namespace: &str,
    clock: &dyn ConformanceClock,
) where
    S: ControlDispatchQueue + DispatchOperationalFeed + ?Sized,
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
    clock.set(0);
    let first = store
        .claim_run(&settled_run, "operations-a", LEASE_MS, &Default::default())
        .await
        .expect("first operational claim")
        .expect("settled run is runnable");
    clock.advance_past(first.lease.expires_ms).await;
    clock.set(first.lease.expires_ms.saturating_add(1));
    let recovered = store
        .claim_run(&settled_run, "operations-b", LEASE_MS, &Default::default())
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
    clock.set(2_000);
    let dead_first = store
        .claim_run(&dead_run, "operations-c", LEASE_MS, &Default::default())
        .await
        .expect("dead-letter first claim")
        .expect("dead-letter run is runnable");
    clock.advance_past(dead_first.lease.expires_ms).await;
    clock.set(dead_first.lease.expires_ms.saturating_add(1));
    let dead_recovered = store
        .claim_run(&dead_run, "operations-d", LEASE_MS, &Default::default())
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
    clock.set(5_000);
    store
        .claim_run(
            &cancelled_run,
            "operations-e",
            LEASE_MS,
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

async fn local_claims_skip_remote_only_work(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
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

    clock.set(0);
    let local = store
        .claim("conformance-local", LEASE_MS, &Default::default())
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

    let mut manifest = ready_dispatch_worker(ns, "local-credential").manifest;
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
    clock.set(0);
    let remote = store
        .claim_compatible(&worker, LEASE_MS)
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
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    // Cause/effect graph: C0 the durable request is one first-class delegated
    // child with its own Run/Thread, exact origin, and distinct parent-Session
    // affinity; C1 it freezes an exact credential, provider, route, primary and
    // ordered fallback; C2 a fresh claim succeeds; C3 the first lease is live
    // or exactly at its boundary; C4 it is strictly expired and another owner
    // reclaims it. Effects: E0 the child remains a SessionChild with every
    // identity coordinate frozen; E1 C2 returns the complete persisted request;
    // E2 C3 admits no replacement owner; E3 C4 returns the byte-equivalent
    // request under exactly epoch+1; E4 the old epoch is fenced and the new
    // epoch settles.
    //
    // | Rule | Claim state | Time | Effects |
    // | R1 | Pending | fresh | E0+E1 |
    // | R2 | Leased | live/equal deadline | E2 |
    // | R3 | Leased | strictly expired | E3+E4 |
    //
    // Constraint: Memory, SQLite and PostgreSQL invoke this one table; the
    // stored request is the sole binding authority and no retry-time resolver
    // or backend-specific projection is permitted.
    clock.set(0);
    let target = run_id(ns, "target");
    let unrelated = run_id(ns, "unrelated");
    store
        .enqueue(dispatch(ns, "unrelated", "unrelated-thread"))
        .await
        .expect("enqueue unrelated conformance run");
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let capabilities = CredentialRealizationCapabilities {
        holders: [holder.clone()].into_iter().collect(),
        material_sources: [CredentialMaterialSource::ControlPlaneReference]
            .into_iter()
            .collect(),
        realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
            .into_iter()
            .collect(),
        recipient_bound_envelopes: false,
        envelope_required: false,
        extension_consumers: Default::default(),
        alternatives: Vec::new(),
    };
    let mut frozen = credential_dispatch(ns, "target", "target-thread", &holder);
    frozen.activation.delegation_origin = Some(DelegationOrigin::root_for_agent(
        run_id(ns, "parent-run"),
        format!("{ns}-parent-call"),
        format!("{ns}-parent-agent"),
    ));
    frozen = frozen.for_session(thread_id(ns, "parent-session-thread"));
    assert_eq!(
        frozen.admission_shape(),
        DispatchAdmissionShape::SessionChild,
        "C0/E0"
    );
    frozen.activation.snapshot.resolved_spec.model_candidates = vec![credential_candidate(
        ns,
        &format!("{ns}-fallback-provider"),
        "fallback-model",
        &format!("{ns}-fallback-credential"),
        &format!("{ns}-fallback-route@2"),
        &holder,
    )];
    store
        .enqueue(frozen.clone())
        .await
        .expect("enqueue exact-claim target");
    if let Some(depth) = store.runnable_depth().await.expect("query runnable depth") {
        assert_eq!(depth, 2, "both freshly enqueued runs are claimable");
    }

    let first = store
        .claim_run(&target, "conformance-a", LEASE_MS, &capabilities)
        .await
        .expect("exact claim succeeds")
        .expect("target is runnable");
    assert_eq!(first.request, frozen, "R1/E1 exact frozen request");
    assert_eq!(first.lease.owner, "conformance-a");
    assert!(!first.recovered, "a fresh exact claim is not recovery");
    assert!(first.lease.epoch > 0, "a claimed lease has a fence epoch");
    assert_eq!(
        first.credential_bindings.len(),
        2,
        "R1/E1 primary and fallback receive exact attempt bindings"
    );
    assert!(
        first
            .credential_bindings
            .iter()
            .all(|binding| binding.claim_epoch == first.lease.epoch),
        "R1/E1 every attempt binding is fenced by the fresh claim"
    );

    let other = store
        .claim("conformance-pool", LEASE_MS, &Default::default())
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
                .claim_run(&target, "conformance-b", LEASE_MS, &capabilities,)
                .await
                .expect("live-boundary claim")
                .is_none(),
            "a lease remains live at its exact expiry boundary"
        );
    }
    clock.advance_past(first.lease.expires_ms).await;
    clock.set(first.lease.expires_ms.saturating_add(1));
    let recovered = store
        .claim_run(&target, "conformance-b", LEASE_MS, &capabilities)
        .await
        .expect("recovery claim succeeds")
        .expect("expired target is recoverable");
    assert!(recovered.recovered, "an expired running lease is recovery");
    assert_eq!(recovered.request, first.request, "R3/E3 frozen request");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(
        recovered
            .credential_bindings
            .iter()
            .map(|binding| (&binding.candidate_fingerprint, &binding.credential))
            .collect::<Vec<_>>(),
        first
            .credential_bindings
            .iter()
            .map(|binding| (&binding.candidate_fingerprint, &binding.credential))
            .collect::<Vec<_>>(),
        "R3/E3 reclaim retains the ordered candidate and credential identities"
    );
    assert!(
        recovered
            .credential_bindings
            .iter()
            .all(|binding| binding.claim_epoch == recovered.lease.epoch),
        "R3/E4 reclaim re-fences every exact binding to the new epoch"
    );
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
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    // Cause/effect graph: C1 one delegated child carries its own Run/Thread,
    // durable origin and distinct parent-Session affinity; C2 its parent creates
    // and exact-claims it while the pool races; C3 the child awaits and receives
    // one correlated input. Effects: E1 creation and lease are atomic over the
    // complete frozen child; E2 the pool cannot interleave; E3 input delivery
    // and resume claim are atomic and expose exactly that input. Decision rules:
    // R1=C1+C2=>E1+E2; R2=R1+Awaiting+C3=>E2+E3.
    clock.set(10_000);
    let child = run_id(ns, "child");
    let mut child_dispatch = dispatch(ns, "child", "child-thread");
    child_dispatch.activation.delegation_origin = Some(DelegationOrigin::root_for_agent(
        run_id(ns, "parent-run"),
        format!("{ns}-parent-call"),
        format!("{ns}-parent-agent"),
    ));
    child_dispatch = child_dispatch.for_session(thread_id(ns, "parent-session-thread"));
    assert_eq!(
        child_dispatch.admission_shape(),
        DispatchAdmissionShape::SessionChild,
        "C1"
    );
    let claimed = store
        .claim_new_run(
            child_dispatch.clone(),
            "conformance-parent",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim_new_run succeeds")
        .expect("new child is returned already claimed");
    assert_eq!(claimed.request.run_id(), &child);
    assert_eq!(claimed.request, child_dispatch, "R1/E1");
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, &Default::default(),)
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
            &Default::default(),
        )
        .await
        .expect("deliver_and_claim succeeds")
        .expect("awaiting child is returned already claimed");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, message_id);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, &Default::default(),)
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
