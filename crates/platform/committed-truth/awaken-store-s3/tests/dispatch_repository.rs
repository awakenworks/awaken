mod support;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch::{ObjectDispatchPartition, ObjectDispatchStore};
use awaken_dispatch_contract::{
    ClaimAuthority, DispatchClaimOwnership as _, DispatchClaims as _, DispatchCommitAuthority as _,
    DispatchEnqueue as _, DispatchExecution as _, DispatchOutcome, DispatchReads as _,
    DispatchSettlement as _,
};
use awaken_session_store::SessionObjectStore as _;
use awaken_store_s3::SessionKey;
use std::sync::Arc;

#[tokio::test]
async fn physical_occupancy_checks_all_sessions_without_releasing_registry_exclusion() {
    use awaken_dispatch_contract::{DispatchAttemptControl as _, DispatchControlReads as _};
    use awaken_worker_contract::{RunClaim, WorkerCurrentSource as _, WorkerIncarnationGuard};

    let (_, clock, engine, _) = support::setup().await;
    let objects = Arc::new(engine.component_store("dispatch").unwrap());
    let queue = ObjectDispatchStore::new(objects.clone(), "tenant".into(), clock.clone());
    let empty = ObjectDispatchPartition::new(
        objects.clone(),
        "tenant".into(),
        "a-empty".into(),
        clock.clone(),
    );
    let occupied =
        ObjectDispatchPartition::new(objects, "tenant".into(), "z-occupied".into(), clock);
    for (partition, run, session) in [
        (&empty, "empty-run", "a-empty"),
        (&occupied, "occupied-run", "z-occupied"),
    ] {
        partition
            .enqueue(support::dispatch(run, session, session))
            .await
            .unwrap();
    }
    let directory = awaken_dispatch_testkit::worker_http::ready_workers(&["worker", "other"]);
    let snapshot = directory.current("worker").await.unwrap().unwrap().snapshot;
    let exclusion = Arc::new(tokio::sync::Mutex::new(()));
    let mut guard =
        WorkerIncarnationGuard::new(exclusion.clone().lock_owned().await, snapshot.clone());
    assert!(
        !queue
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    assert!(
        exclusion.try_lock().is_err(),
        "occupancy reads retain the original exclusion"
    );
    guard.release().await.unwrap();

    let claimed = occupied
        .claim(&snapshot.identity.lease_owner(), 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    queue
        .admit_attempt_guarded(
            &claim,
            None,
            WorkerIncarnationGuard::new(exclusion.clone().lock_owned().await, snapshot.clone()),
            false,
        )
        .await
        .unwrap();
    let mut guard = WorkerIncarnationGuard::new(exclusion.clone().lock_owned().await, snapshot);
    assert!(
        !empty
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    assert!(
        occupied
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    assert!(
        queue
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    assert!(
        exclusion.try_lock().is_err(),
        "partition scans cannot release Registry authority"
    );
    guard.release().await.unwrap();
    assert!(exclusion.try_lock().is_ok());

    let other = directory.current("other").await.unwrap().unwrap().snapshot;
    let mut guard = WorkerIncarnationGuard::new(exclusion.clone().lock_owned().await, other);
    assert!(
        !queue
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    guard.release().await.unwrap();
    assert!(queue.finish_attempt(&claim, None).await.unwrap().applied());
    let worker = directory.current("worker").await.unwrap().unwrap().snapshot;
    let mut guard = WorkerIncarnationGuard::new(exclusion.lock_owned().await, worker);
    assert!(
        !queue
            .physical_slot_occupied_guarded(&mut guard)
            .await
            .unwrap()
    );
    guard.release().await.unwrap();
}

#[tokio::test]
async fn object_dispatch_shares_the_postgres_worker_visible_suite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(1000);
    let engine = awaken_store_s3::S3Storage::new(
        support::object_transport(),
        "dispatch-suite",
        clock.clock(),
        Default::default(),
    )
    .unwrap();
    let store = ObjectDispatchStore::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        clock.clock(),
    );
    awaken_dispatch_testkit::assert_dispatch_conformance_with_clock(
        &store,
        "s3",
        awaken_dispatch_testkit::ConformanceCapabilities {
            local_commit_guard: false,
            sandbox_binding: true,
            completion_events: false,
        },
        &clock,
    )
    .await;
}
#[tokio::test]
async fn canonical_dispatch_and_thread_truth_share_one_fenced_publication() {
    let (_, clock, engine, session) = support::setup().await;
    let objects = Arc::new(engine.component_store("dispatch").unwrap());
    let queue = ObjectDispatchPartition::new(
        objects.clone(),
        "tenant".into(),
        "session".into(),
        clock.clone(),
    );
    queue
        .enqueue(support::dispatch("run", "thread", "session"))
        .await
        .unwrap();
    let capabilities = Default::default();
    let claimed = queue
        .claim("worker", 1000, &capabilities)
        .await
        .unwrap()
        .unwrap();
    let claim = awaken_worker_contract::RunClaim::from(&claimed.lease);
    let ClaimAuthority::Conditional(authority) = queue
        .acquire_commit_authority(&claim)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("object engine must issue a condition")
    };
    assert!(
        queue.lock_commit_epoch(&claim).await.is_err(),
        "no fabricated held lock"
    );
    // Renewal must not invalidate the exact owner's condition.
    queue.renew_lease(&claim, 2000).await.unwrap();
    let operation = support::operation("run", "thread", 0, 0, "persisted");
    let first = session
        .commit_conditionally(&claim, operation.clone(), &authority.condition)
        .await
        .unwrap();
    let duplicate = session
        .commit_conditionally(&claim, operation.clone(), &authority.condition)
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.commit_sequence, first.commit_sequence);
    assert_eq!(duplicate.thread_version, first.thread_version);
    assert!(queue.claim_is_current(&claim).await.unwrap());
    let located = objects.locate_record("run:run").await.unwrap().unwrap();
    assert_eq!(located.owner_scope, "tenant");
    let restored =
        ObjectDispatchPartition::new(objects, "tenant".into(), "session".into(), clock.clone());
    assert_eq!(restored.list_dispatches().await.unwrap().len(), 1);
    clock.advance(3000);
    let replacement = restored
        .claim("replacement", 1000, &capabilities)
        .await
        .unwrap()
        .unwrap();
    assert!(replacement.lease.epoch() > claimed.lease.epoch());
    assert!(
        session
            .commit_conditionally(&claim, operation, &authority.condition)
            .await
            .is_err(),
        "even a known receipt is fenced after takeover"
    );
    // An exact grant from another scope cannot reach this Session component.
    let foreign = ObjectDispatchPartition::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "other".into(),
        "session".into(),
        clock,
    );
    assert!(
        foreign
            .claim_run(&RunId("run".into()), "other-worker", 1000, &capabilities)
            .await
            .is_err()
    );
    assert_eq!(
        session.key(),
        &SessionKey::new("tenant", "session").unwrap()
    );
}

#[tokio::test]
async fn object_dispatch_shares_postgres_portable_partition_scan_laws() {
    let (_, clock, engine, _) = support::setup().await;
    let store = ObjectDispatchStore::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        clock,
    );
    awaken_dispatch_testkit::assert_portable_dispatch_scan_conformance(&store, "s3-portable").await;
}

