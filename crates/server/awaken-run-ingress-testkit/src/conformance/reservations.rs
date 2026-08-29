// Session reservation, claim guard, sandbox recovery, and completion rules.
// Included at crate root to retain all existing conformance function paths.
/// Session Run reservation cause/effect graph. Causes: C1 canonical dispatch is
/// absent/exact/conflicting; C2 state is Reserved/ReservationLeased/activated/
/// completed/missing; C3 activity epoch and Session Thread are exact/invalid;
/// C4 repair claim is current/stale; C5 resolution is admit/retry/reject/invalid.
/// C6 admission surface is reservation/ordinary enqueue/atomic local claim/
/// atomic compatible claim.
/// C7 reservation input is a nonzero relative TTL interpreted by the store's
/// authority clock, never a caller-authored absolute timestamp.
/// C8 replacement intent preserves/supersedes prior live work and is replayed
/// exactly from the immutable reservation.
/// C9 cold replay reconstructs the same Session command with different current
/// Agent/model/placement projections.
/// Effects: E1 persist one unclaimable intent; E2 never open or overwrite the
/// wrong activity; E3 exact replay reports the durable phase; E4 repair never
/// binds a Sandbox or executes; E5 admitted repair publishes ordinary Pending;
/// E6 retry preserves Reserved; E7 rejection removes only the unstarted intent;
/// E8 completion cannot resurrect; E9 cancellation cannot shorten the original
/// admission window; E10 every claim surface bypasses execution placement only
/// for reservation repair; E11 an expired repair lease advances the epoch and
/// fences its crashed owner; E12 initial and retry TTLs are converted to durable
/// deadlines by the store authority rather than trusted as absolute caller time.
/// E13 newest-wins reservation and prior-state transition share one store
/// transaction; ordinary reservations never mutate prior work.
///
/// | Rule | Identity/state | Epoch/claim | Command | Effect |
/// |---|---|---|---|---|
/// | SR0 | absent Session root | no activity receipt | ordinary admission surfaces | E2; no row |
/// | SR1 | absent | valid TTL | reserve | E1/E12 |
/// | SR2 | exact command/changed command/changed projection | - | reserve replay | E3 / E2 / E3 |
/// | SR3 | Reserved | exact/invalid | activate | Pending / E2 |
/// | SR4 | ReservationLeased | current | replay/bind Sandbox | E3 / E4 |
/// | SR5 | ReservationLeased | stale/current | resolve | fenced / E5-E7 |
/// | SR6 | activated/completed/missing | exact | replay | E3/E8 |
/// | SR7 | Reserved(cancelled) | before deadline | claim/activate | E9, then Pending(cancelled) |
/// | SR8 | ReservationLeased | expired lease/retry TTL | every claim surface | E10-E12 |
/// | SR9 | prior Awaiting/unsafe live phase | preserve/supersede/replay | reserve | preserve / E13 / reject / E3 |
async fn session_run_reservation_is_atomic_and_recoverable(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    const RESERVATION_TTL_MS: u64 = 60_000;
    // Worker transports do not own reservation admission. Memory, SQLite, and
    // PostgreSQL all expose the local claim guard and run this complete table.
    if !capabilities.local_commit_guard {
        return;
    }

    // SR0 proves both rejection and absence of a partial write. Each rejected
    // Run id must remain immediately reservable through the one authoritative
    // Session admission command; this distinguishes validation from a backend
    // that returns an error after inserting a hidden or claimable row.
    let enqueue_session = thread_id(ns, "reservation-ordinary-enqueue-session");
    let enqueue_request = dispatch(
        ns,
        "reservation-ordinary-enqueue",
        "reservation-ordinary-enqueue-session",
    )
    .for_session(enqueue_session);
    assert!(
        store.enqueue(enqueue_request.clone()).await.is_err(),
        "SR0/E2 ordinary enqueue rejects an unreceipted Session root"
    );
    assert_eq!(
        store
            .reserve_session_run(enqueue_request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR0 enqueue left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR0/no-row enqueue"
    );
    assert!(
        store
            .reject_session_run_reservation(enqueue_request.run_id())
            .await
            .expect("SR0 remove enqueue probe reservation")
    );

    let options_session = thread_id(ns, "reservation-ordinary-options-session");
    let options_request = dispatch(
        ns,
        "reservation-ordinary-options",
        "reservation-ordinary-options-session",
    )
    .for_session(options_session);
    assert!(
        store
            .enqueue_with(options_request.clone(), SubmitOptions::default())
            .await
            .is_err(),
        "SR0/E2 enqueue_with rejects an unreceipted Session root"
    );
    assert_eq!(
        store
            .reserve_session_run(options_request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR0 enqueue_with left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR0/no-row enqueue_with"
    );
    assert!(
        store
            .reject_session_run_reservation(options_request.run_id())
            .await
            .expect("SR0 remove enqueue_with probe reservation")
    );

    let claim_session = thread_id(ns, "reservation-ordinary-claim-session");
    let claim_request = dispatch(
        ns,
        "reservation-ordinary-claim",
        "reservation-ordinary-claim-session",
    )
    .for_session(claim_session);
    assert!(
        store
            .claim_new_run(
                claim_request.clone(),
                "reservation-ordinary-claim-owner",
                LEASE_MS,
                69_000,
                &Default::default(),
            )
            .await
            .is_err(),
        "SR0/E2 atomic local claim rejects an unreceipted Session root"
    );
    assert_eq!(
        store
            .reserve_session_run(claim_request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR0 atomic local claim left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR0/no-row claim_new_run"
    );
    assert!(
        store
            .reject_session_run_reservation(claim_request.run_id())
            .await
            .expect("SR0 remove claim probe reservation")
    );

    let compatible_session = thread_id(ns, "reservation-ordinary-compatible-session");
    let compatible_request = dispatch(
        ns,
        "reservation-ordinary-compatible",
        "reservation-ordinary-compatible-session",
    )
    .for_session(compatible_session);
    let worker = ready_dispatch_worker(ns, "reservation-ordinary-compatible");
    assert!(
        store
            .claim_new_run_compatible(compatible_request.clone(), &worker, LEASE_MS, 69_000)
            .await
            .is_err(),
        "SR0/E2 atomic compatible claim rejects an unreceipted Session root"
    );
    assert_eq!(
        store
            .reserve_session_run(compatible_request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR0 atomic compatible claim left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR0/no-row claim_new_run_compatible"
    );
    assert!(
        store
            .reject_session_run_reservation(compatible_request.run_id())
            .await
            .expect("SR0 remove compatible probe reservation")
    );

    let invalid_thread = thread_id(ns, "reservation-invalid-thread");
    assert!(
        store
            .reserve_session_run(
                dispatch(ns, "reservation-no-affinity", "reservation-invalid-thread"),
                RESERVATION_TTL_MS,
            )
            .await
            .is_err(),
        "SR1/E2 requires self-affinity"
    );
    assert!(
        store
            .reserve_session_run(
                dispatch(ns, "reservation-preactivated", "reservation-invalid-thread",)
                    .for_session(invalid_thread.clone())
                    .with_session_activity_epoch(1),
                RESERVATION_TTL_MS,
            )
            .await
            .is_err(),
        "SR1/E2 rejects a pre-bound activity"
    );
    assert!(
        store
            .reserve_session_run(
                dispatch(
                    ns,
                    "reservation-zero-deadline",
                    "reservation-invalid-thread",
                )
                .for_session(invalid_thread),
                0,
            )
            .await
            .is_err(),
        "SR1/E2 requires a nonzero TTL"
    );

    let session = thread_id(ns, "reservation-session");
    let request =
        dispatch(ns, "reservation-activate", "reservation-session").for_session(session.clone());
    let run = request.run_id().clone();
    clock.set(70_000);
    assert_eq!(
        store
            .reserve_session_run(request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR1 reserve"),
        SessionRunReservationOutcome::Reserved,
        "SR1/E1"
    );
    assert!(
        store
            .claim(
                "reservation-too-early",
                LEASE_MS,
                70_000,
                &Default::default()
            )
            .await
            .expect("SR1 early claim")
            .is_none(),
        "SR1/E1 remains unclaimable"
    );
    assert_eq!(
        store
            .reserve_session_run(request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR2 exact replay"),
        SessionRunReservationOutcome::AlreadyReserved,
        "SR2/E3"
    );
    let mut reprojected = request.clone();
    reprojected.activation.snapshot.resolved_spec.instructions =
        "a newer current projection that must not replace the reservation".into();
    reprojected.activation.model_ref_override = Some("current-model-route".into());
    reprojected.placement = PlacementRequirements::remote_required();
    assert_eq!(
        store
            .reserve_session_run(reprojected, RESERVATION_TTL_MS)
            .await
            .expect("SR2 cold projection replay"),
        SessionRunReservationOutcome::AlreadyReserved,
        "SR2/C8/E3"
    );
    let mut changed_input = request.clone();
    changed_input.activation.input = vec![awaken_agent_contract::agent::message::Message::text(
        awaken_agent_contract::agent::message::Id("changed-reservation-input".into()),
        awaken_agent_contract::agent::message::Role::User,
        "different Session command",
    )];
    assert_eq!(
        store
            .reserve_session_run(changed_input, RESERVATION_TTL_MS)
            .await
            .expect("SR2 command conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR2/E2"
    );
    let conflicting = dispatch(ns, "reservation-activate", "reservation-other")
        .for_session(thread_id(ns, "reservation-other"));
    assert_eq!(
        store
            .reserve_session_run(conflicting, RESERVATION_TTL_MS)
            .await
            .expect("SR2 conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR2/E2"
    );
    assert!(
        store
            .activate_session_run_reservation(&run, &session, 0)
            .await
            .is_err(),
        "SR3/E2 zero epoch"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run, &thread_id(ns, "reservation-other"), 7)
            .await
            .expect("SR3 wrong Session"),
        SessionRunReservationActivation::Conflict,
        "SR3/E2 Session fence"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run, &session, 7)
            .await
            .expect("SR3 activate"),
        SessionRunReservationActivation::Activated,
        "SR3/E1"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run, &session, 7)
            .await
            .expect("SR6 activation replay"),
        SessionRunReservationActivation::AlreadyActivated {
            session_activity_epoch: 7,
        },
        "SR6/E3"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run, &session, 8)
            .await
            .expect("SR3 conflicting epoch"),
        SessionRunReservationActivation::Conflict,
        "SR3/E2 epoch fence"
    );
    let activated = store
        .claim_run(
            &run,
            "reservation-executor",
            LEASE_MS,
            70_001,
            &Default::default(),
        )
        .await
        .expect("SR3 ordinary claim")
        .expect("SR3 activated Run is Pending");
    assert!(!activated.session_activity_admission_required, "SR3/E5");
    assert_eq!(activated.request.session_activity_epoch, Some(7), "SR3/E5");
    assert_eq!(
        store
            .settle(&run, activated.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("SR6 complete"),
        SettleOutcome::Applied,
        "SR6/E8"
    );
    assert_eq!(
        store
            .reserve_session_run(request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR6 completed replay"),
        SessionRunReservationOutcome::Completed,
        "SR6/E8"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run, &session, 7)
            .await
            .expect("SR6 completed activation"),
        SessionRunReservationActivation::Completed,
        "SR6/E8"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&run_id(ns, "reservation-missing"), &session, 1,)
            .await
            .expect("SR6 missing activation"),
        SessionRunReservationActivation::MissingOrRejected,
        "SR6/E3"
    );

    // SR9 exercises the typed replacement axis independently of activity
    // admission. Causes: C1 one older Session Run has reached Awaiting; C2 a
    // second reservation says PreservePrior or SupersedePrior; C3 the exact
    // replacement is replayed; C4 a prior row is still Pending. Effects: E1
    // preserve leaves Awaiting unchanged; E2 supersede atomically stores the
    // replacement as Reserved and marks the older row Superseded; E3 replay
    // creates no additional state transition; E4 C4 rejects without inserting
    // or mutating either row. Constraint: Awaiting has already crossed the
    // Session activity-settlement boundary; every other live phase may still
    // own activity and is therefore unsafe to replace in queue storage.
    let replacement_session = thread_id(ns, "reservation-replacement-session");
    let prior = dispatch(
        ns,
        "reservation-replacement-prior",
        "reservation-replacement-session",
    )
    .for_session(replacement_session.clone());
    store
        .reserve_session_run(prior.clone(), RESERVATION_TTL_MS)
        .await
        .expect("SR9 reserve prior");
    store
        .activate_session_run_reservation(prior.run_id(), &replacement_session, 21)
        .await
        .expect("SR9 activate prior");
    let prior_claim = store
        .claim_run(
            prior.run_id(),
            "reservation-replacement-prior-worker",
            LEASE_MS,
            70_010,
            &Default::default(),
        )
        .await
        .expect("SR9 claim prior")
        .expect("SR9 prior is executable");
    assert_eq!(
        store
            .settle(
                prior.run_id(),
                prior_claim.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("SR9 park prior Awaiting"),
        SettleOutcome::Applied,
        "SR9/C1"
    );

    let preserved = dispatch(
        ns,
        "reservation-replacement-preserve",
        "reservation-replacement-session",
    )
    .for_session(replacement_session.clone());
    store
        .reserve_session_run(preserved.clone(), RESERVATION_TTL_MS)
        .await
        .expect("SR9 preserve reservation");
    let rows = store.list_dispatches().await.expect("SR9 preserve rows");
    assert_eq!(
        rows.iter()
            .find(|row| row.run_id == *prior.run_id())
            .map(|row| row.state),
        Some(DispatchState::Awaiting),
        "SR9/E1 preserve does not mutate prior work"
    );
    assert!(
        store
            .reject_session_run_reservation(preserved.run_id())
            .await
            .expect("SR9 remove preserve probe")
    );

    let replacement = dispatch(
        ns,
        "reservation-replacement-newest",
        "reservation-replacement-session",
    )
    .for_session(replacement_session.clone())
    .with_session_run_replacement(SessionRunReplacement::SupersedePrior);
    assert_eq!(
        store
            .reserve_session_run(replacement.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR9 superseding reservation"),
        SessionRunReservationOutcome::Reserved,
        "SR9/E2"
    );
    let rows = store.list_dispatches().await.expect("SR9 replacement rows");
    assert_eq!(
        rows.iter()
            .find(|row| row.run_id == *prior.run_id())
            .map(|row| row.state),
        Some(DispatchState::Superseded),
        "SR9/E2 prior transition is atomic with replacement reservation"
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.run_id == *replacement.run_id())
            .map(|row| row.state),
        Some(DispatchState::Reserved),
        "SR9/E2 replacement is still unclaimable"
    );
    assert_eq!(
        store
            .reserve_session_run(replacement.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR9 exact replay"),
        SessionRunReservationOutcome::AlreadyReserved,
        "SR9/E3"
    );
    assert!(
        store
            .reject_session_run_reservation(replacement.run_id())
            .await
            .expect("SR9 remove replacement probe")
    );

    let unsafe_session = thread_id(ns, "reservation-replacement-unsafe-session");
    let unsafe_prior = dispatch(
        ns,
        "reservation-replacement-unsafe-prior",
        "reservation-replacement-unsafe-session",
    )
    .for_session(unsafe_session.clone());
    store
        .reserve_session_run(unsafe_prior.clone(), RESERVATION_TTL_MS)
        .await
        .expect("SR9 reserve unsafe prior");
    store
        .activate_session_run_reservation(unsafe_prior.run_id(), &unsafe_session, 22)
        .await
        .expect("SR9 activate unsafe prior");
    let unsafe_replacement = dispatch(
        ns,
        "reservation-replacement-unsafe-new",
        "reservation-replacement-unsafe-session",
    )
    .for_session(unsafe_session)
    .with_session_run_replacement(SessionRunReplacement::SupersedePrior);
    assert!(
        store
            .reserve_session_run(unsafe_replacement.clone(), RESERVATION_TTL_MS)
            .await
            .is_err(),
        "SR9/E4 Pending Session activity cannot be stranded"
    );
    let rows = store.list_dispatches().await.expect("SR9 unsafe rows");
    assert_eq!(
        rows.iter()
            .find(|row| row.run_id == *unsafe_prior.run_id())
            .map(|row| row.state),
        Some(DispatchState::Pending),
        "SR9/E4 prior remains executable"
    );
    assert!(
        rows.iter()
            .all(|row| row.run_id != *unsafe_replacement.run_id()),
        "SR9/E4 rejected replacement leaves no partial row"
    );
    let unsafe_claim = store
        .claim_run(
            unsafe_prior.run_id(),
            "reservation-replacement-unsafe-worker",
            LEASE_MS,
            70_011,
            &Default::default(),
        )
        .await
        .expect("SR9 claim preserved unsafe prior")
        .expect("SR9 unsafe prior remains claimable");
    settle_done(store, &unsafe_claim).await;

    // SR7 models the initial caller's crash window without inventing a second
    // admission owner: cancellation records terminal intent, but the original
    // caller can still commit its Session CAS and activate before the deadline.
    let cancelled_request = dispatch(
        ns,
        "reservation-cancel-window",
        "reservation-cancel-window-thread",
    )
    .for_session(thread_id(ns, "reservation-cancel-window-thread"));
    let cancelled_run = cancelled_request.run_id().clone();
    let cancelled_session = cancelled_request.thread_id().clone();
    store
        .reserve_session_run(cancelled_request, RESERVATION_TTL_MS)
        .await
        .expect("SR7 reserve cancellation window");
    assert_eq!(
        store.cancel(&cancelled_run).await.expect("SR7 cancel"),
        Some(cancelled_session.clone()),
        "SR7/E9 records cancellation"
    );
    assert!(
        store
            .claim_run(
                &cancelled_run,
                "reservation-cancel-too-early",
                LEASE_MS,
                75_001,
                &Default::default(),
            )
            .await
            .expect("SR7 early cancelled claim")
            .is_none(),
        "SR7/E9 cancellation cannot steal the exclusive admission window"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&cancelled_run, &cancelled_session, 6)
            .await
            .expect("SR7 initial caller activation"),
        SessionRunReservationActivation::Activated,
        "SR7/E9"
    );
    let cancelled = store
        .claim_run(
            &cancelled_run,
            "reservation-cancel-executor",
            LEASE_MS,
            75_002,
            &Default::default(),
        )
        .await
        .expect("SR7 cancellation claim")
        .expect("SR7 activated cancellation is ordinary Pending");
    assert!(cancelled.cancellation_requested, "SR7/E9");
    assert!(
        !cancelled.session_activity_admission_required,
        "SR7/E9 uses the ordinary cancellation path"
    );
    settle_done(store, &cancelled).await;

    let mut repair_placement = PlacementRequirements::remote_required();
    let repair_capability = format!("{ns}-missing-repair-capability");
    repair_placement
        .required_capabilities
        .insert(repair_capability.clone());
    let repair_request = dispatch(ns, "reservation-repair", "reservation-repair-thread")
        .for_session(thread_id(ns, "reservation-repair-thread"))
        .with_placement(repair_placement);
    let repair_run = repair_request.run_id().clone();
    let repair_session = repair_request.thread_id().clone();
    clock.set(80_000);
    store
        .reserve_session_run(repair_request.clone(), 1)
        .await
        .expect("SR4 reserve repair");
    clock.set(80_002);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let first_repair = store
        .claim(
            "reservation-repairer",
            LEASE_MS,
            80_002,
            &Default::default(),
        )
        .await
        .expect("SR4 repair claim")
        .expect("SR4 expired reservation");
    assert_eq!(first_repair.lease.run_id, repair_run, "SR4/E3");
    assert!(first_repair.session_activity_admission_required, "SR4/E4");
    assert!(first_repair.assignment.is_none(), "SR4/E4");
    assert!(first_repair.credential_bindings.is_empty(), "SR4/E4");
    assert_eq!(
        store
            .reserve_session_run(repair_request.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR4 replay during repair"),
        SessionRunReservationOutcome::RecoveryClaimed,
        "SR4/E3"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&repair_run, &repair_session, 9)
            .await
            .expect("SR4 activate during repair"),
        SessionRunReservationActivation::RecoveryClaimed,
        "SR4/E3"
    );
    assert_eq!(
        store
            .bind_sandbox(&RunClaim::from(&first_repair.lease), "must-not-bind")
            .await
            .expect("SR4 sandbox fence"),
        SettleOutcome::Fenced,
        "SR4/E4"
    );

    // The exact expiry boundary remains owned by the first repairer. Its next
    // millisecond is reclaimed through claim_new_run even though this request is
    // deliberately impossible for ordinary local execution.
    if clock.exact_boundary_is_controllable() {
        clock.set(first_repair.lease.expires_ms);
        assert!(
            store
                .claim_new_run(
                    repair_request.clone(),
                    "reservation-repair-boundary",
                    LEASE_MS,
                    first_repair.lease.expires_ms,
                    &Default::default(),
                )
                .await
                .expect("SR8 exact lease boundary")
                .is_none(),
            "SR8/E11 exact expiry remains live"
        );
    }
    clock.advance_past(first_repair.lease.expires_ms).await;
    let mut repair = store
        .claim_new_run(
            repair_request.clone(),
            "reservation-repairer-after-crash",
            LEASE_MS,
            first_repair.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("SR8 crashed repair reclaim")
        .expect("SR8 expired repair is reclaimed without local placement");
    assert!(repair.session_activity_admission_required, "SR8/E10");
    assert!(repair.assignment.is_none(), "SR8/E10");
    assert!(repair.credential_bindings.is_empty(), "SR8/E10");
    assert!(repair.lease.epoch > first_repair.lease.epoch, "SR8/E11");
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &RunClaim::from(&first_repair.lease),
                SessionRunReservationResolution::Admitted {
                    session_activity_epoch: 9,
                },
            )
            .await
            .expect("SR8 crashed owner resolution"),
        SettleOutcome::Fenced,
        "SR8/E11"
    );
    assert!(
        store
            .resolve_claimed_session_run_reservation(
                &RunClaim::from(&repair.lease),
                SessionRunReservationResolution::Admitted {
                    session_activity_epoch: 0,
                },
            )
            .await
            .is_err(),
        "SR5/E2 invalid admitted epoch"
    );
    assert!(
        store
            .resolve_claimed_session_run_reservation(
                &RunClaim::from(&repair.lease),
                SessionRunReservationResolution::Retry {
                    reservation_ttl_ms: 0,
                },
            )
            .await
            .is_err(),
        "SR5/E2 invalid retry deadline"
    );

    #[derive(Debug, Clone, Copy)]
    enum RepairClaimSurface {
        ClaimNewCompatible,
        ExactCompatible,
        DeliverCompatible,
        BroadCompatible,
        Placed,
    }
    let incompatible_worker = ready_dispatch_worker(ns, "reservation-incompatible");
    for (index, surface) in [
        RepairClaimSurface::ClaimNewCompatible,
        RepairClaimSurface::ExactCompatible,
        RepairClaimSurface::DeliverCompatible,
        RepairClaimSurface::BroadCompatible,
        RepairClaimSurface::Placed,
    ]
    .into_iter()
    .enumerate()
    {
        let deadline = 82_000 + index as u64 * 1_000;
        assert_eq!(
            store
                .resolve_claimed_session_run_reservation(
                    &RunClaim::from(&repair.lease),
                    SessionRunReservationResolution::Retry {
                        reservation_ttl_ms: 1,
                    },
                )
                .await
                .expect("SR8 retry before claim-surface check"),
            SettleOutcome::Applied,
            "SR8/E6 {surface:?}"
        );
        clock.set(deadline + 1);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let claimed = match surface {
            RepairClaimSurface::ClaimNewCompatible => {
                store
                    .claim_new_run_compatible(
                        repair_request.clone(),
                        &incompatible_worker,
                        LEASE_MS,
                        deadline + 1,
                    )
                    .await
            }
            RepairClaimSurface::ExactCompatible => {
                store
                    .claim_run_compatible(&repair_run, &incompatible_worker, LEASE_MS, deadline + 1)
                    .await
            }
            RepairClaimSurface::DeliverCompatible => {
                store
                    .deliver_and_claim_compatible(
                        PendingInput {
                            message_id: format!("{ns}-reservation-repair-input"),
                            run_id: repair_run.clone(),
                            thread_id: repair_session.clone(),
                            correlation_id: String::new(),
                            available_at_ms: None,
                            context_messages: Vec::new(),
                            result: ResumeResult::Input("repair probe".to_string()),
                        },
                        &incompatible_worker,
                        LEASE_MS,
                        deadline + 1,
                    )
                    .await
            }
            RepairClaimSurface::BroadCompatible => {
                store
                    .claim_compatible(&incompatible_worker, LEASE_MS, deadline + 1)
                    .await
            }
            RepairClaimSurface::Placed => {
                store
                    .claim_placed(
                        &incompatible_worker,
                        vec![incompatible_worker.clone()],
                        std::sync::Arc::new(LeastLoadedPolicy),
                        LEASE_MS,
                        deadline + 1,
                    )
                    .await
            }
        }
        .unwrap_or_else(|error| panic!("SR8 {surface:?} claim failed: {error}"))
        .unwrap_or_else(|| panic!("SR8 {surface:?} skipped admission-only repair"));
        assert_eq!(claimed.lease.run_id, repair_run, "SR8/E10 {surface:?}");
        assert!(
            claimed.session_activity_admission_required,
            "SR8/E10 {surface:?}"
        );
        assert!(claimed.assignment.is_none(), "SR8/E10 {surface:?}");
        assert!(
            claimed.credential_bindings.is_empty(),
            "SR8/E10 {surface:?}"
        );
        repair = claimed;
    }

    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &RunClaim::from(&repair.lease),
                SessionRunReservationResolution::Admitted {
                    session_activity_epoch: 9,
                },
            )
            .await
            .expect("SR5 admit repair"),
        SettleOutcome::Applied,
        "SR5/E5"
    );
    let mut compatible_worker = ready_dispatch_worker(ns, "reservation-compatible");
    compatible_worker
        .manifest
        .capabilities
        .insert(repair_capability);
    compatible_worker.capability_fingerprint = compatible_worker
        .manifest
        .fingerprint()
        .expect("reservation-compatible Worker manifest fingerprints");
    let repaired = store
        .claim_run_compatible(&repair_run, &compatible_worker, LEASE_MS, 87_002)
        .await
        .expect("SR5 ordinary compatible repaired claim")
        .expect("SR5 repaired reservation is compatible Pending work");
    assert!(!repaired.session_activity_admission_required, "SR5/E5");
    assert_eq!(repaired.request.session_activity_epoch, Some(9), "SR5/E5");
    assert_eq!(
        store
            .bind_sandbox(&RunClaim::from(&repaired.lease), "ordinary-sandbox")
            .await
            .expect("SR5 ordinary sandbox bind"),
        SettleOutcome::Applied,
        "SR5/E5"
    );
    store
        .settle(
            &repair_run,
            repaired.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("SR5 settle repaired Run");

    let rejected_request = dispatch(ns, "reservation-reject", "reservation-reject-thread")
        .for_session(thread_id(ns, "reservation-reject-thread"));
    let rejected_run = rejected_request.run_id().clone();
    let rejected_session = rejected_request.thread_id().clone();
    clock.set(90_000);
    store
        .reserve_session_run(rejected_request, 1)
        .await
        .expect("SR5 reserve rejection");
    clock.set(90_002);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let rejected = store
        .claim(
            "reservation-rejecter",
            LEASE_MS,
            90_002,
            &Default::default(),
        )
        .await
        .expect("SR5 rejection claim")
        .expect("SR5 rejection reservation");
    assert_eq!(rejected.lease.run_id, rejected_run, "SR5/E7");
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &RunClaim::from(&rejected.lease),
                SessionRunReservationResolution::Rejected,
            )
            .await
            .expect("SR5 reject"),
        SettleOutcome::Applied,
        "SR5/E7"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&rejected_run, &rejected_session, 11)
            .await
            .expect("SR6 rejected activation"),
        SessionRunReservationActivation::MissingOrRejected,
        "SR6/E7"
    );

    let direct_request = dispatch(
        ns,
        "reservation-direct-reject",
        "reservation-direct-reject-thread",
    )
    .for_session(thread_id(ns, "reservation-direct-reject-thread"));
    let direct_run = direct_request.run_id().clone();
    let direct_session = direct_request.thread_id().clone();
    store
        .reserve_session_run(direct_request, RESERVATION_TTL_MS)
        .await
        .expect("SR5 reserve direct rejection");
    assert!(
        store
            .reject_session_run_reservation(&direct_run)
            .await
            .expect("SR5 direct rejection"),
        "SR5/E7 removes an unleased reservation"
    );
    assert!(
        !store
            .reject_session_run_reservation(&direct_run)
            .await
            .expect("SR5 direct rejection replay"),
        "SR5/E7 exact rejection replay is a no-op"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(&direct_run, &direct_session, 12)
            .await
            .expect("SR6 directly rejected activation"),
        SessionRunReservationActivation::MissingOrRejected,
        "SR6/E7"
    );
}

