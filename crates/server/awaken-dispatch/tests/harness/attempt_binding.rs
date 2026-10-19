// @formal evidence session_execution.one_physical_attempt_per_thread
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch::{
    AttemptAdmission as A, AttemptClosureKind, AttemptClosureRecord, AttemptExecutionBinding,
    PhysicalAttempt, SettleOutcome as S,
};
use awaken_dispatch::{
    PlacementRequirements, RunClaim, RunDispatch, WorkerIdentity, WorkerSnapshot,
};

pub async fn bound_claim(
    store: &impl ConformanceDispatch,
    name: &str,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) -> (RunClaim, AttemptExecutionBinding, WorkerSnapshot) {
    let (directory, identity) = awaken_dispatch_testkit::worker_http::ready_worker(name).await;
    let mut worker = directory
        .current(&identity.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    worker.identity = WorkerIdentity::new(format!("{name}:1"), "boot", 1);
    store
        .enqueue(
            RunDispatch::new(super::activation_on(name, name))
                .with_placement(PlacementRequirements::remote_required()),
        )
        .await
        .unwrap();
    clock.set(0);
    let claim = store
        .claim_compatible(&worker, 60_000)
        .await
        .unwrap()
        .unwrap();
    let binding = AttemptExecutionBinding::new(
        worker.identity.clone(),
        "kubernetes-installation-a",
        "pod-uid/container-id/start-time",
    )
    .unwrap();
    (RunClaim::from(&claim.lease), binding, worker)
}

/// Shared decision table, executed against all three stores and Any delegation.
pub async fn assert_binding_lifecycle(
    store: &(impl ConformanceDispatch + awaken_dispatch::DispatchOperationalFeed),
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // C1 exact registered Worker tuple (not merely its owner string); C2 empty,
    // same-claim or predecessor slot; C3 absent/equal/changed provider coordinate;
    // C4 current/new/retired epoch; C5 ordinary begin or durable handoff.
    // B1 empty + C1 + binding => freeze with admission; B2 !C1 => zero mutation.
    // B3 same claim + same binding + either C5 => idempotent.
    // B4 same claim + changed/removed/added binding => reject, zero mutation.
    // B5 newer claim cannot overwrite an occupied predecessor on begin.
    // B6 exact old ACK clears binding and slot together; delayed ACK cannot
    // clear a successor. B7 trusted durable handoff replaces both coordinates.
    // B8 ACK retires identity even when the old binding is supplied again.
    // B9 an unregistered owner string cannot substitute for C1.
    // No rule proves provider provenance, process stop, or remote termination.
    let (a, binding_a, worker_a) = bound_claim(store, "bound-attempt", clock).await;
    assert_eq!(store.load_physical_attempt(&a.run_id).await.unwrap(), None);
    // Different structured identities can share the legacy owner encoding.
    let collision = AttemptExecutionBinding::new(
        WorkerIdentity::new("bound-attempt", "1:boot", 1),
        "kubernetes-installation-a",
        "wrong-process",
    )
    .unwrap();
    assert_eq!(collision.worker().lease_owner(), a.owner);
    clock.set(0);
    assert!(
        store.begin_attempt(&a, Some(&collision)).await.is_err(),
        "B2"
    );
    assert_eq!(
        store.load_physical_attempt(&a.run_id).await.unwrap(),
        None,
        "B2 no slot"
    );
    clock.set(0);
    assert_eq!(
        store.begin_attempt(&a, Some(&binding_a)).await.unwrap(),
        A::Applied,
        "B1"
    );
    let expected_a = Some(PhysicalAttempt {
        closing: false,
        claim: a.clone(),
        binding: Some(binding_a.clone()),
        thread_id: Some(ThreadId("bound-attempt".into())),
        session_thread_id: None,
    });
    assert_eq!(
        store.load_physical_attempt(&a.run_id).await.unwrap(),
        expected_a
    );
    for handoff in [false, true] {
        let retry = if handoff {
            clock.set(0);
            store.handoff_attempt(&a, Some(&binding_a)).await
        } else {
            clock.set(0);
            store.begin_attempt(&a, Some(&binding_a)).await
        };
        assert_eq!(retry.unwrap(), A::AlreadyApplied, "B3");
        let other_provider = AttemptExecutionBinding::new(
            worker_a.identity.clone(),
            "other-installation",
            binding_a.execution_ref(),
        )
        .unwrap();
        let other_instance = AttemptExecutionBinding::new(
            worker_a.identity.clone(),
            binding_a.provider_id(),
            "replacement-uid",
        )
        .unwrap();
        for changed in [
            None,
            Some(&other_provider),
            Some(&other_instance),
            Some(&collision),
        ] {
            let result = if handoff {
                clock.set(0);
                store.handoff_attempt(&a, changed).await
            } else {
                clock.set(0);
                store.begin_attempt(&a, changed).await
            };
            assert!(result.is_err(), "B2/B4");
            assert_eq!(
                store.load_physical_attempt(&a.run_id).await.unwrap(),
                expected_a,
                "B4 immutable"
            );
        }
    }
    // B10 cause/effect rule: successful physical admission must be retained in
    // the same durable operation prefix before executor entry. Exact retries
    // and rejected binding changes above add no admission; the one record
    // preserves the exact provider/Worker/Thread tuple. This is entry evidence,
    // never closure evidence or permission to replay the provider effect.
    let entries = store
        .events_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&a.run_id),
        )
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|event| serde_json::to_value(event.operation).unwrap())
        .filter(|operation| operation["type"] == "attempt_admitted")
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        1,
        "B10 every admitted attempt is retained once"
    );
    assert_eq!(
        entries[0]["attempt"],
        serde_json::to_value(&expected_a).unwrap(),
        "B10 exact entry tuple"
    );
    // I1 the selected original inventory skips earlier delivery operations
    // before LIMIT=1, keeps the exact physical tuple and global cursor; I2 an
    // unrelated scope is empty, with its original cursor retained. The same
    // assertions execute through Memory, SQLite, PostgreSQL and erasure.
    let root = ThreadId("bound-attempt".into());
    let page = store
        .attempt_admissions_after(
            awaken_dispatch::DispatchCursor::default(),
            1,
            &root,
            std::slice::from_ref(&root),
        )
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1, "I1");
    assert_eq!(
        serde_json::to_value(&page.events[0].operation).unwrap()["attempt"],
        serde_json::to_value(&expected_a).unwrap(),
        "I1 original tuple"
    );
    let foreign = ThreadId("unrelated-inventory".into());
    let empty = store
        .attempt_admissions_after(
            page.next_cursor,
            1,
            &foreign,
            std::slice::from_ref(&foreign),
        )
        .await
        .unwrap();
    assert!(empty.events.is_empty(), "I2 no scope borrowing");
    assert_eq!(
        empty.next_cursor, page.next_cursor,
        "I2 no fabricated cursor"
    );
    clock.set(0);
    store.renew_lease(&a, 0).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let mut worker_b = worker_a.clone();
    worker_b.identity = WorkerIdentity::new(&worker_a.identity.worker_id, "boot-b", 2);
    clock.set(1);
    let b = store
        .claim_compatible(&worker_b, 60_000)
        .await
        .unwrap()
        .unwrap();
    let b = RunClaim::from(&b.lease);
    let binding_b = AttemptExecutionBinding::new(
        worker_b.identity.clone(),
        "kubernetes-installation-a",
        "container-b",
    )
    .unwrap();
    clock.set(1);
    assert_eq!(
        store.begin_attempt(&b, Some(&binding_b)).await.unwrap(),
        A::Blocked,
        "B5"
    );
    assert_eq!(
        store.load_physical_attempt(&a.run_id).await.unwrap(),
        expected_a,
        "new assignment preserves old domain"
    );
    assert_eq!(
        store.finish_attempt(&a, None).await.unwrap(),
        S::Applied,
        "B6"
    );
    assert_eq!(store.load_physical_attempt(&a.run_id).await.unwrap(), None);
    assert_eq!(
        store.finish_attempt(&a, None).await.unwrap(),
        S::Applied,
        "ACK response-loss retry"
    );
    clock.set(1);
    assert_eq!(
        store.begin_attempt(&b, Some(&binding_b)).await.unwrap(),
        A::Applied
    );
    assert_eq!(
        store.finish_attempt(&a, None).await.unwrap(),
        S::Fenced,
        "B6 old evidence"
    );
    assert_eq!(
        store
            .load_physical_attempt(&b.run_id)
            .await
            .unwrap()
            .unwrap()
            .binding,
        Some(binding_b)
    );

    clock.set(1);
    store.renew_lease(&b, 0).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let mut worker_c = worker_b;
    worker_c.identity = WorkerIdentity::new(&worker_c.identity.worker_id, "boot-c", 3);
    clock.set(2);
    let c = store
        .claim_compatible(&worker_c, 60_000)
        .await
        .unwrap()
        .unwrap();
    let c = RunClaim::from(&c.lease);
    let binding_c = AttemptExecutionBinding::new(
        worker_c.identity.clone(),
        "kubernetes-installation-b",
        "container-c",
    )
    .unwrap();
    clock.set(2);
    assert_eq!(
        store.handoff_attempt(&c, Some(&binding_c)).await.unwrap(),
        A::Replaced,
        "B7"
    );
    // B11 a durable handoff retains the successor entry as well as each old
    // admitted identity; later closure must account for every original claim.
    let admissions = store
        .events_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&c.run_id),
        )
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| match event.operation {
            awaken_dispatch::DispatchOperation::AttemptAdmitted { attempt } => Some(attempt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        admissions.iter().map(|a| &a.claim).collect::<Vec<_>>(),
        vec![&a, &b, &c],
        "B11 complete admission order"
    );
    assert_eq!(
        admissions[2].binding.as_ref(),
        Some(&binding_c),
        "B11 immutable successor binding"
    );
    assert_eq!(
        store.load_physical_attempt(&c.run_id).await.unwrap(),
        Some(PhysicalAttempt {
            closing: false,
            claim: c.clone(),
            binding: Some(binding_c.clone()),
            thread_id: Some(ThreadId("bound-attempt".into())),
            session_thread_id: None,
        })
    );
    assert_eq!(store.finish_attempt(&b, None).await.unwrap(), S::Fenced);
    assert_eq!(store.finish_attempt(&c, None).await.unwrap(), S::Applied);
    clock.set(2);
    assert_eq!(
        store.begin_attempt(&c, Some(&binding_c)).await.unwrap(),
        A::Fenced,
        "B8"
    );
    assert_eq!(store.relinquish_claim(&c).await.unwrap(), S::Applied);
    clock.set(2);
    let d = store
        .claim_compatible(&worker_c, 60_000)
        .await
        .unwrap()
        .unwrap();
    let d = RunClaim::from(&d.lease);
    clock.set(2);
    assert_eq!(store.begin_attempt(&d, None).await.unwrap(), A::Applied);
    clock.set(2);
    assert!(
        store.begin_attempt(&d, Some(&binding_c)).await.is_err(),
        "B4 no post-effect backfill"
    );
    clock.set(2);
    assert!(
        store.handoff_attempt(&d, Some(&binding_c)).await.is_err(),
        "B4 no handoff backfill"
    );
    assert_eq!(
        store
            .load_physical_attempt(&d.run_id)
            .await
            .unwrap()
            .unwrap()
            .binding,
        None
    );
    assert_eq!(store.finish_attempt(&d, None).await.unwrap(), S::Applied);

    store
        .enqueue(RunDispatch::new(super::activation_on(
            "unregistered-binding",
            "unregistered-thread",
        )))
        .await
        .unwrap();
    clock.set(2);
    let local = store
        .claim(
            &worker_c.identity.lease_owner(),
            60_000,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let local = RunClaim::from(&local.lease);
    clock.set(2);
    assert!(
        store.begin_attempt(&local, Some(&binding_c)).await.is_err(),
        "B9"
    );
    assert_eq!(
        store.load_physical_attempt(&local.run_id).await.unwrap(),
        None
    );

    // B14 concurrent same-claim requests with different coordinates: one wins
    // the slot transaction, the other conflicts; observation equals the winner.
    // B15 concurrent exact retries: both stutter, neither replaces the binding.
    let (race, left, _) = bound_claim(store, "binding-race", clock).await;
    let right =
        AttemptExecutionBinding::new(left.worker().clone(), left.provider_id(), "other-instance")
            .unwrap();
    clock.set(0);
    let (a, b) = tokio::join!(
        store.begin_attempt(&race, Some(&left)),
        store.begin_attempt(&race, Some(&right)),
    );
    let winner = match (a, b) {
        (Ok(A::Applied), Err(awaken_dispatch::DispatchError::Conflict(_))) => left,
        (Err(awaken_dispatch::DispatchError::Conflict(_)), Ok(A::Applied)) => right,
        other => panic!("B14 unexpected race outcomes: {other:?}"),
    };
    assert_eq!(
        store.load_physical_attempt(&race.run_id).await.unwrap(),
        Some(PhysicalAttempt {
            closing: false,
            claim: race.clone(),
            binding: Some(winner.clone()),
            thread_id: Some(ThreadId("binding-race".into())),
            session_thread_id: None,
        })
    );
    clock.set(0);
    let (a, b) = tokio::join!(
        store.begin_attempt(&race, Some(&winner)),
        store.handoff_attempt(&race, Some(&winner)),
    );
    assert_eq!(a.unwrap(), A::AlreadyApplied, "B15");
    assert_eq!(b.unwrap(), A::AlreadyApplied, "B15");
    assert_eq!(store.finish_attempt(&race, None).await.unwrap(), S::Applied);

    // B16 a Session-affined child freezes both its logical Thread and physical
    // Session partition from the canonical RunDispatch payload. B17 all store
    // adapters reconstruct the same coordinates without a parallel schema
    // column; the ordinary exact ACK still clears the one physical slot.
    store
        .enqueue(
            RunDispatch::new(super::activation_on(
                "partitioned-attempt",
                "logical-thread",
            ))
            .for_session(ThreadId("session-thread".into()))
            .with_placement(PlacementRequirements::remote_required()),
        )
        .await
        .unwrap();
    clock.set(3);
    let partitioned = store
        .claim_compatible(&worker_c, 60_000)
        .await
        .unwrap()
        .unwrap();
    let partitioned = RunClaim::from(&partitioned.lease);
    clock.set(3);
    assert_eq!(
        store.begin_attempt(&partitioned, None).await.unwrap(),
        A::Applied,
        "B16"
    );
    assert_eq!(
        store
            .load_physical_attempt(&partitioned.run_id)
            .await
            .unwrap(),
        Some(PhysicalAttempt {
            closing: false,
            claim: partitioned.clone(),
            binding: None,
            thread_id: Some(ThreadId("logical-thread".into())),
            session_thread_id: Some(ThreadId("session-thread".into())),
        }),
        "B16/B17"
    );
    assert_eq!(
        store.finish_attempt(&partitioned, None).await.unwrap(),
        S::Applied
    );
}

pub async fn assert_closing_lifecycle(
    store: &(impl ConformanceDispatch + awaken_dispatch::DispatchOperationalFeed),
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // C1 exact old entry vs foreign epoch; C2 durable closing phase; C3 late
    // ordinary ACK or successor handoff; C4 complete Coordinator closure.
    // D1 foreign close => Fenced, no phase change. D2 exact close (even after
    // cancellation revokes its mutation lease) => retained identity+binding. D3 closing => ACK and
    // handoff blocked. D4 exact closure => retire once; replay stutters. D5
    // admission at the retired epoch => Fenced. Run across every real backend;
    // this tests storage atomicity/provenance, not provider stop authenticity.
    // C5 native request encoding is SQLite text or PostgreSQL Json; the one
    // physical row projection must preserve original owner/epoch/binding and
    // exact request Thread/partition/closing in D2/D4 below. These real-store
    // rules cover it without a second tuple-shape test.
    assert!(
        store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                None
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "D0 previous ordinary finish/handoff never produced closure evidence"
    );
    let (old, binding, mut worker) = bound_claim(store, "closing-attempt", clock).await;
    store.begin_attempt(&old, Some(&binding)).await.unwrap();
    let foreign = RunClaim {
        epoch: old.epoch + 100,
        ..old.clone()
    };
    assert_eq!(
        store.close_attempt(&foreign).await.unwrap(),
        S::Fenced,
        "D1"
    );
    assert!(
        !store
            .load_physical_attempt(&old.run_id)
            .await
            .unwrap()
            .unwrap()
            .closing
    );
    store.cancel(&old.run_id).await.unwrap();
    assert_eq!(store.close_attempt(&old).await.unwrap(), S::Applied, "D2");
    assert_eq!(
        store.close_attempt(&old).await.unwrap(),
        S::Applied,
        "D2 retry"
    );
    let closed = store
        .load_physical_attempt(&old.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(closed.closing);
    assert_eq!(closed.claim, old);
    assert_eq!(closed.binding.as_ref(), Some(&binding));
    assert_eq!(
        store.finish_attempt(&old, None).await.unwrap(),
        S::Fenced,
        "D3 late ACK"
    );
    worker.identity = WorkerIdentity::new("closing-attempt:2", "boot", 2);
    let next = store
        .claim_run_compatible(&old.run_id, &worker, 60_000)
        .await
        .unwrap()
        .unwrap();
    let next = RunClaim::from(&next.lease);
    let next_binding = AttemptExecutionBinding::new(
        worker.identity.clone(),
        "kubernetes-installation-a",
        "new-process",
    )
    .unwrap();
    assert_eq!(
        store
            .handoff_attempt(&next, Some(&next_binding))
            .await
            .unwrap(),
        A::Blocked,
        "D3 handoff"
    );
    assert_eq!(
        store.load_physical_attempt(&old.run_id).await.unwrap(),
        Some(closed.clone())
    );
    let mut foreign_subject = closed.clone();
    foreign_subject.claim = foreign;
    let foreign_record = AttemptClosureRecord::try_new(
        foreign_subject,
        AttemptClosureKind::ControlledExecutionClosed,
        "store-fenced-foreign-closure",
    )
    .unwrap();
    let record = AttemptClosureRecord::try_new(
        closed,
        AttemptClosureKind::ControlledExecutionClosed,
        "store-original-controlled-closure",
    )
    .unwrap();
    assert_eq!(
        store.finish_attempt_closure(&foreign_record).await.unwrap(),
        S::Fenced,
        "D4 wrong evidence subject"
    );
    // D4b the claim alone is insufficient: changed physical reference or
    // Thread partition must reject before either retirement or record append.
    let mut wrong_binding = record.attempt().clone();
    wrong_binding.binding = Some(
        AttemptExecutionBinding::new(
            binding.worker().clone(),
            binding.provider_id(),
            "different-physical-reference",
        )
        .unwrap(),
    );
    let wrong_binding =
        AttemptClosureRecord::try_new(wrong_binding, record.kind(), record.evidence_ref()).unwrap();
    assert!(
        store.finish_attempt_closure(&wrong_binding).await.is_err(),
        "D4b binding"
    );
    let mut wrong_thread = record.attempt().clone();
    wrong_thread.thread_id = Some(ThreadId("foreign-closure-thread".into()));
    let wrong_thread =
        AttemptClosureRecord::try_new(wrong_thread, record.kind(), record.evidence_ref()).unwrap();
    assert!(
        store.finish_attempt_closure(&wrong_thread).await.is_err(),
        "D4b Thread"
    );
    assert_eq!(
        store
            .load_physical_attempt(&old.run_id)
            .await
            .unwrap()
            .as_ref(),
        Some(record.attempt()),
        "D4b zero physical mutation"
    );
    assert!(
        store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&old.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "D4b zero append"
    );
    assert_eq!(
        store.finish_attempt_closure(&record).await.unwrap(),
        S::Applied,
        "D4"
    );
    assert_eq!(
        store.finish_attempt_closure(&record).await.unwrap(),
        S::Applied,
        "D4 replay"
    );
    assert_eq!(
        store.load_physical_attempt(&old.run_id).await.unwrap(),
        None
    );
    assert_eq!(
        store.begin_attempt(&old, Some(&binding)).await.unwrap(),
        A::Fenced,
        "D5"
    );
    // D6 captured metadata is the same first record, not a last-proof cache.
    let retained = store
        .attempt_closure_records_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&old.run_id),
        )
        .await
        .unwrap();
    assert_eq!(
        retained.events.len(),
        1,
        "D6 duplicate response has one fact"
    );
    assert_eq!(
        retained.events[0].record,
        awaken_dispatch::AttemptPhysicalFact::IndependentClosure {
            record: record.clone()
        }
    );
    let changed = AttemptClosureRecord::try_new(
        record.attempt().clone(),
        AttemptClosureKind::AllExecutionStopped,
        "changed-late-ref",
    )
    .unwrap();
    assert!(
        store.finish_attempt_closure(&changed).await.is_err(),
        "D7 no kind/reference overwrite on replay"
    );
    assert_eq!(
        store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&old.run_id)
            )
            .await
            .unwrap(),
        retained
    );
    assert_internal_history_preserves_public_projection(store, clock).await;
    assert_ordinary_acknowledgement_history(store, clock).await;
}

