use awaken_dispatch_contract::{WorkerHeartbeat, WorkerIdentity, WorkerManifest};
use awaken_coordinator_client::{WorkerControlClient, WorkerRegistrationError};
use awaken_worker_transport_security::WorkerUpstream;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn worker_control_rpc_continues_trace_without_exporting_payload() {
    // Causes: C1 tracing enabled/disabled; C2 a sampled admitting context;
    // C3 an authenticated control RPC with identity/payload. R1 C1+C2 ->
    // propagate the same W3C trace over the actual HTTP boundary; R2 !C1 ->
    // same RPC result, no trace dependency; both rows export no request body.
    // Subscriber/meter globals are isolated in a child of this test binary.
    const CHILD: &str = "AWAKEN_CONTROL_TRACE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for enabled in ["enabled", "disabled"] {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "worker_control_rpc_continues_trace_without_exporting_payload",
                    "--nocapture",
                ])
                .env(CHILD, enabled)
                .env(
                    "AWAKEN_CONTROL_TRACE_TEST_FILE",
                    directory.path().join("trace.jsonl"),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{enabled}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let enabled = std::env::var(CHILD).unwrap() == "enabled";
    let trace_file =
        std::path::PathBuf::from(std::env::var_os("AWAKEN_CONTROL_TRACE_TEST_FILE").unwrap());
    let config = awaken_observability::ObservabilityConfig {
        trace_file: enabled.then(|| trace_file.clone()),
        ..Default::default()
    };
    awaken_observability::init(&config);
    let (header, observed) = tokio::sync::oneshot::channel();
    let header = Arc::new(std::sync::Mutex::new(Some(header)));
    let app = Router::new().route(
        "/v1/worker/register",
        post(move |headers: axum::http::HeaderMap| {
            let header = header.clone();
            async move {
                header
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(
                        headers
                            .get("traceparent")
                            .map(|value| value.to_str().unwrap().to_owned()),
                    )
                    .unwrap();
                register_zero_ttl().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let span = awaken_observability::span_with_remote_parent(
        tracing::info_span!(parent: None, "test.admitting"),
        Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
    );
    use tracing::Instrument as _;
    WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-a"),
    )
    .register_classified("private-payload-marker", WorkerManifest::default())
    .instrument(span)
    .await
    .unwrap();
    let carrier = observed.await.unwrap();
    server.abort();
    awaken_observability::shutdown();
    if enabled {
        assert!(
            carrier
                .unwrap()
                .contains("0af7651916cd43dd8448eb211c80319c"),
            "R1 missing RPC trace continuity"
        );
        let exported = std::fs::read_to_string(trace_file).unwrap();
        assert!(
            !exported.contains("private-payload-marker"),
            "request bodies never enter diagnostics"
        );
    } else {
        assert!(carrier.is_none(), "R2 unconfigured tracing");
        assert!(!trace_file.exists(), "R2 no exporter effect");
    }
}

async fn register_zero_ttl() -> Json<Value> {
    Json(json!({
        "worker": {
            "snapshot": {
                "identity": {"worker_id": "worker-a", "incarnation_id": "inc-a", "generation": 1},
                "state": "starting",
                "manifest": WorkerManifest::default(),
                "capability_fingerprint": "fixture",
                "in_flight": 0,
                "expires_at_ms": 1
            },
            "heartbeat_sequence": 0,
            "observation_sequence": 0,
            "registered_at_ms": 0,
            "heartbeat_at_ms": 0,
            "drain_deadline_ms": null
        },
        "lease_ttl_ms": 0
    }))
}

async fn applied_without_ttl() -> Json<Value> {
    Json(json!({"mutation": "applied"}))
}

async fn spawn_server() -> String {
    let app = Router::new()
        .route("/v1/worker/register", post(register_zero_ttl))
        .route("/v1/worker/heartbeat", post(applied_without_ttl));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve fixture") });
    format!("http://{address}")
}

async fn spawn_registration_status(status: axum::http::StatusCode) -> String {
    let app = Router::new().route(
        "/v1/worker/register",
        post(move || async move { (status, Json(json!({ "error": "fixture rejection" }))) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind registration fixture");
    let address = listener.local_addr().expect("fixture address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve fixture") });
    format!("http://{address}")
}

#[tokio::test]
async fn registration_retries_only_unknown_or_temporarily_unavailable_admission() {
    // Cause/effect decision table for startup admission:
    // C1 no TCP listener; C2 Coordinator 503; C3 Coordinator 429; C4 explicit
    // 400; C5 malformed local endpoint. E1 C1-C3 preserve the same Worker
    // incarnation for retry; E2 C4-C5 terminate startup without retrying an
    // invalid request. Registry 409 remains the existing SlotOccupied branch.
    let register =
        |base| WorkerControlClient::new(WorkerUpstream::new(base).with_worker_id("worker-a"));

    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve refused coordinate")
        .local_addr()
        .unwrap();
    assert!(matches!(
        register(format!("http://{unavailable}"))
            .register_classified("inc-a", WorkerManifest::default())
            .await,
        Err(WorkerRegistrationError::Unavailable(_))
    ));

    for status in [
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        axum::http::StatusCode::TOO_MANY_REQUESTS,
    ] {
        assert!(matches!(
            register(spawn_registration_status(status).await)
                .register_classified("inc-a", WorkerManifest::default())
                .await,
            Err(WorkerRegistrationError::Unavailable(_))
        ));
    }
    assert!(matches!(
        register(spawn_registration_status(axum::http::StatusCode::BAD_REQUEST).await)
            .register_classified("inc-a", WorkerManifest::default())
            .await,
        Err(WorkerRegistrationError::Rejected(_))
    ));
    assert!(matches!(
        register("://invalid".to_owned())
            .register_classified("inc-a", WorkerManifest::default())
            .await,
        Err(WorkerRegistrationError::Rejected(_))
    ));
}

#[tokio::test]
async fn registration_facts_survive_invalid_ttl_but_heartbeat_grants_require_positive_ttl() {
    // Cause/effect decision table for Coordinator timing receipts:
    //
    // | Rule | operation | mutation | TTL | effect |
    // |---|---|---|---|---|
    // | T1 | register | n/a | zero | preserve identity for Node withdrawal; no invented timing |
    // | T2 | heartbeat | Applied | missing | reject; no ownership proof |
    // | T3 | heartbeat | non-Applied | missing | accepted as explicit loss (covered by lifecycle) |
    let base = spawn_server().await;
    let bootstrap = WorkerUpstream::new(base.clone()).with_worker_id("worker-a");
    let registration = WorkerControlClient::new(bootstrap)
        .register_classified("inc-a", WorkerManifest::default())
        .await
        .expect("T1 retain registration fact, not execution permission");
    assert_eq!(registration.lease_ttl_ms, 0, "T1 no fabricated horizon");
    assert_eq!(
        registration.runtime_protocol_version, None,
        "T1 no fabricated protocol"
    );
    assert_eq!(
        registration.worker.snapshot.identity,
        WorkerIdentity::new("worker-a", "inc-a", 1),
        "T1 exact cleanup identity"
    );
    // The real WorkerNode lifecycle matrix proves zero TTL/unknown protocol
    // stops before every factory/probe/Ready/claim and withdraws exactly once.

    let identity = WorkerIdentity::new("worker-a", "inc-a", 1);
    let worker = WorkerUpstream::new(base)
        .with_worker_id("worker-a")
        .with_worker_identity(identity.clone());
    let heartbeat = WorkerControlClient::new(worker)
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .expect_err("T2 missing applied TTL fails closed");
    assert!(heartbeat.contains("positive lease TTL"), "T2");
}

#[derive(Default)]
struct ConcurrentControlState {
    cleanup_started: tokio::sync::Notify,
    cleanup_release: tokio::sync::Notify,
    heartbeats: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn session_renewal_receipt_wire_requires_positive_horizon_and_preserves_failures() {
    use awaken_session_realization_contract::{
        RenewSessionRealization, SessionRealizationControlFailure as Failure,
    };
    use awaken_session_resource_contract::SessionRealizationLease;
    use axum::http::StatusCode;

    // Cause/effect graph: HTTP success AND complete lease AND positive integer
    // horizon -> decode the exact authority response; any missing/invalid
    // horizon or lease -> Unavailable, never infer a TTL from local wall time.
    // HTTP failure -> preserve the typed Control disposition, never grant.
    // Decision rules: R1 positive horizon (1/max included) -> exact receipt;
    // R2 missing/legacy, R3 zero, R4 null, R5 negative, R6 string/fraction,
    // R7 absent lease -> no receipt; R8 typed failure -> exact failure.
    // Constraint: this is the client wire boundary, not local admission. The
    // deliberately ancient absolute expiry is opaque here; only the Runtime
    // request-start observation can assess response latency and live ownership.
    let lease = SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "inc-a".into(),
        epoch: 7,
        expires_at_unix_ms: 100,
    };
    let reply = Arc::new(std::sync::Mutex::new((StatusCode::OK, Value::Null)));
    let expected_request = RenewSessionRealization {
        session_id: "session-a".into(),
        asserted_lease: lease.clone(),
        requested_expires_at_unix_ms: 200,
    };
    let command = expected_request.clone();
    let app = Router::new()
        .route(
            "/v1/worker/session/realization/renew",
            post(
                move |State(reply): State<Arc<std::sync::Mutex<(StatusCode, Value)>>>,
                      Json(body): Json<Value>| {
                    let command = command.clone();
                    async move {
                        assert_eq!(body["command"], json!(command));
                        assert_eq!(body["identity"]["worker_id"], "worker-a");
                        let (status, body) = reply.lock().unwrap().clone();
                        (status, Json(body))
                    }
                },
            ),
        )
        .with_state(reply.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let identity = WorkerIdentity::new("worker-a", "inc-a", 1);
    let client = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}"))
            .with_worker_id("worker-a")
            .with_worker_identity(identity.clone()),
    );

    for horizon in [1, 30_000, u64::MAX] {
        *reply.lock().unwrap() = (
            StatusCode::OK,
            json!({"lease": lease, "remaining_ms": horizon}),
        );
        let receipt = client
            .renew_session_realization(&identity, expected_request.clone())
            .await
            .unwrap();
        assert_eq!(receipt.lease, lease, "R1/{horizon}");
        assert_eq!(receipt.remaining_ms.get(), horizon, "R1/{horizon}");
    }
    for (rule, body) in [
        ("R2 legacy", json!({"lease": lease})),
        ("R3 zero", json!({"lease": lease, "remaining_ms": 0})),
        ("R4 null", json!({"lease": lease, "remaining_ms": null})),
        ("R5 negative", json!({"lease": lease, "remaining_ms": -1})),
        ("R6 string", json!({"lease": lease, "remaining_ms": "30"})),
        ("R6 fraction", json!({"lease": lease, "remaining_ms": 0.5})),
        ("R7 no lease", json!({"remaining_ms": 30})),
    ] {
        *reply.lock().unwrap() = (StatusCode::OK, body);
        let error = client
            .renew_session_realization(&identity, expected_request.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Failure::Unavailable(_)),
            "{rule}: {error:?}"
        );
    }
    for failure in [
        Failure::StaleOwnership,
        Failure::Retired,
        Failure::NotReady,
        Failure::Unavailable("fixture".into()),
    ] {
        *reply.lock().unwrap() = (StatusCode::CONFLICT, json!({"realization_error": failure}));
        assert_eq!(
            client
                .renew_session_realization(&identity, expected_request.clone())
                .await
                .unwrap_err(),
            failure,
            "R8"
        );
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

async fn blocked_cleanup_poll(State(state): State<Arc<ConcurrentControlState>>) -> Json<Value> {
    state.cleanup_started.notify_one();
    state.cleanup_release.notified().await;
    Json(json!({"work": null}))
}

async fn concurrent_heartbeat(State(state): State<Arc<ConcurrentControlState>>) -> Json<Value> {
    state
        .heartbeats
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(json!({"mutation": "applied", "lease_ttl_ms": 30_000}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_session_control_request_does_not_serialize_worker_heartbeat() {
    /* Shared-transport cause/effect table: C1 one Session cleanup request is
     * accepted by the server but never responds; C2 a heartbeat uses a clone of
     * the exact same identity-bound WorkerUpstream client. Effect E1 heartbeat
     * completes while C1 remains blocked; E2 releasing C1 completes the
     * original request. This proves the transport has no process-local serial
     * request mutex; durable Worker and Session authorities remain distinct.
     *
     * | Rule | cleanup | heartbeat | Effect |
     * |---|---|---|---|
     * | H1 | blocked | concurrent | E1, then E2 |
     */
    let state = Arc::new(ConcurrentControlState::default());
    let app = Router::new()
        .route(
            "/v1/worker/session/cleanup/poll",
            post(blocked_cleanup_poll),
        )
        .route("/v1/worker/heartbeat", post(concurrent_heartbeat))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("H1 bind fixture");
    let address = listener.local_addr().expect("H1 fixture address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("H1 serve fixture") });

    let identity = WorkerIdentity::new("worker-a", "inc-a", 1);
    let upstream = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("worker-a")
        .with_worker_identity(identity.clone());
    let control = WorkerControlClient::new(upstream);
    let cleanup_control = control.clone();
    let cleanup_identity = identity.clone();
    let cleanup = tokio::spawn(async move {
        cleanup_control
            .terminal_cleanup_work(
                &cleanup_identity,
                "session-a",
                &awaken_session_resource_contract::SessionRealizationLease {
                    owner: "worker-a".into(),
                    runtime_incarnation: "inc-a".into(),
                    epoch: 1,
                    expires_at_unix_ms: 30_000,
                },
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        state.cleanup_started.notified(),
    )
    .await
    .expect("H1 cleanup entered server");

    let receipt = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        control.heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        ),
    )
    .await
    .expect("H1/E1 heartbeat is independently scheduled")
    .expect("H1/E1 applied heartbeat");
    assert_eq!(receipt.lease_ttl_ms, Some(30_000), "H1/E1");
    assert_eq!(
        state.heartbeats.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "H1/E1"
    );
    assert!(!cleanup.is_finished(), "H1 cleanup remains blocked");

    state.cleanup_release.notify_one();
    assert_eq!(
        cleanup.await.expect("H1 cleanup task").expect("H1/E2"),
        None,
        "H1/E2"
    );
}
