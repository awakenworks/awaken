// @formal evidence remote_worker.maintenance_scheduler_respects_adapter_ownership
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
//! (The transport router itself lives in `awaken-session-executor`, which depends on
//! this crate; to avoid a dev-dependency cycle and the router's process-global
//! store injection, the test mirrors the same six routes over the same production
//! in-memory store — the client, the socket, and the store are all real.)

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_dispatch::{
    AttemptAdmission, ClaimedStreamPublisher, DispatchError, DispatchOutcome, MemoryDispatchStore,
    RunClaim, RunDispatch, SettleOutcome, StreamObservationRequest, SubmitOptions, WorkerIdentity,
};
use awaken_dispatch_contract::ClaimedCheckpointTransport as _;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_coordinator_client::HttpDispatchQueue;
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
                max_steps: Some(16),
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
    awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
        awaken_runtime_contract::ModelBinding::new("provider@1", "gateway-model", "genai"),
        "provider@1",
        "route@1",
        "workspace-a",
        Some(
            awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: reference.into(),
                    revision: 1,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                "provider",
            )),
        ),
        awaken_runtime_contract::InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://gateway.invalid/v1".into(),
            upstream_model: "gateway-model".into(),
            processing_placement: None,
        },
    )
    .expect("coherent Worker dispatch provider candidate")
}

/// Stand up a live server mirroring the Control Node's Worker dispatch routes
/// over a shared [`MemoryDispatchStore`], bound to an ephemeral port. Returns the
/// base URL a `HttpDispatchQueue` points at plus the shared store handle, so a test
/// can assert server-side state directly.
struct TransportState {
    store: Arc<MemoryDispatchStore>,
    stream_calls: Arc<AtomicUsize>,
    credential_capabilities: awaken_credential_contract::CredentialRealizationCapabilities,
}

async fn spawn_transport_server() -> (
    String,
    Arc<MemoryDispatchStore>,
    Arc<awaken_lease_testkit::ManualClock>,
    Arc<AtomicUsize>,
) {
    // The server's dispatch authority now answers from its own clock, so the
    // test's server clock is the store's clock.
    let now_ms = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(now_ms.clone()));
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let state = Arc::new(TransportState {
        store: store.clone(),
        stream_calls: stream_calls.clone(),
        credential_capabilities: awaken_credential_contract::CredentialRealizationCapabilities {
            holders: [holder].into_iter().collect(),
            material_sources: [
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes: false,
            envelope_required: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        },
    });
    let app = Router::new()
        .route("/v1/worker/dispatch/enqueue", post(enqueue))
        .route("/v1/worker/dispatch/claim", post(claim))
        .route("/v1/worker/dispatch/claim_run", post(claim_run))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/attempt/begin", post(begin_attempt))
        .route("/v1/worker/dispatch/attempt/handoff", post(handoff_attempt))
        .route("/v1/worker/dispatch/attempt/finish", post(finish_attempt))
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
        .claim(worker_id(&headers), 1_000, &state.credential_capabilities)
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
    let claim = RunClaim {
        run_id: run_id(&req),
        owner: worker_id(&headers).to_string(),
        epoch: req["lease_epoch"].as_u64().expect("lease_epoch"),
    };
    let outcome = state.store.renew_lease(&claim, 1_000).await.expect("renew");
    Json(json!({ "outcome": outcome }))
}

async fn begin_attempt(
    State(state): State<Arc<TransportState>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    assert!(
        req.get("identity").is_some(),
        "exact Worker identity crosses"
    );
    let claim: RunClaim = serde_json::from_value(req["claim"].clone()).expect("claim");
    let admission = state
        .store
        .begin_attempt(&claim, None)
        .await
        .expect("begin attempt");
    Json(json!({ "admission": admission }))
}

async fn handoff_attempt(
    State(state): State<Arc<TransportState>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    assert!(
        req.get("identity").is_some(),
        "exact Worker identity crosses"
    );
    let claim: RunClaim = serde_json::from_value(req["claim"].clone()).expect("claim");
    let admission = state
        .store
        .handoff_attempt(&claim, None)
        .await
        .expect("handoff attempt");
    Json(json!({ "admission": admission }))
}

async fn finish_attempt(
    State(state): State<Arc<TransportState>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    assert!(
        req.get("identity").is_some(),
        "exact Worker identity crosses"
    );
    let request: awaken_dispatch::AttemptExecutionRequest =
        serde_json::from_value(req).expect("exact finish request");
    let finished = state
        .store
        .finish_attempt(&request.claim, request.return_cause)
        .await
        .expect("finish attempt")
        .applied();
    Json(json!({ "finished": finished }))
}

