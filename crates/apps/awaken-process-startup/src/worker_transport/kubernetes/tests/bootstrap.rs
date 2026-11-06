// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Bootstrap closure through the shipped transport, original ledger and Pod stopper.
use super::*;
use awaken_container_contract::{CreateTicketRequest, K8sCreateBrokerEffects, K8sLedgerBinding};

#[tokio::test]
async fn bootstrap_retirement_requires_original_empty_ledger_before_any_pod_mutation() {
    // Cause/effect graph: C1 original exact ledger; C2 no process binding;
    // C3 no Create ticket; C4 bounded capture; C5 exact Pod UID; C6 restart>0;
    // C7 API-owned deletion intent. B1 C1..C6+!C7 -> absorbing seal only,
    // exact Pod/evidence untouched across replay; B2 same anchor+C7 -> release
    // only Worker finalizer through the original CAS, no DELETE or exit receipt.
    // B3 failed ledger conjunct/foreign anchor -> no Pod write. B4 replacement
    // UID -> successor untouched. Original CAS race tests own binding-vs-seal.
    for cause in [
        "restarted",
        "bound",
        "ticket",
        "legacy",
        "missing",
        "foreign-anchor",
        "replacement",
    ] {
        let (transport, facts, client) = fixture_with_client();
        let ledger = K8sWorkerCreateLedger::new(
            client.clone(),
            "create-control",
            "workers",
            "workers",
            "test-cluster",
        )
        .unwrap();
        let (name, uid) = ledger
            .initialize("workers", "worker-a", "uid-a", "uid-a")
            .await
            .unwrap();
        let anchor = WorkerCreateLedger::new(
            "test-cluster",
            "create-control",
            &name,
            &uid,
            ledger.configuration_fingerprint(),
        )
        .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "workers",
            worker_name: "worker-a",
            worker_uid: "uid-a",
            worker_incarnation: "uid-a",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: ledger.configuration_fingerprint(),
        };
        let source = transport.binding_source().unwrap();
        if cause == "bound" {
            // A historical (Pod-UID incarnation) ledger that froze its process.
            let process =
                awaken_worker_fleet_kubernetes::PodProcessObserver::new(client.clone(), "workers")
                    .unwrap()
                    .bind_pod_process("worker-a", "uid-a")
                    .await
                    .unwrap();
            ledger
                .retain_process_binding(binding, &process)
                .await
                .unwrap();
        } else if cause == "ticket" {
            ledger
                .admit_request(
                    binding,
                    &CreateTicketRequest::new(
                        "effect-a",
                        "pods",
                        "workers",
                        "sandbox",
                        "a".repeat(64),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        } else if cause == "missing" {
            *facts.ledger.lock().unwrap() = None;
        } else if cause == "legacy" {
            let mut stored = facts.ledger.lock().unwrap();
            let object = stored.as_mut().unwrap();
            let mut state: Value =
                serde_json::from_str(object["data"]["ledger"].as_str().unwrap()).unwrap();
            state.as_object_mut().unwrap().remove("capture_policy");
            object["data"]["ledger"] = json!(state.to_string());
        }
        facts.pod.lock().unwrap().1["status"]["containerStatuses"][0]["restartCount"] = json!(3);
        facts.pod.lock().unwrap().1["metadata"]["finalizers"] =
            json!(["awaken.dev/worker-process-evidence", "other.example/keep"]);
        if cause == "replacement" {
            facts.pod.lock().unwrap().1["metadata"]["uid"] = json!("successor");
        }
        let expected = if cause == "foreign-anchor" {
            WorkerCreateLedger::new(
                "test-cluster",
                "create-control",
                &name,
                "different-uid",
                ledger.configuration_fingerprint(),
            )
            .unwrap()
        } else {
            anchor.clone()
        };
        let before = facts.ledger.lock().unwrap().clone();
        let result = source
            .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &expected)
            .await;
        let success = matches!(cause, "restarted" | "replacement");
        assert_eq!(result.is_ok(), success, "B1/B2/B3 {cause}");
        assert!(
            facts.deletes.lock().unwrap().is_empty(),
            "B1 no early replacement: {cause}"
        );
        assert!(
            facts.updates.lock().unwrap().is_empty(),
            "B1/B3/B4 no early release"
        );
        if cause == "restarted" {
            assert_eq!(result.unwrap(), anchor);
            let stored = facts.ledger.lock().unwrap().clone().unwrap();
            assert!(stored["data"].get("worker_process_binding").is_none());
            let state: Value =
                serde_json::from_str(stored["data"]["ledger"].as_str().unwrap()).unwrap();
            assert_eq!(state["worker_sealed"], true);
            assert_eq!(state["tickets"], json!({}));
            source
                .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &anchor)
                .await
                .unwrap();
            assert!(
                facts.updates.lock().unwrap().is_empty(),
                "B1 repeated seal keeps original set"
            );
            // The normal release/Deployment controller, not bootstrap close,
            // has now accepted deletion after its zero target and stable cut.
            facts.pod.lock().unwrap().1["metadata"]["deletionTimestamp"] =
                json!("2026-10-23T00:00:00Z");
            source
                .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &anchor)
                .await
                .unwrap();
            assert_eq!(facts.updates.lock().unwrap().len(), 1, "B2 one exact CAS");
            assert_eq!(
                facts.pod.lock().unwrap().1["metadata"]["finalizers"],
                json!(["other.example/keep"])
            );
        } else {
            assert!(
                facts.updates.lock().unwrap().is_empty(),
                "B2/B3 no Pod mutation"
            );
            if !success {
                assert_eq!(*facts.ledger.lock().unwrap(), before);
            }
        }
        if success {
            source
                .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &anchor)
                .await
                .unwrap();
            assert!(
                facts.deletes.lock().unwrap().is_empty(),
                "B2/B4 no DELETE even on replay"
            );
        }
    }
}

