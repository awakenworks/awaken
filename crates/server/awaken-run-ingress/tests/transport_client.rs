//! The database-less worker's HTTP dispatch client (`HttpDispatchQueue`) over a real
//! socket.
//!
//! `transport_client.rs` is otherwise an entirely untested public module. These
//! tests drive the real client's claim/settle verbs across a real TCP port
//! (ephemeral) against a live `axum` server that mirrors a cell server's
//! `dispatch_transport_router`, backed by the production `MemoryDispatchStore`.
//! No mock transport — the seam under test is the client's reqwest encode/decode
//! against the wire the server actually serves.
//!
//! (The transport router itself lives in `awaken-runtime-host`, which depends on
//! this crate; to avoid a dev-dependency cycle and the router's process-global
//! store injection, the test mirrors the same six routes over the same production
//! in-memory store — the client, the socket, and the store are all real.)

mod harness;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    DispatchError, DispatchOutcome, DispatchQueue, HttpDispatchQueue, Inbox, InferenceAccess,
    MemoryDispatchStore, Outbox, PendingInput, RunClaim, RunDispatch, SettleOutcome, SubmitOptions,
};
use awaken_runtime_contract::resume::ResumeResult;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use harness::activation;

/// Stand up a live server mirroring a cell server's `dispatch_transport_router`
/// over a shared [`MemoryDispatchStore`], bound to an ephemeral port. Returns the
/// base URL a `HttpDispatchQueue` points at plus the shared store handle, so a test
/// can assert server-side state directly.
struct TransportState {
    store: Arc<MemoryDispatchStore>,
    now_ms: Arc<AtomicU64>,
}

async fn spawn_transport_server() -> (String, Arc<MemoryDispatchStore>, Arc<AtomicU64>) {
    let store = Arc::new(MemoryDispatchStore::new());
    let now_ms = Arc::new(AtomicU64::new(0));
    let state = Arc::new(TransportState {
        store: store.clone(),
        now_ms: now_ms.clone(),
    });
    let app = Router::new()
        .route("/v1/worker/dispatch/enqueue", post(enqueue))
        .route("/v1/worker/dispatch/claim", post(claim))
        .route("/v1/worker/dispatch/claim_run", post(claim_run))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/renew_owned", post(renew_owned))
        .route("/v1/worker/dispatch/bind_sandbox", post(bind_sandbox))
        .route("/v1/worker/dispatch/settle", post(settle))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), store, now_ms)
}

fn run_id(v: &Value) -> RunId {
    RunId(v["run_id"].as_str().expect("run_id").to_string())
}

fn worker_id(headers: &HeaderMap) -> &str {
    headers
        .get("x-awaken-worker-id")
        .and_then(|value| value.to_str().ok())
        .expect("authenticated worker header")
}

fn assert_server_authority_fields_absent(request: &Value) {
    for field in ["owner", "lease_ms", "now_ms"] {
        assert!(
            request.get(field).is_none(),
            "client must not send server-authoritative field {field}"
        );
    }
}

async fn enqueue(State(state): State<Arc<TransportState>>, Json(req): Json<Value>) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let request: RunDispatch = serde_json::from_value(req["request"].clone()).expect("request");
    let options: SubmitOptions =
        serde_json::from_value(req.get("options").cloned().unwrap_or(Value::Null))
            .unwrap_or_default();
    state
        .store
        .enqueue_with(request, options)
        .await
        .expect("enqueue");
    Json(json!({ "enqueued": true }))
}

async fn claim(
    State(state): State<Arc<TransportState>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let claimed = state
        .store
        .claim(
            worker_id(&headers),
            1_000,
            state.now_ms.load(Ordering::SeqCst),
        )
        .await
        .expect("claim");
    Json(json!({ "claimed": claimed }))
}

async fn claim_run(
    State(state): State<Arc<TransportState>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let claimed = state
        .store
        .claim_run(
            &run_id(&req),
            worker_id(&headers),
            1_000,
            state.now_ms.load(Ordering::SeqCst),
        )
        .await
        .expect("claim_run");
    Json(json!({ "claimed": claimed }))
}

async fn renew(
    State(state): State<Arc<TransportState>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let renewed = state
        .store
        .renew_lease(
            &run_id(&req),
            worker_id(&headers),
            1_000,
            state.now_ms.load(Ordering::SeqCst),
        )
        .await
        .expect("renew");
    Json(json!({ "renewed": renewed }))
}

