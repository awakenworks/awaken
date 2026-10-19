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
                Arc::new(ledger),
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

#[tokio::test]
async fn original_raw_request_build_refusal_precedes_the_one_service() {
    // P1: invalid URI -> RequestBuild with service0 for both actual backend
    // methods, unchanged physical ledger. Legal Broker POST paths are fixed
    // collections, so this phase is tested at the real producer rather than
    // weakening admission or adding a fake role to invent an unreachable RPC.
    // P2: local admitted-name syntax permits an internal space; the original
    // readback caller projects the same build phase to its original error.
    let f = super::fixture::broker_fixture(|_| {}).await;
    let calls = f.api_calls.load(Ordering::SeqCst);
    let before = f.stored.lock().unwrap().clone();
    assert_eq!(
        f.ledger
            .read_original("invalid uri\n", CreateReplyProfile::FreshCreate)
            .await,
        Err(RawCreateFailurePhase::RequestBuild)
    );
    assert_eq!(
        f.ledger
            .post_create(
                "invalid uri\n",
                b"{}".to_vec(),
                CreateReplyProfile::FreshCreate
            )
            .await,
        Err(RawCreateFailurePhase::RequestBuild)
    );
    let mut create = f.create.clone();
    create.object["metadata"]["name"] = "bad name".into();
    let validated = validate_create(&f.identity, "sandbox-ns", create).unwrap();
    let error = f
        .broker
        .read_original_result(
            &validated,
            &TicketResult::Created {
                uid: "original".into(),
            },
            "nonce",
            CreateReplyProfile::FreshCreate,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        awaken_session_execution_contract::ApplicationErrorKind::Internal
    );
    assert_eq!(error.message, "original Create readback path invalid");
    assert_eq!(f.api_calls.load(Ordering::SeqCst), calls);
    assert_eq!(*f.stored.lock().unwrap(), before);
}

