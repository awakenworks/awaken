use super::*;
use awaken_container_contract::{CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER};

#[tokio::test]
async fn current_private_wire_refuses_missing_or_wrong_version_before_provider_effects() {
    // V1/V3: both existing endpoints require one current version. Missing
    // fails DTO extraction; wrong version fails before Registry/control/API.
    // Same endpoint and existing signed identity, no old serving fallback.
    for path in [PATH, SEAL_PATH] {
        for version in [None, Some(1_u8), Some(3)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let service = tower::service_fn({
                let calls = calls.clone();
                move |_request: Request<Body>| {
                    calls.fetch_add(1, Ordering::SeqCst);
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
            let ledger = K8sWorkerCreateLedger::new(
                client.clone(),
                "control-ns",
                "sandbox-ns",
                "provider-a",
            )
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
            let identity = WorkerIdentity::new("k8s:sandbox-ns:worker:pod-uid", "pod-uid", 1);
            let mut wire = if path == PATH {
                json!({"identity":identity,"create":{"path":"/api/v1/namespaces/sandbox-ns/pods","object":{"metadata":{"name":"pod"}}}})
            } else {
                json!({"identity":identity,"scope":"scope","predecessor":SandboxEffectFence::new("old","worker","boot",1,u64::MAX).unwrap(),"expected_pod_name":"awaken-scope"})
            };
            if let Some(version) = version {
                wire["wire_version"] = version.into();
            }
            let response = router
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header(WORKER_ID_HEADER, "k8s:sandbox-ns:worker:pod-uid")
                        .body(AxumBody::from(wire.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if version.is_none() {
                    StatusCode::UNPROCESSABLE_ENTITY
                } else {
                    StatusCode::BAD_REQUEST
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn original_response_bytes_are_checked_and_are_the_bytes_sent() {
    // R5/R6/R8/R9: API object may fit while complete status/object body
    // does not. Capture fails before finish. Once already Finished this
    // helper refusal affects replay only, never downgrades the ticket.
    // Explicit legacy body remains full and marked, not a truncation.
    let limit = CreateReplyProfile::FreshCreate.body_limit().unwrap();
    let response = K8sCreateResponse {
        status: 201,
        object: json!({"payload":"x".repeat(limit)}),
    };
    assert!(BrokerCreateReply::new(response, CreateReplyProfile::FreshCreate).is_err());
    let legacy = BrokerCreateReply::new(
        K8sCreateResponse {
            status: 200,
            object: json!({"payload":"x".repeat(limit)}),
        },
        CreateReplyProfile::LegacyRetainedCreate,
    )
    .unwrap();
    assert!(legacy.body.len() > limit);
    let http = legacy.into_http_response();
    assert_eq!(
        http.headers()[K8S_CREATE_CAPTURE_HEADER],
        CreateReplyProfile::LegacyRetainedCreate.header_value()
    );
}

#[tokio::test]
async fn fresh_api_or_complete_reply_or_result_overflow_never_finishes_or_reposts() {
    // R2-R7/G2: all three real producer boundaries. API bytes>B, API=B but
    // whole broker reply>B, and API/reply fit but result>R each leave the
    // original ticket InFlight. Same RPC retry cannot cause a second Pod
    // POST or claim a partial durable UID. Original fake API retains the
    // actual object separately from this uncommitted result.
    for mutation in [
        (|object: &mut Value| {
            object["payload"] = "x"
                .repeat(CreateReplyProfile::FreshCreate.body_limit().unwrap())
                .into();
        }) as fn(&mut Value),
        |object: &mut Value| {
            let limit = CreateReplyProfile::FreshCreate.body_limit().unwrap();
            object["payload"] = "".into();
            let base = serde_json::to_vec(&*object).unwrap().len();
            object["payload"] = "x".repeat(limit - base).into();
            assert_eq!(serde_json::to_vec(&*object).unwrap().len(), limit);
        },
        |object: &mut Value| {
            object["metadata"]["uid"] = "u".repeat(8192).into();
        },
    ] {
        let fixture = super::fixture::broker_fixture(mutation).await;
        fixture.release.notify_one();
        assert!(
            fixture
                .broker
                .create(
                    fixture.directory.clone(),
                    fixture.clock.clone(),
                    fixture.identity.clone(),
                    fixture.create.clone()
                )
                .await
                .is_err()
        );
        let original = fixture.stored.lock().unwrap().clone().unwrap();
        let state: awaken_sandbox_kubernetes::WorkerCreateLedgerState =
            awaken_sandbox_kubernetes::WorkerCreateLedgerState::decode(
                original["data"]["ledger"].as_str().unwrap(),
            )
            .unwrap();
        let ticket = validate_create(&fixture.identity, "sandbox-ns", fixture.create.clone())
            .unwrap()
            .ticket;
        assert_eq!(
            state.ticket(&ticket).unwrap().status,
            awaken_container_contract::TicketStatus::InFlight
        );
        assert!(
            fixture
                .broker
                .create(
                    fixture.directory.clone(),
                    fixture.clock.clone(),
                    fixture.identity.clone(),
                    fixture.create.clone()
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.pod_posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn finished_readback_overflow_preserves_receipt_and_legacy_read_scope() {
    // R8/R9/V6: capture is already Finished. A later large GET cannot
    // downgrade its exact result or create a second original object. For
    // explicit historical data, the same whole readback is preserved. The
    // fixture policy removal represents the old stored format only; there
    // is no production upgrade/downgrade command or manufactured UID.
    let fixture = super::fixture::broker_fixture(|_| {}).await;
    fixture.release.notify_one();
    fixture
        .broker
        .create(
            fixture.directory.clone(),
            fixture.clock.clone(),
            fixture.identity.clone(),
            fixture.create.clone(),
        )
        .await
        .unwrap();
    let before = fixture.stored.lock().unwrap().clone();
    fixture.created_pod.lock().unwrap().as_mut().unwrap()["payload"] = "x"
        .repeat(CreateReplyProfile::FreshCreate.body_limit().unwrap())
        .into();
    assert!(
        fixture
            .broker
            .create(
                fixture.directory.clone(),
                fixture.clock.clone(),
                fixture.identity.clone(),
                fixture.create.clone()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.stored.lock().unwrap().clone(), before);
    {
        let mut stored = fixture.stored.lock().unwrap();
        let encoded = stored.as_ref().unwrap()["data"]["ledger"].as_str().unwrap();
        let mut old: Value = serde_json::from_str(encoded).unwrap();
        old.as_object_mut().unwrap().remove("capture_policy");
        stored.as_mut().unwrap()["data"]["ledger"] = old.to_string().into();
    }
    let reply = fixture
        .broker
        .create(
            fixture.directory.clone(),
            fixture.clock.clone(),
            fixture.identity.clone(),
            fixture.create.clone(),
        )
        .await
        .unwrap();
    assert!(reply.body.len() > CreateReplyProfile::FreshCreate.body_limit().unwrap());
    assert_eq!(
        reply.into_http_response().headers()[K8S_CREATE_CAPTURE_HEADER],
        CreateReplyProfile::LegacyRetainedCreate.header_value()
    );
    assert_eq!(fixture.pod_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn original_readback_status_precedes_body_poll_without_changing_receipt() {
    // Cause/effect table at the original HTTP producer and real Kube Body.
    // C1 ticket Finished/InFlight; C2 GET status 200/404/409/503; C3 a ready
    // single-frame body carries the entire original object / invalid JSON.
    // R1 Finished+non200+invalid body => original refusal BEFORE any body
    // poll; original UID/Finished receipt stays unchanged; no POST/finish CAS.
    // R2 Finished+200+complete original object => poll the complete frame,
    // return every object byte/field and original UID; same no-mutation law.
    // R3 InFlight+any prepared response => no GET, no poll or second POST;
    // the original unknown outcome stays InFlight and is never finished.
    // Constraint: the body's first poll is immediately ready, so the frame
    // callback counts every possible first data poll, not a Pending shortcut.
    // This extends the sole existing API/client/ledger fixture, not a fake
    // classifier, status-only transport or new request/cleanup authority.
    for status in [200, 404, 409, 503, 0] {
        let in_flight = status == 0;
        let fixture = super::fixture::broker_fixture(if in_flight {
            |object: &mut Value| {
                object["payload"] = "x"
                    .repeat(CreateReplyProfile::FreshCreate.body_limit().unwrap())
                    .into();
            }
        } else {
            |_| {}
        })
        .await;
        fixture.release.notify_one();
        let first = fixture
            .broker
            .create(
                fixture.directory.clone(),
                fixture.clock.clone(),
                fixture.identity.clone(),
                fixture.create.clone(),
            )
            .await;
        assert_eq!(first.is_err(), in_flight, "initial original result");
        let before = fixture.stored.lock().unwrap().clone();
        let original = fixture.created_pod.lock().unwrap().clone().unwrap();
        let mut complete = original.clone();
        complete["readback_payload"] = "complete original response".repeat(64).into();
        let polls = Arc::new(AtomicUsize::new(0));
        *fixture.readback_response.lock().unwrap() = Some(super::fixture::ReadbackResponse {
            status: if in_flight { 503 } else { status },
            body: if status == 200 {
                serde_json::to_vec(&complete).unwrap()
            } else {
                b"an invalid body must never be polled".to_vec()
            },
            polls: polls.clone(),
        });
        let result = fixture
            .broker
            .create(
                fixture.directory.clone(),
                fixture.clock.clone(),
                fixture.identity.clone(),
                fixture.create.clone(),
            )
            .await;
        if status == 200 {
            let reply = result.unwrap();
            let decoded: K8sCreateResponse = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(decoded.status, 200, "R2 original readback");
            assert_eq!(decoded.object, complete, "R2 complete original object");
            assert_eq!(polls.load(Ordering::SeqCst), 1, "R2 complete ready frame");
        } else {
            assert!(result.is_err(), "R1/R3 status {status}");
            assert_eq!(polls.load(Ordering::SeqCst), 0, "R1/R3 status {status}");
        }
        assert_eq!(
            fixture.stored.lock().unwrap().clone(),
            before,
            "R1-R3 no finish CAS"
        );
        assert_eq!(
            fixture.pod_posts.load(Ordering::SeqCst),
            1,
            "R1-R3 no new POST"
        );
        assert_eq!(
            fixture.created_pod.lock().unwrap().as_ref().unwrap()["metadata"]["uid"],
            "pod-uid",
            "R1-R3 original physical UID"
        );
        assert_eq!(
            fixture.readback_response.lock().unwrap().is_some(),
            in_flight,
            "R3 InFlight does not even issue a readback GET"
        );
    }
}
