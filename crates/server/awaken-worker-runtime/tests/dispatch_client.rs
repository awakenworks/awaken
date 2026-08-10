//! The database-less worker's HTTP dispatch client (`HttpDispatchQueue`) over a real
//! socket.
//!
//! `transport_client.rs` is otherwise an entirely untested public module. These
//! tests drive the real client's claim/settle verbs across a real TCP port
//! (ephemeral) against a live `axum` server that mirrors a cell server's
//! registered Worker router, backed by the production `MemoryDispatchStore`.
//! No mock transport — the seam under test is the client's reqwest encode/decode
//! against the wire the server actually serves.
//!
//! (The transport router itself lives in `awaken-runtime-host`, which depends on
//! this crate; to avoid a dev-dependency cycle and the router's process-global
//! store injection, the test mirrors the same six routes over the same production
//! in-memory store — the client, the socket, and the store are all real.)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_run_ingress::{
    ClaimedStreamPublisher, DispatchError, DispatchOutcome, DispatchQueue, Inbox,
    MemoryDispatchStore, Outbox, PendingInput, RunClaim, RunDispatch, SettleOutcome,
    StreamEventRequest, SubmitOptions, WorkerIdentity,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_worker_runtime::HttpDispatchQueue;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

fn activation(run: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".into());
    RunActivation::new(
        RunId(run.into()),
        ThreadId("thread-1".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".into()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("p", "m", "b"),
                ),
                model_candidates: Vec::new(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        vec![Message::text(MessageId("m1".into()), Role::User, "go")],
    )
}

fn provider_candidate(
    reference: &str,
) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
    awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
        awaken_runtime_contract::ModelBinding::new("provider@1", "gateway-model", "genai"),
        "provider@1",
        "route@1",
        "workspace-a",
        Some(awaken_runtime_contract::CredentialAccess::new(
            awaken_runtime_contract::CredentialRef {
                id: reference.into(),
                revision: 1,
            },
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_runtime_contract::CredentialUsage::ProviderAdapter,
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )),
        awaken_runtime_contract::InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://gateway.invalid/v1".into(),
            upstream_model: "gateway-model".into(),
            processing_placement: None,
        },
    )
}

/// Stand up a live server mirroring the Control Node's Worker dispatch routes
/// over a shared [`MemoryDispatchStore`], bound to an ephemeral port. Returns the
/// base URL a `HttpDispatchQueue` points at plus the shared store handle, so a test
/// can assert server-side state directly.
struct TransportState {
    store: Arc<MemoryDispatchStore>,
    now_ms: Arc<AtomicU64>,
    stream_calls: Arc<AtomicUsize>,
    credential_capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
}

async fn spawn_transport_server() -> (
    String,
    Arc<MemoryDispatchStore>,
    Arc<AtomicU64>,
    Arc<AtomicUsize>,
) {
    let store = Arc::new(MemoryDispatchStore::new());
    let now_ms = Arc::new(AtomicU64::new(0));
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let state = Arc::new(TransportState {
        store: store.clone(),
        now_ms: now_ms.clone(),
        stream_calls: stream_calls.clone(),
        credential_capabilities: awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [holder].into_iter().collect(),
            material_sources: [
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        },
    });
    let app = Router::new()
        .route("/v1/worker/dispatch/enqueue", post(enqueue))
        .route("/v1/worker/dispatch/claim", post(claim))
        .route("/v1/worker/dispatch/claim_run", post(claim_run))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/renew_owned", post(renew_owned))
        .route("/v1/worker/dispatch/bind_sandbox", post(bind_sandbox))
        .route("/v1/worker/dispatch/settle", post(settle))
        .route("/v1/worker/dispatch/stream", post(stream_event))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    // Keep one externally observable counter while the Router owns the state.
    // The handler increments both values through this shared wrapper.
    let observed = stream_calls;
    (format!("http://{addr}"), store, now_ms, observed)
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
            &state.credential_capabilities,
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
            &state.credential_capabilities,
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

async fn stream_event(
    State(state): State<Arc<TransportState>>,
    headers: HeaderMap,
    Json(request): Json<StreamEventRequest>,
) -> Json<Value> {
    assert_eq!(worker_id(&headers), request.identity.worker_id);
    assert_eq!(request.claim.run_id, request.event.run_id);
    state.stream_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "accepted": true }))
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

