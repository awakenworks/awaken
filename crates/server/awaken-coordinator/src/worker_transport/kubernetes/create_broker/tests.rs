mod capture_tests;
mod fixture;
use super::*;
use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{WorkerHeartbeat, WorkerManifest, WorkerRegistration};
use awaken_worker_registry::MemoryWorkerDirectory;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WORKER_ID_HEADER};
use axum::body::Body as AxumBody;
use axum::http::Response;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt as _;

#[test]
fn create_path_and_object_admission_are_total_and_namespace_bound() {
    // Causes: C1 collection resource is one of Pod/PVC/ConfigMap/Secret/
    // Job/DaemonSet; C2 namespace exact; C3 no subresource or query;
    // C4 deterministic name and stamped digest. Effects: E1 only the full
    // tuple yields one ticket; E2 a one-axis mutation is rejected before
    // any provider CAS or Kubernetes API effect.
    let identity = WorkerIdentity::new("k8s:ns:worker:uid", "uid", 1);
    let object = json!({"metadata":{
        "name":"target", "namespace":"ns",
        "annotations": {"awaken.dev/realization-digest": "a".repeat(64)}
    }});
    for path in [
        "/api/v1/namespaces/ns/pods",
        "/api/v1/namespaces/ns/configmaps",
        "/api/v1/namespaces/ns/secrets",
        "/api/v1/namespaces/ns/persistentvolumeclaims",
        "/apis/batch/v1/namespaces/ns/jobs",
        "/apis/apps/v1/namespaces/ns/daemonsets",
    ] {
        assert!(
            validate_create(
                &identity,
                "ns",
                K8sCreateRequest {
                    path: path.into(),
                    object: object.clone()
                }
            )
            .is_ok(),
            "E1 {path}"
        );
    }
    for path in [
        "/api/v1/namespaces/foreign/pods",
        "/api/v1/namespaces/ns/pods/target/exec",
        "/api/v1/namespaces/ns/pods?dryRun=All",
        "/apis/batch/v1/namespaces/ns/cronjobs",
    ] {
        assert!(
            validate_create(
                &identity,
                "ns",
                K8sCreateRequest {
                    path: path.into(),
                    object: object.clone()
                }
            )
            .is_err(),
            "E2 {path}"
        );
    }
    for mutation in [
        json!({"metadata":{"name":"target","namespace":"foreign","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
        json!({"metadata":{"name":"target","namespace":"ns","generateName":"other-","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
        json!({"metadata":{"name":"target","namespace":"ns"}}),
        json!({"metadata":{"name":"target","namespace":"ns","labels":{(ISOLATION_LABEL):"true"},"annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
        json!({"metadata":{"name":"target","namespace":"ns","annotations":{"awaken.dev/realization-digest":"a".repeat(64),(ISOLATION_ID_ANNOTATION):"forged"}}}),
        json!({"metadata":{"name":"target","namespace":"ns","finalizers":[ORIGINAL_GUARD_FINALIZER],"annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
    ] {
        assert!(
            validate_create(
                &identity,
                "ns",
                K8sCreateRequest {
                    path: "/api/v1/namespaces/ns/pods".into(),
                    object: mutation
                }
            )
            .is_err(),
            "E2 object axis"
        );
    }
}

#[test]
fn provider_isolation_metadata_is_reserved_on_every_brokered_kind() {
    // Causes: C1 each of the six admitted Kubernetes collections; C2
    // provider-owned evidence is absent, or the Worker supplies the
    // reservation label, isolation annotation, or either provider
    // finalizer. Constraint: exact namespace/name and realization digest
    // are otherwise valid. Effects: E1 absent C2 admits one ticket; E2
    // every forged C2 is refused before ledger CAS or API I/O.
    // Decision rows M1=C1+absent->E1; M2-M4=C1+each forged axis->E2.
    let identity = WorkerIdentity::new("k8s:ns:worker:uid", "uid", 1);
    for path in [
        "/api/v1/namespaces/ns/pods",
        "/api/v1/namespaces/ns/persistentvolumeclaims",
        "/api/v1/namespaces/ns/configmaps",
        "/api/v1/namespaces/ns/secrets",
        "/apis/batch/v1/namespaces/ns/jobs",
        "/apis/apps/v1/namespaces/ns/daemonsets",
    ] {
        let base = json!({"metadata":{
            "name":"target", "namespace":"ns",
            "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
        }});
        let request = |object| K8sCreateRequest {
            path: path.into(),
            object,
        };
        assert!(
            validate_create(&identity, "ns", request(base.clone())).is_ok(),
            "M1 {path}"
        );
        let mut label = base.clone();
        label["metadata"]["labels"] = json!({(ISOLATION_LABEL):"true"});
        let mut annotation = base.clone();
        annotation["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "forged".into();
        let mut finalizer = base;
        finalizer["metadata"]["finalizers"] = json!([ORIGINAL_GUARD_FINALIZER]);
        let mut reservation_finalizer = finalizer.clone();
        reservation_finalizer["metadata"]["finalizers"] = json!([ISOLATION_LABEL]);
        for (axis, object) in [
            ("label", label),
            ("annotation", annotation),
            ("finalizer", finalizer),
            ("reservation finalizer", reservation_finalizer),
        ] {
            assert!(
                validate_create(&identity, "ns", request(object)).is_err(),
                "M2-M4 {path} {axis}"
            );
        }
    }
}

#[test]
fn original_readback_field_view_keeps_exact_uid_and_json_compatibility() {
    // C1 shared original name/namespace/nonce/request-digest/undeleted law;
    // C2 exact retained Created UID and nonempty provider nonce; C3 unrelated
    // JSON fields malformed or isolation marker present. B1 C1+C2 -> match;
    // B2 missing/wrong/blank C1 or C2 -> refuse; B3 C3 leaves this raw reader
    // unchanged (no whole ObjectMeta decode or borrowed isolation purpose).
    // Fresh201 and Finished(Created) use this matcher; an Isolated UID
    // cannot replace the original UID. Response-loss replay's one-POST
    // end-to-end case remains in worker_response_loss below.
    // B4 delegates definitive-result validation to its existing owner:
    // expected empty/blank/spaced -> refuse; exact embedded controls and
    // long opaque UIDs retain this raw reader's old acceptance. B5 valid
    // Isolated/Rejected result shapes -> refuse original-Create purpose,
    // never borrow a reservation UID or turn a refusal into Created.
    let ticket =
        CreateTicketRequest::new("group", "pods", "sandbox-ns", "sandbox-pod", "a".repeat(64))
            .unwrap();
    let nonce = "0123456789abcdef0123456789abcdef";
    let original = json!({"metadata":{
        "name":"sandbox-pod", "namespace":"sandbox-ns", "uid":"original-uid",
        "annotations":{(ISOLATION_ID_ANNOTATION):nonce,(REQUEST_DIGEST_ANNOTATION):ticket.digest}
    }});
    for row in [
        "exact",
        "name",
        "namespace",
        "nonce",
        "digest",
        "deleting",
        "delete-malformed",
        "uid-missing",
        "uid-empty",
        "uid-blank",
        "uid-wrong",
        "uid-spaced",
        "expected-spaced",
        "expected-empty",
        "expected-blank",
        "expected-control",
        "expected-long",
        "result-isolated",
        "result-rejected",
        "nonce-empty",
        "name-malformed",
        "annotations-malformed",
        "unrelated-malformed",
        "isolation-marker",
        "delete-null",
    ] {
        let mut object = original.clone();
        let mut result = TicketResult::Created {
            uid: "original-uid".into(),
        };
        let mut expected_nonce = nonce;
        match row {
            "name" => object["metadata"]["name"] = "other".into(),
            "namespace" => object["metadata"]["namespace"] = "other".into(),
            "nonce" => object["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "other".into(),
            "digest" => {
                object["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION] = "b".repeat(64).into()
            }
            "deleting" => object["metadata"]["deletionTimestamp"] = "2026-10-12T00:00:00Z".into(),
            "delete-malformed" => object["metadata"]["deletionTimestamp"] = json!(false),
            "delete-null" => object["metadata"]["deletionTimestamp"] = Value::Null,
            "uid-missing" => {
                object["metadata"].as_object_mut().unwrap().remove("uid");
            }
            "uid-empty" => object["metadata"]["uid"] = "".into(),
            "uid-blank" => object["metadata"]["uid"] = " \t ".into(),
            "uid-wrong" => object["metadata"]["uid"] = "reserved-uid".into(),
            "uid-spaced" => object["metadata"]["uid"] = " original-uid ".into(),
            "expected-spaced" | "expected-empty" | "expected-blank" | "expected-control"
            | "expected-long" => {
                let uid = match row {
                    "expected-spaced" => " original-uid ".into(),
                    "expected-empty" => String::new(),
                    "expected-blank" => " \t ".into(),
                    "expected-control" => "original\nuid\0".into(),
                    "expected-long" => "u".repeat(4097),
                    _ => unreachable!(),
                };
                object["metadata"]["uid"] = Value::String(uid.clone());
                result = TicketResult::Created { uid };
            }
            "result-isolated" => {
                result = TicketResult::Isolated {
                    uid: "original-uid".into(),
                };
            }
            "result-rejected" => result = TicketResult::RejectedNoEffect { code: 403 },
            "nonce-empty" => {
                expected_nonce = "";
                object["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "".into();
            }
            "name-malformed" => object["metadata"]["name"] = json!(5),
            "annotations-malformed" => object["metadata"]["annotations"] = json!([]),
            "unrelated-malformed" => {
                object["metadata"]["resourceVersion"] = json!([false]);
                object["metadata"]["creationTimestamp"] = json!(5);
                object["metadata"]["ownerReferences"] = "unparsed".into();
                object["metadata"]["labels"] = json!(false);
            }
            "isolation-marker" => object["metadata"]["labels"] = json!({(ISOLATION_LABEL):"true"}),
            _ => {}
        }
        assert_eq!(
            original_readback_matches(&object, &ticket, &result, expected_nonce),
            matches!(
                row,
                "exact"
                    | "unrelated-malformed"
                    | "isolation-marker"
                    | "delete-null"
                    | "expected-control"
                    | "expected-long"
            ),
            "B1-B3/{row}"
        );
    }
}

#[test]
fn broker_stamps_identity_on_every_kind_and_guards_only_pod() {
    // Causes: C1 one of six brokered Kind paths has won the durable CAS;
    // C2 its ticket has a provider nonce or a historical missing nonce;
    // C3 the Worker supplies ordinary metadata and no protected markers.
    // Effects: E1 C1+C2(valid)+C3 adds the exact nonce/digest without
    // changing the Worker projection; E2 only Pod gains the guard its
    // existing disposal owner can release; E3 C2(missing) refuses
    // forwarding before an API call. Decision rules B1=valid+C3->E1,
    // B2=B1+Pod->E2, B3=missing->E3.
    let body = serde_json::to_vec(&json!({
        "metadata":{"name":"target","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}},
        "spec":{"keep":"worker"}
    }))
    .unwrap();
    for resource in [
        "pods",
        "persistentvolumeclaims",
        "configmaps",
        "secrets",
        "jobs",
        "daemonsets",
    ] {
        let stamped = stamp_admitted_create(
            body.clone(),
            resource,
            "0123456789abcdef0123456789abcdef",
            &"b".repeat(64),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&stamped).unwrap();
        assert_eq!(value["spec"]["keep"], "worker", "B1 {resource}");
        assert_eq!(
            value["metadata"]["annotations"][ISOLATION_ID_ANNOTATION],
            "0123456789abcdef0123456789abcdef",
            "B1 {resource}"
        );
        assert_eq!(
            value["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION],
            "b".repeat(64),
            "B1 {resource}"
        );
        assert_eq!(
            value["metadata"]["finalizers"],
            if resource == "pods" {
                json!([ORIGINAL_GUARD_FINALIZER])
            } else {
                Value::Null
            },
            "B2 {resource}"
        );
        assert!(
            stamp_admitted_create(body.clone(), resource, "", &"b".repeat(64)).is_err(),
            "B3 {resource}"
        );
    }
}

#[tokio::test]
async fn worker_response_loss_does_not_cancel_the_one_admitted_create() {
    // Causes: C1 exact Ready Worker and Registry-frozen ledger UID; C2 a
    // Pod Create has won ticket CAS but its API response is delayed; C3
    // Worker HTTP future disappears; C4 a retry arrives before/after the
    // independent executor durably records API UID. Effects: E1 one API
    // Create only, E2 early retry stays InFlight, E3 detached completion
    // survives C3, E4 late retry reads back the original ticket UID/nonce/digest; no new POST.
    // Rules R1=C1+C2=>E1, R2=R1+C4(before)=>E2,
    // R3=R1+C3+API success=>E3, R4=R3+C4(after)=>E4. C5 a
    // Registry-persisted exact Pod exit arrives; E5 only C5 plus settled
    // tickets can seal and join, never an expired lease alone. C6 the
    // broker forwards a Pod after first CAS; E6 its two provider-owned
    // annotations equal the admitted ticket, and its original-Pod guard
    // finalizer is installed before Kubernetes can publish the Pod.
    let fixture::BrokerFixture {
        broker,
        directory,
        clock,
        identity,
        create,
        pod_posts,
        created_pod,
        started,
        release,
        completed,
        ..
    } = fixture::broker_fixture(|_| {}).await;
    let first = tokio::spawn({
        let (broker, directory, clock, identity, create) = (
            broker.clone(),
            directory.clone(),
            clock.clone(),
            identity.clone(),
            create.clone(),
        );
        async move { broker.create(directory, clock, identity, create).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    assert_eq!(pod_posts.load(Ordering::SeqCst), 1, "R1");
    assert!(
        broker
            .create(
                directory.clone(),
                clock.clone(),
                identity.clone(),
                create.clone()
            )
            .await
            .is_err(),
        "R2 InFlight retry"
    );
    assert_eq!(
        pod_posts.load(Ordering::SeqCst),
        1,
        "R2 no duplicate API request"
    );
    first.abort();
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), completed.notified())
        .await
        .unwrap();
    let replay = broker
        .create(
            directory.clone(),
            clock.clone(),
            identity.clone(),
            create.clone(),
        )
        .await
        .unwrap();
    let replay: K8sCreateResponse = serde_json::from_slice(&replay.body).unwrap();
    assert_eq!(
        replay.status, 200,
        "R4 only the original ticket-bound Pod can be replayed"
    );
    assert_eq!(
        pod_posts.load(Ordering::SeqCst),
        1,
        "R3 independent executor made exactly one API call"
    );
    // R5 causes: retained Created ticket + a same-name readback with one
    // changed original UID/nonce/digest/ns/name or deleting state. Effect:
    // reject with no next POST and no object receipt. R4 is its exact row.
    let original = created_pod.lock().unwrap().clone().unwrap();
    for axis in ["uid", "nonce", "digest", "namespace", "name", "deleting"] {
        let mut changed = original.clone();
        match axis {
            "nonce" => {
                changed["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "foreign".into()
            }
            "digest" => {
                changed["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION] = "foreign".into()
            }
            "deleting" => changed["metadata"]["deletionTimestamp"] = "2026-10-12T00:00:00Z".into(),
            field => changed["metadata"][field] = "foreign".into(),
        }
        *created_pod.lock().unwrap() = Some(changed);
        assert!(
            broker
                .create(
                    directory.clone(),
                    clock.clone(),
                    identity.clone(),
                    create.clone()
                )
                .await
                .is_err(),
            "R5/{axis}"
        );
        assert_eq!(pod_posts.load(Ordering::SeqCst), 1, "R5 no new API Create");
    }
    *created_pod.lock().unwrap() = Some(original);
    let predecessor = ContainerEffectFence::new(
        "effect-a",
        &identity.worker_id,
        identity.lease_owner(),
        1,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(
        broker
            .seal_predecessor(directory.clone(), "scope", &predecessor, "awaken-scope")
            .await
            .unwrap(),
        RequestJoin::Unknown,
        "E5 a live Worker is not closed by a completed Create alone"
    );
    directory.deregister(&identity).await.unwrap();
    let process = json!({
        "namespace":"sandbox-ns", "name":"worker", "uid":"pod-uid",
        "node":"node-a", "containers":[], "retained":true
    });
    directory
        .record_process_exit(
            &identity,
            awaken_dispatch::WorkerProcessExit::new(
                "provider-a",
                process.to_string(),
                "resource-version-7",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            broker.seal_predecessor(directory, "scope", &predecessor, "awaken-scope").await.unwrap(),
            RequestJoin::Complete(tickets) if tickets.len() == 1
        ),
        "E5 exact persisted exit plus one finished ticket joins"
    );
}

#[tokio::test]
async fn private_create_route_requires_transport_and_current_registry_identity() {
    // Causes: A no authenticated header, B authenticated other Worker,
    // C matching header with no durable Registry incarnation. Effects:
    // each is refused before a provider GET, ticket CAS or Kubernetes
    // Create; neither application JSON nor a copied Worker id is proof.
    let api_calls = Arc::new(AtomicUsize::new(0));
    let service = tower::service_fn({
        let api_calls = api_calls.clone();
        move |_request: Request<Body>| {
            api_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(500)
                        .body(Body::from(b"{}".to_vec()))
                        .unwrap(),
                )
            }
        }
    });
    let client = Client::new(service, "sandbox-ns");
    let ledger =
        K8sWorkerCreateLedger::new(client.clone(), "control-ns", "sandbox-ns", "provider-a")
            .unwrap();
    let router = Arc::new(K8sCreateBroker::new(
        client,
        ledger,
        "sandbox-ns",
        "provider-a",
    ))
    .router(
        Arc::new(MemoryWorkerDirectory::new()),
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualClock::new(100)),
    );
    let wire = json!({
        "wire_version": K8S_CREATE_WIRE_VERSION,
        "identity": WorkerIdentity::new("k8s:sandbox-ns:worker:pod-uid", "pod-uid", 1),
        "create": {
            "path":"/api/v1/namespaces/sandbox-ns/pods",
            "object":{"metadata":{"name":"pod","namespace":"sandbox-ns"}}
        }
    });
    let body = serde_json::to_vec(&wire).unwrap();
    let request = |header: Option<&str>| {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(PATH)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = header {
            builder = builder.header(WORKER_ID_HEADER, value);
        }
        builder.body(AxumBody::from(body.clone())).unwrap()
    };
    assert_eq!(
        router
            .clone()
            .oneshot(request(None))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED,
        "A"
    );
    assert_eq!(
        router
            .clone()
            .oneshot(request(Some("foreign")))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST,
        "B"
    );
    assert_eq!(
        router
            .oneshot(request(Some("k8s:sandbox-ns:worker:pod-uid")))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST,
        "C"
    );
    assert_eq!(
        api_calls.load(Ordering::SeqCst),
        0,
        "no rejected identity reached Kubernetes"
    );
}