#[tokio::test]
async fn bootstrap_retirement_failure_keeps_deny_fence_and_retries_same_uid() {
    // B5 successful seal + deletion intent + unavailable finalizer CAS -> error,
    // Worker evidence stays,
    // late first binding denied. B6 provider recovers -> same original ledger
    // and UID retire, no new identity/ticket/receipt. Stopper fault tables own
    // response-loss, RV conflict, replacement, finalizer and sibling-retention.
    let (transport, facts, client) = fixture_with_client();
    let ledger = K8sWorkerCreateLedger::new(
        client,
        "create-control",
        "workers",
        "workers",
        "test-cluster",
    )
    .unwrap();
    let (name, uid) = ledger
        .initialize("workers", "worker-a", "uid-a", "uid-a")
        .await
        .unwrap();
    let anchor = WorkerCreateLedger::new(
        "test-cluster",
        "create-control",
        &name,
        &uid,
        ledger.configuration_fingerprint(),
    )
    .unwrap();
    let source = transport.binding_source().unwrap();
    facts.pod.lock().unwrap().1["metadata"]["deletionTimestamp"] = json!("2026-10-23T00:00:00Z");
    facts.update_failure.store(true, Ordering::SeqCst);
    assert!(
        source
            .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &anchor)
            .await
            .is_err()
    );
    assert!(facts.updates.lock().unwrap().is_empty());
    let stored = facts.ledger.lock().unwrap().clone().unwrap();
    let state: Value = serde_json::from_str(stored["data"]["ledger"].as_str().unwrap()).unwrap();
    assert_eq!(state["worker_sealed"], true);
    assert!(
        source
            .prepare_registration_physical_evidence(&WorkerRegistration {
                worker_id: "k8s:workers:worker-a:uid-a".into(),
                incarnation_id: "uid-a".into(),
                manifest: manifest(),
                create_ledger: None,
            })
            .await
            .is_err(),
        "B4 absorbing denial"
    );
    facts.update_failure.store(false, Ordering::SeqCst);
    assert_eq!(
        source
            .seal_unattempted_worker("k8s:workers:worker-a:uid-a", &anchor)
            .await
            .unwrap(),
        anchor
    );
    assert!(facts.deletes.lock().unwrap().is_empty());
    assert_eq!(facts.updates.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ended_unregistered_worker_pods_release_retention_only_when_provably_unregistered() {
    // Finalizer decision table. The Worker process-evidence finalizer is never
    // in a Pod template; registration adds it once the Pod can bind. Causes:
    // R registration {registered: a Registry record exists, never: none};
    // P phase {Running, Failed, Succeeded}; L Create ledger {absent, unbound,
    // bound: a process binding was frozen before a Registry insert}.
    // | R          | P                 | L              | finalizer                       |
    // | registered | any               | any            | present (exact-exit path owns)  |
    // | never      | Running           | any            | present (live Worker may finish registering) |
    // | never      | Failed/Succeeded  | absent/unbound | removed (unbound ledger sealed first) |
    // | never      | Failed/Succeeded  | bound          | present, error (insert may be pending) |
    // Every removal is one UID/resourceVersion-fenced finalizer PUT that keeps
    // sibling finalizers; no row DELETEs a Pod or writes a Registry record.
    use awaken_dispatch::WorkerDirectory;
    use awaken_worker_contract::WorkerCurrentSource as _;
    for registered in [false, true] {
        for phase in ["Running", "Failed", "Succeeded"] {
            for ledger_state in ["absent", "unbound", "bound"] {
                let row = format!("registered={registered} {phase} ledger={ledger_state}");
                let (transport, facts, client) = fixture_with_client();
                let ledger = K8sWorkerCreateLedger::new(
                    client,
                    "create-control",
                    "workers",
                    "workers",
                    "test-cluster",
                )
                .unwrap();
                facts.pod.lock().unwrap().1["metadata"]["finalizers"] =
                    json!(["awaken.dev/worker-process-evidence", "other.example/keep"]);
                // The Pod's container instance names its incarnation and ledger.
                let incarnation = transport
                    .binding_source()
                    .unwrap()
                    .registration_incarnation(&WorkerRegistration {
                        worker_id: "k8s:workers:worker-a:uid-a".into(),
                        incarnation_id: "process-proposed".into(),
                        manifest: manifest(),
                        create_ledger: None,
                    })
                    .await
                    .unwrap()
                    .unwrap();
                match ledger_state {
                    "unbound" => {
                        ledger
                            .initialize("workers", "worker-a", "uid-a", &incarnation)
                            .await
                            .unwrap();
                    }
                    "bound" => {
                        transport
                            .binding_source()
                            .unwrap()
                            .prepare_registration_physical_evidence(&WorkerRegistration {
                                worker_id: "k8s:workers:worker-a:uid-a".into(),
                                incarnation_id: incarnation.clone(),
                                manifest: manifest(),
                                create_ledger: None,
                            })
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
                let directory = awaken_worker_registry::MemoryWorkerDirectory::new();
                if registered {
                    directory
                        .register(
                            WorkerRegistration {
                                worker_id: "k8s:workers:worker-a:uid-a".into(),
                                incarnation_id: incarnation.clone(),
                                manifest: manifest(),
                                create_ledger: None,
                            },
                            100,
                            1_000,
                        )
                        .await
                        .unwrap();
                }
                {
                    let mut pod = facts.pod.lock().unwrap();
                    pod.1["status"]["phase"] = json!(phase);
                    if phase != "Running" {
                        pod.1["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
                            "containerID":"containerd://exact", "exitCode":1, "reason":"Error",
                            "startedAt":"2026-09-09T00:00:00Z", "finishedAt":"2026-09-09T00:00:01Z"
                        }});
                    }
                }
                let updates_before = facts.updates.lock().unwrap().len();
                let result = transport
                    .stop_source()
                    .unwrap()
                    .release_unregistered_ended_worker_evidence(&directory)
                    .await;
                let ended = phase != "Running";
                let removed = !registered && ended && ledger_state != "bound";
                let refused = !registered && ended && ledger_state == "bound";
                assert_eq!(result.is_err(), refused, "{row}");
                let finalizers = facts.pod.lock().unwrap().1["metadata"]["finalizers"].clone();
                assert_eq!(
                    finalizers,
                    if removed {
                        json!(["other.example/keep"])
                    } else {
                        json!(["awaken.dev/worker-process-evidence", "other.example/keep"])
                    },
                    "{row}"
                );
                assert_eq!(
                    facts.updates.lock().unwrap().len() - updates_before,
                    usize::from(removed),
                    "{row}: one exact finalizer CAS"
                );
                assert!(facts.deletes.lock().unwrap().is_empty(), "{row}: no DELETE");
                if removed && ledger_state == "unbound" {
                    let stored = facts.ledger.lock().unwrap().clone().unwrap();
                    let state: Value =
                        serde_json::from_str(stored["data"]["ledger"].as_str().unwrap()).unwrap();
                    assert_eq!(state["worker_sealed"], true, "{row}: deny fence first");
                }
                assert_eq!(
                    directory
                        .current("k8s:workers:worker-a:uid-a")
                        .await
                        .unwrap()
                        .is_some(),
                    registered,
                    "{row}: no Registry write"
                );
            }
        }
    }
}