async fn renew_owned(
    State(state): State<Arc<TransportState>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let renewed = state
        .store
        .renew_owned_leases(
            worker_id(&headers),
            1_000,
            state.now_ms.load(Ordering::SeqCst),
        )
        .await
        .expect("renew_owned");
    Json(json!({ "renewed": renewed }))
}

async fn settle(
    State(state): State<Arc<TransportState>>,
    _headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let outcome: DispatchOutcome = serde_json::from_value(req["outcome"].clone()).expect("outcome");
    let consumed: Vec<String> =
        serde_json::from_value(req.get("consumed").cloned().unwrap_or(json!([])))
            .unwrap_or_default();
    let outcome = state
        .store
        .settle(
            &run_id(&req),
            req["epoch"].as_u64().unwrap_or(0),
            outcome,
            &consumed,
        )
        .await
        .expect("settle");
    Json(json!({ "settled": outcome.applied() }))
}

async fn bind_sandbox(
    State(state): State<Arc<TransportState>>,
    _headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let claim: awaken_run_ingress::RunClaim =
        serde_json::from_value(req["claim"].clone()).expect("claim");
    let outcome = state
        .store
        .bind_sandbox(&claim, req["sandbox_ref"].as_str().expect("sandbox_ref"))
        .await
        .expect("bind sandbox");
    Json(json!({ "applied": outcome.applied() }))
}

// ── 1. The db-less remote-worker seam: enqueue → claim → fence → settle ──────────