async fn current_claim_guard_is_exact(
    store: &dyn DispatchQueue,
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
        .claim_run(
            &run,
            &identity.lease_owner(),
            LEASE_MS,
            20_000,
            &Default::default(),
        )
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
            .claim_is_current(&stale, 20_000)
            .await
            .expect("stale current-claim query"),
        "a stale epoch is never current"
    );
    assert!(
        store
            .claim_is_current(&current, claimed.lease.expires_ms)
            .await
            .expect("exact-boundary current-claim query"),
        "the exact lease expiry boundary remains current"
    );
    assert_eq!(
        store
            .worker_owns_run(&identity, &run, claimed.lease.expires_ms)
            .await
            .expect("registered Worker ownership query")
            .as_ref(),
        Some(&current),
        "the exact Worker incarnation receives the live claim"
    );
    clock.advance_past(claimed.lease.expires_ms).await;
    assert!(
        !store
            .claim_is_current(&current, claimed.lease.expires_ms.saturating_add(1))
            .await
            .expect("expired current-claim query"),
        "an expired claim is not current before it is reclaimed"
    );
    let stale_identity = WorkerIdentity::new(identity.worker_id.clone(), "replacement", 2);
    assert!(
        store
            .worker_owns_run(&stale_identity, &run, 20_000)
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
    assert!(
        store
            .worker_owns_run(&identity, &run, 20_000)
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
    let cancel_claimed = store
        .claim_run(
            &cancel_run,
            &identity.lease_owner(),
            LEASE_MS,
            20_000,
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
    assert!(
        store
            .worker_owns_run(&identity, &cancel_run, 20_000)
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
    let cancellation_claim = store
        .claim_run(
            &cancel_run,
            &identity.lease_owner(),
            LEASE_MS,
            20_000,
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
    store: &dyn DispatchQueue,
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
        .claim_run(
            &run,
            "conformance-sandbox-a",
            LEASE_MS,
            30_000,
            &Default::default(),
        )
        .await
        .expect("claim sandbox run")
        .expect("sandbox run is runnable");
    let sandbox_ref = format!("opaque:{ns}");
    let bound = store
        .bind_sandbox(&RunClaim::from(&first.lease), &sandbox_ref)
        .await
        .expect("bind sandbox");
    assert_eq!(bound, SettleOutcome::Applied, "current claim binds sandbox");
    clock.advance_past(first.lease.expires_ms).await;
    let recovered = store
        .claim_run(
            &run,
            "conformance-sandbox-b",
            LEASE_MS,
            first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .expect("recover sandbox run")
        .expect("expired sandbox run is recoverable");
    assert_eq!(recovered.sandbox.as_deref(), Some(sandbox_ref.as_str()));
    assert!(recovered.recovered);
    assert_eq!(
        store
            .settle(&run, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle sandbox run"),
        SettleOutcome::Applied
    );
}

async fn completion_is_atomic_and_prevents_resurrection(
    store: &dyn DispatchQueue,
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
            40_000,
            &Default::default(),
        )
        .await
        .expect("claim awaiting control run")
        .expect("awaiting control run is runnable");
    assert_eq!(
        store
            .settle(
                &awaiting,
                awaiting_claim.lease.epoch,
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
            40_000,
            &Default::default(),
        )
        .await
        .expect("claim completion run")
        .expect("completion run is runnable");
    assert_eq!(
        store
            .settle(
                &done,
                claim.lease.epoch.saturating_sub(1),
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
            .settle(&done, claim.lease.epoch, DispatchOutcome::Done, &[])
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
    assert!(
        store
            .claim_run(
                &done,
                "conformance-replay",
                LEASE_MS,
                40_001,
                &Default::default(),
            )
            .await
            .expect("exact claim after replay")
            .is_none(),
        "completed run id does not resurrect through enqueue"
    );
    assert!(
        store
            .claim_new_run(
                request.clone(),
                "conformance-replay",
                LEASE_MS,
                40_001,
                &Default::default(),
            )
            .await
            .expect("atomic admission after replay")
            .is_none(),
        "completed run id does not resurrect through claim_new_run"
    );
    let worker = ready_dispatch_worker(ns, "completion");
    assert!(
        store
            .claim_new_run_compatible(request, &worker, LEASE_MS, 40_001)
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