async fn assert_ordinary_acknowledgement_history(
    store: &(impl ConformanceDispatch + awaken_dispatch::DispatchOperationalFeed),
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    use awaken_dispatch::{
        AttemptPhysicalFact as F, AttemptReturnCause as C, DispatchCursor, DispatchOutcome,
    };
    // Cause graph: O1 actual local cause (Future not entered or structured
    // return); O2 exact/changed cause; O3 same slot/closing/newer slot; O4 first
    // response lost/row removed. O5 legacy None has no retained observation.
    // R1 O1+exact+open -> one original binding+Thread/cause fact and retirement;
    // R2 response loss/cold reader/row deletion -> same event once, no refill;
    // R3 changed/closing/successor -> reject without mutation; R4 O5 stays
    // Unknown. Local return includes response-only result uncertainty, and
    // pre-driver Create/Adopt/MCP IO is outside OperationFutureNotEntered;
    // therefore NO row produces an IndependentClosure/All/Controlled proof.
    for (name, cause) in [
        ("ack-not-entered", C::OperationFutureNotEntered),
        ("ack-returned", C::StructuredReturnObserved),
    ] {
        let (claim, binding, _) = bound_claim(store, name, clock).await;
        assert_eq!(
            store.begin_attempt(&claim, Some(&binding)).await.unwrap(),
            A::Applied
        );
        let original = store
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store.finish_attempt(&claim, Some(cause)).await.unwrap(),
            S::Applied,
            "R1"
        );
        assert!(
            store
                .load_physical_attempt(&claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
        let retained = store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id),
            )
            .await
            .unwrap();
        assert_eq!(retained.events.len(), 1);
        let F::Acknowledgement { record } = &retained.events[0].record else {
            panic!("R1 ordinary cause cannot become an independent stop kind")
        };
        assert_eq!(
            record.attempt(),
            &original,
            "R1 original binding/Thread/partition"
        );
        assert_eq!(record.cause(), cause);
        assert!(!record.attempt().closing, "R1 do not fabricate closing");
        assert_eq!(
            store.finish_attempt(&claim, Some(cause)).await.unwrap(),
            S::Applied,
            "R2 response loss"
        );
        let different = if cause == C::OperationFutureNotEntered {
            C::StructuredReturnObserved
        } else {
            C::OperationFutureNotEntered
        };
        assert!(
            store.finish_attempt(&claim, Some(different)).await.is_err(),
            "R3 cause rewrite"
        );
        assert_eq!(
            store.finish_attempt(&claim, None).await.unwrap(),
            S::Applied,
            "R4 legacy replay preserves existing fact"
        );
        assert_eq!(
            store
                .settle(&claim.run_id, claim.epoch, DispatchOutcome::Done, &[])
                .await
                .unwrap(),
            S::Applied
        );
        assert_eq!(
            store.finish_attempt(&claim, Some(cause)).await.unwrap(),
            S::Applied,
            "R2 row deletion replay"
        );
        assert_eq!(
            store
                .attempt_closure_records_after(
                    DispatchCursor::default(),
                    usize::MAX,
                    Some(&claim.run_id)
                )
                .await
                .unwrap(),
            retained,
            "R2 same original event cursor/time"
        );
    }
    let (claim, binding, _) = bound_claim(store, "ack-closed", clock).await;
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    store.close_attempt(&claim).await.unwrap();
    let closed = store.load_physical_attempt(&claim.run_id).await.unwrap();
    assert_eq!(
        store
            .finish_attempt(&claim, Some(C::StructuredReturnObserved))
            .await
            .unwrap(),
        S::Fenced,
        "R3 independent closing rejects ordinary Some"
    );
    assert_eq!(
        store.load_physical_attempt(&claim.run_id).await.unwrap(),
        closed
    );
    assert!(
        store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty()
    );

    let (old, old_binding, mut worker) = bound_claim(store, "ack-successor", clock).await;
    store.begin_attempt(&old, Some(&old_binding)).await.unwrap();
    store
        .finish_attempt(&old, Some(C::StructuredReturnObserved))
        .await
        .unwrap();
    // Use the original cancellation claim succession, not wall-clock expiry
    // manipulation; PostgreSQL reads its own authority clock.
    store.cancel(&old.run_id).await.unwrap();
    worker.identity = WorkerIdentity::new(&worker.identity.worker_id, "new-boot", 2);
    let next = store
        .claim_run_compatible(&old.run_id, &worker, 60_000)
        .await
        .unwrap()
        .unwrap();
    let next = RunClaim::from(&next.lease);
    let next_binding = AttemptExecutionBinding::new(
        worker.identity.clone(),
        "kubernetes-installation-a",
        "successor-process",
    )
    .unwrap();
    assert_eq!(
        store
            .begin_attempt(&next, Some(&next_binding))
            .await
            .unwrap(),
        A::Applied
    );
    let successor = store.load_physical_attempt(&old.run_id).await.unwrap();
    assert_eq!(
        store
            .finish_attempt(&old, Some(C::StructuredReturnObserved))
            .await
            .unwrap(),
        S::Fenced,
        "R3 old replay cannot clear successor"
    );
    assert_eq!(
        store.load_physical_attempt(&old.run_id).await.unwrap(),
        successor
    );
    let (legacy, legacy_binding, _) = bound_claim(store, "ack-missing-old-fact", clock).await;
    store
        .begin_attempt(&legacy, Some(&legacy_binding))
        .await
        .unwrap();
    store.finish_attempt(&legacy, None).await.unwrap();
    assert_eq!(
        store
            .finish_attempt(&legacy, Some(C::OperationFutureNotEntered))
            .await
            .unwrap(),
        S::Fenced,
        "R4 no post-hoc backfill"
    );
    assert!(
        store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&legacy.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty()
    );
}

