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
    let run = RunId("run-A".into());

    // S8 fence read #1 — no row yet: the remote epoch is unknown, and the COMMIT fence
    // fails OPEN (a db-less worker's write is never rejected because the store cannot
    // see the run). Without the current_epoch route this silently returned None from
    // the trait default, so the whole fence was a no-op over the wire.
    assert_eq!(queue.current_epoch(&run).await.unwrap(), None);
    assert!(queue.holds_current_epoch(&run, 0).await.unwrap());

    queue
        .enqueue(RunExecutionRequest::new(activation("run-A", "t1")))
        .await
        .expect("enqueue over http");
    assert_eq!(
        queue.current_epoch(&run).await.unwrap(),
        Some(0),
        "enqueued-but-unclaimed reads epoch 0 over the wire"
    );

    let claimed = queue
        .claim("worker-1", 30_000, 0)
        .await
        .expect("claim over http")
        .expect("a run is claimable");
    assert_eq!(claimed.request.activation.run_id.0, "run-A");
    assert_eq!(claimed.lease.owner, "worker-1");

    // S8 fence read #2 — the claim bumped the epoch; the owner holds the fence over
    // http, a stale lower epoch does not.
    assert_eq!(
        queue.current_epoch(&run).await.unwrap(),
        Some(claimed.lease.epoch)
    );
    assert!(
        queue
            .holds_current_epoch(&run, claimed.lease.epoch)
            .await
            .unwrap()
    );
    assert!(!queue.holds_current_epoch(&run, 0).await.unwrap());

    assert!(
        queue
            .renew_lease(&RunId("run-A".into()), "worker-1", 30_000, 1_000)
            .await
            .expect("renew over http"),
        "the owner renews its live lease"
    );

    // The fence crosses the wire: after the lease lapses, a recovery claim by another
    // worker bumps the epoch, so the original owner's settle carrying its now-stale
    // epoch is fenced server-side and changes nothing.
    let reclaimed = queue
        .claim("worker-2", 30_000, 40_000)
        .await
        .expect("reclaim over http")
        .expect("the lapsed lease is reclaimable");
    assert!(
        reclaimed.lease.epoch > claimed.lease.epoch,
        "the recovery re-claim bumped the fence epoch"
    );
    // S8 fence read #3 — the reclaim superseded worker-1: its commit fence must now
    // REJECT over the wire (the exact double-apply the co-located fence blocks), while
    // the current owner still holds.
    assert_eq!(
        queue.current_epoch(&run).await.unwrap(),
        Some(reclaimed.lease.epoch)
    );
    assert!(
        !queue
            .holds_current_epoch(&run, claimed.lease.epoch)
            .await
            .unwrap(),
        "the superseded remote worker no longer holds the commit fence over http"
    );
    assert!(
        queue
            .holds_current_epoch(&run, reclaimed.lease.epoch)
            .await
            .unwrap()
    );
    assert_eq!(
        queue
            .settle(
                &RunId("run-A".into()),
                claimed.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle over http"),
        awaken_run_ingress::SettleOutcome::Fenced,
        "the stale owner's settle is fenced over the wire"
    );
    // The current owner settles under the fresh epoch: applied, the run is removed.
    assert_eq!(
        queue
            .settle(
                &RunId("run-A".into()),
                reclaimed.lease.epoch,
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle over http"),
        awaken_run_ingress::SettleOutcome::Applied,
        "the current owner's settle applies over the wire"
    );

    assert!(
        queue
            .claim("worker-1", 30_000, 60_000)
            .await
            .expect("claim over http")
            .is_none(),
        "a settled run is gone"
    );

    // S8 fence read #4 — the row is gone, so the epoch is unknown again and the fence
    // fails open over the wire (a terminal commit racing its own settle is never
    // rejected). This is the None-→-fail-open half of the contract.
    assert_eq!(queue.current_epoch(&run).await.unwrap(), None);
    assert!(
        queue
            .holds_current_epoch(&run, reclaimed.lease.epoch)
            .await
            .unwrap()
    );

    // A server-local write verb is refused on the worker transport (cancel is the
    // server's to make — a worker never cancels a peer's run).
    assert!(
        queue.cancel(&RunId("no-such-run".into())).await.is_err(),
        "cancel is not available on the worker dispatch transport"
    );
}