#[tokio::test]
async fn raw_send_and_capture_failures_keep_original_caller_errors_and_receipts() {
    // P3 table: operation GET(after Finished)/POST(after original admission)
    // x actual service error/body>B/partial-data-then-body-error. Send and
    // Capture retain their distinct
    // original messages. GET cannot alter Finished; POST remains InFlight,
    // retry cannot issue a second raw POST. These are real Client/Body faults
    // on the original fixture, not a role fake or a second error classifier.
    for (readback, failure) in [
        (false, "send"),
        (false, "limit"),
        (false, "body"),
        (true, "send"),
        (true, "limit"),
        (true, "body"),
    ] {
        let capture = failure != "send";
        let polls = Arc::new(AtomicUsize::new(0));
        let f = super::fixture::broker_fixture(|_| {}).await;
        f.release.notify_one();
        if readback {
            f.broker
                .create(
                    f.directory.clone(),
                    f.clock.clone(),
                    f.identity.clone(),
                    f.create.clone(),
                )
                .await
                .unwrap();
        }
        let before = f.stored.lock().unwrap().clone();
        if capture {
            let response = super::fixture::ReadbackResponse {
                status: if readback { 200 } else { 403 },
                body: if failure == "limit" {
                    vec![b'x'; CreateReplyProfile::FreshCreate.body_limit().unwrap() + 1]
                } else {
                    b"partial original bytes".to_vec()
                },
                polls: polls.clone(),
            };
            if failure == "body" {
                *f.capture_error.lock().unwrap() =
                    Some(if readback { Method::GET } else { Method::POST });
            }
            if readback {
                *f.readback_response.lock().unwrap() = Some(response);
            } else {
                *f.post_response.lock().unwrap() = Some(response);
            }
        } else {
            *f.fail_send.lock().unwrap() = Some(if readback { Method::GET } else { Method::POST });
        }
        let result = f
            .broker
            .create(
                f.directory.clone(),
                f.clock.clone(),
                f.identity.clone(),
                f.create.clone(),
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("P3 fault was accepted"),
        };
        assert_eq!(
            error.kind,
            awaken_session_execution_contract::ApplicationErrorKind::Unavailable
        );
        assert_eq!(
            error.message,
            match (readback, capture) {
                (true, false) => "original Create API readback unavailable",
                (true, true) => "original Create API readback incomplete or outside capture policy",
                (false, false) => "Worker Create API outcome is unknown",
                (false, true) =>
                    "Worker Create API response is incomplete or outside capture policy",
            }
        );
        if failure == "body" {
            assert_eq!(
                polls.load(Ordering::SeqCst),
                1,
                "P3 partial data before original body failure"
            );
        }
        if readback {
            assert_eq!(
                *f.stored.lock().unwrap(),
                before,
                "P3 Finished receipt unchanged"
            );
        } else {
            let state = super::fixture::ledger_state(&f.stored);
            assert!(
                state["tickets"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|ticket| ticket["status"] == "in_flight")
            );
            assert!(
                f.broker
                    .create(
                        f.directory.clone(),
                        f.clock.clone(),
                        f.identity.clone(),
                        f.create.clone()
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            f.pod_posts.load(Ordering::SeqCst),
            1,
            "P3 no additional POST"
        );
    }
}

#[tokio::test]
async fn every_post_status_captures_whole_bytes_before_definitive_result_selection() {
    // P4 table: definitive400/401/403/409/422 vs nondefinitive202/500,
    // complete ready body -> every status polls the same original body fully.
    // Only the original TicketResult owner admits the five refusal codes;
    // other statuses remain InFlight, no second POST. P3's403 oversized body
    // separately proves a refusal status cannot bypass capture or finish early.
    // Fresh201/UID and raw/outward/result overflow remain in the original cases.
    for status in [400, 401, 403, 409, 422, 202, 500] {
        let f = super::fixture::broker_fixture(|_| {}).await;
        let object =
            json!({"kind":"Status","code":status,"message":"complete original failure".repeat(64)});
        let polls = Arc::new(AtomicUsize::new(0));
        *f.post_response.lock().unwrap() = Some(super::fixture::ReadbackResponse {
            status,
            body: serde_json::to_vec(&object).unwrap(),
            polls: polls.clone(),
        });
        f.release.notify_one();
        let result = f
            .broker
            .create(
                f.directory.clone(),
                f.clock.clone(),
                f.identity.clone(),
                f.create.clone(),
            )
            .await;
        let definitive = matches!(status, 400 | 401 | 403 | 409 | 422);
        let state = super::fixture::ledger_state(&f.stored);
        if definitive {
            let reply = result.unwrap();
            let decoded: K8sCreateResponse = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(decoded.status, status);
            assert_eq!(decoded.object, object);
            let request = validate_create(&f.identity, "sandbox-ns", f.create.clone())
                .unwrap()
                .ticket;
            let retained = awaken_sandbox_kubernetes::WorkerCreateLedgerState::decode(
                f.stored.lock().unwrap().as_ref().unwrap()["data"]["ledger"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                retained.ticket(&request).unwrap().status,
                awaken_container_contract::TicketStatus::Finished(TicketResult::RejectedNoEffect {
                    code: status
                })
            );
        } else {
            assert!(result.is_err());
            assert!(
                state["tickets"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|ticket| ticket["status"] == "in_flight")
            );
            assert!(
                f.broker
                    .create(
                        f.directory.clone(),
                        f.clock.clone(),
                        f.identity.clone(),
                        f.create.clone()
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "P4 whole ready frame for {status}"
        );
        assert_eq!(
            f.pod_posts.load(Ordering::SeqCst),
            1,
            "P4 single POST for {status}"
        );
    }
}

#[tokio::test]
async fn registry_changes_across_original_io_keep_both_authority_rechecks() {
    // P5 table: close the real Registry while original admission PUT response
    // is pending -> detached executor rechecks before POST, fully encodes403,
    // then finishes refusal. P6 close while original Finished GET response is
    // pending -> decode original bytes but refuse after await; no new POST or
    // finish CAS. Uses the original tower Body and MemoryWorkerDirectory; no
    // alternate Registry/role implementation, wall-clock sleep or API authority.
    for readback in [false, true] {
        let f = super::fixture::broker_fixture(|_| {}).await;
        if readback {
            f.release.notify_one();
            f.broker
                .create(
                    f.directory.clone(),
                    f.clock.clone(),
                    f.identity.clone(),
                    f.create.clone(),
                )
                .await
                .unwrap();
        }
        let before = f.stored.lock().unwrap().clone();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let path = if readback {
            "/api/v1/namespaces/sandbox-ns/pods/sandbox-pod".to_owned()
        } else {
            format!(
                "/api/v1/namespaces/control-ns/configmaps/{}",
                before.as_ref().unwrap()["metadata"]["name"]
                    .as_str()
                    .unwrap()
            )
        };
        *f.response_pause.lock().unwrap() = Some(super::fixture::ResponsePause {
            method: if readback { Method::GET } else { Method::PUT },
            path,
            entered: entered.clone(),
            release: release.clone(),
        });
        let (broker, directory, clock, identity, create) = (
            f.broker.clone(),
            f.directory.clone(),
            f.clock.clone(),
            f.identity.clone(),
            f.create.clone(),
        );
        let task =
            tokio::spawn(async move { broker.create(directory, clock, identity, create).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        f.directory.deregister(&f.identity).await.unwrap();
        release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        if readback {
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("P6 authority change accepted"),
            };
            assert_eq!(
                error.message,
                "original Create Worker authority changed during readback"
            );
            assert_eq!(*f.stored.lock().unwrap(), before, "P6 no finish CAS");
            assert_eq!(f.pod_posts.load(Ordering::SeqCst), 1, "P6 no new POST");
        } else {
            let reply = result.unwrap();
            let decoded: K8sCreateResponse = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(decoded.status, 403, "P5 original complete refusal");
            let request = validate_create(&f.identity, "sandbox-ns", f.create.clone())
                .unwrap()
                .ticket;
            let retained = awaken_sandbox_kubernetes::WorkerCreateLedgerState::decode(
                f.stored.lock().unwrap().as_ref().unwrap()["data"]["ledger"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                retained.ticket(&request).unwrap().status,
                awaken_container_contract::TicketStatus::Finished(TicketResult::RejectedNoEffect {
                    code: 403
                })
            );
            assert_eq!(
                f.pod_posts.load(Ordering::SeqCst),
                0,
                "P5 no POST after authority closes"
            );
        }
    }
}
