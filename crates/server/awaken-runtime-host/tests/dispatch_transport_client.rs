//! End-to-end over real HTTP: a database-less worker's `HttpDispatchQueue` drives
//! a cell server's `dispatch_transport_router` — enqueue → claim → renew → settle —
//! through the `DispatchQueue` trait, exactly as the pool would. Its own test binary
//! (installs a one-shot injected dispatch store).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_run_ingress::{
    DispatchOutcome, DispatchQueue, MemoryDispatchStore, PendingInput, RunClaim, RunDispatch,
};
use awaken_run_ingress_testkit::{ConformanceCapabilities, assert_dispatch_conformance_with_clock};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, HttpDispatchQueue, ManualWorkerClock,
    WorkerDispatchService, dispatch_transport_router_with_service,
};

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

#[tokio::test(flavor = "multi_thread")]
async fn db_less_worker_drives_runs_over_real_http() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let recovery = Arc::new(MemoryCommitCoordinator::new());
    recovery
        .commit(ThreadCommit {
            thread_id: ThreadId("t1".into()),
            run: RunDisposition::ended(RunId("run-A".into()), EndCause::NaturalEnd),
            messages: vec![Message::text(
                MessageId("committed-1".into()),
                Role::Assistant,
                "recover me",
            )],
            state: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("seed committed recovery truth");
    let clock = Arc::new(ManualWorkerClock::new(0));
    let service = Arc::new(
        WorkerDispatchService::new(
            mem.clone() as Arc<dyn DispatchQueue>,
            Arc::new(HeaderWorkerAuthenticator),
            clock.clone(),
            Arc::new(FixedWorkerLeasePolicy::new(1_000)),
        )
        .with_recovery_source(recovery),
    );
    let router = dispatch_transport_router_with_service(service);

    // Serve the transport on an ephemeral localhost port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker holds only this HTTP client — no store handle.
    let queue = HttpDispatchQueue::new(format!("http://{addr}"));
    queue
        .enqueue(RunDispatch::new(activation("run-A", "t1")))
        .await
        .expect("enqueue over http");

    clock.set(0);
    let claimed = queue
        .claim("worker-1", 30_000, 0)
        .await
        .expect("claim over http")
        .expect("a run is claimable");
    assert_eq!(claimed.request.activation.run_id.0, "run-A");
    assert_eq!(claimed.lease.owner, "worker-1");
    let original_claim = RunClaim::from(&claimed.lease);
    let snapshot = queue
        .load_recovery_snapshot(&original_claim)
        .await
        .expect("load claim-fenced recovery snapshot over http");
    assert_eq!(snapshot.thread_id.0, "t1");
    assert_eq!(snapshot.claimed_run_id.0, "run-A");
    assert_eq!(snapshot.latest_run_id, Some(RunId("run-A".into())));
    assert_eq!(
        snapshot.messages,
        vec![Message::text(
            MessageId("committed-1".into()),
            Role::Assistant,
            "recover me",
        )]
    );
    assert_eq!(snapshot.thread_version, 1);
    assert_eq!(snapshot.next_commit_ordinal, 1);

    clock.set(1_000);
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
    clock.set(40_000);
    let reclaimed = queue
        .claim("worker-2", 30_000, 40_000)
        .await
        .expect("reclaim over http")
        .expect("the lapsed lease is reclaimable");
    assert!(
        reclaimed.lease.epoch > claimed.lease.epoch,
        "the recovery re-claim bumped the fence epoch"
    );
    assert!(
        queue.load_recovery_snapshot(&original_claim).await.is_err(),
        "a stale claim cannot read recovery truth after a re-claim"
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

    // Parent-mediated child scheduling uses two compound commands. Each crosses
    // HTTP as one server-side transaction, so the co-located pool never observes
    // the row between enqueue/input delivery and the exact claim.
    clock.set(70_000);
    let child = queue
        .claim_new_run(
            RunDispatch::new(activation("run-B", "child-thread")),
            "worker-1",
            30_000,
            70_000,
        )
        .await
        .expect("atomic child admission over http")
        .expect("child is claimed");
    assert_eq!(child.request.run_id().0, "run-B");
    queue
        .settle(
            &RunId("run-B".into()),
            child.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("child awaits over http");
    clock.set(70_001);
    let resumed = queue
        .deliver_and_claim(
            PendingInput {
                message_id: "child-answer".into(),
                run_id: RunId("run-B".into()),
                thread_id: ThreadId("child-thread".into()),
                correlation_id: "approval-1".into(),
                available_at_ms: None,
                result: ResumeResult::Input("approved".into()),
            },
            "worker-1",
            30_000,
            70_001,
        )
        .await
        .expect("atomic child input over http")
        .expect("awaiting child is claimed");
    assert_eq!(resumed.pending.len(), 1);

    queue
        .settle(
            &RunId("run-B".into()),
            resumed.lease.epoch,
            DispatchOutcome::Done,
            &["child-answer".to_string()],
        )
        .await
        .expect("finish child over http");

    // A server-local write verb is refused on the worker transport (cancel is the
    // server's to make — a worker never cancels a peer's run).
    assert!(
        queue.cancel(&RunId("no-such-run".into())).await.is_err(),
        "cancel is not available on the worker dispatch transport"
    );

    let conformance_clock = |now_ms| clock.set(now_ms);
    assert_dispatch_conformance_with_clock(
        &queue,
        "conformance-http",
        ConformanceCapabilities::WORKER_TRANSPORT,
        &conformance_clock,
    )
    .await;
}
