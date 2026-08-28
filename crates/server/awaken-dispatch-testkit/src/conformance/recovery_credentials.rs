// Retry, identity, terminal recovery, and credential conformance rules. Included
// at crate root; every rule still exercises the one DispatchQueue contract.
/// Retry-exhaustion cause/effect decision table shared by every backend:
/// C1 state=Leased; C2 lease strictly expired; C3 attempts>=max. R1 !C1 => no
/// claim; R2 C1+!C2 (including equality) => no claim; R3 C1+C2+!C3 => no claim;
/// R4 C1+C2+C3 => mint one newer ordinary Claimed epoch without execution
/// credentials; R5 two concurrent claimants for one eligible row => exactly one
/// receives the unique newer epoch and one receives None; R6 the losing/stale
/// epoch is fenced while the current claim settles Done.
async fn retry_exhaustion_claim_is_atomic_and_policy_exact(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let request = dispatch(ns, "retry-exhaustion", "retry-exhaustion-thread");
    let run_id = request.run_id().clone();
    store
        .enqueue(request)
        .await
        .expect("enqueue retry-exhaustion row");
    assert!(
        store
            .claim_retry_exhausted("terminal", LEASE_MS, 0, 1)
            .await
            .expect("R1 query")
            .is_none(),
        "R1"
    );
    let fresh = store
        .claim_run(&run_id, "crashed-a", LEASE_MS, 10_000, &Default::default())
        .await
        .expect("fresh claim")
        .expect("fresh row claimable");
    if clock.exact_boundary_is_controllable() {
        clock.set(fresh.lease.expires_ms);
        assert!(
            store
                .claim_retry_exhausted("terminal", LEASE_MS, fresh.lease.expires_ms, 0)
                .await
                .expect("R2 query")
                .is_none(),
            "R2"
        );
    }
    clock.advance_past(fresh.lease.expires_ms).await;
    assert!(
        store
            .claim_retry_exhausted(
                "terminal",
                LEASE_MS,
                fresh.lease.expires_ms.saturating_add(1),
                1,
            )
            .await
            .expect("R3 query")
            .is_none(),
        "R3"
    );
    let recovered = store
        .claim_run(
            &run_id,
            "crashed-b",
            LEASE_MS,
            fresh.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("ordinary recovery")
        .expect("ordinary recovery is claimable");
    clock.advance_past(recovered.lease.expires_ms).await;
    let terminal_now = recovered.lease.expires_ms.saturating_add(1);
    let (left, right) = tokio::join!(
        store.claim_retry_exhausted("terminal", LEASE_MS, terminal_now, 1),
        store.claim_retry_exhausted("terminal-racer", LEASE_MS, terminal_now, 1),
    );
    let left = left.expect("R5 left query");
    let right = right.expect("R5 right query");
    assert_ne!(left.is_some(), right.is_some(), "R5 one winner");
    let terminal = left.or(right).expect("R4/R5 exhausted row claimable");
    assert!(terminal.recovered, "R4");
    assert!(terminal.credential_bindings.is_empty(), "R4");
    assert_eq!(terminal.lease.run_id, run_id, "R5 unique row");
    assert_eq!(
        terminal.lease.epoch,
        recovered.lease.epoch + 1,
        "R5 unique newer epoch"
    );
    assert!(
        store
            .claim_retry_exhausted("terminal-third", LEASE_MS, terminal_now, 1)
            .await
            .expect("R5 barrier query")
            .is_none(),
        "R5"
    );
    assert_eq!(
        store
            .settle(&run_id, recovered.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("R6 stale settle"),
        SettleOutcome::Fenced,
        "R6"
    );
    assert_eq!(
        store
            .settle(
                &terminal.lease.run_id,
                terminal.lease.epoch,
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
async fn caller_owned_run_identity_is_exact(store: &dyn DispatchQueue, ns: &str) {
    let request = dispatch(ns, "caller-owned", "caller-owned-thread");
    let run_id = request.run_id().clone();
    store
        .enqueue(request.clone())
        .await
        .expect("I1 live enqueue");

    let mut retraced = request.clone();
    retraced.traceparent = Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into());
    let claim = store
        .claim_new_run(
            retraced.clone(),
            "caller-owned-worker",
            LEASE_MS,
            90_000,
            &Default::default(),
        )
        .await
        .expect("I3 live replay")
        .expect("I1 exact live row remains claimable");

    let mut collision = request.clone();
    collision.activation.thread_id = ThreadId(format!("{ns}-collision-thread"));
    collision.placement = PlacementRequirements::remote_required();
    assert!(store.enqueue(collision.clone()).await.is_err(), "I2 live");
    assert!(
        store
            .claim_new_run(
                collision.clone(),
                "ineligible-local",
                LEASE_MS,
                89_999,
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
    assert!(
        store
            .claim_new_run_compatible(collision.clone(), &ineligible_worker, LEASE_MS, 89_999)
            .await
            .is_err(),
        "I4 Worker eligibility cannot mask collision"
    );

    assert_eq!(
        store
            .settle(&run_id, claim.lease.epoch, DispatchOutcome::Done, &[])
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
    let winner_claim = store
        .claim_new_run(
            winner,
            "caller-race-worker",
            LEASE_MS,
            91_000,
            &Default::default(),
        )
        .await
        .expect("I5 winner replay")
        .expect("I5 winner remains claimable");
    assert_eq!(
        store
            .settle(
                &winner_id,
                winner_claim.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle I5 winner"),
        SettleOutcome::Applied
    );
}

/// Committed-terminal recovery cause/effect graph. Causes: C1 the target is
/// quiescent Awaiting; C2 a distinct fresh peer is Pending; C3 a distinct peer is
/// Running; C4 the target is Pending or has a live repair lease; C5 the repair
/// lease expires after a recovery-process crash; C6 the recovery claim epoch is
/// current. Effects: E1 ordinary input-free claims and fresh peers remain queued;
/// E2 terminal recovery reclaims the exact Awaiting target only when no peer is
/// Running; E3 C3/C4 leave the target untouched; E4 reclaiming C5 advances the
/// epoch and fences the crashed owner; E5 current Done emits the existing
/// completion tombstone and releases the fresh peer. The Run authority is outside
/// this store test; callers invoke recovery only after committed RunState::Ended.
///
/// | Rule | Target | Peer | Repair lease | Effect |
/// |---|---|---|---|---|
/// | T1 | Awaiting | none/Pending | none | E1 |
/// | T2 | Awaiting | Running | none | E3 |
/// | T3 | Awaiting | Pending | none | E2 |
/// | T4 | Running(repair) | Pending | live/boundary | E3 |
/// | T5 | Running(repair) | Pending | expired | E2+E4 |
/// | T6 | Running(repair) | Pending | current | E5 |
async fn committed_terminal_recovery_reuses_fenced_settlement(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let awaiting = dispatch(ns, "terminal-awaiting", "terminal-thread");
    let awaiting_id = awaiting.run_id().clone();
    store
        .enqueue(awaiting)
        .await
        .expect("enqueue awaiting repair row");
    let initial = store
        .claim_run(
            &awaiting_id,
            "terminal-initial",
            LEASE_MS,
            60_000,
            &Default::default(),
        )
        .await
        .expect("claim initial")
        .expect("initial row runnable");
    store
        .settle(
            &awaiting_id,
            initial.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("settle awaiting checkpoint");
    assert!(
        store
            .claim_run(
                &awaiting_id,
                "ordinary",
                LEASE_MS,
                60_001,
                &Default::default()
            )
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
    assert!(
        store
            .claim_run(
                &control_id,
                "ordinary-peer",
                LEASE_MS,
                60_002,
                &Default::default(),
            )
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
    let control_claim = store
        .claim_run(
            &control_id,
            "terminal-control",
            LEASE_MS,
            60_003,
            &Default::default(),
        )
        .await
        .expect("claim cancelled control peer")
        .expect("cancellation may pass an Awaiting peer");
    assert!(
        store
            .claim_for_terminal_recovery(&awaiting_id, "repair", LEASE_MS, 60_004)
            .await
            .expect("repair blocked by Running control")
            .is_none(),
        "T2: a Running peer preserves single-writer-per-thread"
    );
    assert_eq!(
        store
            .settle(
                &control_id,
                control_claim.lease.epoch,
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
    assert!(
        store
            .claim_run(&peer_id, "peer", LEASE_MS, 60_005, &Default::default())
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
    assert!(
        store
            .claim_for_terminal_recovery(&pending_id, "repair", LEASE_MS, 60_003)
            .await
            .expect("pending recovery query")
            .is_none(),
        "T4: pending rows are not terminal-recovery claimable"
    );

    let repair = store
        .claim_for_terminal_recovery(&awaiting_id, "repair", LEASE_MS, 60_006)
        .await
        .expect("claim terminal repair")
        .expect("the exact Awaiting target ignores its Pending peer");
    assert!(
        store
            .claim_run(
                &peer_id,
                "peer-during-repair",
                LEASE_MS,
                60_007,
                &Default::default(),
            )
            .await
            .expect("peer claim during repair")
            .is_none(),
        "T3/T4: the repair claim remains the sole Running writer"
    );
    assert!(
        store
            .claim_for_terminal_recovery(&awaiting_id, "other", LEASE_MS, 60_007)
            .await
            .expect("running repair claim query")
            .is_none(),
        "T4: a currently claimed repair row cannot be claimed twice"
    );
    if clock.exact_boundary_is_controllable() {
        clock.set(repair.lease.expires_ms);
        assert!(
            store
                .claim_for_terminal_recovery(
                    &awaiting_id,
                    "boundary",
                    LEASE_MS,
                    repair.lease.expires_ms,
                )
                .await
                .expect("repair claim at lease boundary")
                .is_none(),
            "T4: a lease remains live at its exact expiry boundary"
        );
    }
    clock.advance_past(repair.lease.expires_ms).await;
    let repair_after_crash = store
        .claim_for_terminal_recovery(
            &awaiting_id,
            "repair-after-crash",
            LEASE_MS,
            repair.lease.expires_ms.saturating_add(1),
        )
        .await
        .expect("reclaim expired terminal repair")
        .expect("expired repair lease is recovery claimable");
    assert_eq!(
        store
            .settle(&awaiting_id, repair.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale repair settle"),
        SettleOutcome::Fenced,
        "T5"
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
                repair_after_crash.lease.epoch,
                DispatchOutcome::Done,
                &[]
            )
            .await
            .expect("current repair settle"),
        SettleOutcome::Applied,
        "T6"
    );
    let completion = store
        .completion_events_after(cursor, 1)
        .await
        .expect("repair completion");
    assert_eq!(completion.len(), 1, "T6");
    assert_eq!(completion[0].run_id, awaiting_id, "T6");

    let peer_claim = store
        .claim_run(
            &peer_id,
            "peer-after-repair",
            LEASE_MS,
            60_006 + LEASE_MS + 2,
            &Default::default(),
        )
        .await
        .expect("claim peer after repair")
        .expect("terminal settlement releases the fresh peer");
    assert_eq!(
        store
            .settle(&peer_id, peer_claim.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle released peer"),
        SettleOutcome::Applied,
        "T6"
    );
    let pending_claim = store
        .claim_run(
            &pending_id,
            "pending-cleanup",
            LEASE_MS,
            60_006 + LEASE_MS + 3,
            &Default::default(),
        )
        .await
        .expect("claim pending control after its negative rule")
        .expect("pending control remains ordinary work");
    assert_eq!(
        store
            .settle(
                &pending_id,
                pending_claim.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle pending control"),
        SettleOutcome::Applied
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
async fn incompatible_credentials_do_not_poison_broad_claims(store: &dyn DispatchQueue, ns: &str) {
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
    let claimed = store
        .claim_compatible(&worker, LEASE_MS, 50_000)
        .await
        .expect("Q1 broad claim")
        .expect("Q1 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), valid.run_id(), "Q1");
    store
        .settle(
            &claimed.lease.run_id,
            claimed.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q1 settle valid row");
    assert!(
        store
            .claim_run_compatible(invalid.run_id(), &worker, LEASE_MS, 50_002)
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
    let claimed = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            std::sync::Arc::new(LeastLoadedPolicy),
            LEASE_MS,
            50_003,
        )
        .await
        .expect("Q3 policy broad claim")
        .expect("Q3 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), placed.run_id(), "Q3");
    store
        .settle(
            &claimed.lease.run_id,
            claimed.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q3 settle valid row");
}

/// Claim-credential cause-effect graph:
///
/// publication credential + exact holder + implemented backend cell + installed
/// capability -> one binding committed with the lease epoch. Missing capability
/// rejects without advancing the epoch; cancellation bypasses materialization and
/// stores no binding; recovery advances the epoch and replaces the whole binding
/// set. A receipt is accepted only for the current exact binding.
///
/// | Rule | credential | cancel | capability | claim | Result |
/// |---|---|---|---|---|---|
/// | A1 | none | F | - | fresh | empty binding set |
/// | A2 | exact | F | exact local evidence | fresh | epoch-1 binding |
/// | A3 | exact | F | missing local evidence | fresh | reject, no mutation |
/// | A4 | exact | F | exact registered manifest after reject | fresh | epoch remains 1 |
/// | A5 | exact | T | missing | fresh | claim, empty binding set |
/// | A6 | exact | F | exact | recovery | replacement binding at epoch+1 |
///
/// | Rule | claim | receipt | Result |
/// |---|---|---|---|
/// | R1 | current | exact | applied and exact replay applied |
/// | R2 | current | wrong mechanism | rejected |
/// | R3 | stale after recovery | formerly exact | fenced |
async fn attempt_credentials_are_atomic_and_epoch_fenced(
    store: &dyn DispatchQueue,
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
        assert!(
            store
                .claim_run(
                    &local_id,
                    "credential-incapable-local-owner",
                    LEASE_MS,
                    50_000,
                    &incapable_capabilities,
                )
                .await
                .is_err(),
            "A3 local claim cannot synthesize capability evidence from the request"
        );
    }
    let first = store
        .claim_run(
            &local_id,
            "credential-local-owner",
            LEASE_MS,
            50_000,
            &exact_capabilities,
        )
        .await
        .expect("A2 local claim succeeds")
        .expect("A2 credential run is runnable");
    assert_eq!(first.credential_bindings.len(), 1, "A2 one exact binding");
    assert_eq!(
        first.lease.epoch, 1,
        "A3 rejected admission did not mutate the durable lease epoch"
    );
    let first_binding = &first.credential_bindings[0];
    assert_eq!(first_binding.claim_epoch, first.lease.epoch);
    assert_eq!(
        first_binding.selected_realization_kind,
        CredentialRealizationKind::WorkerProviderAdapter
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

    clock.advance_past(first.lease.expires_ms).await;
    let recovered = store
        .claim_run(
            &local_id,
            "credential-recovery-owner",
            LEASE_MS,
            first.lease.expires_ms.saturating_add(1),
            &exact_capabilities,
        )
        .await
        .expect("A6 recovery succeeds")
        .expect("A6 expired credential run is recoverable");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(recovered.credential_bindings.len(), 1);
    assert_eq!(
        recovered.credential_bindings[0].claim_epoch, recovered.lease.epoch,
        "A6 the binding set is replaced under the new claim epoch"
    );
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt)
            .await
            .expect("R3 stale receipt returns a fence verdict"),
        SettleOutcome::Fenced
    );
    store
        .settle(&local_id, recovered.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("settle recovered credential run");

    let remote = credential_dispatch(ns, "credential-remote", "credential-remote-thread", &holder)
        .with_placement(PlacementRequirements::remote_required());
    let remote_id = remote.run_id().clone();
    store
        .enqueue(remote)
        .await
        .expect("A3 enqueue remote credential run");
    assert!(
        store
            .claim_run_compatible(&remote_id, &incapable_worker, LEASE_MS, 60_000)
            .await
            .is_err(),
        "A3 immutable Worker capability evidence rejects the claim"
    );
    let admitted = store
        .claim_run_compatible(&remote_id, &exact_worker, LEASE_MS, 60_000)
        .await
        .expect("A4 exact-capability claim succeeds")
        .expect("A4 failed admission did not consume the runnable row");
    assert_eq!(
        admitted.lease.epoch, 1,
        "A4 failed admission did not advance the durable epoch"
    );
    assert_eq!(admitted.credential_bindings.len(), 1);
    store
        .settle(&remote_id, admitted.lease.epoch, DispatchOutcome::Done, &[])
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
    let cancellation_claim = store
        .claim_run_compatible(&cancellation_id, &incapable_worker, LEASE_MS, 70_000)
        .await
        .expect("A5 control claim bypasses credential admission")
        .expect("A5 cancellation is runnable");
    assert!(cancellation_claim.cancellation_requested);
    assert!(
        cancellation_claim.credential_bindings.is_empty(),
        "A5 terminal control never materializes a credential"
    );
    store
        .settle(
            &cancellation_id,
            cancellation_claim.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle cancellation control run");
}
