use awaken_run_ingress_contract::{WorkerHeartbeat, WorkerIdentity, WorkerManifest};
use awaken_worker_runtime::WorkerControlClient;
use awaken_worker_transport_security::WorkerUpstream;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

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
