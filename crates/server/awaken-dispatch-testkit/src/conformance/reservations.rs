// @formal evidence run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal evidence session_run.recovery_claim_is_admission_only
// @formal evidence session_run.reservation_never_executes
// @formal evidence session_run.reservation_precedes_activity
// @formal evidence session_run.reservation_ttl_uses_store_clock
// @formal evidence session_terminal.unstarted_reservation_is_rejected_after_cancel
// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Static owner: dispatch-reservation conformance cases. Dynamic path: the tooling-only
// @formal reason: testkit includes this exact file to assert store behavior; it does not reserve product
// @formal reason: work itself. Evidence: crates/server/awaken-dispatch-testkit/src/lib.rs.
// Session reservation, claim guard, sandbox recovery, and completion rules.
// Included at crate root to retain all existing conformance function paths.

async fn current_claim_guard_is_exact(
    store: &dyn ConformanceDispatch,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    // Cause/effect and state-machine rules C1-C6: exact incarnation + live
    // lease returns the authoritative RunClaim (C1); stale incarnation,
    // expired lease, requested cancellation, or settled row returns None
    // (C2-C5). Claimed(e) --cancel--> cancellation-pending revokes e; only the
    // subsequent cancellation claim e+1 may acquire a guard, and that guard
    // exposes cancellation_requested=true (C6). The ownership query never
    // accepts a caller epoch and therefore cannot bless a stale claim.
    if !capabilities.local_commit_guard {
        return;
    }
    clock.set(20_000);
    let run = run_id(ns, "guard");
    store
        .enqueue(dispatch(ns, "guard", "guard-thread"))
        .await
        .expect("enqueue guard run");
    let identity = WorkerIdentity::new(format!("{ns}-guard-worker"), "boot", 1);
    let claimed = store
        .claim_run(&run, &identity.lease_owner(), LEASE_MS, &Default::default())
        .await
        .expect("claim guard run")
        .expect("guard run is runnable");
    let current = RunClaim::from(&claimed.lease);
    let stale = RunClaim {
        epoch: current.epoch.saturating_sub(1),
        ..current.clone()
    };
    assert!(
        store
            .lock_commit_epoch(&stale)
            .await
            .expect("stale guard lookup")
            .is_none(),
        "a stale epoch never acquires commit authority"
    );
    assert!(
        !store
            .claim_is_current(&stale)
            .await
            .expect("stale current-claim query"),
        "a stale epoch is never current"
    );
    clock.set(claimed.lease.expires_ms() - 1);
    assert!(
        store
            .claim_is_current(&current)
            .await
            .expect("pre-boundary current-claim query"),
        "the lease is current one instant before its expiry"
    );
    assert_eq!(
        store
            .worker_owns_run(&identity, &run)
            .await
            .expect("registered Worker ownership query")
            .as_ref(),
        Some(&current),
        "the exact Worker incarnation receives the live claim"
    );
    clock.advance_past(claimed.lease.expires_ms()).await;
    // At its exact expiry instant (a controllable clock) the claim has expired.
    clock.set(claimed.lease.expires_ms());
    assert!(
        !store
            .claim_is_current(&current)
            .await
            .expect("expired current-claim query"),
        "an expired claim is not current from its expiry instant, before it is reclaimed"
    );
    let stale_identity = WorkerIdentity::new(identity.worker_id.clone(), "replacement", 2);
    clock.set(20_000);
    assert!(
        store
            .worker_owns_run(&stale_identity, &run)
            .await
            .expect("stale Worker ownership query")
            .is_none(),
        "another incarnation never owns the run"
    );
    let guard = store
        .lock_commit_epoch(&current)
        .await
        .expect("current guard lookup")
        .expect("the exact current claim acquires authority");
    drop(guard);
    assert_eq!(
        store
            .settle(&run, current.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle guarded run"),
        SettleOutcome::Applied
    );
    clock.set(20_000);
    assert!(
        store
            .worker_owns_run(&identity, &run)
            .await
            .expect("settled Worker ownership query")
            .is_none(),
        "settlement removes Worker ownership"
    );

    let cancel_run = run_id(ns, "guard-cancel");
    store
        .enqueue(dispatch(ns, "guard-cancel", "guard-cancel-thread"))
        .await
        .expect("enqueue cancellation guard run");
    clock.set(20_000);
    let cancel_claimed = store
        .claim_run(
            &cancel_run,
            &identity.lease_owner(),
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim cancellation guard run")
        .expect("cancellation guard run is runnable");
    let cancel_claim = RunClaim::from(&cancel_claimed.lease);
    assert!(
        store
            .cancel(&cancel_run)
            .await
            .expect("cancel guarded run")
            .is_some(),
        "the live run accepts a durable cancellation intent"
    );
    clock.set(20_000);
    assert!(
        store
            .worker_owns_run(&identity, &cancel_run)
            .await
            .expect("cancelled Worker ownership query")
            .is_none(),
        "cancellation removes capability-issuance authority"
    );
    assert!(
        store
            .lock_commit_epoch(&cancel_claim)
            .await
            .expect("revoked cancellation guard lookup")
            .is_none(),
        "the revoked claim epoch is fenced immediately"
    );
    clock.set(20_000);
    let cancellation_claim = store
        .claim_run(
            &cancel_run,
            &identity.lease_owner(),
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim durable cancellation")
        .expect("the cancellation intent is runnable");
    let cancellation_claim = RunClaim::from(&cancellation_claim.lease);
    assert!(
        store
            .lock_commit_epoch(&cancellation_claim)
            .await
            .expect("cancelled guard lookup")
            .is_some_and(|guard| guard.cancellation_requested()),
        "the locked authority row exposes cancellation to capability issuers"
    );
}

async fn sandbox_binding_survives_recovery(
    store: &dyn ConformanceDispatch,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.sandbox_binding {
        return;
    }
    clock.set(30_000);
    let run = run_id(ns, "sandbox");
    store
        .enqueue(dispatch(ns, "sandbox", "sandbox-thread"))
        .await
        .expect("enqueue sandbox run");
    let first = store
        .claim_run(&run, "conformance-sandbox-a", LEASE_MS, &Default::default())
        .await
        .expect("claim sandbox run")
        .expect("sandbox run is runnable");
    let sandbox_ref = format!("opaque:{ns}");
    let bound = store
        .bind_sandbox(&RunClaim::from(&first.lease), &sandbox_ref)
        .await
        .expect("bind sandbox");
    assert_eq!(bound, SettleOutcome::Applied, "current claim binds sandbox");
    clock.advance_past(first.lease.expires_ms()).await;
    clock.set(first.lease.expires_ms().saturating_add(1));
    let recovered = store
        .claim_run(&run, "conformance-sandbox-b", LEASE_MS, &Default::default())
        .await
        .expect("recover sandbox run")
        .expect("expired sandbox run is recoverable");
    assert_eq!(recovered.sandbox.as_deref(), Some(sandbox_ref.as_str()));
    assert!(recovered.recovered);
    assert_eq!(
        store
            .settle(&run, recovered.lease.epoch(), DispatchOutcome::Done, &[],)
            .await
            .expect("settle sandbox run"),
        SettleOutcome::Applied
    );
}

async fn completion_is_atomic_and_prevents_resurrection(
    store: &dyn ConformanceDispatch,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.completion_events {
        return;
    }

    let baseline = store
        .completion_events_after(0, usize::MAX)
        .await
        .expect("read completion baseline");
    let cursor = baseline.last().map_or(0, |event| event.sequence);

    // Awaiting and a stale fenced Done are negative partitions: neither may
    // publish a completion fact.
    clock.set(40_000);
    let awaiting = run_id(ns, "completion-awaiting");
    store
        .enqueue(dispatch(
            ns,
            "completion-awaiting",
            "completion-awaiting-thread",
        ))
        .await
        .expect("enqueue awaiting control run");
    let awaiting_claim = store
        .claim_run(
            &awaiting,
            "conformance-completion",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim awaiting control run")
        .expect("awaiting control run is runnable");
    assert_eq!(
        store
            .settle(
                &awaiting,
                awaiting_claim.lease.epoch(),
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("settle awaiting control run"),
        SettleOutcome::Applied
    );
    assert!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("query after awaiting settle")
            .is_empty(),
        "Awaiting does not emit a completion fact"
    );
    assert_eq!(
        store
            .cancel(&awaiting)
            .await
            .expect("remove awaiting control run"),
        Some(thread_id(ns, "completion-awaiting-thread"))
    );

    let request = dispatch(ns, "completion-done", "completion-done-thread");
    let done = request.run_id().clone();
    store
        .enqueue(request.clone())
        .await
        .expect("enqueue completion run");
    let claim = store
        .claim_run(
            &done,
            "conformance-completion",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim completion run")
        .expect("completion run is runnable");
    assert_eq!(
        store
            .settle(
                &done,
                claim.lease.epoch().saturating_sub(1),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("stale completion settle"),
        SettleOutcome::Fenced
    );
    assert!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("query after fenced settle")
            .is_empty(),
        "a fenced Done emits no completion fact"
    );

    assert_eq!(
        store
            .settle(&done, claim.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("apply completion settle"),
        SettleOutcome::Applied
    );
    let first_page = store
        .completion_events_after(cursor, 1)
        .await
        .expect("read first completion page");
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].run_id, done);
    assert!(first_page[0].sequence > cursor);
    // Lost-settlement-ACK matrix: the caller cannot distinguish a committed
    // Done from a failed response. Retrying the same or an older claim must
    // observe a retired authority, retain the exact completion fact, and never
    // create another physical execution or duplicate completion.
    for epoch in [claim.lease.epoch(), claim.lease.epoch().saturating_sub(1)] {
        assert_eq!(
            store
                .settle(&done, epoch, DispatchOutcome::Done, &[])
                .await
                .expect("retry a completion after response loss"),
            SettleOutcome::Fenced,
            "a completed claim cannot settle twice",
        );
        assert_eq!(
            store
                .completion_events_after(cursor, 2)
                .await
                .expect("observe retained completion"),
            first_page,
            "response loss preserves exactly one durable completion",
        );
    }
    assert_eq!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("replay first completion page"),
        first_page,
        "a consumer may replay the same cursor idempotently"
    );

    // Both admission commands consult the tombstone. A completed durable
    // identity can never become runnable or emit a second event.
    store
        .enqueue(request.clone())
        .await
        .expect("replayed enqueue is accepted as a no-op");
    clock.set(40_001);
    assert!(
        store
            .claim_run(&done, "conformance-replay", LEASE_MS, &Default::default(),)
            .await
            .expect("exact claim after replay")
            .is_none(),
        "completed run id does not resurrect through enqueue"
    );
    clock.set(40_001);
    assert!(
        store
            .claim_new_run(
                request.clone(),
                "conformance-replay",
                LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("atomic admission after replay")
            .is_none(),
        "completed run id does not resurrect through claim_new_run"
    );
    let worker = ready_dispatch_worker(ns, "completion");
    clock.set(40_001);
    assert!(
        store
            .claim_new_run_compatible(request, &worker, LEASE_MS)
            .await
            .expect("compatible atomic admission after replay")
            .is_none(),
        "completed run id does not resurrect through compatible claim_new_run"
    );
    assert_eq!(
        store
            .completion_events_after(cursor, 2)
            .await
            .expect("read completion events after replay"),
        first_page,
        "replayed admission creates no duplicate completion"
    );
    assert!(
        store
            .completion_events_after(first_page[0].sequence, 1)
            .await
            .expect("advance completion cursor")
            .is_empty(),
        "an advanced cursor excludes the acknowledged event"
    );
    assert!(
        store
            .completion_events_after(cursor, 0)
            .await
            .expect("zero-sized completion page")
            .is_empty(),
        "a zero limit returns an empty page"
    );
}