#[tokio::test]
async fn an_input_only_component_inherits_the_existing_session_owner() {
    use awaken_dispatch_contract::{Inbox as _, InboxRead as _, PendingInput};
    use awaken_object_storage::{SessionObjectStore as _, SessionObjectUpdate};
    let (_, clock, engine, _) = support::setup().await;
    engine
        .component_store("owner_seed")
        .unwrap()
        .insert(
            "tenant",
            "session",
            &SessionObjectUpdate::snapshot("{}".into()),
        )
        .await
        .unwrap();
    let store = ObjectDispatchStore::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "wrong-default".into(),
        clock.clone(),
    );
    let input = PendingInput {
        message_id: "idle-input".into(),
        run_id: RunId(String::new()),
        thread_id: awaken_agent_contract::ThreadId("session".into()),
        correlation_id: String::new(),
        available_at_ms: None,
        result: awaken_runtime_contract::resume::ResumeResult::Input("hello".into()),
        context_messages: Vec::new(),
    };
    assert!(store.append(input).await.unwrap());
    assert_eq!(
        store
            .list(&awaken_agent_contract::ThreadId("session".into()))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        engine
            .discover_sessions("wrong-default")
            .await
            .unwrap()
            .is_empty()
    );
    let persisted = engine
        .component_store("dispatch")
        .unwrap()
        .load("session")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.owner_scope, "tenant");
    assert!(persisted.payload.is_some());
    store
        .enqueue(support::dispatch("legacy-scopeless", "session", "session"))
        .await
        .unwrap();
    assert!(
        engine
            .discover_sessions("wrong-default")
            .await
            .unwrap()
            .is_empty()
    );
    let wrong = ObjectDispatchPartition::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "foreign".into(),
        "session".into(),
        clock,
    );
    assert!(
        wrong
            .enqueue(support::dispatch("foreign-run", "session", "session"))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn absent_components_cannot_rebind_another_components_session_owner() {
    use awaken_object_storage::{SessionObjectStore as _, SessionObjectUpdate};
    let (_, clock, engine, _) = support::setup().await;
    engine
        .component_store("owner_seed")
        .unwrap()
        .insert(
            "tenant",
            "session",
            &SessionObjectUpdate::snapshot("{}".into()),
        )
        .await
        .unwrap();
    let objects = Arc::new(engine.component_store("dispatch").unwrap());
    let header = objects.load("session").await.unwrap().unwrap();
    assert!(
        header.payload.is_none(),
        "a missing component preserves the partition identity"
    );
    assert_eq!(header.owner_scope, "tenant");
    let wrong = ObjectDispatchPartition::new(objects, "foreign".into(), "session".into(), clock);
    assert!(
        wrong
            .enqueue(support::dispatch("foreign-run", "session", "session"))
            .await
            .is_err()
    );
    assert!(
        engine
            .discover_sessions("foreign")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn discovery_reads_due_candidates_independently_of_cold_sessions() {
    let (_, clock, engine, _) = support::setup().await;
    let objects = Arc::new(engine.component_store("dispatch").unwrap());
    let queue = ObjectDispatchStore::new(objects.clone(), "tenant".into(), clock.clone());
    let capabilities = Default::default();
    assert!(
        queue
            .claim("worker", 1_000, &capabilities)
            .await
            .unwrap()
            .is_none()
    );

    for id in 0..32 {
        let session = format!("cold-session-{id}");
        let run = format!("closed-run-{id}");
        let cold = ObjectDispatchPartition::new(
            objects.clone(),
            "tenant".into(),
            session.clone(),
            clock.clone(),
        );
        cold.enqueue(support::dispatch(&run, &session, &session))
            .await
            .unwrap();
        let claimed = cold
            .claim("worker", 1_000, &capabilities)
            .await
            .unwrap()
            .unwrap();
        assert!(
            cold.settle(
                &RunId(run),
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap()
            .applied()
        );
    }
    let before = engine.request_counts();
    assert!(
        queue
            .claim("worker", 1_000, &capabilities)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        engine.request_counts().get,
        before.get,
        "empty discovery reads no historical head"
    );

    queue
        .enqueue(support::dispatch("active-run", "active", "active"))
        .await
        .unwrap();
    let first = queue
        .claim("worker", 1_000, &capabilities)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.request.run_id().0, "active-run");
    assert!(
        queue
            .claim("worker", 1_000, &capabilities)
            .await
            .unwrap()
            .is_none()
    );
    clock.advance(1_000);
    let cold_restart = ObjectDispatchStore::new(objects, "tenant".into(), clock);
    let takeover = cold_restart
        .claim("replacement", 1_000, &capabilities)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(takeover.request.run_id(), first.request.run_id());
    assert!(takeover.lease.epoch() > first.lease.epoch());
}

#[tokio::test]
async fn hot_shard_continues_past_incompatible_sessions_with_bounded_reads() {
    let (_, clock, engine, _) = support::setup().await;
    let objects = Arc::new(engine.component_store("dispatch").unwrap());
    let queue = ObjectDispatchStore::new(objects, "tenant".into(), clock.clone());
    let capabilities = Default::default();
    assert!(
        queue
            .claim("worker", 1_000, &capabilities)
            .await
            .unwrap()
            .is_none()
    );

    let selected = awaken_dispatch_contract::dispatch_shard("tenant", "target");
    let names = (0..100_000)
        .map(|index| format!("hot-{index}"))
        .filter(|session| awaken_dispatch_contract::dispatch_shard("tenant", session) == selected)
        .take(144)
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 144);
    for session in names {
        let mut request = support::dispatch(&session, &session, &session);
        request.placement.location = awaken_worker_contract::ExecutionLocation::RemoteRequired;
        queue.enqueue(request).await.unwrap();
        clock.advance(1);
    }
    queue
        .enqueue(support::dispatch("target-run", "target", "target"))
        .await
        .unwrap();
    // Expired leases sort after all ready (deadline zero) markers.
    queue
        .claim_run(
            &RunId("target-run".into()),
            "previous",
            1_000,
            &capabilities,
        )
        .await
        .unwrap()
        .unwrap();
    clock.advance(1_000);

    let mut admitted = None;
    for round in 0..8 {
        let before = engine.request_counts();
        let result = queue.claim("worker", 1_000, &capabilities).await.unwrap();
        let reads = engine.request_counts().get - before.get;
        assert!(reads <= 132, "round {round} read {reads} head/body objects");
        if result.is_some() {
            admitted = result;
            break;
        }
    }
    let admitted = admitted.expect("discovery resumes through incompatible rows");
    assert_eq!(admitted.request.run_id().0, "target-run");
}

#[tokio::test]
async fn registered_worker_continues_after_an_empty_shard_scan() {
    use awaken_dispatch_contract::{
        ClaimRequest, ClaimRequestOutcome, ClaimSearch, DispatchControlClaims as _,
    };
    use awaken_time::WallClockExt as _;
    use awaken_worker_contract::{ExecutionLocation, LeastLoadedPolicy, WorkerCurrentSource as _};

    let (_, clock, engine, _) = support::setup().await;
    let queue = ObjectDispatchStore::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        clock.clone(),
    );
    let selected = awaken_dispatch_contract::dispatch_shard("tenant", "target");
    let blocked_sessions = (0..100_000)
        .map(|index| format!("blocked-{index}"))
        .filter(|session| awaken_dispatch_contract::dispatch_shard("tenant", session) == selected)
        .take(72)
        .collect::<Vec<_>>();
    assert_eq!(blocked_sessions.len(), 72);
    for session in blocked_sessions {
        let mut request = support::dispatch(&session, &session, &session);
        request.placement.location = ExecutionLocation::LocalOnly;
        queue.enqueue(request).await.unwrap();
        clock.advance(1);
    }
    let mut target = support::dispatch("target-run", "target", "target");
    target.placement = awaken_worker_contract::PlacementRequirements::remote_preferred();
    queue
        .enqueue(target.with_worker_codec_requirements())
        .await
        .unwrap();
    queue
        .claim_run(
            &RunId("target-run".into()),
            "previous",
            1_000,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    clock.advance(1_000);

    let workers = awaken_dispatch_testkit::worker_http::ready_workers(&["worker"]);
    let worker = workers.current("worker").await.unwrap().unwrap().snapshot;
    let now = clock.now_ms();
    let issued = uuid::Timestamp::from_unix(
        uuid::NoContext,
        now / 1_000,
        (now % 1_000) as u32 * 1_000_000,
    );
    let claim = ClaimRequest {
        request_id: uuid::Uuid::new_v7(issued).to_string(),
        run_id: None,
    };
    let search = || {
        queue.search_claim_request(
            &claim,
            &worker,
            vec![worker.clone()],
            Arc::new(LeastLoadedPolicy),
            1_000,
            ClaimSearch::Shard(selected),
        )
    };
    assert!(matches!(
        search().await.unwrap(),
        ClaimRequestOutcome::Empty
    ));
    let mut admitted = None;
    for _ in 0..7 {
        match search().await.unwrap() {
            ClaimRequestOutcome::Claimed(claim) => {
                admitted = Some(claim);
                break;
            }
            ClaimRequestOutcome::Empty => {}
            ClaimRequestOutcome::Unsupported => panic!("object dispatch has no registered path"),
        }
    }
    let admitted = admitted.expect("registered search must resume in the same shard");
    assert_eq!(admitted.request.run_id().0, "target-run");
    let ClaimRequestOutcome::Claimed(replayed) = search().await.unwrap() else {
        panic!("the same request must replay its published claim");
    };
    assert_eq!(replayed.lease, admitted.lease);
}
