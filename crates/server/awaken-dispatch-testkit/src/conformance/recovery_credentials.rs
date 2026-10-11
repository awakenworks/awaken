// @formal evidence credential_effect.boundary_preserves_exact_materialization_authority
// @formal evidence ingress.epoch_reclaim
// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Static owner: recovery-credential conformance cases. Dynamic path: the tooling-only
// @formal reason: testkit includes this exact file to exercise claimed recovery; it does not resolve
// @formal reason: credentials in production. Evidence:
// @formal reason: crates/server/awaken-dispatch-testkit/src/lib.rs.
// Retry, identity, terminal recovery, and credential conformance rules. Included
// at crate root; every rule still exercises the one DispatchQueue contract.
/// Retry-exhaustion cause/effect decision table shared by every backend:
/// C1 state=Leased; C2 lease expired (the Expiry law: from `now >= expiry` on);
/// C3 attempts>=max. R1 !C1 => no claim; R2 C1+!C2 (one instant before expiry)
/// => no claim; R3 C1+C2+!C3 => no claim;
/// R4 C1+C2+C3 => mint one newer ordinary Claimed epoch without execution
/// credentials; R5 two concurrent claimants for one eligible row => exactly one
/// receives the unique newer epoch and one receives None; R6 the losing/stale
/// epoch is fenced while the current claim settles Done.
async fn retry_exhaustion_claim_is_atomic_and_policy_exact(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let request = dispatch(ns, "retry-exhaustion", "retry-exhaustion-thread");
    let run_id = request.run_id().clone();
    store
        .enqueue(request)
        .await
        .expect("enqueue retry-exhaustion row");
    clock.set(0);
    assert!(
        store
            .claim_retry_exhausted("terminal", LEASE_MS, 1)
            .await
            .expect("R1 query")
            .is_none(),
        "R1"
    );
    clock.set(10_000);
    let fresh = store
        .claim_run(&run_id, "crashed-a", LEASE_MS, &Default::default())
        .await
        .expect("fresh claim")
        .expect("fresh row claimable");
    // A controllable clock judges C2 at the exact boundary: live one instant
    // before the expiry, expired at it. A moving clock is advanced past it.
    let exact = clock.exact_boundary_is_controllable();
    if exact {
        clock.set(fresh.lease.expires_ms() - 1);
        assert!(
            store
                .claim_retry_exhausted("terminal", LEASE_MS, 0)
                .await
                .expect("R2 query")
                .is_none(),
            "R2"
        );
        clock.set(fresh.lease.expires_ms());
    } else {
        clock.advance_past(fresh.lease.expires_ms()).await;
    }
    assert!(
        store
            .claim_retry_exhausted("terminal", LEASE_MS, 1,)
            .await
            .expect("R3 query")
            .is_none(),
        "R3"
    );
    let recovered = store
        .claim_run(&run_id, "crashed-b", LEASE_MS, &Default::default())
        .await
        .expect("ordinary recovery")
        .expect("ordinary recovery is claimable");
    // R4 at the exact expiry of the recovered lease when the clock allows it.
    let terminal_now = recovered.lease.expires_ms();
    if exact {
        clock.set(terminal_now);
    } else {
        clock.advance_past(terminal_now).await;
    }
    let (left, right) = tokio::join!(
        store.claim_retry_exhausted("terminal", LEASE_MS, 1),
        store.claim_retry_exhausted("terminal-racer", LEASE_MS, 1),
    );
    let left = left.expect("R5 left query");
    let right = right.expect("R5 right query");
    assert_ne!(left.is_some(), right.is_some(), "R5 one winner");
    let terminal = left.or(right).expect("R4/R5 exhausted row claimable");
    assert!(terminal.recovered, "R4");
    assert!(terminal.credential_bindings.is_empty(), "R4");
    assert_eq!(terminal.lease.run_id(), &run_id, "R5 unique row");
    assert_eq!(
        terminal.lease.epoch(),
        recovered.lease.epoch() + 1,
        "R5 unique newer epoch"
    );
    clock.set(terminal_now);
    assert!(
        store
            .claim_retry_exhausted("terminal-third", LEASE_MS, 1)
            .await
            .expect("R5 barrier query")
            .is_none(),
        "R5"
    );
    assert_eq!(
        store
            .settle(&run_id, recovered.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("R6 stale settle"),
        SettleOutcome::Fenced,
        "R6"
    );
    assert_eq!(
        store
            .settle(
                terminal.lease.run_id(),
                terminal.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("R6 settle"),
        SettleOutcome::Applied,
        "R6"
    );
}

/// Caller-owned Run-id cause/effect table. C1 same RunId; C2 live/completed;
/// C3 canonical dispatch same/different; C4 traceparent same/different; C5 the
/// incoming collision is ineligible for the local/selected Worker; C6 two
/// different payloads race before either row exists. Effects:
/// E1 same payload replays, including exact claim of a runnable live row; E2 a
/// collision fails before mutation; E3 trace-only change remains observational.
/// Rules: I1=C1+C2+C3(same)=>E1; I2=C1+C2+C3(diff)=>E2;
/// I3=I1+C4(diff)=>E1+E3; I4=I2+C5=>E2 (identity precedes eligibility);
/// I5=C1+C3(diff)+C6=>exactly one admission and one rejection.
/// Both live and tombstone phases exercise every backend.
async fn caller_owned_run_identity_is_exact(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let request = dispatch(ns, "caller-owned", "caller-owned-thread");
    let run_id = request.run_id().clone();
    store
        .enqueue(request.clone())
        .await
        .expect("I1 live enqueue");

    let mut retraced = request.clone();
    retraced.traceparent = Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into());
    clock.set(90_000);
    let claim = store
        .claim_new_run(
            retraced.clone(),
            "caller-owned-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("I3 live replay")
        .expect("I1 exact live row remains claimable");

    let mut collision = request.clone();
    collision.activation.thread_id = ThreadId(format!("{ns}-collision-thread"));
    collision.placement = PlacementRequirements::remote_required();
    assert!(store.enqueue(collision.clone()).await.is_err(), "I2 live");
    clock.set(89_999);
    assert!(
        store
            .claim_new_run(
                collision.clone(),
                "ineligible-local",
                LEASE_MS,
                &Default::default(),
            )
            .await
            .is_err(),
        "I4 local eligibility cannot mask collision"
    );
    let manifest = WorkerManifest::default();
    let ineligible_worker = WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-ineligible"), "boot", 1),
        capability_fingerprint: manifest.fingerprint().expect("manifest fingerprints"),
        manifest,
        state: WorkerState::Draining,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        expires_at_ms: 100_000,
    };
    clock.set(89_999);
    assert!(
        store
            .claim_new_run_compatible(collision.clone(), &ineligible_worker, LEASE_MS)
            .await
            .is_err(),
        "I4 Worker eligibility cannot mask collision"
    );

    assert_eq!(
        store
            .settle(&run_id, claim.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("settle caller-owned run"),
        SettleOutcome::Applied
    );
    store.enqueue(retraced).await.expect("I3 tombstone replay");
    assert!(store.enqueue(collision).await.is_err(), "I2 tombstone");

    let left = dispatch(ns, "caller-race", "caller-race-left");
    let mut right = left.clone();
    right.activation.thread_id = ThreadId(format!("{ns}-caller-race-right"));
    let (left_result, right_result) =
        tokio::join!(store.enqueue(left.clone()), store.enqueue(right.clone()));
    assert_ne!(left_result.is_ok(), right_result.is_ok(), "I5");
    let winner = if left_result.is_ok() { left } else { right };
    let winner_id = winner.run_id().clone();
    clock.set(91_000);
    let winner_claim = store
        .claim_new_run(winner, "caller-race-worker", LEASE_MS, &Default::default())
        .await
        .expect("I5 winner replay")
        .expect("I5 winner remains claimable");
    assert_eq!(
        store
            .settle(
                &winner_id,
                winner_claim.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle I5 winner"),
        SettleOutcome::Applied
    );
}

/// Deferred-terminal recovery decision table. Causes: C1 a quiescent Pending
/// target has an exact claim whose settlement was deferred; C2 retry deadline is
/// not due or due; C3 another owner uses ordinary claim or terminal recovery.
/// Effects: E1 neither path may claim before the deadline; E2 an ordinary
/// Worker may reclaim at a higher epoch after it; E3 the former owner is fenced.
/// Constraint: the same persisted retry deadline governs both claim verbs; an
/// undeferred Pending row remains immediately repairable under T3 below.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// |---|---|---|---|---|
/// | D1 | yes | not due | terminal/ordinary | E1 |
/// | D2 | yes | due | ordinary | E2 |
/// | D3 | yes | due, B claimed | former owner | E3 |
async fn deferred_pending_terminal_recovery_obeys_retry_deadline(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    clock.set(120_000);
    let request = dispatch(ns, "deferred-terminal", "deferred-terminal-thread");
    let run_id = request.run_id().clone();
    store.enqueue(request).await.expect("D1 enqueue");
    let first = store
        .claim_run(&run_id, "deferred-first", LEASE_MS, &Default::default())
        .await
        .expect("D1 claim")
        .expect("D1 initial claim");
    let old_epoch = first.lease.epoch();
    assert_eq!(
        store
            .defer_claim(&RunClaim::from(&first.lease), 1_000)
            .await
            .expect("D1 defer exact claim"),
        SettleOutcome::Applied
    );
    assert!(
        store
            .claim_for_terminal_recovery(&run_id, "deferred-scanner", LEASE_MS)
            .await
            .expect("D1 terminal claim")
            .is_none(),
        "D1/E1 scanner must honor the same retry deadline"
    );
    assert!(
        store
            .claim_run(&run_id, "deferred-second", LEASE_MS, &Default::default())
            .await
            .expect("D1 ordinary claim")
            .is_none(),
        "D1/E1 Worker must honor retry deadline"
    );
    clock
        .advance_past(first.lease.expires_ms().saturating_add(1_000))
        .await;
    let second = store
        .claim_run(&run_id, "deferred-second", LEASE_MS, &Default::default())
        .await
        .expect("D2 ordinary claim")
        .expect("D2 retry is due");
    assert!(second.lease.epoch() > old_epoch, "D2/E2 higher epoch");
    assert_eq!(
        store
            .settle(&run_id, old_epoch, DispatchOutcome::Done, &[])
            .await
            .expect("D3 stale settlement"),
        SettleOutcome::Fenced,
        "D3/E3 former owner remains fenced"
    );
    assert_eq!(
        store
            .settle(&run_id, second.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("D2 current settlement"),
        SettleOutcome::Applied,
        "D2/E2 current claim can complete"
    );
}

/// Committed-terminal recovery cause/effect graph. Causes: C1 the target is a
/// quiescent unleased Pending or Awaiting row; C2 a distinct fresh peer is
/// Pending; C3 a distinct peer is Running; C4 the target has a live repair lease;
/// C5 the repair lease expires after a recovery-process crash; C6 the recovery
/// claim epoch is current. Effects: E1 ordinary input-free claims and fresh peers
/// remain queued; E2 terminal recovery claims the exact target only when no peer
/// is Running; E3 C3/C4 leave the target untouched; E4 reclaiming C5 advances the
/// epoch and fences the crashed owner; E5 current Done emits the existing
/// completion tombstone and releases the fresh peer. The Run authority is outside
/// this store test; callers invoke recovery only after committed RunState::Ended.
///
/// | Rule | Target | Peer | Repair lease | Effect |
/// |---|---|---|---|---|
/// | T1 | Awaiting | none/Pending | none | E1 |
/// | T2 | Awaiting | Running | none | E3 |
/// | T3 | Pending | none | none | E2, not crash-labelled |
/// | T4 | Pending | none | occupied | E3 until exact physical ACK, then E2 |
/// | T5 | Awaiting | Pending | none | E2 |
/// | T6 | Running(repair) | Pending | live/boundary | E3 |
/// | T7 | Running(repair) | Pending | expired | E2+E4 |
/// | T8 | Running(repair) | Pending | current | E5 |
async fn committed_terminal_recovery_reuses_fenced_settlement(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let awaiting = dispatch(ns, "terminal-awaiting", "terminal-thread");
    let awaiting_id = awaiting.run_id().clone();
    store
        .enqueue(awaiting)
        .await
        .expect("enqueue awaiting repair row");
    clock.set(60_000);
    let initial = store
        .claim_run(
            &awaiting_id,
            "terminal-initial",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim initial")
        .expect("initial row runnable");
    store
        .settle(
            &awaiting_id,
            initial.lease.epoch(),
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("settle awaiting checkpoint");
    clock.set(60_001);
    assert!(
        store
            .claim_run(&awaiting_id, "ordinary", LEASE_MS, &Default::default())
            .await
            .expect("ordinary exact claim")
            .is_none(),
        "T1: no input means ordinary claim cannot wake the row"
    );

    let control = dispatch(ns, "terminal-control", "terminal-thread");
    let control_id = control.run_id().clone();
    store
        .enqueue(control)
        .await
        .expect("enqueue same-thread control");
    clock.set(60_002);
    assert!(
        store
            .claim_run(&control_id, "ordinary-peer", LEASE_MS, &Default::default(),)
            .await
            .expect("ordinary peer claim")
            .is_none(),
        "T1: Awaiting remains the open writer and keeps a fresh peer queued"
    );
    store
        .cancel(&control_id)
        .await
        .expect("cancel control peer")
        .expect("control peer exists");
    clock.set(60_003);
    let control_claim = store
        .claim_run(
            &control_id,
            "terminal-control",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim cancelled control peer")
        .expect("cancellation may pass an Awaiting peer");
    clock.set(60_004);
    assert!(
        store
            .claim_for_terminal_recovery(&awaiting_id, "repair", LEASE_MS)
            .await
            .expect("repair blocked by Running control")
            .is_none(),
        "T2: a Running peer preserves single-writer-per-thread"
    );
    assert_eq!(
        store
            .settle(
                &control_id,
                control_claim.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle cancelled control"),
        SettleOutcome::Applied
    );

    let peer = dispatch(ns, "terminal-peer", "terminal-thread");
    let peer_id = peer.run_id().clone();
    store.enqueue(peer).await.expect("enqueue same-thread peer");
    clock.set(60_005);
    assert!(
        store
            .claim_run(&peer_id, "peer", LEASE_MS, &Default::default())
            .await
            .expect("claim fresh peer")
            .is_none(),
        "T1: a fresh peer stays queued behind Awaiting"
    );

    let pending = dispatch(ns, "terminal-pending", "terminal-pending-thread");
    let pending_id = pending.run_id().clone();
    store
        .enqueue(pending)
        .await
        .expect("enqueue pending control");
    clock.set(60_003);
    let pending_repair = store
        .claim_for_terminal_recovery(&pending_id, "pending-repair", LEASE_MS)
        .await
        .expect("pending recovery query")
        .expect("T3: quiescent Pending is terminal-recovery claimable");
    assert!(!pending_repair.recovered, "T3 is not crash recovery");
    assert_eq!(
        store
            .settle(
                &pending_id,
                pending_repair.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle pending repair"),
        SettleOutcome::Applied,
        "T3"
    );

    let occupied = dispatch(ns, "terminal-occupied", "terminal-occupied-thread");
    let occupied_id = occupied.run_id().clone();
    store.enqueue(occupied).await.expect("enqueue occupied row");
    clock.set(60_004);
    let occupied_claim = store
        .claim_run(
            &occupied_id,
            "occupied-owner",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim occupied row")
        .expect("occupied row is initially runnable");
    let occupied_claim = RunClaim::from(&occupied_claim.lease);
    clock.set(60_004);
    assert_eq!(
        store
            .begin_attempt(&occupied_claim, None)
            .await
            .expect("begin occupied attempt"),
        AttemptAdmission::Applied,
        "T4"
    );
    store
        .cancel(&occupied_id)
        .await
        .expect("cancel occupied row")
        .expect("occupied row remains present");
    clock.set(60_005);
    assert!(
        store
            .claim_for_terminal_recovery(&occupied_id, "occupied-repair", LEASE_MS)
            .await
            .expect("occupied recovery query")
            .is_none(),
        "T4: physical ownership fences terminal recovery"
    );
    assert_eq!(
        store
            .finish_attempt(&occupied_claim)
            .await
            .expect("finish occupied attempt"),
        SettleOutcome::Applied,
        "T4"
    );
    clock.set(60_005);
    let after_ack = store
        .claim_for_terminal_recovery(&occupied_id, "occupied-repair", LEASE_MS)
        .await
        .expect("post-ACK recovery query")
        .expect("T4: exact physical ACK makes the Pending row claimable");
    assert_eq!(
        store
            .settle(
                &occupied_id,
                after_ack.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle occupied recovery"),
        SettleOutcome::Applied,
        "T4"
    );

    clock.set(60_006);
    let repair = store
        .claim_for_terminal_recovery(&awaiting_id, "repair", LEASE_MS)
        .await
        .expect("claim terminal repair")
        .expect("the exact Awaiting target ignores its Pending peer");
    clock.set(60_007);
    assert!(
        store
            .claim_run(
                &peer_id,
                "peer-during-repair",
                LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("peer claim during repair")
            .is_none(),
        "T5/T6: the repair claim remains the sole Running writer"
    );
    clock.set(60_007);
    assert!(
        store
            .claim_for_terminal_recovery(&awaiting_id, "other", LEASE_MS)
            .await
            .expect("running repair claim query")
            .is_none(),
        "T6: a currently claimed repair row cannot be claimed twice"
    );
    if clock.exact_boundary_is_controllable() {
        clock.set(repair.lease.expires_ms() - 1);
        assert!(
            store
                .claim_for_terminal_recovery(&awaiting_id, "boundary", LEASE_MS,)
                .await
                .expect("repair claim at lease boundary")
                .is_none(),
            "T6: a lease is live one instant before its expiry"
        );
        // The Expiry law: from the exact expiry instant the lease has passed.
        clock.set(repair.lease.expires_ms());
    } else {
        clock.advance_past(repair.lease.expires_ms()).await;
    }
    let repair_after_crash = store
        .claim_for_terminal_recovery(&awaiting_id, "repair-after-crash", LEASE_MS)
        .await
        .expect("reclaim expired terminal repair")
        .expect("expired repair lease is recovery claimable");
    assert_eq!(
        store
            .settle(
                &awaiting_id,
                repair.lease.epoch(),
                DispatchOutcome::Done,
                &[]
            )
            .await
            .expect("stale repair settle"),
        SettleOutcome::Fenced,
        "T7"
    );
    let cursor = store
        .completion_events_after(0, usize::MAX)
        .await
        .expect("completion cursor before repair")
        .last()
        .map_or(0, |event| event.sequence);
    assert_eq!(
        store
            .settle(
                &awaiting_id,
                repair_after_crash.lease.epoch(),
                DispatchOutcome::Done,
                &[]
            )
            .await
            .expect("current repair settle"),
        SettleOutcome::Applied,
        "T8"
    );
    let completion = store
        .completion_events_after(cursor, 1)
        .await
        .expect("repair completion");
    assert_eq!(completion.len(), 1, "T8");
    assert_eq!(completion[0].run_id, awaiting_id, "T8");

    clock.set(60_006 + LEASE_MS + 2);
    let peer_claim = store
        .claim_run(&peer_id, "peer-after-repair", LEASE_MS, &Default::default())
        .await
        .expect("claim peer after repair")
        .expect("terminal settlement releases the fresh peer");
    assert_eq!(
        store
            .settle(
                &peer_id,
                peer_claim.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle released peer"),
        SettleOutcome::Applied,
        "T8"
    );
}

/// Broad-claim credential admission cause graph:
///
/// C1 row is placement-compatible -> C2 credential attempt is admissible
///  ├─ F -> E1 broad selection skips this row and evaluates the next row
///  └─ T -> E2 broad selection claims it atomically.
/// An exact claim names one row, so C2 false remains an explicit error.
///
/// | Rule | Claim | First row C1 | First row C2 | Later valid row | Result |
/// |---|---|---|---|---|---|
/// | Q1 | broad | T | F | T | claim later valid row |
/// | Q2 | exact invalid | T | F | - | admission error |
/// | Q3 | policy broad | T | F | T | claim later valid row |
/// | Q4 | broad cancel | remote auth | missing relay | non-remote cancel | skip remote, claim non-remote |
/// | Q5 | policy cancel | remote auth | missing relay | non-remote cancel | skip remote, claim non-remote |
/// | Q6 | local broad cancel | remote auth | missing relay | non-remote cancel | skip remote, claim non-remote |
/// | Q7 | broad | malformed Session projection | - | valid row | skip malformed, claim valid |
/// | Q8 | exact | malformed Session projection | - | - | typed admission error, no mutation |
/// | Q9 | exact cancel | malformed Session projection | not used by cancel | - | claim without execution credentials |
async fn incompatible_credentials_do_not_poison_broad_claims(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let incompatible_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "other-worker");
    let mut invalid =
        credential_dispatch(ns, "poison-invalid", "poison-invalid", &incompatible_holder);
    invalid.placement = PlacementRequirements::remote_required();
    let mut valid = credential_dispatch(ns, "poison-valid", "poison-valid", &holder);
    valid.placement = PlacementRequirements::remote_required();
    store
        .enqueue_with(
            invalid.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q1 enqueue incompatible row");
    store
        .enqueue(valid.clone())
        .await
        .expect("Q1 enqueue valid row");
    let worker = credential_worker(ns, &holder, true);
    clock.set(50_000);
    let claimed = store
        .claim_compatible(&worker, LEASE_MS)
        .await
        .expect("Q1 broad claim")
        .expect("Q1 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), valid.run_id(), "Q1");
    store
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q1 settle valid row");
    clock.set(50_002);
    assert!(
        store
            .claim_run_compatible(invalid.run_id(), &worker, LEASE_MS)
            .await
            .is_err(),
        "Q2 exact claim preserves the admission failure"
    );
    let mut placed = credential_dispatch(ns, "poison-placed", "poison-placed", &holder);
    placed.placement = PlacementRequirements::remote_required();
    store
        .enqueue(placed.clone())
        .await
        .expect("Q3 enqueue valid row");
    clock.set(50_003);
    let claimed = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            std::sync::Arc::new(LeastLoadedPolicy),
            LEASE_MS,
        )
        .await
        .expect("Q3 policy broad claim")
        .expect("Q3 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), placed.run_id(), "Q3");
    store
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q3 settle valid row");

    let mut remote_cancel =
        remote_credential_dispatch(ns, "poison-remote-cancel", "poison-remote-cancel", &holder);
    remote_cancel.placement = PlacementRequirements::remote_required();
    let mut nonremote_cancel =
        credential_dispatch(ns, "poison-local-cancel", "poison-local-cancel", &holder);
    nonremote_cancel.placement = PlacementRequirements::remote_required();
    nonremote_cancel.inference_plaintext_holder = None;
    store
        .enqueue_with(
            remote_cancel.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q4 enqueue authenticated remote cancellation");
    store
        .cancel(remote_cancel.run_id())
        .await
        .expect("Q4 persist remote cancellation");
    store
        .enqueue(nonremote_cancel.clone())
        .await
        .expect("Q4 enqueue credential-free cancellation");
    store
        .cancel(nonremote_cancel.run_id())
        .await
        .expect("Q4 persist credential-free cancellation");
    clock.set(50_004);
    let claimed = store
        .claim_compatible(&worker, LEASE_MS)
        .await
        .expect("Q4 broad cancellation claim")
        .expect("Q4 later credential-free cancellation is claimable");
    assert_eq!(claimed.request.run_id(), nonremote_cancel.run_id(), "Q4");
    assert!(claimed.credential_bindings.is_empty(), "Q4");
    store
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q4 settle credential-free cancellation");

    let mut remote_placed_cancel = remote_credential_dispatch(
        ns,
        "poison-remote-placed-cancel",
        "poison-remote-placed-cancel",
        &holder,
    );
    remote_placed_cancel.placement = PlacementRequirements::remote_required();
    let mut nonremote_placed_cancel = credential_dispatch(
        ns,
        "poison-local-placed-cancel",
        "poison-local-placed-cancel",
        &holder,
    );
    nonremote_placed_cancel.placement = PlacementRequirements::remote_required();
    nonremote_placed_cancel.inference_plaintext_holder = None;
    store
        .enqueue_with(
            remote_placed_cancel.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q5 enqueue authenticated remote cancellation");
    store
        .cancel(remote_placed_cancel.run_id())
        .await
        .expect("Q5 persist remote cancellation");
    store
        .enqueue(nonremote_placed_cancel.clone())
        .await
        .expect("Q5 enqueue credential-free cancellation");
    store
        .cancel(nonremote_placed_cancel.run_id())
        .await
        .expect("Q5 persist credential-free cancellation");
    clock.set(50_005);
    let claimed = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            std::sync::Arc::new(LeastLoadedPolicy),
            LEASE_MS,
        )
        .await
        .expect("Q5 policy cancellation claim")
        .expect("Q5 later credential-free cancellation is claimable");
    assert_eq!(
        claimed.request.run_id(),
        nonremote_placed_cancel.run_id(),
        "Q5"
    );
    assert!(claimed.credential_bindings.is_empty(), "Q5");
    store
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q5 settle credential-free cancellation");

    let mut remote_local_cancel = remote_credential_dispatch(
        ns,
        "poison-remote-local-cancel",
        "poison-remote-local-cancel",
        &holder,
    );
    remote_local_cancel.placement = PlacementRequirements::remote_required();
    let mut nonremote_local_cancel = credential_dispatch(
        ns,
        "poison-local-local-cancel",
        "poison-local-local-cancel",
        &holder,
    );
    nonremote_local_cancel.placement = PlacementRequirements::remote_required();
    nonremote_local_cancel.inference_plaintext_holder = None;
    store
        .enqueue_with(
            remote_local_cancel.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q6 enqueue authenticated remote cancellation");
    store
        .cancel(remote_local_cancel.run_id())
        .await
        .expect("Q6 persist remote cancellation");
    store
        .enqueue(nonremote_local_cancel.clone())
        .await
        .expect("Q6 enqueue credential-free cancellation");
    store
        .cancel(nonremote_local_cancel.run_id())
        .await
        .expect("Q6 persist credential-free cancellation");
    let local_capabilities = CredentialRealizationCapabilities::from_manifest_capabilities(
        &worker.manifest.capabilities,
    )
    .expect("Q6 Worker capabilities decode");
    clock.set(50_006);
    let claimed = store
        .claim(
            "credential-local-broad-worker",
            LEASE_MS,
            &local_capabilities,
        )
        .await
        .expect("Q6 local broad cancellation claim")
        .expect("Q6 later credential-free cancellation is claimable");
    assert_eq!(
        claimed.request.run_id(),
        nonremote_local_cancel.run_id(),
        "Q6"
    );
    assert!(claimed.credential_bindings.is_empty(), "Q6");
    store
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q6 settle credential-free cancellation");

    let invalid_runtime = dispatch(
        ns,
        "poison-runtime-invalid",
        "poison-runtime-invalid-thread",
    )
    .with_session_runtime(SessionRuntimeEnvelope::new("{"));
    let valid_runtime = dispatch(ns, "poison-runtime-valid", "poison-runtime-valid-thread");
    store
        .enqueue_with(
            invalid_runtime.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q7 enqueue malformed Session projection");
    store
        .enqueue(valid_runtime.clone())
        .await
        .expect("Q7 enqueue later valid row");
    clock.set(50_007);
    let claimed = store
        .claim(
            "runtime-projection-broad-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("Q7 broad claim")
        .expect("Q7 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), valid_runtime.run_id(), "Q7");
    store
        .settle(
            valid_runtime.run_id(),
            claimed.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q7 settle valid row");
    clock.set(50_008);
    let exact_error = store
        .claim_run(
            invalid_runtime.run_id(),
            "runtime-projection-exact-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect_err("Q8 exact claim rejects malformed Session projection");
    assert!(
        exact_error
            .to_string()
            .contains("Session runtime credential projection is invalid"),
        "Q8: {exact_error}"
    );
    store
        .cancel(invalid_runtime.run_id())
        .await
        .expect("Q9 persist cancel for malformed execution projection");
    clock.set(50_009);
    let cancellation = store
        .claim_run(
            invalid_runtime.run_id(),
            "runtime-projection-cancel-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("Q9 cancellation uses its operation-specific admission")
        .expect("Q9 canceled row is claimable");
    assert!(cancellation.cancellation_requested, "Q9");
    assert!(cancellation.credential_bindings.is_empty(), "Q9");
    assert_eq!(cancellation.lease.epoch(), 1, "Q8 did not mutate the row");
    store
        .settle(
            invalid_runtime.run_id(),
            cancellation.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q9 settle malformed-projection cancellation");
}

/// Claim-credential cause-effect graph:
///
/// publication credential + exact holder + implemented backend cell + installed
/// capability -> one binding committed with the lease epoch. Missing capability
/// rejects without advancing the epoch. Non-remote cancellation stays
/// credential-free; authenticated A2A cancellation freezes only its exact
/// WorkerRelay binding. Recovery advances the epoch and replaces the whole
/// binding set. A receipt is accepted only for the current exact binding.
///
/// | Rule | credential | cancel | capability | claim | Result |
/// |---|---|---|---|---|---|
/// | A1 | none | F | - | fresh | empty binding set |
/// | A2 | exact | F | exact local evidence | fresh | epoch-1 binding |
/// | A3 | exact | F | missing local evidence | fresh | reject, no mutation |
/// | A4 | exact | F | exact registered manifest after reject | fresh | epoch remains 1 |
/// | A5 | exact | T | missing | fresh | claim, empty binding set |
/// | A6 | exact | F | exact | recovery | replacement binding at epoch+1 |
/// | A7 | remote exact | T | missing | fresh | reject, no mutation |
/// | A8 | remote exact | T | exact relay | fresh | epoch-1 remote binding |
///
/// | Rule | claim | receipt | Result |
/// |---|---|---|---|
/// | R1 | current | exact | applied and exact replay applied |
/// | R2 | current | wrong mechanism | rejected |
/// | R3 | stale after recovery | formerly exact | fenced |
/// | R4 | current/live | exact candidate | return persisted binding |
/// | R5 | expired/stale/foreign candidate | any | return no binding |
async fn attempt_credentials_are_atomic_and_epoch_fenced(
    store: &dyn ControlDispatchQueue,
    ns: &str,
    conformance: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        format!("{ns}.worker.credentials"),
    );
    let exact_worker = credential_worker(ns, &holder, true);
    let incapable_worker = credential_worker(ns, &holder, false);
    let exact_capabilities = CredentialRealizationCapabilities::from_manifest_capabilities(
        &exact_worker.manifest.capabilities,
    )
    .expect("exact Worker credential capabilities decode");
    let incapable_capabilities = CredentialRealizationCapabilities::from_manifest_capabilities(
        &incapable_worker.manifest.capabilities,
    )
    .expect("empty Worker credential capabilities decode");

    let local = credential_dispatch(ns, "credential-local", "credential-local-thread", &holder);
    let local_id = local.run_id().clone();
    store
        .enqueue(local)
        .await
        .expect("A2 enqueue local credential run");
    if conformance.local_commit_guard {
        clock.set(50_000);
        assert!(
            store
                .claim_run(
                    &local_id,
                    "credential-incapable-local-owner",
                    LEASE_MS,
                    &incapable_capabilities,
                )
                .await
                .is_err(),
            "A3 local claim cannot synthesize capability evidence from the request"
        );
    }
    clock.set(50_000);
    let first = store
        .claim_run(
            &local_id,
            "credential-local-owner",
            LEASE_MS,
            &exact_capabilities,
        )
        .await
        .expect("A2 local claim succeeds")
        .expect("A2 credential run is runnable");
    assert_eq!(first.credential_bindings.len(), 1, "A2 one exact binding");
    assert_eq!(
        first.lease.epoch(),
        1,
        "A3 rejected admission did not mutate the durable lease epoch"
    );
    let first_binding = &first.credential_bindings[0];
    assert_eq!(first_binding.claim_epoch, first.lease.epoch());
    assert_eq!(
        first_binding.selected_realization_kind,
        CredentialRealizationKind::WorkerProviderAdapter
    );
    assert_eq!(
        store
            .current_credential_binding(
                &RunClaim::from(&first.lease),
                &first_binding.candidate_fingerprint,
            )
            .await
            .expect("R4 binding query"),
        Some(first_binding.clone()),
        "R4 current claim reads only its persisted binding"
    );
    assert!(
        store
            .current_credential_binding(
                &RunClaim::from(&first.lease),
                &awaken_runtime_contract::CandidateFingerprint("foreign".into()),
            )
            .await
            .expect("R5 foreign candidate query")
            .is_none(),
        "R5 a foreign candidate cannot select another binding"
    );
    let receipt = CredentialRealizationReceipt::new(
        first_binding,
        CredentialRealizationKind::WorkerProviderAdapter,
    )
    .expect("R1 exact receipt builds");
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt.clone())
            .await
            .expect("R1 receipt persists"),
        SettleOutcome::Applied
    );
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt.clone())
            .await
            .expect("R1 exact retry is idempotent"),
        SettleOutcome::Applied
    );
    let mut wrong_mechanism = receipt.clone();
    wrong_mechanism.actual_realization_kind = CredentialRealizationKind::WorkerRelay;
    assert!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), wrong_mechanism)
            .await
            .is_err(),
        "R2 a mechanism mismatch is rejected"
    );

    clock.advance_past(first.lease.expires_ms()).await;
    clock.set(first.lease.expires_ms().saturating_add(1));
    assert!(
        store
            .current_credential_binding(
                &RunClaim::from(&first.lease),
                &first_binding.candidate_fingerprint,
            )
            .await
            .expect("R5 expired query")
            .is_none(),
        "R5 an expired claim cannot obtain a new carrier"
    );
    let recovered = store
        .claim_run(
            &local_id,
            "credential-recovery-owner",
            LEASE_MS,
            &exact_capabilities,
        )
        .await
        .expect("A6 recovery succeeds")
        .expect("A6 expired credential run is recoverable");
    assert_eq!(recovered.lease.epoch(), first.lease.epoch() + 1);
    assert_eq!(recovered.credential_bindings.len(), 1);
    assert_eq!(
        recovered.credential_bindings[0].claim_epoch,
        recovered.lease.epoch(),
        "A6 the binding set is replaced under the new claim epoch"
    );
    assert_eq!(
        store
            .current_credential_binding(
                &RunClaim::from(&recovered.lease),
                &recovered.credential_bindings[0].candidate_fingerprint,
            )
            .await
            .expect("R4 recovered binding query"),
        Some(recovered.credential_bindings[0].clone()),
        "R4 the successor reads only its new epoch binding"
    );
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt)
            .await
            .expect("R3 stale receipt returns a fence verdict"),
        SettleOutcome::Fenced
    );
    store
        .settle(
            &local_id,
            recovered.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle recovered credential run");

    let remote = credential_dispatch(ns, "credential-remote", "credential-remote-thread", &holder)
        .with_placement(PlacementRequirements::remote_required());
    let remote_id = remote.run_id().clone();
    store
        .enqueue(remote)
        .await
        .expect("A3 enqueue remote credential run");
    clock.set(60_000);
    assert!(
        store
            .claim_run_compatible(&remote_id, &incapable_worker, LEASE_MS)
            .await
            .is_err(),
        "A3 immutable Worker capability evidence rejects the claim"
    );
    clock.set(60_000);
    let admitted = store
        .claim_run_compatible(&remote_id, &exact_worker, LEASE_MS)
        .await
        .expect("A4 exact-capability claim succeeds")
        .expect("A4 failed admission did not consume the runnable row");
    assert_eq!(
        admitted.lease.epoch(),
        1,
        "A4 failed admission did not advance the durable epoch"
    );
    assert_eq!(admitted.credential_bindings.len(), 1);
    store
        .settle(
            &remote_id,
            admitted.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle exact-capability run");

    let mut cancellation =
        credential_dispatch(ns, "credential-cancel", "credential-cancel-thread", &holder)
            .with_placement(PlacementRequirements::remote_required());
    cancellation.inference_plaintext_holder = None;
    let cancellation_id = cancellation.run_id().clone();
    store
        .enqueue(cancellation)
        .await
        .expect("A5 enqueue cancellation control run");
    store
        .cancel(&cancellation_id)
        .await
        .expect("A5 cancellation becomes durable");
    clock.set(70_000);
    let cancellation_claim = store
        .claim_run_compatible(&cancellation_id, &incapable_worker, LEASE_MS)
        .await
        .expect("A5 control claim bypasses credential admission")
        .expect("A5 cancellation is runnable");
    assert!(cancellation_claim.cancellation_requested);
    assert!(
        cancellation_claim.credential_bindings.is_empty(),
        "A5 non-remote terminal control does not materialize a credential"
    );
    store
        .settle(
            &cancellation_id,
            cancellation_claim.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle cancellation control run");

    let remote_cancel = remote_credential_dispatch(
        ns,
        "credential-remote-cancel",
        "credential-remote-cancel-thread",
        &holder,
    )
    .with_placement(PlacementRequirements::remote_required());
    let remote_cancel_id = remote_cancel.run_id().clone();
    store
        .enqueue(remote_cancel)
        .await
        .expect("A7 enqueue authenticated remote cancellation");
    store
        .cancel(&remote_cancel_id)
        .await
        .expect("A7 cancellation becomes durable");
    clock.set(71_000);
    assert!(
        store
            .claim_run_compatible(&remote_cancel_id, &incapable_worker, LEASE_MS,)
            .await
            .is_err(),
        "A7 authenticated remote cancel cannot downgrade to anonymous"
    );
    let remote_worker = remote_credential_worker(ns, &holder);
    clock.set(71_001);
    let remote_claim = store
        .claim_run_compatible(&remote_cancel_id, &remote_worker, LEASE_MS)
        .await
        .expect("A8 exact remote cancellation claim")
        .expect("A8 failed admission retained the runnable row");
    assert_eq!(remote_claim.lease.epoch(), 1, "A7 no claim mutation");
    assert!(remote_claim.cancellation_requested, "A8");
    assert_eq!(remote_claim.credential_bindings.len(), 1, "A8");
    assert_eq!(
        remote_claim.credential_bindings[0].credential.id,
        format!("{ns}-remote-credential"),
        "A8 exact remote credential"
    );
    assert_eq!(
        remote_claim.credential_bindings[0].selected_realization_kind,
        CredentialRealizationKind::WorkerRelay,
        "A8 exact cancel mechanism"
    );
    assert_eq!(
        remote_claim.credential_bindings[0].claim_epoch,
        remote_claim.lease.epoch(),
        "A8 current epoch"
    );
    store
        .settle(
            &remote_cancel_id,
            remote_claim.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle authenticated remote cancellation");
}
