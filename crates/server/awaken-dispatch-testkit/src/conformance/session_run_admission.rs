// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Original Session admission conformance, extracted as one whole caller responsibility.
// @formal reason: Included at the same crate root; existing reservation/lifecycle fixtures and function paths stay authoritative.
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
/// Agent/model/Resource/placement/Skill projections. C10 live or completed
/// durable evidence has a current/missing/unknown versioned command fingerprint,
/// and its operation/capability inputs are exact or changed.
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
/// E14 only a current fingerprint can create a row; operation/capability changes
/// conflict against live and completed evidence while trace/projection drift
/// neither conflicts nor overwrites.
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
/// | SR10 | current/missing/unknown; exact/changed op or caps | live/completed | reserve/replay | E1/E14/E2 |
async fn session_run_reservation_is_atomic_and_recoverable(
    store: &dyn ConformanceDispatch,
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
    let enqueue_request = current_session_command(
        dispatch(
            ns,
            "reservation-ordinary-enqueue",
            "reservation-ordinary-enqueue-session",
        )
        .for_session(enqueue_session),
    );
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
    // O2 Reserved: observe the original port-owned full request, not just its
    // id. The same row and original completion cursor must stutter; this read
    // neither activates the reservation nor freezes another projection.
    let reserved_rows = store.list_dispatches().await.unwrap();
    let reserved_completion = store.completion_events_after(0, usize::MAX).await.unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(enqueue_request.run_id())
            .await
            .unwrap(),
        awaken_dispatch_contract::SessionRunAdmissionObservation::Live(Box::new(
            enqueue_request.clone().with_session_command_identity()
        )),
        "O2 Reserved full A"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        reserved_rows,
        "O2 Reserved state stutter"
    );
    assert_eq!(
        store.completion_events_after(0, usize::MAX).await.unwrap(),
        reserved_completion,
        "O2 Reserved completion cursor stutter"
    );
    assert!(
        store
            .reject_session_run_reservation(enqueue_request.run_id())
            .await
            .expect("SR0 remove enqueue probe reservation")
    );

    let options_session = thread_id(ns, "reservation-ordinary-options-session");
    let options_request = current_session_command(
        dispatch(
            ns,
            "reservation-ordinary-options",
            "reservation-ordinary-options-session",
        )
        .for_session(options_session),
    );
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
    let claim_request = current_session_command(
        dispatch(
            ns,
            "reservation-ordinary-claim",
            "reservation-ordinary-claim-session",
        )
        .for_session(claim_session),
    );
    clock.set(69_000);
    assert!(
        store
            .claim_new_run(
                claim_request.clone(),
                "reservation-ordinary-claim-owner",
                LEASE_MS,
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
    let compatible_request = current_session_command(
        dispatch(
            ns,
            "reservation-ordinary-compatible",
            "reservation-ordinary-compatible-session",
        )
        .for_session(compatible_session),
    );
    let worker = ready_dispatch_worker(ns, "reservation-ordinary-compatible");
    clock.set(69_000);
    assert!(
        store
            .claim_new_run_compatible(compatible_request.clone(), &worker, LEASE_MS)
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

    // SR10 negative wire partitions prove validation precedes persistence:
    // missing and unknown incoming versions fail closed, then the same Run id
    // remains available to its current command.
    let missing = dispatch(
        ns,
        "reservation-missing-fingerprint",
        "reservation-missing-fingerprint-session",
    )
    .for_session(thread_id(ns, "reservation-missing-fingerprint-session"));
    assert!(
        store
            .reserve_session_run(missing.clone(), RESERVATION_TTL_MS)
            .await
            .is_err(),
        "SR10/E14 missing incoming fingerprint is rejected"
    );
    let missing_current = current_session_command(missing);
    assert_eq!(
        store
            .reserve_session_run(missing_current.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR10 missing rejection left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR10/E1"
    );
    assert!(
        store
            .reject_session_run_reservation(missing_current.run_id())
            .await
            .expect("SR10 remove missing-version probe")
    );

    let mut unknown = current_session_command(
        dispatch(
            ns,
            "reservation-unknown-fingerprint",
            "reservation-unknown-fingerprint-session",
        )
        .for_session(thread_id(ns, "reservation-unknown-fingerprint-session")),
    );
    unknown.session_command_fingerprint = Some(
        serde_json::from_value(serde_json::Value::String(
            "session-command-v2:sha256:future".into(),
        ))
        .expect("unknown durable value remains inspectable"),
    );
    assert!(
        store
            .reserve_session_run(unknown.clone(), RESERVATION_TTL_MS)
            .await
            .is_err(),
        "SR10/E14 unknown incoming fingerprint is rejected"
    );
    let unknown_current = current_session_command(unknown);
    assert_eq!(
        store
            .reserve_session_run(unknown_current.clone(), RESERVATION_TTL_MS)
            .await
            .expect("SR10 unknown rejection left no row"),
        SessionRunReservationOutcome::Reserved,
        "SR10/E1"
    );
    assert!(
        store
            .reject_session_run_reservation(unknown_current.run_id())
            .await
            .expect("SR10 remove unknown-version probe")
    );

    let invalid_thread = thread_id(ns, "reservation-invalid-thread");
    assert!(
        store
            .reserve_session_run(
                current_session_command(dispatch(
                    ns,
                    "reservation-no-affinity",
                    "reservation-invalid-thread",
                )),
                RESERVATION_TTL_MS,
            )
            .await
            .is_err(),
        "SR1/E2 requires self-affinity"
    );
    assert!(
        store
            .reserve_session_run(
                current_session_command(
                    dispatch(ns, "reservation-preactivated", "reservation-invalid-thread",)
                        .for_session(invalid_thread.clone())
                        .with_session_activity_epoch(1),
                ),
                RESERVATION_TTL_MS,
            )
            .await
            .is_err(),
        "SR1/E2 rejects a pre-bound activity"
    );
    assert!(
        store
            .reserve_session_run(
                current_session_command(
                    dispatch(
                        ns,
                        "reservation-zero-deadline",
                        "reservation-invalid-thread",
                    )
                    .for_session(invalid_thread),
                ),
                0,
            )
            .await
            .is_err(),
        "SR1/E2 requires a nonzero TTL"
    );

    let session = thread_id(ns, "reservation-session");
    let request = current_session_command(
        dispatch(ns, "reservation-activate", "reservation-session").for_session(session.clone()),
    );
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
            .claim("reservation-too-early", LEASE_MS, &Default::default())
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
    reprojected.activation.snapshot.id = ExecutableAgentSnapshotId("current-snapshot".into());
    reprojected.activation.input = vec![Message::text(
        MessageId("expanded-skill-input".into()),
        Role::User,
        "expanded by the current Skill projection",
    )];
    reprojected.activation.model_ref_override = Some("current-model-route".into());
    reprojected.traceparent =
        Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into());
    reprojected.session_resources = Some(
        awaken_dispatch_contract::SessionResourceEnvelope::at_revision(
            "current-workspace",
            9,
            r#"{"inputs":[]}"#,
        ),
    );
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
    let changed_input = current_session_command(changed_input);
    assert_eq!(
        store
            .reserve_session_run(changed_input, RESERVATION_TTL_MS)
            .await
            .expect("SR2 command conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR2/E2"
    );
    let changed_operation =
        current_session_command_with_operation(request.clone(), "changed-operation");
    assert_eq!(
        store
            .reserve_session_run(changed_operation, RESERVATION_TTL_MS)
            .await
            .expect("SR10 operation conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR10/E2 operation identity is immutable"
    );
    let mut changed_capabilities = request.clone();
    changed_capabilities
        .placement
        .required_capabilities
        .insert("new-required-capability".into());
    let changed_capabilities = current_session_command(changed_capabilities);
    assert_eq!(
        store
            .reserve_session_run(changed_capabilities, RESERVATION_TTL_MS)
            .await
            .expect("SR10 capability conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR10/E2 empty-to-present required capability is immutable"
    );
    let conflicting = current_session_command(
        dispatch(ns, "reservation-activate", "reservation-other")
            .for_session(thread_id(ns, "reservation-other")),
    );
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
    clock.set(70_001);
    let activated = store
        .claim_run(&run, "reservation-executor", LEASE_MS, &Default::default())
        .await
        .expect("SR3 ordinary claim")
        .expect("SR3 activated Run is Pending");
    assert!(!activated.session_activity_admission_required, "SR3/E5");
    assert_eq!(activated.request.session_activity_epoch, Some(7), "SR3/E5");
    assert_eq!(
        activated.request,
        request
            .clone()
            .with_session_command_identity()
            .with_session_activity_epoch(7),
        "SR2/E14 projection replay never overwrites the first dispatch"
    );
    assert_eq!(
        store
            .settle(&run, activated.lease.epoch(), DispatchOutcome::Done, &[])
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
    let completed_changed_operation =
        current_session_command_with_operation(request.clone(), "changed-completed-operation");
    assert_eq!(
        store
            .reserve_session_run(completed_changed_operation, RESERVATION_TTL_MS)
            .await
            .expect("SR10 completed operation conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR10/E2 completed operation identity is immutable"
    );
    assert!(
        request.placement.required_capabilities.is_empty(),
        "SR10 empty-to-present completed capability precondition"
    );
    let mut completed_changed_capabilities = request.clone();
    completed_changed_capabilities
        .placement
        .required_capabilities
        .insert("new-required-capability-after-completion".into());
    let completed_changed_capabilities = current_session_command(completed_changed_capabilities);
    assert_eq!(
        store
            .reserve_session_run(completed_changed_capabilities, RESERVATION_TTL_MS)
            .await
            .expect("SR10 completed capability conflict classification"),
        SessionRunReservationOutcome::Conflict,
        "SR10/E2 completed empty-to-present required capability is immutable"
    );
    if capabilities.completion_events {
        let completion = store
            .completion_events_after(0, usize::MAX)
            .await
            .expect("SR6 completion fingerprint query")
            .into_iter()
            .find(|completion| completion.run_id == run)
            .expect("SR6 current completion exists");
        assert_eq!(
            completion.request_fingerprint.as_deref(),
            request
                .session_command_fingerprint
                .as_ref()
                .map(SessionRunCommandFingerprint::as_str),
            "SR6/E8 current completion stores the compact current identity"
        );
    }
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

    // Stored-evidence compatibility table. Causes: L1 a stored live row has a
    // current, absent-legacy, or unknown fingerprint; L2 a completion carries
    // current, old sha256, or unknown identity; L3 the incoming command is
    // current and legacy-equivalent. Effects: LE1 current and stored-legacy
    // equivalents replay their exact durable phase; LE2 unknown evidence fails
    // closed in both live and completed phases. Rules L1=absent+L3=>LE1,
    // L2=old-sha256+L3=>LE1, L3=unknown live/completion=>LE2. The stored row,
    // never the incoming request, selects the legacy branch.
    let legacy_session = thread_id(ns, "reservation-legacy-session");
    let legacy_stored = dispatch(ns, "reservation-legacy", "reservation-legacy-session")
        .for_session(legacy_session.clone())
        .with_session_command_identity()
        .with_session_activity_epoch(31);
    store
        .enqueue(legacy_stored.clone())
        .await
        .expect("L1 insert pre-fingerprint live row");
    let legacy_current = current_session_command(
        dispatch(ns, "reservation-legacy", "reservation-legacy-session")
            .for_session(legacy_session),
    );
    assert_eq!(
        store
            .reserve_session_run(legacy_current.clone(), RESERVATION_TTL_MS)
            .await
            .expect("L1 legacy live replay"),
        SessionRunReservationOutcome::AlreadyActivated {
            session_activity_epoch: 31,
        },
        "L1/LE1"
    );
    // P1 a prepared command never borrows the historical reduced Session
    // identity, even when the old fixture above proves ordinary compatibility.
    assert_eq!(
        store
            .reserve_session_run(
                legacy_current
                    .clone()
                    .with_prepared_session_command_identity(),
                RESERVATION_TTL_MS
            )
            .await
            .expect("P1 live legacy refusal"),
        SessionRunReservationOutcome::Conflict,
        "P1 prepared cannot use a live legacy subset",
    );
    clock.set(70_002);
    let legacy_claim = store
        .claim_run(
            legacy_stored.run_id(),
            "reservation-legacy-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("L2 claim legacy row")
        .expect("L2 legacy row is Pending");
    settle_done(store, &legacy_claim).await;
    if capabilities.completion_events {
        let completion = store
            .completion_events_after(0, usize::MAX)
            .await
            .expect("L2 legacy completion fingerprint query")
            .into_iter()
            .find(|completion| completion.run_id == *legacy_stored.run_id())
            .expect("L2 legacy completion exists");
        assert!(
            completion
                .request_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| {
                    fingerprint.starts_with("sha256:")
                        && !fingerprint.starts_with(SessionRunCommandFingerprint::CURRENT_PREFIX)
                }),
            "L2/LE1 old completion retains its legacy sha256 evidence"
        );
    }
    assert_eq!(
        store
            .reserve_session_run(legacy_current.clone(), RESERVATION_TTL_MS)
            .await
            .expect("L2 legacy completion replay"),
        SessionRunReservationOutcome::Completed,
        "L2/LE1"
    );
    assert_eq!(
        store
            .reserve_session_run(
                legacy_current.with_prepared_session_command_identity(),
                RESERVATION_TTL_MS
            )
            .await
            .expect("P1 completed legacy refusal"),
        SessionRunReservationOutcome::Conflict,
        "P1 prepared cannot use an old completion tuple",
    );

    let unknown_session = thread_id(ns, "reservation-unknown-stored-session");
    let mut unknown_stored = current_session_command(
        dispatch(
            ns,
            "reservation-unknown-stored",
            "reservation-unknown-stored-session",
        )
        .for_session(unknown_session.clone()),
    )
    .with_session_command_identity()
    .with_session_activity_epoch(32);
    unknown_stored.session_command_fingerprint = Some(
        serde_json::from_value(serde_json::Value::String(
            "session-command-v2:sha256:future".into(),
        ))
        .expect("L3 unknown durable value remains inspectable"),
    );
    store
        .enqueue(unknown_stored.clone())
        .await
        .expect("L3 insert unknown-version live row");
    let unknown_current = current_session_command(
        dispatch(
            ns,
            "reservation-unknown-stored",
            "reservation-unknown-stored-session",
        )
        .for_session(unknown_session),
    );
    assert_eq!(
        store
            .reserve_session_run(unknown_current.clone(), RESERVATION_TTL_MS)
            .await
            .expect("L3 unknown live classification"),
        SessionRunReservationOutcome::Conflict,
        "L3/LE2"
    );
    clock.set(70_003);
    let unknown_claim = store
        .claim_run(
            unknown_stored.run_id(),
            "reservation-unknown-stored-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("L3 claim unknown stored row")
        .expect("L3 unknown row is Pending");
    settle_done(store, &unknown_claim).await;
    if capabilities.completion_events {
        let completion = store
            .completion_events_after(0, usize::MAX)
            .await
            .expect("L3 unknown completion fingerprint query")
            .into_iter()
            .find(|completion| completion.run_id == *unknown_stored.run_id())
            .expect("L3 unknown completion exists");
        assert_eq!(
            completion.request_fingerprint.as_deref(),
            Some("session-command-v2:sha256:future"),
            "L3/LE2 unknown evidence stays inspectable but grants no replay"
        );
    }
    assert_eq!(
        store
            .reserve_session_run(unknown_current, RESERVATION_TTL_MS)
            .await
            .expect("L3 unknown completion classification"),
        SessionRunReservationOutcome::Conflict,
        "L3/LE2"
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
    let prior = current_session_command(
        dispatch(
            ns,
            "reservation-replacement-prior",
            "reservation-replacement-session",
        )
        .for_session(replacement_session.clone()),
    );
    store
        .reserve_session_run(prior.clone(), RESERVATION_TTL_MS)
        .await
        .expect("SR9 reserve prior");
    store
        .activate_session_run_reservation(prior.run_id(), &replacement_session, 21)
        .await
        .expect("SR9 activate prior");
    clock.set(70_010);
    let prior_claim = store
        .claim_run(
            prior.run_id(),
            "reservation-replacement-prior-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("SR9 claim prior")
        .expect("SR9 prior is executable");
    assert_eq!(
        store
            .settle(
                prior.run_id(),
                prior_claim.lease.epoch(),
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("SR9 park prior Awaiting"),
        SettleOutcome::Applied,
        "SR9/C1"
    );

    let preserved = current_session_command(
        dispatch(
            ns,
            "reservation-replacement-preserve",
            "reservation-replacement-session",
        )
        .for_session(replacement_session.clone()),
    );
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

    let replacement = current_session_command(
        dispatch(
            ns,
            "reservation-replacement-newest",
            "reservation-replacement-session",
        )
        .for_session(replacement_session.clone())
        .with_session_run_replacement(SessionRunReplacement::SupersedePrior),
    );
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
    // O2 Superseded: the existing SR9 transition changed state, not frozen A.
    // Its actual previously claimed request remains readable without making it
    // runnable or consuming the original completion sequence.
    let superseded_completion = store.completion_events_after(0, usize::MAX).await.unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(prior.run_id())
            .await
            .unwrap(),
        awaken_dispatch_contract::SessionRunAdmissionObservation::Live(Box::new(
            prior_claim.request.clone()
        )),
        "O2 Superseded full A"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        rows,
        "O2 Superseded state stutter"
    );
    assert_eq!(
        store.completion_events_after(0, usize::MAX).await.unwrap(),
        superseded_completion,
        "O2 Superseded completion cursor stutter"
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
    let unsafe_prior = current_session_command(
        dispatch(
            ns,
            "reservation-replacement-unsafe-prior",
            "reservation-replacement-unsafe-session",
        )
        .for_session(unsafe_session.clone()),
    );
    store
        .reserve_session_run(unsafe_prior.clone(), RESERVATION_TTL_MS)
        .await
        .expect("SR9 reserve unsafe prior");
    store
        .activate_session_run_reservation(unsafe_prior.run_id(), &unsafe_session, 22)
        .await
        .expect("SR9 activate unsafe prior");
    let unsafe_replacement = current_session_command(
        dispatch(
            ns,
            "reservation-replacement-unsafe-new",
            "reservation-replacement-unsafe-session",
        )
        .for_session(unsafe_session)
        .with_session_run_replacement(SessionRunReplacement::SupersedePrior),
    );
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
    clock.set(70_011);
    let unsafe_claim = store
        .claim_run(
            unsafe_prior.run_id(),
            "reservation-replacement-unsafe-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("SR9 claim preserved unsafe prior")
        .expect("SR9 unsafe prior remains claimable");
    settle_done(store, &unsafe_claim).await;

    // SR7 models the initial caller's crash window without inventing a second
    // admission owner: cancellation records terminal intent, but the original
    // caller can still commit its Session CAS and activate before the deadline.
    let cancelled_request = current_session_command(
        dispatch(
            ns,
            "reservation-cancel-window",
            "reservation-cancel-window-thread",
        )
        .for_session(thread_id(ns, "reservation-cancel-window-thread")),
    );
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
    clock.set(75_001);
    assert!(
        store
            .claim_run(
                &cancelled_run,
                "reservation-cancel-too-early",
                LEASE_MS,
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
    clock.set(75_002);
    let cancelled = store
        .claim_run(
            &cancelled_run,
            "reservation-cancel-executor",
            LEASE_MS,
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
    let repair_request = current_session_command(
        dispatch(ns, "reservation-repair", "reservation-repair-thread")
            .for_session(thread_id(ns, "reservation-repair-thread"))
            .with_placement(repair_placement),
    );
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
        .claim("reservation-repairer", LEASE_MS, &Default::default())
        .await
        .expect("SR4 repair claim")
        .expect("SR4 expired reservation");
    assert_eq!(first_repair.lease.run_id(), &repair_run, "SR4/E3");
    // O2 ReservationLeased: the original admission-repair claim is not an
    // execution grant. Observation returns full A and leaves its claim/state and
    // original completion cursor unchanged.
    let repair_rows = store.list_dispatches().await.unwrap();
    assert_eq!(
        repair_rows
            .iter()
            .find(|row| row.run_id == repair_run)
            .unwrap()
            .state,
        DispatchState::ReservationLeased,
        "O2 original repair phase"
    );
    let repair_completion = store.completion_events_after(0, usize::MAX).await.unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(&repair_run)
            .await
            .unwrap(),
        awaken_dispatch_contract::SessionRunAdmissionObservation::Live(Box::new(
            first_repair.request.clone()
        )),
        "O2 ReservationLeased full A"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        repair_rows,
        "O2 repair state stutter"
    );
    assert_eq!(
        store.completion_events_after(0, usize::MAX).await.unwrap(),
        repair_completion,
        "O2 repair completion cursor stutter"
    );

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

    // The first repairer owns the lease until its expiry instant; from that
    // instant it is reclaimed through claim_new_run even though this request is
    // deliberately impossible for ordinary local execution.
    if clock.exact_boundary_is_controllable() {
        clock.set(first_repair.lease.expires_ms() - 1);
        assert!(
            store
                .claim_new_run(
                    repair_request.clone(),
                    "reservation-repair-boundary",
                    LEASE_MS,
                    &Default::default(),
                )
                .await
                .expect("SR8 exact lease boundary")
                .is_none(),
            "SR8/E11 the repair lease is live one instant before its expiry"
        );
        // The Expiry law: from the exact expiry instant the lease has passed.
        clock.set(first_repair.lease.expires_ms());
    } else {
        clock.advance_past(first_repair.lease.expires_ms()).await;
    }
    let mut repair = store
        .claim_new_run(
            repair_request.clone(),
            "reservation-repairer-after-crash",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("SR8 crashed repair reclaim")
        .expect("SR8 expired repair is reclaimed without local placement");
    assert!(repair.session_activity_admission_required, "SR8/E10");
    assert!(repair.assignment.is_none(), "SR8/E10");
    assert!(repair.credential_bindings.is_empty(), "SR8/E10");
    assert!(repair.lease.epoch() > first_repair.lease.epoch(), "SR8/E11");
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
                    )
                    .await
            }
            RepairClaimSurface::ExactCompatible => {
                store
                    .claim_run_compatible(&repair_run, &incompatible_worker, LEASE_MS)
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
                    )
                    .await
            }
            RepairClaimSurface::BroadCompatible => {
                store.claim_compatible(&incompatible_worker, LEASE_MS).await
            }
            RepairClaimSurface::Placed => {
                store
                    .claim_placed(
                        &incompatible_worker,
                        vec![incompatible_worker.clone()],
                        std::sync::Arc::new(LeastLoadedPolicy),
                        LEASE_MS,
                    )
                    .await
            }
        }
        .unwrap_or_else(|error| panic!("SR8 {surface:?} claim failed: {error}"))
        .unwrap_or_else(|| panic!("SR8 {surface:?} skipped admission-only repair"));
        assert_eq!(claimed.lease.run_id(), &repair_run, "SR8/E10 {surface:?}");
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
    clock.set(87_002);
    let repaired = store
        .claim_run_compatible(&repair_run, &compatible_worker, LEASE_MS)
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
            repaired.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("SR5 settle repaired Run");

    let rejected_request = current_session_command(
        dispatch(ns, "reservation-reject", "reservation-reject-thread")
            .for_session(thread_id(ns, "reservation-reject-thread")),
    );
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
        .claim("reservation-rejecter", LEASE_MS, &Default::default())
        .await
        .expect("SR5 rejection claim")
        .expect("SR5 rejection reservation");
    assert_eq!(rejected.lease.run_id(), &rejected_run, "SR5/E7");
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

    let direct_request = current_session_command(
        dispatch(
            ns,
            "reservation-direct-reject",
            "reservation-direct-reject-thread",
        )
        .for_session(thread_id(ns, "reservation-direct-reject-thread")),
    );
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
    legacy_pending_session_run_adoption(store, ns, clock).await;
}

/// Causes/effects for the real old Outcome crash cut, separate from the
/// pre-fingerprint SessionCommand fixture above. A1 ordinary unbound
/// FullDispatch Pending + exact original + vacant authority -> one admission
/// tenure, no execution. A2 any changed frozen activation/envelope axis -> no
/// tenure and a full state stutter. A3 invalid input/TTL -> error before mutation.
/// A4 current/stale tenure and Retry/Admitted/Rejected -> the existing fenced
/// reservation resolution only. A5 completion/current prepared replay is exact;
/// it cannot reopen the old original or use a legacy tuple. The application
/// separately proves authoritative no-Run observation and Session receipt CAS.
async fn legacy_pending_session_run_adoption(
    store: &dyn ConformanceDispatch,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    // C1 this is an explicit current-v5 Worker but its codec predates the
    // prepared identity. C2 recovery/execution/cancel may bypass other
    // placement axes. E1 all modes still refuse this Worker; local original
    // assertions below remain intact, and a supported Worker cancels normally.
    let mut old_codec = ready_dispatch_worker(ns, "old-prepared-codec");
    old_codec
        .manifest
        .capabilities
        .remove(awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY);
    old_codec.capability_fingerprint = old_codec.manifest.fingerprint().unwrap();
    let original = dispatch(ns, "outcome-old-pending", "outcome-old-pending-thread");
    let fingerprint = SessionRunCommandFingerprint::from_sha256_hex(&"a".repeat(64)).unwrap();
    let incoming = original
        .clone()
        .for_session(original.thread_id().clone())
        .with_session_command_fingerprint(fingerprint.clone())
        .with_prepared_session_command_identity();
    store
        .enqueue(original.clone())
        .await
        .expect("A1 original ordinary enqueue");
    assert_eq!(
        store
            .reserve_session_run(incoming.clone(), LEASE_MS)
            .await
            .unwrap(),
        SessionRunReservationOutcome::Conflict,
        "A1 ordinary reservation cannot claim old Pending"
    );
    let before = store.list_dispatches().await.unwrap();
    let completions_before = store.completion_events_after(0, usize::MAX).await.unwrap();
    let mut changed = Vec::new();
    let mut candidate = original.clone();
    candidate.activation.run_id = run_id(ns, "outcome-other-run");
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation.thread_id = thread_id(ns, "outcome-other-thread");
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate
        .activation
        .snapshot
        .resolved_spec
        .instructions
        .push_str(" changed");
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation.input[0] =
        Message::text(MessageId("changed-input".into()), Role::User, "different");
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation.delegation_origin = Some(DelegationOrigin::root_for_agent(
        RunId("parent".into()),
        "call",
        "agent",
    ));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation.model_ref_override = Some("another-model".into());
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation.data_subject_id =
        Some(awaken_runtime_contract::DataSubjectId("subject".into()));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation = candidate.activation.without_tools();
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.activation = candidate.activation.with_inherited_toolsets(Some(
        awaken_agent_contract::toolset::InheritedToolsets::V1(Vec::new()),
    ));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate
        .placement
        .required_capabilities
        .insert("different-capability".into());
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.placement.session_mcp_credentials = Some(Vec::new());
    assert!(
        original.same_canonical_dispatch(&candidate),
        "A2 precondition: ordinary old-MCP replay has a fallback"
    );
    assert!(
        !original.same_original_dispatch(&candidate),
        "A2 adoption does not borrow even that ordinary fallback"
    );
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.session_runtime = Some(SessionRuntimeEnvelope::new("different-runtime"));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.session_resources =
        Some(awaken_dispatch_contract::SessionResourceEnvelope::at_revision(ns, 1, "{}"));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.execution_scope =
        Some(serde_json::from_value(serde_json::json!("different-scope")).unwrap());
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate
        .agent_publications
        .push(candidate.activation.snapshot.clone());
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.preferred_environment_shape =
        Some(serde_json::from_value(serde_json::json!("different-shape")).unwrap());
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.inference_plaintext_holder = Some(PlaintextHolder::new(
        PlaintextBoundary::Worker,
        "different-holder",
    ));
    changed.push(candidate);
    let mut candidate = original.clone();
    candidate.session_run_replacement = SessionRunReplacement::SupersedePrior;
    changed.push(candidate);
    for candidate in changed {
        assert!(
            store
                .adopt_pending_session_run(
                    candidate,
                    fingerprint.clone(),
                    "adoption-owner",
                    LEASE_MS
                )
                .await
                .unwrap()
                .is_none(),
            "A2 strict original full identity"
        );
        assert_eq!(
            store.list_dispatches().await.unwrap(),
            before,
            "A2 state stutter"
        );
    }
    for (candidate, fp, owner, ttl) in [
        (original.clone(), fingerprint.clone(), "adoption-owner", 0),
        (original.clone(), fingerprint.clone(), "", LEASE_MS),
        (
            incoming.clone(),
            fingerprint.clone(),
            "adoption-owner",
            LEASE_MS,
        ),
        (
            original.clone(),
            serde_json::from_value(serde_json::json!("session-command-v2:sha256:future")).unwrap(),
            "adoption-owner",
            LEASE_MS,
        ),
    ] {
        assert!(
            store
                .adopt_pending_session_run(candidate, fp, owner, ttl)
                .await
                .is_err(),
            "A3 invalid adoption input"
        );
        assert_eq!(
            store.list_dispatches().await.unwrap(),
            before,
            "A3 no partial mutation"
        );
    }
    let claim = store
        .adopt_pending_session_run(
            // Trace is attempt identity, not full execution identity. This
            // retry may be retraced but must retain the first stored request.
            original.clone().with_traceparent(Some(
                "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into(),
            )),
            fingerprint.clone(),
            "adoption-owner",
            LEASE_MS,
        )
        .await
        .unwrap()
        .expect("A1 admission tenure");
    let row = store
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == *original.run_id())
        .unwrap();
    assert_eq!(
        row.state,
        DispatchState::ReservationLeased,
        "A1 admission only"
    );
    assert_eq!(row.session_activity_epoch, None, "A1 no invented receipt");
    assert_eq!(row.attempt_count, 0, "A1 no execution attempt spent");
    assert!(!row.physical_attempt_active, "A1 no physical effect");
    assert_eq!(
        store
            .observe_session_run_admission(original.run_id())
            .await
            .unwrap(),
        awaken_dispatch_contract::SessionRunAdmissionObservation::Live(Box::new(incoming.clone())),
        "A1 all original frozen execution inputs retained"
    );
    assert_eq!(
        store.completion_events_after(0, usize::MAX).await.unwrap(),
        completions_before,
        "A1 no completion fabricated"
    );
    assert!(
        store.lock_commit_epoch(&claim).await.unwrap().is_none(),
        "A1 no execution commit authority"
    );
    let guard = store
        .lock_session_run_reservation_epoch(&claim)
        .await
        .unwrap()
        .expect("A4 admission guard");
    assert_eq!(guard.request(), &incoming, "A4 locked original payload");
    assert!(guard.is_live_at(guard.expires_ms() - 1), "A4 live lease");
    assert!(
        !guard.is_live_at(guard.expires_ms()),
        "A4 exact expiry boundary"
    );
    guard
        .release()
        .await
        .expect("A4 explicit release before second Dispatch operation");
    assert_eq!(
        store
            .reserve_session_run(incoming.clone(), LEASE_MS)
            .await
            .unwrap(),
        SessionRunReservationOutcome::RecoveryClaimed,
        "A4 replay is observation, not authority"
    );
    assert!(
        store
            .adopt_pending_session_run(
                original.clone(),
                fingerprint.clone(),
                "other-owner",
                LEASE_MS
            )
            .await
            .unwrap()
            .is_none(),
        "A4 no duplicate tenure"
    );
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &claim,
                SessionRunReservationResolution::Retry {
                    reservation_ttl_ms: 1
                }
            )
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "A4 Retry remains unclaimable"
    );
    assert!(
        store
            .lock_session_run_reservation_epoch(&claim)
            .await
            .unwrap()
            .is_none(),
        "A4 old claim fenced after Retry"
    );
    let row = store
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == *original.run_id())
        .unwrap();
    assert_eq!(row.state, DispatchState::Reserved, "A4 Retry");
    assert!(
        store
            .adopt_pending_session_run(
                original.clone(),
                fingerprint.clone(),
                "other-owner",
                LEASE_MS
            )
            .await
            .unwrap()
            .is_none(),
        "A4 Reserved cannot acquire another adoption"
    );
    clock
        .advance_past(row.reservation_deadline_ms.expect("A4 store deadline"))
        .await;
    assert!(
        store
            .claim_run_compatible(original.run_id(), &old_codec, LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "C1/C2 prepared recovery cannot bypass codec admission"
    );
    let repair = store
        .claim_run(
            original.run_id(),
            "adoption-repair",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("A4 canonical repair claim");
    assert!(
        repair.session_activity_admission_required,
        "A4 repair still never executes"
    );
    let repair_claim = RunClaim::from(&repair.lease);
    assert!(repair_claim.epoch > claim.epoch, "A4 epoch advances");
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &claim,
                SessionRunReservationResolution::Rejected
            )
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "A4 stale rejection stutters"
    );
    let guard = store
        .lock_session_run_reservation_epoch(&repair_claim)
        .await
        .unwrap()
        .expect("A4 repair guard");
    guard.release().await.unwrap();
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &repair_claim,
                SessionRunReservationResolution::Admitted {
                    session_activity_epoch: 4_001
                }
            )
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "A4 only canonical admitted resolution publishes Pending"
    );
    assert!(
        store
            .claim_run_compatible(original.run_id(), &old_codec, LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "C1/C2 ordinary prepared execution cannot bypass codec admission"
    );
    let execution = store
        .claim_run(
            original.run_id(),
            "adoption-executor",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("A4 ordinary execution claim after admission");
    assert!(
        !execution.session_activity_admission_required,
        "A4 ordinary claim owns execution"
    );
    assert_eq!(
        execution.request.session_activity_epoch,
        Some(4_001),
        "A4 exact receipt"
    );
    settle_done(store, &execution).await;

    assert_eq!(
        store.reserve_session_run(incoming, LEASE_MS).await.unwrap(),
        SessionRunReservationOutcome::Completed,
        "A5 exact prepared completion"
    );
    assert!(
        store
            .adopt_pending_session_run(original, fingerprint.clone(), "later", LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "A5 cannot resurrect completion"
    );

    let refused = dispatch(
        ns,
        "outcome-old-pending-refused",
        "outcome-old-pending-refused-thread",
    );
    store.enqueue(refused.clone()).await.unwrap();
    let claim = store
        .adopt_pending_session_run(refused.clone(), fingerprint, "refusal-owner", LEASE_MS)
        .await
        .unwrap()
        .unwrap();
    let guard = store
        .lock_session_run_reservation_epoch(&claim)
        .await
        .unwrap()
        .unwrap();
    guard.release().await.unwrap();
    assert_eq!(
        store
            .resolve_claimed_session_run_reservation(
                &claim,
                SessionRunReservationResolution::Rejected
            )
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "A4 canonical negative removes only the unstarted intent"
    );
    assert_eq!(
        store
            .observe_session_run_admission(refused.run_id())
            .await
            .unwrap(),
        awaken_dispatch_contract::SessionRunAdmissionObservation::Missing,
        "A4 refusal invents no completion"
    );

    // A6 a real ordinary execution claim is not admission-only authority.
    // Its complete state stutters on an adoption attempt, both while leased
    // and after Awaiting; the existing cancellation path owns later cleanup.
    let leased = dispatch(ns, "outcome-old-leased", "outcome-old-leased-thread");
    store.enqueue(leased.clone()).await.unwrap();
    let execution = store
        .claim_run(
            leased.run_id(),
            "existing-executor",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    for awaiting in [false, true] {
        if awaiting {
            assert_eq!(
                store
                    .settle(
                        leased.run_id(),
                        execution.lease.epoch(),
                        DispatchOutcome::Awaiting,
                        &[]
                    )
                    .await
                    .unwrap(),
                SettleOutcome::Applied,
                "A6 existing Awaiting boundary"
            );
        }
        let before = store.list_dispatches().await.unwrap();
        let fp = SessionRunCommandFingerprint::from_sha256_hex(&"b".repeat(64)).unwrap();
        assert!(
            store
                .adopt_pending_session_run(leased.clone(), fp, "adoption-cannot-execute", LEASE_MS)
                .await
                .unwrap()
                .is_none(),
            "A6 execution or Awaiting authority wins"
        );
        assert_eq!(
            store.list_dispatches().await.unwrap(),
            before,
            "A6 full state stutter"
        );
    }
    store.cancel(leased.run_id()).await.unwrap();
    let cancellation = store
        .claim_run(
            leased.run_id(),
            "existing-cancellation",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    settle_done(store, &cancellation).await;

    // A7 cancellation can return a real claimed row to Pending while its old
    // physical owner remains inside an effect. Pending alone is insufficient:
    // adoption must retain that exact slot and the original request unchanged.
    let occupied = dispatch(ns, "outcome-old-occupied", "outcome-old-occupied-thread");
    store.enqueue(occupied.clone()).await.unwrap();
    let execution = store
        .claim_run(
            occupied.run_id(),
            "physical-predecessor",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let predecessor = RunClaim::from(&execution.lease);
    assert_eq!(
        store.begin_attempt(&predecessor, None).await.unwrap(),
        AttemptAdmission::Applied,
        "A7 original physical owner"
    );
    store.cancel(occupied.run_id()).await.unwrap();
    let before = store.list_dispatches().await.unwrap();
    let pending = before
        .iter()
        .find(|row| row.run_id == *occupied.run_id())
        .unwrap();
    assert_eq!(
        pending.state,
        DispatchState::Pending,
        "A7 actual Pending cut"
    );
    assert!(
        pending.physical_attempt_active,
        "A7 retained physical authority"
    );
    let fp = SessionRunCommandFingerprint::from_sha256_hex(&"c".repeat(64)).unwrap();
    assert!(
        store
            .adopt_pending_session_run(occupied.clone(), fp, "blocked-admission", LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "A7 no adoption over a physical owner"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        before,
        "A7 retained complete row"
    );
    assert_eq!(
        store.finish_attempt(&predecessor, None).await.unwrap(),
        SettleOutcome::Applied,
        "A7 only original return clears physical owner"
    );
    let cancellation = store
        .claim_run(
            occupied.run_id(),
            "physical-cancellation",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    settle_done(store, &cancellation).await;
    let cancel_session = thread_id(ns, "prepared-codec-cancel-thread");
    let cancelled = current_session_command(
        dispatch(ns, "prepared-codec-cancel", "prepared-codec-cancel-thread")
            .for_session(cancel_session),
    )
    .with_prepared_session_command_identity()
    .with_session_activity_epoch(7);
    store.enqueue(cancelled.clone()).await.unwrap();
    store.cancel(cancelled.run_id()).await.unwrap();
    assert!(
        store
            .claim_run_compatible(cancelled.run_id(), &old_codec, LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "C1/C2 cancellation cannot bypass prepared codec admission"
    );
    let supported = ready_dispatch_worker(ns, "new-prepared-codec");
    let cancellation = store
        .claim_run_compatible(cancelled.run_id(), &supported, LEASE_MS)
        .await
        .unwrap()
        .expect("C3 supported declared codec claims cancellation");
    assert!(
        cancellation.cancellation_requested,
        "C3 original cancellation intent"
    );
    settle_done(store, &cancellation).await;
}