struct ScriptedBindState {
    first_status: StatusCode,
    calls: AtomicUsize,
}

async fn scripted_bind(State(state): State<Arc<ScriptedBindState>>) -> (StatusCode, Json<Value>) {
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    if call == 0 && state.first_status != StatusCode::OK {
        return (state.first_status, Json(json!({ "error": "injected" })));
    }
    (
        StatusCode::OK,
        Json(json!({ "applied": true, "settled": true })),
    )
}

async fn scripted_recovery(
    State(state): State<Arc<ScriptedBindState>>,
) -> (StatusCode, Json<Value>) {
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    if call == 0 && state.first_status != StatusCode::OK {
        return (state.first_status, Json(json!({ "error": "injected" })));
    }
    (
        StatusCode::OK,
        Json(json!({
            "snapshot": {
                "thread_id": "recovery-thread",
                "claimed_run_id": "recovery-retry",
                "runs": [],
                "latest_run_id": null,
                "messages": [],
                "state": [],
                "resume_tickets": [],
                "thread_version": 0,
                "store_cursor": 0,
                "next_commit_ordinal": 0
            }
        })),
    )
}

async fn spawn_scripted_idempotent_server(
    first_status: StatusCode,
) -> (String, Arc<ScriptedBindState>) {
    let state = Arc::new(ScriptedBindState {
        first_status,
        calls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/v1/worker/dispatch/bind_sandbox", post(scripted_bind))
        .route("/v1/worker/dispatch/settle", post(scripted_bind))
        .route("/v1/worker/recovery/snapshot", post(scripted_recovery))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted transport");
    let address = listener.local_addr().expect("scripted address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve scripted bind");
    });
    (format!("http://{address}"), state)
}

async fn stalled_bind(State(state): State<Arc<ScriptedBindState>>) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    std::future::pending::<Json<Value>>().await
}

async fn spawn_stalled_server() -> (String, Arc<ScriptedBindState>) {
    let state = Arc::new(ScriptedBindState {
        first_status: StatusCode::OK,
        calls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/v1/worker/dispatch/bind_sandbox", post(stalled_bind))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), state)
}

#[tokio::test]
async fn transport_deadline_and_caller_cancellation_bound_body_stalls() {
    // FMECA cause/effect graph: C1 server accepts then never produces a body;
    // C2 the transport client has a total request deadline; C3 service shutdown
    // drops the in-flight future first. Effects: E1 C1+C2 returns a bounded
    // transport error after only the configured idempotent retries; E2 C1+C3
    // cancels promptly without waiting for the HTTP deadline. An unbounded Worker
    // call starves shutdown and lease recovery (S=9,O=5,D=7,RPN=315).
    // Decision rules: D1=C1+C2+!C3->E1; D2=C1+C3->E2. Authoritative 4xx and
    // applied-but-lost retry rules remain covered by T3/T2 below.
    let claim = RunClaim {
        run_id: RunId("body-stall".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };
    let (base, state) = spawn_stalled_server().await;
    let deadline_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(25))
        .build()
        .unwrap();
    let queue = HttpDispatchQueue::new(base.clone(), WorkerIdentity::new("worker-A", "boot-A", 1))
        .with_client(deadline_client);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            queue.bind_sandbox(&claim, "sandbox-stall"),
        )
        .await
        .expect("D1 deadline bounds all retries")
        .is_err(),
        "D1"
    );
    assert!(
        state.calls.load(Ordering::SeqCst) <= 3,
        "D1 bounded retries"
    );

    let cancellation_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let cancelling = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-A", "boot-A", 1))
        .with_client(cancellation_client);
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        cancelling.bind_sandbox(&claim, "sandbox-cancel"),
    )
    .await;
    assert!(cancelled.is_err(), "D2 caller cancellation wins promptly");
}

