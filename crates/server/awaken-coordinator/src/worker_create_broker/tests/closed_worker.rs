//! Original Worker closure reuses the registered provider ledger before release.
use super::*;
use awaken_dispatch::{
    AttemptExecutionBinding, DispatchClaims, DispatchControlReads, DispatchEnqueue,
    DispatchExecution, WorkerDirectory,
};
use awaken_worker_contract::WorkerProcessExit;

#[tokio::test]
async fn closed_worker_create_fence_survives_missing_process_history() {
    // M1 closed exact Worker + original ledger + absent process binding -> seal
    // Create admission, keep exit Unknown. M2 the retained binding exists but
    // original Pod/process observation is unavailable -> the same seal survives
    // that error. Neither row releases retention, stops a Pod, records an exit
    // or settles physical work. Both production StopSource operations use the
    // original provider ledger; the Ingress suite owns their independent join.
    for binding_exists in [false, true] {
        let f = fixture::broker_fixture(|_| {}).await;
        if binding_exists {
            let record = f.directory.incarnation(&f.identity).await.unwrap().unwrap();
            let pod = KubernetesPodIdentity::from_worker_id(&f.identity.worker_id).unwrap();
            let anchor = record.create_ledger.as_ref().unwrap();
            let exact = binding(&f.broker.ledger, anchor, &pod, "provider-a").unwrap();
            let process = serde_json::from_str(&original_process()).unwrap();
            f.broker
                .ledger
                .retain_process_binding(exact, &process)
                .await
                .unwrap();
        }
        fixture::quiesce_worker(f.directory.as_ref(), &f.identity).await;
        let record = f.directory.incarnation(&f.identity).await.unwrap().unwrap();
        let source = source(&f);
        awaken_dispatch::WorkerExecutionStopSource::seal_closed_worker_admission(
            source.as_ref(),
            &f.identity,
            record.create_ledger.as_ref(),
        )
        .await
        .unwrap();
        let result = awaken_dispatch::WorkerExecutionStopSource::observe_closed_idle_worker(
            source.as_ref(),
            &f.identity,
            false,
            record.create_ledger.as_ref(),
        )
        .await;
        if binding_exists {
            assert!(
                result.is_err(),
                "M2 original Pod observation remains unavailable"
            );
        } else {
            assert_eq!(result.unwrap(), None, "M1 missing process remains Unknown");
        }
        let state = fixture::ledger_state(&f.stored);
        assert_eq!(
            state["worker_sealed"], true,
            "M1/M2 independent Create fence"
        );
        assert_eq!(state["groups"][group_for(&f.identity)], "sealed");
        assert!(
            f.directory
                .incarnation(&f.identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_none()
        );
        assert_eq!(
            f.pod_posts.load(Ordering::SeqCst),
            0,
            "M1/M2 no Pod Create or stop"
        );
    }
}

#[tokio::test]
async fn create_seal_uses_closed_registry_and_original_ledger_without_process_exit() {
    // Cause/effect graph: C1 exact Registry incarnation is Ready/expired-Ready/
    // Quiesced/Dead; C2 original ledger UID and configuration match; C3 process
    // exit is unavailable. E1 only closed C1+C2 seals the existing Create owner
    // and joins its empty request group; E2 late Create is denied by that same
    // ledger; E3 no Registry exit, Pod stop, evidence release or cleanup receipt
    // is fabricated. E4 live/merely expired C1 returns Unknown without a CAS;
    // E5 foreign C2 returns an error without changing the observed ledger.
    // Rules S1 Quiesced/Dead+C2+C3 -> E1,E2,E3; S2 Ready/expired-Ready -> E3,E4;
    // S3 closed+foreign UID/configuration -> E3,E5. Ticket/isolation fault and
    // response-loss tests independently cover already-admitted requests.
    for cause in [
        "ready",
        "expired-ready",
        "quiesced",
        "dead",
        "foreign-ledger",
        "foreign-configuration",
    ] {
        let f = fixture::broker_fixture(|_| {}).await;
        let closed = !matches!(cause, "ready" | "expired-ready");
        if cause == "dead" {
            assert_eq!(
                f.directory
                    .expire_incarnation(&f.identity, 70_000)
                    .await
                    .unwrap(),
                awaken_dispatch::RegistryMutation::Applied,
                "real Dead precondition"
            );
        } else if closed {
            fixture::quiesce_worker(f.directory.as_ref(), &f.identity).await;
        }
        if cause == "expired-ready" {
            f.clock.set(70_000);
        }
        if cause == "foreign-ledger" {
            f.stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = "foreign-ledger".into();
        } else if cause == "foreign-configuration" {
            f.stored.lock().unwrap().as_mut().unwrap()["metadata"]["annotations"]["awaken.dev/create-ledger-configuration"] =
                "foreign-configuration".into();
        }
        let before = f.stored.lock().unwrap().clone();
        let predecessor = ContainerEffectFence::new(
            "old-effect",
            &f.identity.worker_id,
            f.identity.lease_owner(),
            1,
            u64::MAX,
        )
        .unwrap();
        let result = f
            .broker
            .seal_predecessor(f.directory.clone(), "scope", &predecessor, "awaken-scope")
            .await;
        if cause.starts_with("foreign-") {
            assert!(result.is_err(), "S3/{cause}");
        } else if closed {
            assert_eq!(
                result.unwrap(),
                RequestJoin::Complete(Vec::new()),
                "S1/{cause}"
            );
            let record = f.directory.incarnation(&f.identity).await.unwrap().unwrap();
            let pod = KubernetesPodIdentity::from_worker_id(&f.identity.worker_id).unwrap();
            let anchor = record.create_ledger.as_ref().unwrap();
            let exact = binding(&f.broker.ledger, anchor, &pod, "provider-a").unwrap();
            let late = CreateTicketRequest::new(
                group_for(&f.identity),
                "pods",
                "sandbox-ns",
                "late-pod",
                "c".repeat(64),
            )
            .unwrap();
            assert_eq!(
                f.broker.ledger.admit_request(exact, &late).await.unwrap().0,
                TicketAdmission::Sealed,
                "S1/{cause} same owner denies late Create"
            );
        } else {
            assert_eq!(result.unwrap(), RequestJoin::Unknown, "S2/{cause}");
        }
        if !closed || cause.starts_with("foreign-") {
            assert_eq!(
                *f.stored.lock().unwrap(),
                before,
                "S2/S3 no ledger mutation"
            );
        }
        assert!(
            f.directory
                .incarnation(&f.identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_none(),
            "S1/S2/S3 Create fence is not physical exit"
        );
        assert_eq!(
            f.pod_posts.load(Ordering::SeqCst),
            0,
            "S1/S2/S3 no Pod Create or stop"
        );
    }
}

#[tokio::test]
async fn closed_predecessor_without_original_ledger_stays_unknown() {
    // S4 closed exact predecessor + absent original anchor -> Unknown and zero
    // provider IO; current caller authentication cannot mint historical proof.
    let (f, caller) = fixture::ready_route_fixture().await;
    let old = f
        .directory
        .register(
            WorkerRegistration {
                worker_id: "k8s:sandbox-ns:old-worker:old-uid".into(),
                incarnation_id: "old-uid".into(),
                manifest: WorkerManifest::default(),
                create_ledger: None,
            },
            100,
            60_000,
        )
        .await
        .unwrap();
    fixture::quiesce_worker(f.directory.as_ref(), &old.snapshot.identity).await;
    let response = f
        .router
        .oneshot(fixture::route_request(
            SEAL_PATH,
            fixture::seal_body(&caller),
            Some(caller.worker_id.as_str()),
        ))
        .await
        .unwrap();
    fixture::assert_unknown_seal(response, "S4").await;
    assert_eq!(f.api_calls.load(Ordering::SeqCst), 0, "S4 zero provider IO");
}

#[tokio::test]
async fn closed_idle_worker_uses_the_same_original_creator_seal() {
    // CI1 exact Registry exit + no occupied physical slot -> the existing idle
    // owner passes the original anchor into the same provider seal before Pod
    // evidence retirement. CI2 exact retry is idempotent and creates nothing.
    // The shared fixture refuses any Worker lookup while its ledger is open.
    let f = fixture::broker_fixture(|_| {}).await;
    f.directory.deregister(&f.identity).await.unwrap();
    let exit =
        WorkerProcessExit::new("provider-a", original_process(), "exact-original-exit").unwrap();
    f.directory
        .record_process_exit(&f.identity, exit.clone())
        .await
        .unwrap();
    let store = awaken_dispatch::MemoryDispatchStore::new(Arc::new(ManualClock::new(200)));
    let source = source(&f);
    for _ in 0..2 {
        awaken_dispatch::release_closed_idle_worker_evidence(
            &store,
            f.directory.as_ref(),
            source.as_ref(),
            &f.identity,
            200,
        )
        .await
        .unwrap();
    }
    let state = fixture::ledger_state(&f.stored);
    assert_eq!(state["worker_sealed"], true);
    assert_eq!(state["groups"][group_for(&f.identity)], "sealed");
    assert_eq!(f.pod_posts.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.directory
            .incarnation(&f.identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit,
        Some(exit)
    );
}

fn original_process() -> String {
    json!({"namespace":"sandbox-ns","name":"worker","uid":"pod-uid",
        "node":"node-a","containers":[{"name":"worker","container_id":"containerd://original",
        "started_at":"2026-10-01T00:00:00Z"}],"retained":true,"restartable":true})
    .to_string()
}

fn source(f: &fixture::BrokerFixture) -> Arc<dyn awaken_dispatch::WorkerExecutionStopSource> {
    // The existing process root selects the real stop adapter over this same
    // client, control namespace and provider fingerprint. All M/CW/CI rows
    // above keep their original Registry, ledger, exit and no-Create effects;
    // the fixture no longer reaches through a foreign private implementation.
    awaken_cli::WorkerTransport::kubernetes(
        f.broker.api_client.clone(),
        "sandbox-ns",
        "worker",
        "awaken-worker",
        "provider-a",
        f.broker.ledger.control_namespace(),
    )
    .unwrap()
    .stop_source()
    .unwrap()
}

#[tokio::test]
async fn exited_worker_closes_its_original_create_ledger_before_releasing_evidence() {
    // CW causes: C1 original Registry anchor; C2 same/different observed ledger
    // UID; C3 independently retained local exit; C4 old physical entry closing.
    // CW1 C1+same+C3+C4 -> seal the same ledger before evidence release; retain
    // the physical slot because complete execution remains unproven.
    // CW2 foreign UID -> refuse before release; keep C3 and the original slot.
    // CW3 restored original UID/replay -> reuse C3, seal idempotently, no new
    // Create or execution. The explicit local receipt is a deterministic trusted
    // fixture; this tests real Registry/Dispatch and provider CAS composition.
    for foreign_uid in [false, true] {
        let f = fixture::broker_fixture(|_| {}).await;
        let old_record = f.directory.incarnation(&f.identity).await.unwrap().unwrap();
        let anchor = old_record.create_ledger.clone().unwrap();
        let clock = Arc::new(ManualClock::new(100));
        let store = awaken_dispatch::MemoryDispatchStore::new(clock.clone());
        let activation = awaken_dispatch_testkit::worker_http::activation("closed-creator");
        let run = activation.run_id.clone();
        store
            .enqueue(awaken_dispatch::RunDispatch::new(activation))
            .await
            .unwrap();
        let old = awaken_dispatch::RunClaim::from(
            &store
                .claim_run_compatible(&run, &old_record.snapshot, 50)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let process = original_process();
        let binding =
            AttemptExecutionBinding::new(f.identity.clone(), "provider-a", &process).unwrap();
        store.begin_attempt(&old, Some(&binding)).await.unwrap();
        store.close_attempt(&old).await.unwrap();
        store.renew_lease(&old, 0).await.unwrap();
        clock.set(200);
        let next = f
            .directory
            .register(
                WorkerRegistration {
                    worker_id: "next".into(),
                    incarnation_id: "next-boot".into(),
                    manifest: old_record.snapshot.manifest,
                    create_ledger: None,
                },
                200,
                60_000,
            )
            .await
            .unwrap();
        f.directory
            .heartbeat(
                &next.snapshot.identity,
                fixture::empty_heartbeat(1, true),
                200,
                60_000,
            )
            .await
            .unwrap();
        let next = f
            .directory
            .incarnation(&next.snapshot.identity)
            .await
            .unwrap()
            .unwrap();
        let next = awaken_dispatch::RunClaim::from(
            &store
                .claim_run_compatible(&run, &next.snapshot, 60_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        f.directory.deregister(&f.identity).await.unwrap();
        let exit = WorkerProcessExit::new("provider-a", process, "exact-original-exit").unwrap();
        f.directory
            .record_process_exit(&f.identity, exit.clone())
            .await
            .unwrap();
        let source = source(&f);
        if foreign_uid {
            f.stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = "foreign-ledger".into();
        }
        let original = store.load_physical_attempt(&run).await.unwrap();
        let result = awaken_dispatch::stop_expired_predecessor(
            &store,
            f.directory.as_ref(),
            source.as_ref(),
            None,
            &next,
            200,
        )
        .await;
        assert_eq!(result.is_err(), foreign_uid, "CW1/CW2");
        assert_eq!(store.load_physical_attempt(&run).await.unwrap(), original);
        assert_eq!(
            f.directory
                .incarnation(&f.identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit,
            Some(exit)
        );
        f.stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = anchor.uid().into();
        awaken_dispatch::stop_expired_predecessor(
            &store,
            f.directory.as_ref(),
            source.as_ref(),
            None,
            &next,
            200,
        )
        .await
        .unwrap();
        let state = fixture::ledger_state(&f.stored);
        assert_eq!(state["worker_sealed"], true, "CW3");
        assert_eq!(state["groups"][group_for(&f.identity)], "sealed", "CW3");
        assert_eq!(f.pod_posts.load(Ordering::SeqCst), 0, "CW3 no Create");
        assert_eq!(store.load_physical_attempt(&run).await.unwrap(), original);
    }
}
