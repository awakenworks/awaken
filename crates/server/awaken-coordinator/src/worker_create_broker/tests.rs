use awaken_container_contract::{
    CreateTicketRequest, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, K8S_CREATE_WIRE_VERSION,
    ORIGINAL_GUARD_FINALIZER, REQUEST_DIGEST_ANNOTATION, RequestJoin, TicketResult,
};
mod capture_tests;
mod closed_worker;
mod fixture;
use super::*;
use awaken_container_contract::K8S_CREATE_CAPTURE_HEADER;
use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{WorkerHeartbeat, WorkerManifest, WorkerRegistration};
use awaken_sandbox_kubernetes::K8sWorkerCreateLedger;
use awaken_worker_contract::WorkerIncarnationSource as _;
use awaken_worker_registry::MemoryWorkerDirectory;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WORKER_ID_HEADER};
use axum::body::Body as AxumBody;
use axum::http::{Method, Request, Response, StatusCode, header};
use kube::{Client, client::Body};
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
    // exact Registry incarnation closes while its process exit is unavailable;
    // E5 C5 plus the original ledger CAS and settled tickets seals and joins
    // Create admission without asserting process exit or physical disposal.
    // A merely expired Ready Worker cannot use that fence. C6 the
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
    let predecessor = SandboxEffectFence::new(
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
    assert!(
        matches!(
            broker.seal_predecessor(directory.clone(), "scope", &predecessor, "awaken-scope").await.unwrap(),
            RequestJoin::Complete(tickets) if tickets.len() == 1
        ),
        "E5 closed admission plus one finished ticket joins without an exit receipt"
    );
    assert!(
        directory
            .incarnation(&identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .is_none(),
        "E5 Create join does not mint process-exit evidence"
    );
}

#[tokio::test]
async fn private_create_route_requires_transport_and_current_registry_identity() {
    // Causes: A no authenticated header, B authenticated other Worker,
    // C matching header with no durable Registry incarnation. Effects:
    // each is refused before a provider GET, ticket CAS or Kubernetes
    // Create; neither application JSON nor a copied Worker id is proof.
    let fixture::NoApiRouteFixture {
        router, api_calls, ..
    } = fixture::no_api_route_fixture();
    let wire = json!({
        "wire_version": K8S_CREATE_WIRE_VERSION,
        "identity": WorkerIdentity::new("k8s:sandbox-ns:worker:pod-uid", "pod-uid", 1),
        "create": {
            "path":"/api/v1/namespaces/sandbox-ns/pods",
            "object":{"metadata":{"name":"pod","namespace":"sandbox-ns"}}
        }
    });
    let body = serde_json::to_vec(&wire).unwrap();
    let request = |header: Option<&str>| fixture::route_request(PATH, body.clone(), header);
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

#[tokio::test]
async fn private_seal_route_allows_draining_without_create_admission() {
    // Cause/effect graph: C1 one authenticated exact current Registry identity
    // with a live lease; C2 it advances Ready -> Draining through begin_drain;
    // C3 the original predecessor incarnation is absent; C4 operation is Seal
    // versus fresh Create. S1=C1+Ready+C3+Seal -> HTTP200/Unknown, zero API.
    // S2=C1+Draining+C4(Create) -> original Ready-only refusal, zero API.
    // S3=C1+Draining+C3+Seal -> HTTP200/Unknown, zero API; old code returns400.
    // Constraints: Registry mutations and the original Router/authenticator
    // produce the caller state; no inserted snapshot, ledger, stop/exit receipt
    // or guessed historical closure. Unknown remains Unknown after acceptance;
    // this is continuation routing evidence, never physical cleanup admission.
    let (ready_fixture, identity) = fixture::ready_route_fixture().await;
    let fixture::NoApiRouteFixture {
        router,
        directory,
        clock,
        api_calls,
    } = ready_fixture;
    let now_ms = clock.now_ms();
    let seal_body = fixture::seal_body(&identity);
    let request = |path: &str, body: Vec<u8>| {
        fixture::route_request(path, body, Some(identity.worker_id.as_str()))
    };
    let ready = router
        .clone()
        .oneshot(request(SEAL_PATH, seal_body.clone()))
        .await
        .unwrap();
    fixture::assert_unknown_seal(ready, "S1 Ready").await;
    assert_eq!(api_calls.load(Ordering::SeqCst), 0, "S1 zero API");
    assert_eq!(
        directory
            .begin_drain(&identity, now_ms + 1_000)
            .await
            .unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "S2/S3 real Draining transition"
    );
    // Check fresh Create refusal before the deliberately red Seal row, so the
    // original admission's negative control remains observable on the red run.
    let create_body = fixture::create_body(&identity);
    let create = router
        .clone()
        .oneshot(request(PATH, create_body))
        .await
        .unwrap();
    assert_eq!(
        create.status(),
        StatusCode::BAD_REQUEST,
        "S2 no fresh Create grant"
    );
    assert_eq!(api_calls.load(Ordering::SeqCst), 0, "S2 zero API");
    let draining = router.oneshot(request(SEAL_PATH, seal_body)).await.unwrap();
    assert_eq!(
        api_calls.load(Ordering::SeqCst),
        0,
        "S3 zero API before response"
    );
    fixture::assert_unknown_seal(draining, "S3 Draining").await;
    assert_eq!(api_calls.load(Ordering::SeqCst), 0, "S3 zero API");
}

#[tokio::test]
async fn private_seal_route_uses_existing_non_ready_control_admission() {
    // The canonical non-Ready control policy also admits Starting and Quiesced
    // when the exact authenticated registration and lease remain live. P1 a
    // real ready=false heartbeat -> Starting; P2 begin_drain -> mark_quiesced.
    // Both -> Seal200/Unknown with the original capture profile, Create400 and
    // zero API. Ready and Draining are S1/S3 above; Dead, exact expiry and wrong
    // authority are N1..N8 below. This covers every Worker lifecycle state
    // without adding another state-policy table or treating Unknown as closure.
    for cause in ["starting", "quiesced"] {
        let (fixture, identity) = fixture::ready_route_fixture().await;
        if cause == "starting" {
            assert_eq!(
                fixture
                    .directory
                    .heartbeat(
                        &identity,
                        fixture::empty_heartbeat(2, false),
                        fixture.clock.now_ms(),
                        60_000,
                    )
                    .await
                    .unwrap(),
                awaken_dispatch::RegistryMutation::Applied,
                "P1 real Starting transition"
            );
        } else {
            fixture::quiesce_worker(fixture.directory.as_ref(), &identity).await;
        }
        let request = |path: &str, body: Vec<u8>| {
            fixture::route_request(path, body, Some(identity.worker_id.as_str()))
        };
        let create_body = fixture::create_body(&identity);
        let create = fixture
            .router
            .clone()
            .oneshot(request(PATH, create_body))
            .await
            .unwrap();
        assert_eq!(
            create.status(),
            StatusCode::BAD_REQUEST,
            "no Create/{cause}"
        );
        assert_eq!(
            fixture.api_calls.load(Ordering::SeqCst),
            0,
            "Create/{cause}"
        );
        let response = fixture
            .router
            .oneshot(request(SEAL_PATH, fixture::seal_body(&identity)))
            .await
            .unwrap();
        fixture::assert_unknown_seal(response, cause).await;
        assert_eq!(fixture.api_calls.load(Ordering::SeqCst), 0, "Seal/{cause}");
    }
}

#[tokio::test]
async fn private_seal_route_keeps_identity_liveness_and_transport_fences() {
    // Cause/effect graph: the positive S3 caller is registered and Draining;
    // independently remove transport authentication (N1), change its asserted
    // transport owner (N2), incarnation (N3), or generation (N4), reach the exact
    // lease expiry (N5), deregister the live caller (N6), publish a real newer
    // incarnation (N7), or use a directory without its registration (N8).
    // Decision table: N1 -> 401; N2..N8 -> 400; every row -> zero Kubernetes
    // calls and no complete-join response profile. S1/S3 in the preceding test
    // are the acceptance controls. State changes use Registry commands and the
    // supplied clock; wrong request coordinates are inputs, never inserted
    // Registry snapshots or fabricated process-exit/ledger facts. This tests
    // the original local authenticator's routing, not signed/mTLS deployment.
    for cause in [
        "missing-auth",
        "foreign-auth",
        "wrong-incarnation",
        "wrong-generation",
        "expired",
        "dead",
        "replaced",
        "unregistered",
    ] {
        let (mut fixture, identity) = fixture::ready_route_fixture().await;
        assert_eq!(
            fixture
                .directory
                .begin_drain(&identity, fixture.clock.now_ms() + 1_000)
                .await
                .unwrap(),
            awaken_dispatch::RegistryMutation::Applied,
            "real Draining precondition/{cause}"
        );
        let mut asserted = identity.clone();
        let mut transport_owner = Some(identity.worker_id.as_str());
        match cause {
            "missing-auth" => transport_owner = None,
            "foreign-auth" => transport_owner = Some("foreign"),
            "wrong-incarnation" => asserted.incarnation_id = "foreign".into(),
            "wrong-generation" => {
                asserted.generation = asserted.generation.checked_add(1).unwrap();
            }
            "expired" => fixture.clock.advance(60_000),
            "dead" | "replaced" => {
                assert_eq!(
                    fixture.directory.deregister(&identity).await.unwrap(),
                    awaken_dispatch::RegistryMutation::Applied,
                    "real Dead transition/{cause}"
                );
                if cause == "replaced" {
                    let replacement = fixture
                        .directory
                        .register(
                            WorkerRegistration {
                                worker_id: identity.worker_id.clone(),
                                incarnation_id: "replacement".into(),
                                manifest: WorkerManifest::default(),
                                create_ledger: None,
                            },
                            fixture.clock.now_ms(),
                            60_000,
                        )
                        .await
                        .unwrap();
                    assert!(replacement.snapshot.identity.generation > identity.generation);
                    assert_eq!(
                        fixture
                            .directory
                            .incarnation(&identity)
                            .await
                            .unwrap()
                            .unwrap()
                            .snapshot
                            .identity,
                        identity,
                        "N7 exact original record is retained"
                    );
                }
            }
            "unregistered" => fixture = fixture::no_api_route_fixture(),
            _ => unreachable!(),
        }
        let request =
            fixture::route_request(SEAL_PATH, fixture::seal_body(&asserted), transport_owner);
        let response = fixture.router.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            if cause == "missing-auth" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            },
            "refusal/{cause}"
        );
        assert!(
            response.headers().get(K8S_CREATE_CAPTURE_HEADER).is_none(),
            "no join profile/{cause}"
        );
        assert_eq!(
            fixture.api_calls.load(Ordering::SeqCst),
            0,
            "zero API/{cause}"
        );
    }
}
