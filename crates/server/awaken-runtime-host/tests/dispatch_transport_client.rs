//! End-to-end over real HTTP: a database-less worker's `HttpDispatchQueue` drives
//! a cell server's `dispatch_transport_router` — enqueue → claim → renew → settle —
//! through the `DispatchQueue` trait, exactly as the pool would. Its own test binary
//! (installs a one-shot injected dispatch store).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    AnyDispatchStore, Dispatch, DispatchOutcome, DispatchQueue, MemoryDispatchStore,
    RunExecutionRequest,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    HttpDispatchQueue, SharedHost, dispatch_transport_router, init_shared_dispatch_store,
};

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

#[tokio::test(flavor = "multi_thread")]
async fn db_less_worker_drives_runs_over_real_http() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let any = Arc::new(AnyDispatchStore::from_dispatch(
        mem.clone() as Arc<dyn Dispatch>
    ));
    init_shared_dispatch_store(any);

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = dispatch_transport_router(host);

    // Serve the transport on an ephemeral localhost port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker holds only this HTTP client — no store handle.
    let queue = HttpDispatchQueue::new(format!("http://{addr}"));

    queue
        .enqueue(RunExecutionRequest::new(activation("run-A", "t1")))
        .await
        .expect("enqueue over http");

    let claimed = queue
        .claim("worker-1", 30_000, 0)
        .await
        .expect("claim over http")
        .expect("a run is claimable");
    assert_eq!(claimed.request.activation.run_id.0, "run-A");
    assert_eq!(claimed.lease.owner, "worker-1");

    assert!(
        queue
            .renew_lease(&RunId("run-A".into()), "worker-1", 30_000, 1_000)
            .await
            .expect("renew over http"),
        "the owner renews its live lease"
    );

    queue
        .settle(&RunId("run-A".into()), DispatchOutcome::Done, &[])
        .await
        .expect("settle over http");

    assert!(
        queue
            .claim("worker-1", 30_000, 2_000)
            .await
            .expect("claim over http")
            .is_none(),
        "a settled run is gone"
    );

    // A server-local operational verb is refused on the worker transport.
    assert!(
        queue.reap(0, 3_000).await.is_err(),
        "reap is not available on the worker dispatch transport"
    );
}