use awaken_dispatch_testkit::ConformanceDispatch;

async fn assert_internal_history_preserves_public_projection(
    store: &(impl ConformanceDispatch + awaken_dispatch::DispatchOperationalFeed),
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    use awaken_dispatch::{DispatchCursor, DispatchOutcome};
    // Cause/effect decisions: H1 exact closing subject + either trusted kind ->
    // one same-transaction retained fact; H2 response lost/exact replay -> no
    // append; H3 changed/foreign/missing record -> no fabricated first fact;
    // H4 three internal facts exceed page limit1 before a visible suffix ->
    // old public shape/cursor returns that suffix (no early empty page); H5
    // normal Done/GC deletes the root row -> internal facts remain; H6 zero or
    // internal-only public page -> empty and original requested cursor.
    // Records cover known attempts only. Neither last nor all-known records
    // certifies complete old-Worker membership, requests, access or cleanup.
    let mut subjects = Vec::new();
    for name in ["history-full", "history-controlled", "history-gc"] {
        let (claim, binding, worker) = bound_claim(store, name, clock).await;
        assert_eq!(
            store.begin_attempt(&claim, Some(&binding)).await.unwrap(),
            A::Applied
        );
        assert_eq!(store.close_attempt(&claim).await.unwrap(), S::Applied);
        subjects.push((claim, worker));
    }
    let public_before = store
        .events_after(DispatchCursor::default(), usize::MAX, None)
        .await
        .unwrap();
    let cursor = public_before.next_cursor;
    let mut records = Vec::new();
    for (index, (claim, _)) in subjects.iter().enumerate() {
        let kind = if index == 1 {
            AttemptClosureKind::ControlledExecutionClosed
        } else {
            AttemptClosureKind::AllExecutionStopped
        };
        let record = AttemptClosureRecord::try_new(
            store
                .load_physical_attempt(&claim.run_id)
                .await
                .unwrap()
                .unwrap(),
            kind,
            format!("history-storage-reference-{index}"),
        )
        .unwrap();
        assert_eq!(
            store.finish_attempt_closure(&record).await.unwrap(),
            S::Applied,
            "H1"
        );
        assert_eq!(
            store.finish_attempt_closure(&record).await.unwrap(),
            S::Applied,
            "H2"
        );
        records.push(record);
    }
    let hidden = store.events_after(cursor, 1, None).await.unwrap();
    assert!(hidden.events.is_empty(), "H6 internal-only diagnostics");
    assert_eq!(
        hidden.next_cursor, cursor,
        "H6 preserve public empty cursor"
    );
    let zero = store
        .attempt_closure_records_after(cursor, 0, None)
        .await
        .unwrap();
    assert!(zero.events.is_empty());
    assert_eq!(zero.next_cursor, cursor, "H6 internal zero cursor");
    let first = store
        .attempt_closure_records_after(cursor, 1, None)
        .await
        .unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(
        first.events[0].record,
        awaken_dispatch::AttemptPhysicalFact::IndependentClosure {
            record: records[0].clone()
        }
    );

    let suffix = "history-public-suffix";
    store
        .enqueue(RunDispatch::new(super::activation_on(suffix, suffix)))
        .await
        .unwrap();
    clock.set(0);
    let suffix_claim = store
        .claim_run(
            &awaken_agent_contract::agent::run::Id(suffix.into()),
            "history-suffix-owner",
            60_000,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let visible = store.events_after(cursor, 1, None).await.unwrap();
    assert_eq!(
        visible.events.len(),
        1,
        "H4 prefix > limit does not hide suffix"
    );
    assert_eq!(
        visible.events[0].operation.run_id(),
        suffix_claim.lease.run_id()
    );
    let encoded = serde_json::to_value(&visible).unwrap();
    assert_eq!(
        encoded["events"][0]["operation"]["type"], "claimed",
        "H4 original public variant"
    );
    assert!(
        encoded.to_string().find("attempt_closure").is_none(),
        "H4 private fact never in public Page"
    );

    let full = &records[0];
    assert_eq!(
        store
            .settle(
                &full.attempt().claim.run_id,
                full.attempt().claim.epoch,
                DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap(),
        S::Applied,
        "H5 original logical Done"
    );
    assert_eq!(
        store.finish_attempt_closure(full).await.unwrap(),
        S::Applied,
        "H2/H5 after row Done"
    );
    let (claim, worker) = &subjects[2];
    store.cancel(&claim.run_id).await.unwrap();
    clock.set(0);
    let next = store
        .claim_run_compatible(&claim.run_id, worker, 1)
        .await
        .unwrap()
        .unwrap();
    clock.advance_past(next.lease.expires_ms()).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(0, next.lease.expires_ms().saturating_add(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.purge_dead_letters().await.unwrap(),
        1,
        "H5 original GC"
    );
    assert_eq!(
        store.finish_attempt_closure(&records[2]).await.unwrap(),
        S::Applied,
        "H2/H5 after GC"
    );
    let retained = store
        .attempt_closure_records_after(cursor, usize::MAX, None)
        .await
        .unwrap();
    assert_eq!(
        retained
            .events
            .iter()
            .map(|event| match &event.record {
                awaken_dispatch::AttemptPhysicalFact::IndependentClosure { record } => record,
                _ => panic!("H5 only independent observations were submitted"),
            })
            .collect::<Vec<_>>(),
        records.iter().collect::<Vec<_>>(),
        "H5 no history mirror or row-lifetime loss"
    );
    // An empty ordinary-retired slot cannot acquire evidence after the fact.
    let source = RunClaim::from(&suffix_claim.lease);
    store.begin_attempt(&source, None).await.unwrap();
    let mut old = store
        .load_physical_attempt(&source.run_id)
        .await
        .unwrap()
        .unwrap();
    store.finish_attempt(&source, None).await.unwrap();
    old.closing = true;
    let invented = AttemptClosureRecord::try_new(
        old,
        AttemptClosureKind::AllExecutionStopped,
        "must-not-backfill-empty",
    )
    .unwrap();
    assert_eq!(
        store.finish_attempt_closure(&invented).await.unwrap(),
        S::Fenced,
        "H3 ordinary/legacy empty is unknown"
    );
    assert!(
        store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&source.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "H3 no generated evidence"
    );
}
