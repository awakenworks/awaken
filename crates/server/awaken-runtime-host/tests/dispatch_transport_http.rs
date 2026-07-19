//! The worker-facing dispatch transport over its real HTTP surface: a database-less
//! worker's `enqueue` → `claim` → `renew` → `settle` round-trip drives the same
//! process-shared dispatch store the co-located pool drains. Its own test binary: it
//! installs a one-shot injected dispatch store, so it must not share a process with
//! other dispatch tests.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{DispatchQueue, MemoryDispatchStore, RunDispatch};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock, WorkerDispatchService,
    dispatch_transport_router_with_service,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn activation(run: &str, thread: &str) -> RunActivation {
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: ModelBinding::new("prov", "model", "acp:test"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        vec![Message::text(MessageId("u1".into()), Role::User, "go")],
    )
}

async fn post(router: &Router, worker: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", worker)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_db_less_worker_claims_renews_and_settles_over_http() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let clock = Arc::new(ManualWorkerClock::new(0));
    let router = dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(
        mem as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )));

    let unauthorized = Request::builder()
        .method("POST")
        .uri("/v1/worker/dispatch/claim")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let unauthorized = router.clone().oneshot(unauthorized).await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    // enqueue a run over the transport.
    let request = serde_json::to_value(RunDispatch::new(activation("run-A", "t1"))).unwrap();
    let (s, _) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/enqueue",
        json!({ "request": request }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // claim it: the self-contained request comes back with a lease.
    let (s, v) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["claimed"]["request"]["activation"]["run_id"], "run-A",
        "claim returns the enqueued run over the wire: {v}"
    );
    assert_eq!(
        v["claimed"]["lease"]["owner"], "worker-1",
        "the lease is owned: {v}"
    );
    // Capture the fence epoch the claim assigned — the settle must carry it.
    let epoch = v["claimed"]["lease"]["epoch"]
        .as_u64()
        .expect("claim returns a lease epoch");
    assert_eq!(epoch, 1, "the first claim assigns fence epoch 1: {v}");

    // renew the lease: still owned → true.
    clock.set(1_000);
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/renew",
        json!({ "run_id": "run-A" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["renewed"], true, "the owner renews its live lease: {v}");

    // a claim by a second worker finds nothing runnable (single owner per run).
    let (_, v) = post(&router, "worker-2", "/v1/worker/dispatch/claim", json!({})).await;
    assert!(
        v["claimed"].is_null(),
        "a leased run is not double-claimed: {v}"
    );

    // a settle carrying a non-current epoch is fenced over the wire — nothing
    // changes (a stale owner past its lease cannot settle behind a reclaimer).
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": 99, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["settled"], false,
        "a non-current-epoch settle is fenced, not applied: {v}"
    );

    let (s, v) = post(
        &router,
        "worker-2",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": epoch, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["settled"], false,
        "an authenticated non-owner cannot settle a current epoch: {v}"
    );

    // settle Done under the current epoch: the dispatch is finished and removed.
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": epoch, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["settled"], true, "the run settles Done: {v}");

    // nothing left to claim.
    let (_, v) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert!(v["claimed"].is_null(), "a settled run is gone: {v}");
}