/// Sandbox-bind transport cause graph:
/// C1=the exact claim is current; C2=response is ambiguous/5xx; C3=response is
/// an authoritative 4xx. The same claim + sandbox reference is idempotent, so
/// C1+C2 may retry; C3 must fail immediately and must never be disguised.
///
/// | Rule | first response | retry | result |
/// |---|---|---|---|
/// | T1 | 200 | no | applied (covered by the main transport test) |
/// | T2 | 503 | yes, bounded | applied on the second response |
/// | T3 | 400 | no | rejected immediately |
#[tokio::test]
async fn sandbox_bind_retries_only_ambiguous_transport_outcomes() {
    let claim = RunClaim {
        run_id: RunId("bind-retry".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };

    let (retry_base, retry_state) =
        spawn_scripted_idempotent_server(StatusCode::SERVICE_UNAVAILABLE).await;
    let retrying = HttpDispatchQueue::new(retry_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(
        retrying
            .bind_sandbox(&claim, "sandbox-stable")
            .await
            .expect("T2 retries the ambiguous response")
            .applied()
    );
    assert_eq!(retry_state.calls.load(Ordering::SeqCst), 2);

    let (reject_base, reject_state) =
        spawn_scripted_idempotent_server(StatusCode::BAD_REQUEST).await;
    let rejecting =
        HttpDispatchQueue::new(reject_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(
        rejecting
            .bind_sandbox(&claim, "sandbox-invalid")
            .await
            .is_err()
    );
    assert_eq!(reject_state.calls.load(Ordering::SeqCst), 1);
}

/// Settle uses the same ambiguity partition as sandbox binding. Its exact epoch
/// is the idempotency/fence key: replay after an applied-but-lost response is a
/// harmless fenced response, while retry after a pre-apply 5xx completes it.
///
/// | Rule | first response | retry | result |
/// |---|---|---|---|
/// | T1 | 200 | no | applied (covered by the main transport test) |
/// | T2 | 503 | yes, bounded | applied on the second response |
/// | T3 | 400 | no | rejected immediately |
#[tokio::test]
async fn settle_retries_only_ambiguous_transport_outcomes() {
    let run = RunId("settle-retry".into());
    let (retry_base, retry_state) =
        spawn_scripted_idempotent_server(StatusCode::SERVICE_UNAVAILABLE).await;
    let retrying = HttpDispatchQueue::new(retry_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert_eq!(
        retrying
            .settle(&run, 7, DispatchOutcome::Done, &[])
            .await
            .expect("T2 retries the ambiguous settle response"),
        SettleOutcome::Applied
    );
    assert_eq!(retry_state.calls.load(Ordering::SeqCst), 2);

    let (reject_base, reject_state) =
        spawn_scripted_idempotent_server(StatusCode::BAD_REQUEST).await;
    let rejecting =
        HttpDispatchQueue::new(reject_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(
        rejecting
            .settle(&run, 7, DispatchOutcome::Done, &[])
            .await
            .is_err()
    );
    assert_eq!(reject_state.calls.load(Ordering::SeqCst), 1);
}

/// Recovery is a claim-fenced read, so retry cannot duplicate a mutation. A
/// transport/5xx ambiguity is retried, while an authoritative 4xx (stale claim,
/// wrong owner, or invalid identity) remains final.
///
/// | Rule | first response | retry | result |
/// |---|---|---|---|
/// | T1 | 200 | no | snapshot returned (covered by runtime-host HTTP test) |
/// | T2 | 503 | yes, bounded | snapshot returned on the second response |
/// | T3 | 400 | no | rejected immediately |
#[tokio::test]
async fn recovery_snapshot_retries_only_ambiguous_transport_outcomes() {
    let claim = RunClaim {
        run_id: RunId("recovery-retry".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };
    let (retry_base, retry_state) =
        spawn_scripted_idempotent_server(StatusCode::SERVICE_UNAVAILABLE).await;
    let retrying = HttpDispatchQueue::new(retry_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    let snapshot = retrying
        .load_recovery_snapshot(&claim)
        .await
        .expect("T2 retries the ambiguous recovery response");
    assert_eq!(snapshot.claimed_run_id, claim.run_id);
    assert_eq!(retry_state.calls.load(Ordering::SeqCst), 2);

    let (reject_base, reject_state) =
        spawn_scripted_idempotent_server(StatusCode::BAD_REQUEST).await;
    let rejecting =
        HttpDispatchQueue::new(reject_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(rejecting.load_recovery_snapshot(&claim).await.is_err());
    assert_eq!(reject_state.calls.load(Ordering::SeqCst), 1);
}

// ── 1. The db-less remote-worker seam: enqueue → claim → fence → settle ──────────

/// Live-publication cause/effect graph: C1 the canonical event classifier marks
/// an event live; C2 it marks a complete content/lifecycle Fact non-live. E1 sends
/// exactly one authenticated HTTP observation; E2 performs no HTTP request and
/// leaves durable commit/settlement as the sole Fact path.
///
/// | Rule | event | classify.live | HTTP calls | effect |
/// |---|---|---|---|---|
/// | S1 | TextDelta | true | 1 | best-effort observation |
/// | S2 | RunFinished | false | unchanged | durable path only |
#[tokio::test]
async fn worker_stream_transport_posts_only_canonically_live_events() {
    let (base, _, _, stream_calls) = spawn_transport_server().await;
    let queue = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-A", "boot-A", 1));
    let claim = RunClaim {
        run_id: RunId("run-live".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };
    let event = |kind| StreamEvent {
        run_id: claim.run_id.clone(),
        kind,
    };

    queue
        .publish(
            &claim,
            event(AgentEvent::Delta(Delta::TextDelta { delta: "x".into() })),
        )
        .await
        .expect("S1 live publication is best effort");
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1, "S1");

    queue
        .publish(
            &claim,
            event(AgentEvent::Fact(Fact::RunFinished { exhausted: false })),
        )
        .await
        .expect("S2 durable Fact is intentionally absent from live transport");
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1, "S2");
}

/// Dispatch lifecycle cause/effect graph: C1 the request is serializable and
/// runnable; C2 the run is already leased; C3 settle epoch is stale; C4 settle
/// epoch is current. Effects are E1 durable enqueue and lossless wire payload,
/// E2 no duplicate claim, E3 fenced/no mutation, and E4 terminal removal.
///
/// | Rule | state/action | condition | effect |
/// |---|---|---|---|
/// | D1 | enqueue then claim | C1 | E1 |
/// | D2 | second claim | C2 | E2 |
/// | D3 | settle | C3 | E3 |
/// | D4 | settle | C4 | E4 |
#[tokio::test]
async fn worker_claims_and_settles_a_run_over_a_real_dispatch_transport() {
    let (base, store, clock, _) = spawn_transport_server().await;
    let queue = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-A", "boot-A", 1));
    let run = RunId("run-1".into());

    // Enqueue a run over the wire; the server-side store records it.
    let candidate = provider_candidate("grant-http");
    let mut activation = activation("run-1");
    activation.snapshot.resolved_spec.model_binding = candidate.clone();
    queue
        .enqueue(
            RunDispatch::new(activation).with_inference_plaintext_holder(
                awaken_runtime_contract::PlaintextHolder::new(
                    awaken_runtime_contract::PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                ),
            ),
        )
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
        .claim("worker-A", 1_000, 0, &Default::default())
        .await
        .expect("claim ok")
        .expect("a runnable dispatch");
    assert_eq!(claimed.request.run_id(), &run);
    assert_eq!(
        &claimed
            .request
            .activation
            .snapshot
            .resolved_spec
            .model_binding,
        &candidate,
        "the complete model candidate survives the worker HTTP boundary"
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
            .claim("worker-A", 1_000, 10, &Default::default())
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

/// Lease-fencing cause/effect graph: C1 owner A's lease is live; C2 it expires;
/// C3 owner B reclaims and advances the epoch; C4 stale A renews or settles.
/// Effects are E1 A renews, E2 B exclusively owns the same row, and E3 A is
/// rejected without mutating B's run. Rules: L1 C1=>E1; L2 C2+C3=>E2;
/// L3 C3+C4=>E3. The rows cover both live and superseded ownership.
#[tokio::test]
async fn renew_lease_returns_false_over_the_wire_when_the_lease_was_stolen() {
    let (base, store, clock, _) = spawn_transport_server().await;
    let queue_a =
        HttpDispatchQueue::new(base.clone(), WorkerIdentity::new("worker-A", "boot-A", 1));
    let queue_b = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-B", "boot-B", 1));
    let run = RunId("run-1".into());

    queue_a
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Worker A claims at t=0 with a 1s lease (epoch 1).
    clock.store(0, Ordering::SeqCst);
    let a = queue_a
        .claim("worker-A", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1);
    // While A still holds it, a renew succeeds — the TRUE path, as a control.
    clock.store(100, Ordering::SeqCst);
    assert!(
        queue_a
            .renew_lease(&run, "worker-A", 1_000, 100)
            .await
            .unwrap(),
        "the current owner renews its own live lease"
    );

    // The lease lapses; worker B recovers the run at t=2s (epoch 2 — B now owns it).
    clock.store(2_000, Ordering::SeqCst);
    let b = queue_b
        .claim("worker-B", 1_000, 2_000, &Default::default())
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
        !queue_a
            .renew_lease(&run, "worker-A", 1_000, 2_100)
            .await
            .unwrap(),
        "a stale owner's renew fails so it abandons the run"
    );
    // And A's settle at its old epoch is fenced, leaving B's in-flight run inviolate.
    assert_eq!(
        queue_a
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

/// Server-authority cause/effect table: C1 a database-less Worker invokes a
/// Coordinator-owned mutation; C2 the verb is a remote-worker write supported
/// by this client. E1 is fail-closed without dialing; E2 is authenticated HTTP
/// dispatch (covered by D1-D4). This test covers rule A1 C1+!C2=>E1 for recovery,
/// cancel, Inbox mutation, and Outbox staging, preventing false success.
#[tokio::test]
async fn server_local_write_verbs_fail_closed() {
    // No server is needed: these verbs resolve locally on the client without a
    // request, so a bad base URL is never dialed.
    let queue = HttpDispatchQueue::new(
        "http://127.0.0.1:1",
        WorkerIdentity::new("worker-local", "boot-local", 1),
    );
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

/// Maintenance/readback cause/effect table: C1 the operation requires
/// Coordinator store authority; C2 it is the Worker drive's local Inbox list;
/// C3 the pending inputs were already carried by `Claimed.pending`. Rules:
/// M1 C1+!C2=>`Rejected` without dialing; M2 C2+C3=>empty list. M2 is the sole
/// legitimate no-op read; every maintenance mutation and authoritative query
/// uses M1 so the client cannot pretend server work happened.
#[tokio::test]
async fn maintenance_verbs_fail_closed_except_the_legitimate_inbox_list_readback() {
    let queue = HttpDispatchQueue::new(
        "http://127.0.0.1:1",
        WorkerIdentity::new("worker-local", "boot-local", 1),
    );
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
