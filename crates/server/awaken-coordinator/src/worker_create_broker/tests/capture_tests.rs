use super::*;
use awaken_sandbox_kubernetes::{CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER};

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
                json!({"identity":identity,"scope":"scope","predecessor":ContainerEffectFence::new("old","worker","boot",1,u64::MAX).unwrap(),"expected_pod_name":"awaken-scope"})
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
            awaken_sandbox_container::TicketStatus::InFlight
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
