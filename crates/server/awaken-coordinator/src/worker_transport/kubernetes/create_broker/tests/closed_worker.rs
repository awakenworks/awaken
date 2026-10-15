//! Original Worker closure reuses the registered provider ledger before release.
use super::*;
use awaken_dispatch::{
    AttemptExecutionBinding, DispatchClaims, DispatchControlReads, DispatchEnqueue,
    DispatchExecution, WorkerDirectory,
};
use awaken_sandbox_container::k8s::{PodProcessObserver, PodProcessStopper};
use awaken_worker_contract::WorkerProcessExit;

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
            &source,
            &f.identity,
            200,
        )
        .await
        .unwrap();
    }
    let state: Value = {
        let object = f.stored.lock().unwrap();
        serde_json::from_str(object.as_ref().unwrap()["data"]["ledger"].as_str().unwrap()).unwrap()
    };
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

fn source(f: &fixture::BrokerFixture) -> super::super::super::KubernetesWorkerStops {
    let observer = PodProcessObserver::new(f.broker.api_client.clone(), "sandbox-ns").unwrap();
    super::super::super::KubernetesWorkerStops {
        stopper: PodProcessStopper::new(observer),
        create_ledger: f.broker.ledger.clone(),
        namespace: "sandbox-ns".into(),
        provider_id: "provider-a".into(),
    }
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
            &source,
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
            &source,
            None,
            &next,
            200,
        )
        .await
        .unwrap();
        let state: Value = {
            let object = f.stored.lock().unwrap();
            serde_json::from_str(object.as_ref().unwrap()["data"]["ledger"].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(state["worker_sealed"], true, "CW3");
        assert_eq!(state["groups"][group_for(&f.identity)], "sealed", "CW3");
        assert_eq!(f.pod_posts.load(Ordering::SeqCst), 0, "CW3 no Create");
        assert_eq!(store.load_physical_attempt(&run).await.unwrap(), original);
    }
}
