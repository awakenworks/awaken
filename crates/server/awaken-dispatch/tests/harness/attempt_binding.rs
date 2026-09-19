use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch::{
    AttemptAdmission as A, AttemptExecutionBinding, PhysicalAttempt, SettleOutcome as S,
};
use awaken_dispatch::{
    DispatchQueue, PlacementRequirements, RunClaim, RunDispatch, WorkerIdentity, WorkerSnapshot,
};

pub async fn bound_claim(
    store: &impl DispatchQueue,
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
    store: &impl DispatchQueue,
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
    assert_eq!(store.finish_attempt(&a).await.unwrap(), S::Applied, "B6");
    assert_eq!(store.load_physical_attempt(&a.run_id).await.unwrap(), None);
    assert_eq!(
        store.finish_attempt(&a).await.unwrap(),
        S::Applied,
        "ACK response-loss retry"
    );
    clock.set(1);
    assert_eq!(
        store.begin_attempt(&b, Some(&binding_b)).await.unwrap(),
        A::Applied
    );
    assert_eq!(
        store.finish_attempt(&a).await.unwrap(),
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
    assert_eq!(
        store.load_physical_attempt(&c.run_id).await.unwrap(),
        Some(PhysicalAttempt {
            claim: c.clone(),
            binding: Some(binding_c.clone()),
            thread_id: Some(ThreadId("bound-attempt".into())),
            session_thread_id: None,
        })
    );
    assert_eq!(store.finish_attempt(&b).await.unwrap(), S::Fenced);
    assert_eq!(store.finish_attempt(&c).await.unwrap(), S::Applied);
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
    assert_eq!(store.finish_attempt(&d).await.unwrap(), S::Applied);

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
    assert_eq!(store.finish_attempt(&race).await.unwrap(), S::Applied);

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
            claim: partitioned.clone(),
            binding: None,
            thread_id: Some(ThreadId("logical-thread".into())),
            session_thread_id: Some(ThreadId("session-thread".into())),
        }),
        "B16/B17"
    );
    assert_eq!(
        store.finish_attempt(&partitioned).await.unwrap(),
        S::Applied
    );
}