#[tokio::test]
async fn worker_claims_and_settles_a_run_over_a_real_dispatch_transport() {
    let (base, store, clock) = spawn_transport_server().await;
    let queue = HttpDispatchQueue::new(base);
    let run = RunId("run-1".into());

    // Enqueue a run over the wire; the server-side store records it.
    let mut activation = activation("run-1");
    activation.snapshot.metadata.inference_access = Some(InferenceAccess::new(
        "credential-reference/v1",
        "grant-http",
    ));
    queue
        .enqueue(RunDispatch::new(activation))
        .await
        .expect("enqueue over transport");
    assert_eq!(
        store.dispatch_count(),
        1,
        "the server store recorded the enqueue"
    );

    // Claim it over the wire → a `Claimed` carrying the self-contained request and a
    // fresh lease (epoch 1) — the whole payload survived the JSON round-trip.
    clock.store(0, Ordering::SeqCst);
    let claimed = queue
        .claim("worker-A", 1_000, 0)
        .await
        .expect("claim ok")
        .expect("a runnable dispatch");
    assert_eq!(claimed.request.run_id(), &run);
    assert_eq!(
        claimed
            .request
            .activation
            .snapshot
            .metadata
            .inference_access,
        Some(InferenceAccess::new(
            "credential-reference/v1",
            "grant-http"
        )),
        "the opaque grant survives the worker HTTP boundary"
    );
    assert_eq!(claimed.lease.owner, "worker-A");
    assert_eq!(claimed.lease.epoch, 1);
    assert!(
        queue
            .bind_sandbox(&RunClaim::from(&claimed.lease), "sandbox-http")
            .await
            .expect("bind sandbox over transport")
            .applied()
    );

    // A second claim finds nothing runnable (the only run is now leased): `None`.
    clock.store(10, Ordering::SeqCst);
    assert!(
        queue
            .claim("worker-A", 1_000, 10)
            .await
            .expect("claim")
            .is_none()
    );

    // A stale-epoch settle is fenced server-side — nothing changes.
    assert_eq!(
        queue
            .settle(&run, 99, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced
    );
    assert_eq!(
        store.dispatch_count(),
        1,
        "a fenced settle left the dispatch intact"
    );

    // The current-epoch settle applies: `Done` removes the dispatch.
    assert_eq!(
        queue
            .settle(&run, claimed.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the applied Done settle removed the dispatch"
    );
}

// ── 3. renew_lease FALSE path over the wire (lease stolen by recovery) ───────────

#[tokio::test]
async fn renew_lease_returns_false_over_the_wire_when_the_lease_was_stolen() {
    let (base, store, clock) = spawn_transport_server().await;
    let queue = HttpDispatchQueue::new(base);
    let run = RunId("run-1".into());

    queue
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Worker A claims at t=0 with a 1s lease (epoch 1).
    clock.store(0, Ordering::SeqCst);
    let a = queue
        .claim("worker-A", 1_000, 0)
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1);
    // While A still holds it, a renew succeeds — the TRUE path, as a control.
    clock.store(100, Ordering::SeqCst);
    assert!(
        queue
            .renew_lease(&run, "worker-A", 1_000, 100)
            .await
            .unwrap(),
        "the current owner renews its own live lease"
    );

    // The lease lapses; worker B recovers the run at t=2s (epoch 2 — B now owns it).
    clock.store(2_000, Ordering::SeqCst);
    let b = queue
        .claim("worker-B", 1_000, 2_000)
        .await
        .unwrap()
        .expect("B recovers the lapsed lease");
    assert_eq!(b.lease.epoch, 2, "recovery bumped the fence epoch");
    assert_eq!(
        store.dispatch_count(),
        1,
        "recovery re-owns the same row, not a second"
    );

    // A's renew now returns FALSE over the wire — the lease was stolen; the stale
    // holder must stop (the whole point of the multi-node liveness knob).
    clock.store(2_100, Ordering::SeqCst);
    assert!(
        !queue
            .renew_lease(&run, "worker-A", 1_000, 2_100)
            .await
            .unwrap(),
        "a stale owner's renew fails so it abandons the run"
    );
    // And A's settle at its old epoch is fenced, leaving B's in-flight run inviolate.
    assert_eq!(
        queue
            .settle(&run, a.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced
    );
    assert_eq!(store.dispatch_count(), 1);
}

// ── 2. Fail-closed server-local stubs (no server: these resolve on the client) ───

fn an_input() -> PendingInput {
    PendingInput {
        message_id: "m1".into(),
        run_id: RunId("run-1".into()),
        thread_id: ThreadId("thread-1".into()),
        correlation_id: "corr-1".into(),
        available_at_ms: None,
        result: ResumeResult::allow(),
    }
}

#[tokio::test]
async fn server_local_write_verbs_fail_closed() {
    // No server is needed: these verbs resolve locally on the client without a
    // request, so a bad base URL is never dialed.
    let queue = HttpDispatchQueue::new("http://127.0.0.1:1");
    let run = RunId("run-1".into());

    // The verbs a database-less worker must never legitimately drive — server-owned
    // recovery/cancel and the write side of the inbox/outbox — reject (fail closed):
    // they never pretend a mutation the server didn't perform.
    assert!(matches!(
        queue.requeue(&run).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.cancel(&run).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        Inbox::append(&queue, an_input()).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        Inbox::retract(&queue, "m1", 1).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        Inbox::edit(&queue, "m1", 1, ResumeResult::allow()).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        Outbox::stage(&queue, an_input()).await,
        Err(DispatchError::Rejected(_))
    ));
}

// The server-local maintenance verbs FAIL CLOSED (`Rejected`) rather than pretend,
// matching the module contract: a database-less worker must never silently no-op a
// reap/dead-letter/purge/supersede/list/awaiting-run/relay it did not perform. The
// pool's maintenance loop still ticks reap/purge/relay, but discards the result
// (`let _ =` / `unwrap_or(0)`), so the rejection is harmless there while surfacing
// anywhere the return value is consumed. The single legitimate exception is
// `Inbox::list`: the db-less worker's own drive reads it (`worker.rs`, with `?`) and
// the pending is already delivered in `Claimed.pending`, so an empty list is the
// correct answer, not a pretended mutation.
#[tokio::test]
async fn maintenance_verbs_fail_closed_except_the_legitimate_inbox_list_readback() {
    let queue = HttpDispatchQueue::new("http://127.0.0.1:1");
    let run = RunId("run-1".into());
    let thread = ThreadId("thread-1".into());
    let _ = &run;

    assert!(matches!(
        queue.reap(5, 0).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.dead_letters().await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.purge_dead_letters().await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.purge_dead_letters_before(0).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.superseded().await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.list_dispatches().await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.awaiting_run(&thread).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        Outbox::relay(&queue).await,
        Err(DispatchError::Rejected(_))
    ));

    // The one legitimate no-op read: an empty inbox is correct, not pretended.
    assert!(Inbox::list(&queue, &thread).await.unwrap().is_empty());
}
