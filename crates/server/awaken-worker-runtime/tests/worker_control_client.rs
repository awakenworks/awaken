use awaken_run_ingress_contract::{WorkerHeartbeat, WorkerIdentity, WorkerManifest};
use awaken_worker_runtime::WorkerControlClient;
use awaken_worker_transport_security::WorkerUpstream;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::Arc;

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

#[tokio::test]
async fn authoritative_lease_receipts_fail_closed_when_ttl_is_not_positive() {
    // Cause/effect decision table for Coordinator timing receipts:
    //
    // | Rule | operation | mutation | TTL | effect |
    // |---|---|---|---|---|
    // | T1 | register | n/a | zero | reject; no local timing is invented |
    // | T2 | heartbeat | Applied | missing | reject; no ownership proof |
    // | T3 | heartbeat | non-Applied | missing | accepted as explicit loss (covered by lifecycle) |
    let base = spawn_server().await;
    let bootstrap = WorkerUpstream::new(base.clone()).with_worker_id("worker-a");
    let registration = WorkerControlClient::new(bootstrap)
        .register_classified("inc-a", WorkerManifest::default())
        .await
        .expect_err("T1 zero TTL fails closed");
    assert!(registration.to_string().contains("zero lease TTL"), "T1");

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
                &awaken_session_contract::SessionRealizationLease {
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
