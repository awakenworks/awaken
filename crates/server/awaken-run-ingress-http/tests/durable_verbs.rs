//! The durable-ingress operational verbs, end-to-end through their HTTP surface.
//!
//! The store-level dispatch state machine (manual quarantine / supersede) is proven
//! in `awaken-run-ingress`; this pins the **host routing layer** — that
//! `SharedHost::{list_dispatches, quarantine_retry_exhausted, dead_letters, requeue_dead_letter,
//! purge_dead_letters, superseded}`
//! reach the process-shared dispatch queue and project it onto the wire shape the
//! `durable_ops_router` returns.
//!
//! Its own test binary: it flips the process-global `SESSION_DEPLOYMENT_INGRESS=durable` and
//! installs the one-shot shared dispatch store, so it must not share a process with
//! the direct-ingress unit tests. One test drives the whole lifecycle sequentially
//! over a single injected in-memory store — no cross-test races on the global state.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    AnyDispatchStore, Dispatch, DispatchQueue, MemoryDispatchStore, RunDispatch, SubmitOptions,
};
use awaken_run_ingress_http::durable_ops_router;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::SharedHost;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A model that never actually runs — the operational verbs don't infer.
struct OkModel;

#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation(run: &str, thread: &str) -> RunActivation {
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("prov", "model", "acp:test"),
                ),
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

async fn call(router: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn contains_id(list: &Value, key: &str, id: &str) -> bool {
    list[key]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == id))
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_operational_verbs_drive_the_dispatch_lifecycle() {
    // Cause/effect decision table: C1 durable queue configured, C2 run lease
    // expired, C3 retry budget exhausted, C4 dead letter present, C5 newer turn
    // submitted. R1 C1 -> list succeeds; R2 C2+C3 -> explicit quarantine creates dead letter;
    // R3 C4 -> purge removes it; R4 C5 -> stale run is superseded. The single
    // sequence also proves every Coordinator HTTP effect reaches the same queue.
    // Inject one in-memory dispatch store we also keep a handle to, so we can drive
    // the queue deterministically and then assert the host verbs route to it.
    let mem = Arc::new(MemoryDispatchStore::new());
    let any = Arc::new(AnyDispatchStore::from_dispatch(
        mem.clone() as Arc<dyn Dispatch>
    ));
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(any));
    let router = durable_ops_router(host);
    let thread = "t-dur";
    let base = format!("/v1/durable/threads/{thread}");

    // Manual quarantine → dead-letter → purge, plus list_dispatches.
    // A fresh run, claimed under a 1ms lease → Leased.
    mem.enqueue(RunDispatch::new(activation("run-A", thread)))
        .await
        .unwrap();
    assert!(
        mem.claim("worker", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "the run is claimable"
    );

    // list_dispatches (host verb → ingress → store) shows the Leased row.
    let (s, v) = call(&router, "GET", &format!("{base}/dispatches")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["dispatches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["run_id"] == "run-A" && r["status"] == "Leased"),
        "dispatches shows run-A Leased: {v}"
    );

    // Explicit operator quarantine as-of a clock past the 1ms lease.
    let (s, v) = call(
        &router,
        "POST",
        &format!("{base}/quarantine-retry-exhausted?max_attempts=0&now_ms=1000"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["quarantined"], 1, "one crashed dispatch quarantined: {v}");

    // dead-letters lists it; an operator can requeue the exact repaired run
    // with a fresh retry budget.
    let (_, v) = call(&router, "GET", &format!("{base}/dead-letters")).await;
    assert!(
        contains_id(&v, "dead_letters", "run-A"),
        "dead-letters: {v}"
    );
    let (s, v) = call(
        &router,
        "POST",
        "/v1/durable/threads/a-different-thread/dead-letters/run-A/requeue",
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|message| message.contains("not dead-lettered")),
        "a run cannot be recovered through another Thread: {v}"
    );
    let (s, v) = call(
        &router,
        "POST",
        &format!("{base}/dead-letters/run-A/requeue"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "run_id": "run-A", "requeued": true }));
    assert!(
        mem.claim("worker", 1, 1000, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "the requeued run is claimable with a fresh budget"
    );
    let (_, v) = call(
        &router,
        "POST",
        &format!("{base}/quarantine-retry-exhausted?max_attempts=0&now_ms=2000"),
    )
    .await;
    assert_eq!(v["quarantined"], 1);

    // Purge removes the newly dead-lettered row; then the list is empty.
    let (_, v) = call(&router, "POST", &format!("{base}/dead-letters/purge")).await;
    assert_eq!(v["purged"], 1, "purged the dead-letter: {v}");
    let (_, v) = call(&router, "GET", &format!("{base}/dead-letters")).await;
    assert!(
        v["dead_letters"].as_array().unwrap().is_empty(),
        "dead-letters empty after purge: {v}"
    );

    // The process queue is shared, but a Thread-addressed operations route must
    // never project another Thread's rows into its monitoring response.
    mem.enqueue(RunDispatch::new(activation("run-other", "other-thread")))
        .await
        .unwrap();
    let (s, v) = call(&router, "GET", &format!("{base}/dispatches")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["dispatches"]
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["run_id"] != "run-other")),
        "Thread monitoring leaked a row from another Thread: {v}"
    );

    // ── supersede → superseded ───────────────────────────────────────────────
    // The store is clean; a newer submission on the thread marks the older pending
    // run superseded (never claimed again, ADR-0022).
    mem.enqueue(RunDispatch::new(activation("run-B", thread)))
        .await
        .unwrap();
    mem.enqueue_with(
        RunDispatch::new(activation("run-C", thread)),
        SubmitOptions {
            supersede: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (s, v) = call(&router, "GET", &format!("{base}/superseded")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        contains_id(&v, "superseded", "run-B"),
        "the superseded older run is reported: {v}"
    );
}

#[tokio::test]
async fn every_dispatch_read_and_repair_verb_requires_the_one_durable_capability() {
    /* Cause/effect decision table. Causes: C1 typed durable ingress installed;
     * C2 operation is read-only (dispatches/superseded/dead-letters) or repair
     * (requeue). Effects: E1 route through the per-Thread durable ingress; E2
     * reject 400 before reading the process dispatch store. D1 C1=T,C2=*=>E1
     * is covered by the lifecycle test above. D2 C1=F,C2=read=>E2; D3
     * C1=F,C2=repair=>E2. This prevents monitoring from becoming a parallel
     * authority/capability path. */
    let router = durable_ops_router(Arc::new(SharedHost::new(Arc::new(OkModel), "stub")));
    for (method, path) in [
        ("GET", "/v1/durable/threads/direct/dispatches"),
        ("GET", "/v1/durable/threads/direct/superseded"),
        ("GET", "/v1/durable/threads/direct/dead-letters"),
        (
            "POST",
            "/v1/durable/threads/direct/dead-letters/run/requeue",
        ),
    ] {
        let (status, body) = call(&router, method, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}: {body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|message| message.contains("durable ingress")),
            "{method} {path}: {body}",
        );
    }
}