#[tokio::test]
async fn local_return_cause_crosses_http_without_fallback_on_old_strict_rejection() {
    use awaken_dispatch::{AttemptPhysicalFact, AttemptReturnCause, DispatchControlReads as _};
    // T1 current codec/client preserves Some through the original finish verb;
    // T2 a strict old two-field JSON extractor returns 422 before its handler;
    // the client must retain that explicit failure, never retry a None body.
    // Handler entry count and signed body shape distinguish a format rejection
    // from a fake successful ACK or a provider-stop assertion.
    let (base, store, clock, _) = spawn_transport_server().await;
    let identity = WorkerIdentity::new("worker-A", "boot-A", 1);
    let queue = HttpDispatchQueue::new(base, identity.clone());
    store
        .enqueue(RunDispatch::new(activation("wire-local-return")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim(&identity.lease_owner(), 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    queue.begin_attempt(&claim, None).await.unwrap();
    assert_eq!(
        queue
            .finish_attempt(&claim, Some(AttemptReturnCause::StructuredReturnObserved))
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "T1 typed cause"
    );
    let records = store
        .attempt_closure_records_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&claim.run_id),
        )
        .await
        .unwrap();
    let AttemptPhysicalFact::Acknowledgement { record } = &records.events[0].record else {
        panic!("T1 no Provider assertion")
    };
    assert_eq!(record.cause(), AttemptReturnCause::StructuredReturnObserved);

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldStrictRequest {
        claim: RunClaim,
        identity: Option<WorkerIdentity>,
    }
    let entered = Arc::new(AtomicUsize::new(0));
    let observed = entered.clone();
    let app = Router::new().route(
        "/v1/worker/dispatch/attempt/finish",
        post(move |Json(request): Json<OldStrictRequest>| {
            let entered = observed.clone();
            async move {
                entered.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    request.identity.as_ref().map(WorkerIdentity::lease_owner),
                    Some(request.claim.owner)
                );
                Json(json!({"finished":true}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let old = HttpDispatchQueue::new(format!("http://{address}"), identity);
    let status = reqwest::Client::new().post(format!("http://{address}/v1/worker/dispatch/attempt/finish"))
        .json(&json!({"claim":claim,"identity":WorkerIdentity::new("worker-A","boot-A",1),"return_cause":"operation_future_not_entered"})).send().await.unwrap().status();
    assert_eq!(
        status,
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "T2 actual old extractor status"
    );
    assert!(
        old.finish_attempt(&claim, Some(AttemptReturnCause::OperationFutureNotEntered))
            .await
            .is_err(),
        "T2 strict 422 does not downgrade"
    );
    assert_eq!(
        entered.load(Ordering::SeqCst),
        0,
        "T2 no fallback None request"
    );
    assert_eq!(
        old.finish_attempt(&claim, None).await.unwrap(),
        SettleOutcome::Applied,
        "T2 genuine legacy request is compatible"
    );
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
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
    Json(request): Json<StreamObservationRequest>,
) -> Json<Value> {
    assert_eq!(worker_id(&headers), request.request.identity.worker_id);
    assert_eq!(request.request.claim.run_id, request.request.event.run_id);
    assert_eq!(
        request
            .assistant_response
            .as_ref()
            .map(|coordinate| coordinate.thread_id.0.as_str()),
        Some("thread-live"),
        "S1 exact coordinate crosses the existing endpoint"
    );
    state.stream_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "accepted": true }))
}

async fn bind_sandbox(
    State(state): State<Arc<TransportState>>,
    _headers: HeaderMap,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_server_authority_fields_absent(&req);
    let claim: awaken_dispatch::RunClaim =
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

async fn always_unavailable(State(calls): State<Arc<AtomicUsize>>) -> (StatusCode, &'static str) {
    calls.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "exact Kubernetes Worker process is unavailable",
    )
}

async fn spawn_unavailable_finish_server() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/worker/dispatch/attempt/finish",
            post(always_unavailable),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unavailable transport");
    let address = listener.local_addr().expect("unavailable address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve unavailable finish");
    });
    (format!("http://{address}"), calls)
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
async fn worker_transport_cannot_silently_erase_an_execution_binding() {
    // B16 Some binding + either admission verb => local Rejected, zero wire
    // calls, even if a server would acknowledge it. B17 unsupported observation
    // => error, never None (which would falsely describe an empty slot).
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let app = Router::new().fallback(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        async { Json(json!({"outcome":"applied"})) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let identity = WorkerIdentity::new("worker-A", "boot-A", 1);
    let claim = RunClaim {
        run_id: RunId("bound-wire".into()),
        owner: identity.lease_owner(),
        epoch: 1,
    };
    let binding =
        awaken_dispatch::AttemptExecutionBinding::new(identity.clone(), "cluster-a", "pod-a")
            .unwrap();
    let queue = HttpDispatchQueue::new(format!("http://{address}"), identity);
    assert!(matches!(
        queue.begin_attempt(&claim, Some(&binding)).await,
        Err(DispatchError::Rejected(_))
    ));
    assert!(matches!(
        queue.handoff_attempt(&claim, Some(&binding)).await,
        Err(DispatchError::Rejected(_))
    ));
    // Control readback is absent at the type boundary (compile-fail docs).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "B16-B17 no downgrade or fabricated observation"
    );
    server.abort();
    let _ = server.await;
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

#[tokio::test]
async fn exhausted_finish_transport_remains_retryable_for_the_physical_ack_loop() {
    // Cause/effect decision table: C1 the executor has physically returned;
    // C2 every bounded HTTP attempt receives 503; C3 the exact finish command
    // is idempotent; C4 the Coordinator returns a precise bounded diagnostic.
    // R1=C1+C2+C3+C4 => the transport exhausts its local attempts, returns
    // Unavailable with C4 retained, and DispatchWorker keeps the observed-return
    // responsibility. It must not return Rejected or erase C4, either of which
    // hides why the occupied slot cannot enter logical settlement.
    let claim = RunClaim {
        run_id: RunId("finish-unavailable".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };
    let (base, calls) = spawn_unavailable_finish_server().await;
    let queue = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-A", "boot-A", 1));
    let error = queue.finish_attempt(&claim, None).await.unwrap_err();
    assert!(matches!(error, DispatchError::Unavailable(_)), "R1");
    assert!(
        error
            .to_string()
            .contains("exact Kubernetes Worker process is unavailable"),
        "R1 retains C4"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3, "R1 bounded local retry");
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
/// | T4 | 409 | no | exact stale-claim conflict |
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
        .load_recovery_snapshot(&claim, None)
        .await
        .expect("T2 retries the ambiguous recovery response");
    assert_eq!(snapshot.claimed_run_id, claim.run_id);
    assert_eq!(retry_state.calls.load(Ordering::SeqCst), 2);

    let (reject_base, reject_state) =
        spawn_scripted_idempotent_server(StatusCode::BAD_REQUEST).await;
    let rejecting =
        HttpDispatchQueue::new(reject_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(
        rejecting
            .load_recovery_snapshot(&claim, None)
            .await
            .is_err()
    );
    assert_eq!(reject_state.calls.load(Ordering::SeqCst), 1);

    let (conflict_base, conflict_state) =
        spawn_scripted_idempotent_server(StatusCode::CONFLICT).await;
    let conflicted =
        HttpDispatchQueue::new(conflict_base, WorkerIdentity::new("worker-A", "boot-A", 1));
    assert!(matches!(
        conflicted.load_recovery_snapshot(&claim, None).await,
        Err(DispatchError::Conflict(_))
    ));
    assert_eq!(conflict_state.calls.load(Ordering::SeqCst), 1, "T4");
}

// ── 1. The db-less remote-worker seam: enqueue → claim → fence → settle ──────────

/// Live-publication cause/effect graph: C1 the canonical event classifier marks
/// an event live; C2 it marks a complete content/lifecycle Fact non-live; C3 the
/// live event has an exact assistant-response coordinate. E1 sends exactly one
/// authenticated HTTP observation; E2 performs no HTTP request and leaves
/// durable commit/settlement as the sole Fact path; E3 preserves C3 on that same
/// endpoint. Constraint/Invariant: live publication is observational and cannot
/// become a second committed Fact path. Decision rule: S1 and S2 cover the
/// canonical live and non-live classifier partitions.
///
/// | Rule | event | classify.live | coordinate | HTTP calls | effect |
/// |---|---|---|---|---|---|
/// | S1 | TextDelta | true | exact | 1 | best-effort observation + E3 |
/// | S2 | RunFinished | false | exact | unchanged | durable path only |
#[tokio::test]
async fn worker_stream_transport_posts_only_canonically_live_events() {
    let (base, _, _, stream_calls) = spawn_transport_server().await;
    let queue = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-A", "boot-A", 1));
    let claim = RunClaim {
        run_id: RunId("run-live".into()),
        owner: "worker-A:1:boot-A".into(),
        epoch: 1,
    };
    let event = |kind| {
        StreamObservation::assistant_delta(
            claim.run_id.clone(),
            awaken_agent_contract::agent::thread::Id("thread-live".into()),
            0,
            0,
            kind,
        )
    };

    queue
        .publish_observation(
            &claim,
            event(AgentEvent::Delta(Delta::TextDelta { delta: "x".into() })),
        )
        .await
        .expect("S1 live publication is best effort");
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1, "S1");

    queue
        .publish_observation(
            &claim,
            event(AgentEvent::Fact(Fact::RunFinished { exhausted: false })),
        )
        .await
        .expect("S2 durable Fact is intentionally absent from live transport");
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1, "S2");
}

/// Dispatch lifecycle cause/effect graph: C1 the request is serializable and
/// runnable; C2 the run is already leased; C3 settle epoch is stale; C4 settle
/// epoch is current; C5 the exact physical slot is active or acknowledged.
/// Effects are E1 durable enqueue and lossless wire payload, E2 no duplicate
/// claim, E3 fenced/no mutation, E4 terminal removal, E5 exact idempotent
/// admission, and E6 settle blocked until quiescence.
///
/// | Rule | state/action | condition | effect |
/// |---|---|---|---|
/// | D1 | enqueue then claim | C1 | E1 |
/// | D2 | second claim | C2 | E2 |
/// | D3 | settle | C3 | E3 |
/// | D4 | begin + retry | C4+C5 active | E5 |
/// | D5 | settle | C4+C5 active | E6 |
/// | D6 | finish then settle | C4+C5 acknowledged | E4 |
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
    clock.set(0);
    let claimed = queue
        .claim("worker-A", 1_000, &Default::default())
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
    assert_eq!(claimed.lease.owner(), "worker-A");
    assert_eq!(claimed.lease.epoch(), 1);
    assert!(
        queue
            .bind_sandbox(&RunClaim::from(&claimed.lease), "sandbox-http")
            .await
            .expect("bind sandbox over transport")
            .applied()
    );
    let claim = RunClaim::from(&claimed.lease);
    assert_eq!(
        queue.begin_attempt(&claim, None).await.expect("D4 begin"),
        AttemptAdmission::Applied,
        "D4/E5"
    );
    assert_eq!(
        queue.begin_attempt(&claim, None).await.expect("D4 retry"),
        AttemptAdmission::AlreadyApplied,
        "D4/E5 response-loss retry"
    );
    assert_eq!(
        queue
            .handoff_attempt(&claim, None)
            .await
            .expect("D4 handoff transport retry"),
        AttemptAdmission::AlreadyApplied,
        "D4 the signed continuation route preserves exact-slot idempotency"
    );

    // A second claim finds nothing runnable (the only run is now leased): `None`.
    clock.set(10);
    assert!(
        queue
            .claim("worker-A", 1_000, &Default::default())
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

    assert_eq!(
        queue
            .settle(&run, claimed.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "D5/E6 active external work cannot be erased"
    );
    assert_eq!(
        queue.finish_attempt(&claim, None).await.expect("D6 finish"),
        SettleOutcome::Applied,
        "D6 quiescence ACK"
    );
    // Only after quiescence does the current-epoch settle remove the dispatch.
    assert_eq!(
        queue
            .settle(&run, claimed.lease.epoch(), DispatchOutcome::Done, &[])
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

#[tokio::test]
async fn continuation_handoff_preserves_replacement_provenance_over_http() {
    // Causes: C1 A occupies the physical slot; C2 A's mutation lease expires
    // and B recovers it; C3 ordinary begin observes A; C4 the executor-authorized
    // handoff replaces A. Effects: E1 ordinary recovery is Blocked; E2 HTTP
    // preserves Replaced instead of flattening it to fresh Applied; E3 only B's
    // exact ACK clears the slot. Decision rules: H1=C1+C2+C3 -> E1;
    // H2=C1+C2+C4 -> E2+E3. This is the real JSON client/server boundary.
    let (base, store, clock, _) = spawn_transport_server().await;
    let queue_a =
        HttpDispatchQueue::new(base.clone(), WorkerIdentity::new("worker-A", "boot-A", 1));
    let queue_b = HttpDispatchQueue::new(base, WorkerIdentity::new("worker-B", "boot-B", 1));
    queue_a
        .enqueue(RunDispatch::new(activation("handoff-provenance")))
        .await
        .unwrap();
    clock.set(0);
    let a = queue_a
        .claim("worker-A", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    let claim_a = RunClaim::from(&a.lease);
    assert_eq!(
        queue_a.begin_attempt(&claim_a, None).await.unwrap(),
        AttemptAdmission::Applied,
        "H1 A installs the fresh slot"
    );

    clock.set(2_000);
    let b = queue_b
        .claim("worker-B", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("B recovers");
    let claim_b = RunClaim::from(&b.lease);
    assert_eq!(
        queue_b.begin_attempt(&claim_b, None).await.unwrap(),
        AttemptAdmission::Blocked,
        "H1/E1 ordinary recovery cannot inherit A's slot"
    );
    assert_eq!(
        queue_b.handoff_attempt(&claim_b, None).await.unwrap(),
        AttemptAdmission::Replaced,
        "H2/E2 replacement provenance survives JSON"
    );
    assert_eq!(
        queue_a.finish_attempt(&claim_a, None).await.unwrap(),
        SettleOutcome::Fenced,
        "H2/E3 A cannot clear B"
    );
    assert!(store.list_dispatches().await.unwrap()[0].physical_attempt_active);
    assert_eq!(
        queue_b.finish_attempt(&claim_b, None).await.unwrap(),
        SettleOutcome::Applied,
        "H2/E3 exact B ACK clears the inherited slot"
    );
}

// ── 3. renew_lease FALSE path over the wire (lease stolen by recovery) ───────────

/// Lease-fencing cause/effect graph: C1 owner A's lease is live; C2 it expires;
/// C3 owner B reclaims and advances the epoch; C4 stale A renews or settles.
/// Effects are E1 A renews, E2 B exclusively owns the same row, and E3 A is
/// rejected without mutating B's run. Rules: L1 C1=>E1; L2 C2+C3=>E2;
/// L3 C3+C4=>E3. The rows cover both live and superseded ownership.
#[tokio::test]
async fn renew_lease_returns_typed_loss_over_the_wire_when_the_lease_was_stolen() {
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
    clock.set(0);
    let a = queue_a
        .claim("worker-A", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch(), 1);
    // While A still holds it, a renew succeeds — the TRUE path, as a control.
    clock.set(100);
    assert_eq!(
        queue_a
            .renew_lease(&RunClaim::from(&a.lease), 1_000)
            .await
            .unwrap(),
        awaken_dispatch::LeaseRenewalOutcome::Current,
        "the current owner renews its own live lease"
    );

    // The lease lapses; worker B recovers the run at t=2s (epoch 2 — B now owns it).
    clock.set(2_000);
    let b = queue_b
        .claim("worker-B", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("B recovers the lapsed lease");
    assert_eq!(b.lease.epoch(), 2, "recovery bumped the fence epoch");
    assert_eq!(
        store.dispatch_count(),
        1,
        "recovery re-owns the same row, not a second"
    );

    // A's renew now returns Lost over the wire — the lease was stolen; the stale
    // holder must stop (the whole point of the multi-node liveness knob).
    clock.set(2_100);
    assert_eq!(
        queue_a
            .renew_lease(&RunClaim::from(&a.lease), 1_000)
            .await
            .unwrap(),
        awaken_dispatch::LeaseRenewalOutcome::Lost,
        "a stale owner's renew fails so it abandons the run",
    );
    // And A's settle at its old epoch is fenced, leaving B's in-flight run inviolate.
    assert_eq!(
        queue_a
            .settle(&run, a.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced
    );
    assert_eq!(store.dispatch_count(), 1);
}

// ── 2. Required Worker roles and compile-time Coordinator refusals ────────────

/// C1 Coordinator mutations fail to compile (HttpDispatchQueue's docs).
/// C2 supported Worker verbs cross authenticated HTTP (D1-D4). This test
/// preserves the positive Worker-role and legitimate Inbox-read partition.
#[tokio::test]
async fn server_local_write_verbs_fail_closed() {
    // The negative write scenarios now fail at compile time, before a client can
    // dial. HttpDispatchQueue's docs cover Administration, Inbox and Outbox.
    let queue = HttpDispatchQueue::new(
        "http://127.0.0.1:1",
        WorkerIdentity::new("worker-local", "boot-local", 1),
    );
    fn worker_only(_: &dyn awaken_dispatch_execution::RemoteDispatch) {}
    worker_only(&queue);
    assert!(
        awaken_dispatch_contract::InboxRead::list(&queue, &ThreadId("thread-1".into()))
            .await
            .unwrap()
            .is_empty()
    );
}

// Type refusals for every Coordinator-only role live in HttpDispatchQueue's
// compile-fail docs. The scheduler's existing 42-row matrix verifies that actual
// remote services/pools construct no local maintenance and still drain claims.
#[tokio::test]
async fn maintenance_verbs_fail_closed_except_the_legitimate_inbox_list_readback() {
    let queue = Arc::new(HttpDispatchQueue::new(
        "http://127.0.0.1:1",
        WorkerIdentity::new("worker-local", "boot-local", 1),
    ));
    let remote = awaken_dispatch::DispatchBinding::remote(queue, Arc::new(SelectionOnlyCommit));
    assert!(matches!(
        remote.placement(),
        awaken_dispatch::DispatchTopology::Remote { .. }
    ));
    assert!(
        awaken_dispatch_contract::InboxRead::list(
            remote.queue().as_ref(),
            &ThreadId("thread-1".into())
        )
        .await
        .unwrap()
        .is_empty()
    );
    let local = Arc::new(
        awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
            MemoryDispatchStore::new(awaken_time::system_clock()),
        )),
    );
    assert!(matches!(
        awaken_dispatch::DispatchBinding::local(local).placement(),
        awaken_dispatch::DispatchTopology::Local(_)
    ));
}

#[tokio::test]
async fn any_dispatch_store_exposes_the_control_rung_only_when_its_inner_handle_owns_it() {
    let owner = Arc::new(
        awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
            MemoryDispatchStore::new(awaken_time::system_clock()),
        )),
    );
    let binding = awaken_dispatch::DispatchBinding::local(owner.clone());
    let awaken_dispatch::DispatchTopology::Local(selected) = binding.placement() else {
        panic!("local authority retained");
    };
    assert!(Arc::ptr_eq(selected, &owner));
    assert!(Arc::ptr_eq(&binding.queue(), &owner.worker_view()));
    assert!(
        !selected
            .reject_session_run_reservation(&RunId("missing".into()))
            .await
            .unwrap()
    );
}

struct SelectionOnlyCommit;
#[async_trait::async_trait]
impl awaken_dispatch_contract::ClaimedRunCommit for SelectionOnlyCommit {
    async fn commit(
        &self,
        _: &RunClaim,
        _: awaken_agent_contract::ThreadCommit,
    ) -> Result<
        awaken_agent_contract::thread::commit::staged::CommitRecord,
        awaken_agent_contract::thread::commit::coordinator::Error,
    > {
        panic!("construction has zero commit effects")
    }
    async fn commit_operation(
        &self,
        _: awaken_dispatch_contract::ClaimedCommitCommand,
    ) -> Result<
        awaken_agent_contract::thread::commit::operation::CommitReceipt,
        awaken_agent_contract::thread::commit::coordinator::Error,
    > {
        panic!("construction has zero commit effects")
    }
}
#[test]
fn any_dispatch_store_preserves_the_claimed_recovery_transport() {
    // Required remote checkpoint, credential and commit Arcs survive selection
    // exactly. A local placement exposes only its selected durable owner.
    let owner = Arc::new(HttpDispatchQueue::new(
        "http://127.0.0.1:1",
        WorkerIdentity::new("worker-local", "boot-local", 1),
    ));
    let checkpoints: Arc<dyn awaken_dispatch_contract::ClaimedCheckpointTransport> =
        owner.clone();
    let credentials: Arc<dyn awaken_dispatch_contract::ClaimedCredentialDeliveryTransport> =
        owner.clone();
    let commit: Arc<dyn awaken_dispatch_contract::ClaimedRunCommit> =
        Arc::new(SelectionOnlyCommit);
    let binding = awaken_dispatch::DispatchBinding::remote(owner, commit.clone());
    let awaken_dispatch::DispatchTopology::Remote {
        checkpoints: selected,
        credentials: delivery,
        commit: selected_commit,
    } = binding.placement()
    else {
        panic!("remote selection");
    };
    assert!(Arc::ptr_eq(selected, &checkpoints));
    assert!(Arc::ptr_eq(delivery, &credentials));
    assert!(Arc::ptr_eq(selected_commit, &commit));
    assert!(Arc::ptr_eq(&binding.queue(), &binding.clone().queue()));
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _, DispatchReads as _,
    DispatchSettlement as _, SessionRunReservations as _,
};
