// Dispatch single-writer and bounded Session-child conformance rules. Included
// at crate root so the shared public conformance API retains its original paths.
async fn settle_done(store: &dyn DispatchQueue, claim: &awaken_run_ingress_contract::Claimed) {
    let consumed = claim
        .pending
        .iter()
        .map(|input| input.message_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        store
            .settle(
                &claim.lease.run_id,
                claim.lease.epoch,
                DispatchOutcome::Done,
                &consumed,
            )
            .await
            .expect("settle claimed Run"),
        SettleOutcome::Applied
    );
}

/// ADR-0022 open-Run single-writer cause/effect graph. Causes: C1 a Thread has a
/// Running or Awaiting Run; C2 the candidate is a fresh peer or the exact Awaiting
/// row; C3 the Awaiting row receives its matching input; C4 the claim uses exact,
/// broad-local, compatible, or placed selection; C5 historical state contains two
/// Awaiting rows and both carry cancellation. Effects: E1 a fresh peer remains
/// queued; E2 the exact Awaiting row wakes; E3 after E2 settles Done the peer is
/// claimable; E4 cancellation drains historical Awaiting rows one at a time.
/// Constraints: an expired Running row reclaims itself under the existing recovery
/// rules; cancellation may bypass Awaiting peers but never a Running peer.
///
/// | Rule | Open peer | Candidate | Trigger/surface | Effect |
/// |---|---|---|---|---|
/// | SW1 | Awaiting | fresh | claim_new + exact | E1 |
/// | SW2 | Awaiting(self) | same Run | exact input | E2 then E3 |
/// | SW3 | Awaiting | fresh | broad local | E1 then E3 |
/// | SW4 | Awaiting | fresh | compatible/exact-compatible | E1 then E3 |
/// | SW5 | Awaiting | fresh | placed | E1 then E3 |
/// | SW6 | two Awaiting | cancelled exact rows | broad local | E4 |
async fn open_run_single_writer_is_uniform(store: &dyn DispatchQueue, ns: &str) {
    async fn checkpoint_awaiting(
        store: &dyn DispatchQueue,
        request: &RunDispatch,
        owner: &str,
        now_ms: u64,
    ) {
        store
            .enqueue(request.clone())
            .await
            .expect("enqueue Run that will Await");
        let claim = store
            .claim_run(
                request.run_id(),
                owner,
                LEASE_MS,
                now_ms,
                &Default::default(),
            )
            .await
            .expect("claim Run that will Await")
            .expect("fresh Run is claimable");
        assert_eq!(
            store
                .settle(
                    request.run_id(),
                    claim.lease.epoch,
                    DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("checkpoint Awaiting Run"),
            SettleOutcome::Applied
        );
    }

    fn reply(request: &RunDispatch, suffix: &str) -> PendingInput {
        PendingInput {
            message_id: format!("{}-{suffix}-reply", request.run_id().0),
            run_id: request.run_id().clone(),
            thread_id: request.thread_id().clone(),
            correlation_id: format!("{}-{suffix}-ticket", request.run_id().0),
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Input("resume exact Awaiting Run".to_string()),
        }
    }

    // SW1/SW2: atomic fresh admission persists the peer but cannot claim it;
    // delivery names and wakes only the original Awaiting row.
    let exact_open = dispatch(ns, "single-writer-exact-open", "single-writer-exact-thread");
    checkpoint_awaiting(store, &exact_open, "single-writer-exact-open", 1_000).await;
    let exact_peer = dispatch(ns, "single-writer-exact-peer", "single-writer-exact-thread");
    assert!(
        store
            .claim_new_run(
                exact_peer.clone(),
                "single-writer-exact-peer",
                LEASE_MS,
                1_001,
                &Default::default(),
            )
            .await
            .expect("SW1 atomic peer admission")
            .is_none(),
        "SW1/E1 Awaiting keeps the fresh exact peer queued"
    );
    assert!(
        store
            .claim_run(
                exact_peer.run_id(),
                "single-writer-exact-peer-retry",
                LEASE_MS,
                1_002,
                &Default::default(),
            )
            .await
            .expect("SW1 exact peer claim")
            .is_none(),
        "SW1/E1 exact selection uses the same open-Run fence"
    );
    if let Some(depth) = store
        .runnable_depth(1_002)
        .await
        .expect("SW1 runnable depth")
    {
        assert_eq!(depth, 0, "SW1/E1 blocked peers are not claimable depth");
    }
    let exact_wake = store
        .deliver_and_claim(
            reply(&exact_open, "exact"),
            "single-writer-exact-wake",
            LEASE_MS,
            1_003,
            &Default::default(),
        )
        .await
        .expect("SW2 exact delivery")
        .expect("SW2 exact Awaiting row wakes");
    assert_eq!(exact_wake.request.run_id(), exact_open.run_id(), "SW2/E2");
    settle_done(store, &exact_wake).await;
    if let Some(depth) = store
        .runnable_depth(1_004)
        .await
        .expect("SW2 runnable depth after owner closes")
    {
        assert_eq!(depth, 1, "SW2/E3 the released peer is claimable depth");
    }
    let exact_peer_claim = store
        .claim_run(
            exact_peer.run_id(),
            "single-writer-exact-peer-after",
            LEASE_MS,
            1_004,
            &Default::default(),
        )
        .await
        .expect("SW2 peer claim after original")
        .expect("SW2 peer is released after original settles");
    settle_done(store, &exact_peer_claim).await;

    // SW3: broad local selection must not skip the Awaiting owner and execute its
    // fresh peer. The same selector admits the peer after the owner closes.
    let broad_open = dispatch(ns, "single-writer-broad-open", "single-writer-broad-thread");
    checkpoint_awaiting(store, &broad_open, "single-writer-broad-open", 2_000).await;
    let broad_peer = dispatch(ns, "single-writer-broad-peer", "single-writer-broad-thread");
    store
        .enqueue(broad_peer.clone())
        .await
        .expect("SW3 enqueue peer");
    assert!(
        store
            .claim(
                "single-writer-broad-blocked",
                LEASE_MS,
                2_001,
                &Default::default()
            )
            .await
            .expect("SW3 broad blocked claim")
            .is_none(),
        "SW3/E1 broad selection leaves the peer queued"
    );
    let broad_wake = store
        .deliver_and_claim(
            reply(&broad_open, "broad"),
            "single-writer-broad-wake",
            LEASE_MS,
            2_002,
            &Default::default(),
        )
        .await
        .expect("SW3 wake owner")
        .expect("SW3 owner wakes");
    settle_done(store, &broad_wake).await;
    let broad_peer_claim = store
        .claim(
            "single-writer-broad-after",
            LEASE_MS,
            2_003,
            &Default::default(),
        )
        .await
        .expect("SW3 broad peer claim")
        .expect("SW3 peer released after owner");
    assert_eq!(
        broad_peer_claim.request.run_id(),
        broad_peer.run_id(),
        "SW3/E3"
    );
    settle_done(store, &broad_peer_claim).await;

    // SW4: both registered-worker selection and its exact variant share the same
    // fence; the exact compatible delivery still wakes the owning Awaiting row.
    let worker = ready_dispatch_worker(ns, "single-writer");
    let compatible_open = dispatch(
        ns,
        "single-writer-compatible-open",
        "single-writer-compatible-thread",
    );
    checkpoint_awaiting(
        store,
        &compatible_open,
        "single-writer-compatible-open",
        3_000,
    )
    .await;
    let compatible_peer = dispatch(
        ns,
        "single-writer-compatible-peer",
        "single-writer-compatible-thread",
    );
    assert!(
        store
            .claim_new_run_compatible(compatible_peer.clone(), &worker, LEASE_MS, 3_001)
            .await
            .expect("SW4 atomic compatible admission")
            .is_none(),
        "SW4/E1 compatible atomic claim leaves peer queued"
    );
    assert!(
        store
            .claim_run_compatible(compatible_peer.run_id(), &worker, LEASE_MS, 3_002)
            .await
            .expect("SW4 exact compatible peer claim")
            .is_none(),
        "SW4/E1 exact-compatible selection uses the same fence"
    );
    assert!(
        store
            .claim_compatible(&worker, LEASE_MS, 3_003)
            .await
            .expect("SW4 broad compatible peer claim")
            .is_none(),
        "SW4/E1 compatible selection uses the same fence"
    );
    let compatible_wake = store
        .deliver_and_claim_compatible(
            reply(&compatible_open, "compatible"),
            &worker,
            LEASE_MS,
            3_004,
        )
        .await
        .expect("SW4 compatible exact delivery")
        .expect("SW4 exact Awaiting row wakes");
    assert_eq!(
        compatible_wake.request.run_id(),
        compatible_open.run_id(),
        "SW4/E2"
    );
    settle_done(store, &compatible_wake).await;
    let compatible_peer_claim = store
        .claim_compatible(&worker, LEASE_MS, 3_005)
        .await
        .expect("SW4 compatible peer claim after owner")
        .expect("SW4 compatible peer released");
    assert_eq!(
        compatible_peer_claim.request.run_id(),
        compatible_peer.run_id(),
        "SW4/E3"
    );
    settle_done(store, &compatible_peer_claim).await;

    // SW5: policy placement may rank work but cannot widen the open-Run fence.
    let placed_open = dispatch(
        ns,
        "single-writer-placed-open",
        "single-writer-placed-thread",
    );
    checkpoint_awaiting(store, &placed_open, "single-writer-placed-open", 4_000).await;
    let placed_peer = dispatch(
        ns,
        "single-writer-placed-peer",
        "single-writer-placed-thread",
    );
    store
        .enqueue(placed_peer.clone())
        .await
        .expect("SW5 enqueue peer");
    assert!(
        store
            .claim_placed(
                &worker,
                vec![worker.clone()],
                std::sync::Arc::new(LeastLoadedPolicy),
                LEASE_MS,
                4_001,
            )
            .await
            .expect("SW5 placed blocked claim")
            .is_none(),
        "SW5/E1 placed selection leaves peer queued"
    );
    let placed_wake = store
        .deliver_and_claim_compatible(reply(&placed_open, "placed"), &worker, LEASE_MS, 4_002)
        .await
        .expect("SW5 wake owner")
        .expect("SW5 owner wakes");
    settle_done(store, &placed_wake).await;
    let placed_peer_claim = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            std::sync::Arc::new(LeastLoadedPolicy),
            LEASE_MS,
            4_003,
        )
        .await
        .expect("SW5 placed peer claim")
        .expect("SW5 placed peer released");
    assert_eq!(
        placed_peer_claim.request.run_id(),
        placed_peer.run_id(),
        "SW5/E3"
    );
    settle_done(store, &placed_peer_claim).await;

    // SW6 builds the historical shape through contract operations: cancellation
    // lets the second row claim past the first Awaiting row, and a legacy worker
    // checkpoint leaves it Awaiting. Once both are Awaiting+cancelled, each must
    // still claim and settle serially instead of mutually blocking forever.
    let legacy_first = dispatch(
        ns,
        "single-writer-legacy-first",
        "single-writer-legacy-thread",
    );
    checkpoint_awaiting(store, &legacy_first, "single-writer-legacy-first", 5_000).await;
    let legacy_second = dispatch(
        ns,
        "single-writer-legacy-second",
        "single-writer-legacy-thread",
    );
    store
        .enqueue(legacy_second.clone())
        .await
        .expect("SW6 enqueue historical peer");
    store
        .cancel(legacy_second.run_id())
        .await
        .expect("SW6 cancel historical peer")
        .expect("SW6 historical peer exists");
    let legacy_second_claim = store
        .claim_run(
            legacy_second.run_id(),
            "single-writer-legacy-shape",
            LEASE_MS,
            5_001,
            &Default::default(),
        )
        .await
        .expect("SW6 claim cancelled peer")
        .expect("SW6 cancellation bypasses Awaiting peer");
    assert_eq!(
        store
            .settle(
                legacy_second.run_id(),
                legacy_second_claim.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("SW6 materialize legacy second Awaiting"),
        SettleOutcome::Applied
    );
    store
        .cancel(legacy_first.run_id())
        .await
        .expect("SW6 cancel first Awaiting")
        .expect("SW6 first Awaiting exists");
    let first_cancel = store
        .claim(
            "single-writer-legacy-cancel-first",
            LEASE_MS,
            5_002,
            &Default::default(),
        )
        .await
        .expect("SW6 first cancellation claim")
        .expect("SW6 one legacy Awaiting cancellation is claimable");
    assert_eq!(
        first_cancel.request.run_id(),
        legacy_first.run_id(),
        "SW6/E4"
    );
    settle_done(store, &first_cancel).await;
    let second_cancel = store
        .claim(
            "single-writer-legacy-cancel-second",
            LEASE_MS,
            5_003,
            &Default::default(),
        )
        .await
        .expect("SW6 second cancellation claim")
        .expect("SW6 remaining legacy Awaiting cancellation is claimable");
    assert_eq!(
        second_cancel.request.run_id(),
        legacy_second.run_id(),
        "SW6/E4"
    );
    settle_done(store, &second_cancel).await;
}

/// Session-child admission cause/effect graph and decision table:
///
/// C1 request has explicit parent affinity; C2 child differs from parent; C3
/// canonical Run identity is absent/exact/conflicting; C4 incoming child Thread
/// is already known; C5 distinct unarchived count is below 25; C6 trusted
/// committed disposition evidence archives a finished Thread; C7 a Session root
/// carries canonical self-affinity; C8 trusted capacity-exempt evidence names a
/// non-ordinary Session child (advisor). E1 insert one ordinary dispatch; E2 exact
/// no-op; E3 reject without a row; E4 archive evidence releases one
/// distinct-Thread slot; E5 root self-affinity never consumes a child slot.
/// Cancellation and completion alone retain a child slot through live rows and
/// completion tombstones. The tombstone also owns permanent Run identity: an
/// exact retry after completion is a no-op, while a changed payload conflicts.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | Effect |
/// |---|---|---|---|---|---|---|---|---|---|
/// | SC1 | N | - | absent | - | - | - | N | N | E3 invalid child |
/// | SC2 | Y | N | absent | - | - | - | N | N | E3 invalid child |
/// | SC3 | Y | Y | exact | - | N | - | N | N | E2 replay at cap |
/// | SC4 | Y | Y | conflict | - | N | - | N | N | E3 identity conflict |
/// | SC5 | Y | Y | absent | N | Y | - | N | N | E1 new child Thread |
/// | SC6 | Y | Y | absent | N | N | - | N | N | E3 capacity reached |
/// | SC7 | Y | Y | absent | Y | N | - | N | N | E1 follow-up, no new slot |
/// | SC8 | Y | Y | absent | N | N | N (cancel/Done only) | N | N | E3, slot retained |
/// | SC9 | Y | Y | two absent | N | one slot | - | N | N | exactly one E1 |
/// | SC10 | Y | Y | absent | N | N | Y | N | N | E4 then E1 |
/// | SC11 | - | - | completed | - | - | - | Y | N | E5 |
/// | SC12 | - | - | completed | - | - | - | N | Y | E5 |
/// | SC13 | Y | Y | exact/conflict after Done | - | N | N | N | N | E2/E3 |
async fn session_child_admission_is_atomic_and_bounded(store: &dyn DispatchQueue, ns: &str) {
    let parent = thread_id(ns, "managed-parent");
    let advisor_thread = thread_id(ns, "managed-advisor-thread");
    let admission = || {
        SessionChildAdmission::new(25, Vec::new())
            .with_capacity_exempt_threads(vec![advisor_thread.clone()])
    };

    let root = dispatch(ns, "managed-missing-parent", "managed-invalid-root");
    assert!(
        store
            .enqueue_session_child(root, admission())
            .await
            .expect_err("SC1 missing parent must be rejected")
            .to_string()
            .contains("explicit parent"),
        "SC1"
    );
    let self_thread = thread_id(ns, "managed-self");
    let self_child = dispatch(ns, "managed-self", "managed-self").for_session(self_thread);
    assert!(
        store
            .enqueue_session_child(self_child, admission())
            .await
            .expect_err("SC2 self-parent must be rejected")
            .to_string()
            .contains("must differ"),
        "SC2"
    );

    let session_root = current_session_command(
        dispatch(ns, "managed-session-root", "managed-parent").for_session(parent.clone()),
    );
    assert_eq!(
        store
            .reserve_session_run(session_root.clone(), 49_000)
            .await
            .expect("SC11 reserve canonical Session root"),
        SessionRunReservationOutcome::Reserved,
        "SC11 root uses the canonical reservation boundary"
    );
    assert_eq!(
        store
            .activate_session_run_reservation(session_root.run_id(), &parent, 1)
            .await
            .expect("SC11 activate canonical Session root"),
        SessionRunReservationActivation::Activated,
        "SC11 root is executable only after its activity receipt"
    );
    let root_claim = store
        .claim_run(
            session_root.run_id(),
            "managed-root-owner",
            LEASE_MS,
            50_000,
            &Default::default(),
        )
        .await
        .expect("SC11 claim Session root")
        .expect("SC11 Session root is runnable");
    assert_eq!(
        store
            .settle(
                session_root.run_id(),
                root_claim.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("SC11 settle Session root"),
        SettleOutcome::Applied,
        "SC11"
    );

    let advisor =
        dispatch(ns, "managed-advisor-run", "managed-advisor-thread").for_session(parent.clone());
    store
        .enqueue(advisor.clone())
        .await
        .expect("SC12 advisor uses ordinary durable child ingress");
    let advisor_claim = store
        .claim_run(
            advisor.run_id(),
            "managed-advisor-owner",
            LEASE_MS,
            55_000,
            &Default::default(),
        )
        .await
        .expect("SC12 claim advisor")
        .expect("SC12 advisor is runnable");
    assert_eq!(
        store
            .settle(
                advisor.run_id(),
                advisor_claim.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("SC12 settle advisor"),
        SettleOutcome::Applied,
        "SC12"
    );

    let mut admitted = Vec::new();
    for index in 0..24 {
        let request = dispatch(
            ns,
            &format!("managed-child-run-{index}"),
            &format!("managed-child-thread-{index}"),
        )
        .for_session(parent.clone());
        store
            .enqueue_session_child(request.clone(), admission())
            .await
            .expect("SC5 each child below capacity is admitted");
        admitted.push(request);
    }

    let left = dispatch(ns, "managed-racer-left", "managed-racer-left").for_session(parent.clone());
    let right =
        dispatch(ns, "managed-racer-right", "managed-racer-right").for_session(parent.clone());
    let (left_result, right_result) = tokio::join!(
        store.enqueue_session_child(left.clone(), admission()),
        store.enqueue_session_child(right.clone(), admission()),
    );
    assert_ne!(left_result.is_ok(), right_result.is_ok(), "SC9");
    let winner = if left_result.is_ok() { left } else { right };
    let loser_error = if left_result.is_err() {
        left_result.expect_err("left lost")
    } else {
        right_result.expect_err("right lost")
    };
    assert!(loser_error.to_string().contains("maximum 25"), "SC6/SC9");
    admitted.push(winner.clone());

    let ordinary_on_exempt =
        dispatch(ns, "managed-exempt-bypass", "managed-advisor-thread").for_session(parent.clone());
    assert!(
        store
            .enqueue_session_child(ordinary_on_exempt, admission())
            .await
            .expect_err("SC12 an ordinary child cannot claim an exempt identity")
            .to_string()
            .contains("capacity-exempt"),
        "SC12"
    );
    assert_eq!(
        store
            .list_dispatches()
            .await
            .expect("SC9 operational projection")
            .into_iter()
            .find(|summary| summary.run_id == *winner.run_id())
            .expect("SC9 admitted winner is projected")
            .session_thread_id,
        Some(parent.clone()),
        "the existing dispatch projection preserves parent affinity for cleanup"
    );

    store
        .enqueue_session_child(winner.clone(), admission())
        .await
        .expect("SC3 exact replay succeeds at capacity");
    let mut collision = winner.clone();
    collision.activation.thread_id = thread_id(ns, "managed-collision-thread");
    let collision = store
        .enqueue_session_child(collision, admission())
        .await
        .expect_err("SC4 same Run with changed payload conflicts");
    assert!(
        matches!(collision, DispatchError::Conflict(message) if message.contains("reused")),
        "SC4 typed conflict retains the original activity and dispatch"
    );

    let mut follow_up = dispatch(ns, "managed-follow-up", "managed-follow-up-placeholder");
    follow_up.activation.thread_id = winner.thread_id().clone();
    follow_up = follow_up.for_session(parent.clone());
    store
        .enqueue_session_child(follow_up, admission())
        .await
        .expect("SC7 a Run on an already-known child Thread uses no new slot");

    store
        .cancel(admitted[0].run_id())
        .await
        .expect("SC8 cancel command")
        .expect("SC8 active child is cancellable");
    let after_cancel =
        dispatch(ns, "managed-after-cancel", "managed-after-cancel").for_session(parent.clone());
    assert!(
        store
            .enqueue_session_child(after_cancel, admission())
            .await
            .expect_err("SC8 cancellation alone must not release a Thread slot")
            .to_string()
            .contains("maximum 25"),
        "SC8"
    );

    let cancelled = store
        .claim_run(
            admitted[0].run_id(),
            "managed-cancel-owner",
            LEASE_MS,
            89_000,
            &Default::default(),
        )
        .await
        .expect("SC8 claim cancellation")
        .expect("SC8 cancelled child remains terminally claimable");
    assert_eq!(
        store
            .settle(
                admitted[0].run_id(),
                cancelled.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("SC8 settle cancelled child Done"),
        SettleOutcome::Applied,
        "SC8"
    );

    let completed = store
        .claim_run(
            admitted[1].run_id(),
            "managed-terminal-owner",
            LEASE_MS,
            90_000,
            &Default::default(),
        )
        .await
        .expect("SC8 claim child for completion")
        .expect("SC8 pending child is claimable");
    assert_eq!(
        store
            .settle(
                admitted[1].run_id(),
                completed.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("SC8 settle child Done"),
        SettleOutcome::Applied,
        "SC8"
    );

    // SC13 models the send_message crash gap: the target child completed and
    // its live Dispatch row disappeared before the parent committed the tool
    // receipt. Exact parent recovery must observe the permanent completion
    // identity without recreating claimable work; a changed activation must
    // fail rather than reuse that receipt for another payload.
    store
        .enqueue_session_child(admitted[1].clone(), admission())
        .await
        .expect("SC13 exact replay after Done is a no-op");
    assert!(
        store
            .claim_run(
                admitted[1].run_id(),
                "managed-completed-replay-owner",
                LEASE_MS,
                91_000,
                &Default::default(),
            )
            .await
            .expect("SC13 completed replay claim probe")
            .is_none(),
        "SC13/E2 completion replay must not recreate physical work"
    );
    let mut completed_collision = admitted[1].clone();
    completed_collision.activation.input =
        vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id("managed-completed-collision-message".into()),
            awaken_agent_contract::agent::message::Role::User,
            "changed payload",
        )];
    assert!(
        matches!(
            store
                .enqueue_session_child(completed_collision, admission())
                .await,
            Err(DispatchError::Conflict(_))
        ),
        "SC13/E3 completion tombstone rejects changed payload"
    );
    let after_done =
        dispatch(ns, "managed-after-done", "managed-after-done").for_session(parent.clone());
    assert!(
        store
            .enqueue_session_child(after_done, admission())
            .await
            .expect_err("SC8 completion tombstones retain the Thread slot")
            .to_string()
            .contains("maximum 25"),
        "SC8"
    );

    let archived = vec![
        admitted[0].thread_id().clone(),
        admitted[1].thread_id().clone(),
    ];
    let archived_admission = || {
        SessionChildAdmission::new(25, archived.clone())
            .with_capacity_exempt_threads(vec![advisor_thread.clone()])
    };
    let after_archive_left = dispatch(
        ns,
        "managed-after-archive-left",
        "managed-after-archive-left",
    )
    .for_session(parent.clone());
    store
        .enqueue_session_child(after_archive_left, archived_admission())
        .await
        .expect("SC10 first committed archive releases one Thread slot");
    let after_archive_right = dispatch(
        ns,
        "managed-after-archive-right",
        "managed-after-archive-right",
    )
    .for_session(parent.clone());
    store
        .enqueue_session_child(after_archive_right, archived_admission())
        .await
        .expect("SC10 second committed archive releases one Thread slot");

    let mut archived_follow_up = dispatch(
        ns,
        "managed-archived-follow-up",
        "managed-archived-placeholder",
    );
    archived_follow_up.activation.thread_id = admitted[0].thread_id().clone();
    archived_follow_up = archived_follow_up.for_session(parent);
    assert!(
        store
            .enqueue_session_child(archived_follow_up, archived_admission())
            .await
            .expect_err("SC10 archived Thread cannot be revived")
            .to_string()
            .contains("is archived"),
        "SC10"
    );
}
