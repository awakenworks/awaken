//! The durable-ingress operational verbs, end-to-end through their HTTP surface.
//!
//! The store-level dispatch state machine (reap / dead-letter / supersede) is proven
//! in `awaken-run-ingress`; this pins the **host routing layer** — that
//! `SharedHost::{list_dispatches, reap, dead_letters, purge_dead_letters, superseded}`
//! reach the process-shared dispatch queue and project it onto the wire shape the
//! `durable_ops_router` returns.
//!
//! Its own test binary: it flips the process-global `AWAKEN_INGRESS=durable` and
//! installs the one-shot shared dispatch store, so it must not share a process with
//! the direct-ingress unit tests. One test drives the whole lifecycle sequentially
//! over a single injected in-memory store — no cross-test races on the global state.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    AnyDispatchStore, Dispatch, DispatchQueue, MemoryDispatchStore, RunExecutionRequest,
    SubmitOptions,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{SharedHost, durable_ops_router, init_shared_dispatch_store};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
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
    RunActivation {
        run_id: RunId(run.into()),
        thread_id: ThreadId(thread.into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                model_binding: ModelBinding::new("prov", "model", "acp:test"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        trace: Default::default(),
    }
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
    // SAFETY: this dedicated test binary sets the process-global durable flag once,
    // before any host is built; no other thread reads it concurrently here.
    unsafe {
        std::env::set_var("AWAKEN_INGRESS", "durable");
    }

    // Inject one in-memory dispatch store we also keep a handle to, so we can drive
    // the queue deterministically and then assert the host verbs route to it.
    let mem = Arc::new(MemoryDispatchStore::new());
    let any = Arc::new(AnyDispatchStore::from_dispatch(
        mem.clone() as Arc<dyn Dispatch>
    ));
    init_shared_dispatch_store(any);

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = durable_ops_router(host);
    let thread = "t-dur";
    let base = format!("/v1/durable/threads/{thread}");

    // ── reap → dead-letter → purge, plus list_dispatches ─────────────────────
    // A fresh run, claimed under a 1ms lease → Running.
    mem.enqueue(RunExecutionRequest::new(activation("run-A", thread)))
        .await
        .unwrap();
    assert!(
        mem.claim("worker", 1, 0).await.unwrap().is_some(),
        "the run is claimable"
    );

    // list_dispatches (host verb → ingress → store) shows the Running row.
    let (s, v) = call(&router, "GET", &format!("{base}/dispatches")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["dispatches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["run_id"] == "run-A" && r["status"] == "Running"),
        "dispatches shows run-A Running: {v}"
    );

    // reap as-of a clock past the 1ms lease → the crashed dispatch is dead-lettered.
    let (s, v) = call(
        &router,
        "POST",
        &format!("{base}/reap?max_attempts=0&now_ms=1000"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["dead_lettered"], 1,
        "one crashed dispatch dead-lettered: {v}"
    );

    // dead-letters lists it; purge removes it; then the list is empty.
    let (_, v) = call(&router, "GET", &format!("{base}/dead-letters")).await;
    assert!(
        contains_id(&v, "dead_letters", "run-A"),
        "dead-letters: {v}"
    );
    let (_, v) = call(&router, "POST", &format!("{base}/dead-letters/purge")).await;
    assert_eq!(v["purged"], 1, "purged the dead-letter: {v}");
    let (_, v) = call(&router, "GET", &format!("{base}/dead-letters")).await;
    assert!(
        v["dead_letters"].as_array().unwrap().is_empty(),
        "dead-letters empty after purge: {v}"
    );

    // ── supersede → superseded ───────────────────────────────────────────────
    // The store is clean; a newer submission on the thread marks the older pending
    // run superseded (never claimed again, ADR-0022).
    mem.enqueue(RunExecutionRequest::new(activation("run-B", thread)))
        .await
        .unwrap();
    mem.enqueue_with(
        RunExecutionRequest::new(activation("run-C", thread)),
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
