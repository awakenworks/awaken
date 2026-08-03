//! End-to-end over real HTTP: a database-less worker's `HttpDispatchQueue` drives
//! the Control Node's Worker routes — enqueue → claim → renew → settle —
//! through the `DispatchQueue` trait, exactly as the pool would. Its own test binary
//! (installs a one-shot injected dispatch store).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_run_ingress::{
    CredentialRealizationReceipt, DispatchOutcome, DispatchQueue, FencedStreamCheckpointStore,
    HttpDispatchQueue, MemoryDispatchStore, PendingInput, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource,
    CredentialRealizationCapabilities, CredentialRealizationKind, CredentialRef, CredentialUsage,
    InferenceEndpoint, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_runtime_host::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamCheckpointStore};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock,
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

fn credential_dispatch(
    run: &str,
    thread: &str,
) -> (RunDispatch, CredentialRealizationCapabilities) {
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let mut activation = activation(run, thread);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
            ModelBinding::new("provider", "model", "native"),
            "provider@1",
            "route@1",
            "workspace",
            Some(CredentialAccess::new(
                CredentialRef {
                    id: "credential".into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: "model".into(),
            },
        );
    let mut dispatch = RunDispatch::new(activation);
    dispatch.inference_plaintext_holder = Some(holder.clone());
    (
        dispatch,
        CredentialRealizationCapabilities {
            holders: [holder].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
                .into_iter()
                .collect(),
            recipient_bound_envelopes: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        },
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
        .with_recovery_source(recovery)
        .with_checkpoint_store(Arc::new(MemoryStreamCheckpointStore::new())),
    );
    let router = dispatch_transport_router_with_service(service);

    // Serve the transport on an ephemeral localhost port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker holds only this HTTP client — no store handle.
    let queue = Arc::new(HttpDispatchQueue::new(
        format!("http://{addr}"),
        WorkerIdentity::new("worker-1", "boot-1", 1),
    ));
    let recovery_queue = Arc::new(HttpDispatchQueue::new(
        format!("http://{addr}"),
        WorkerIdentity::new("worker-2", "boot-2", 1),
    ));
    queue
        .enqueue(RunDispatch::new(activation("run-A", "t1")))
        .await
        .expect("enqueue over http");

    clock.set(0);
    let claimed = queue
        .claim("worker-1", 30_000, 0, &Default::default())
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

    // Checkpoint transport cause/effect decision table:
    // R1 authenticated current claim -> put/load applied over HTTP;
    // R2 stale epoch after replacement -> put/delete fenced and old value retained;
    // R3 replacement's current claim -> overwrite/delete applied. This is the
    // database-less Worker row: no local checkpoint store exists or is consulted.
    let checkpoint = |text: &str| StreamCheckpoint {
        run_id: "run-A".into(),
        thread_id: "t1".into(),
        model: "provider/model".into(),
        partial_text: text.into(),
        partial_tools: Vec::new(),
    };
    assert_eq!(
        queue
            .put_stream_checkpoint(&original_claim, checkpoint("current"))
            .await
            .expect("put checkpoint over http"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R1"
    );
    let remote_checkpoint =
        FencedStreamCheckpointStore::new(None, queue.clone(), original_claim.clone());
    assert_eq!(
        remote_checkpoint.get("run-A").await,
        Some(checkpoint("current")),
        "R1 inner=None delegates through authenticated HTTP"
    );
    assert_eq!(
        queue
            .load_stream_checkpoint(&original_claim)
            .await
            .expect("load checkpoint over http"),
        Some(checkpoint("current")),
        "R1"
    );

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
    let reclaimed = recovery_queue
        .claim("worker-2", 30_000, 40_000, &Default::default())
        .await
        .expect("reclaim over http")
        .expect("the lapsed lease is reclaimable");
    assert!(
        reclaimed.lease.epoch > claimed.lease.epoch,
        "the recovery re-claim bumped the fence epoch"
    );
    assert_eq!(
        queue
            .put_stream_checkpoint(&original_claim, checkpoint("stale"))
            .await
            .expect("stale checkpoint put is a fenced outcome"),
        awaken_run_ingress::SettleOutcome::Fenced,
        "R2"
    );
    assert_eq!(
        queue
            .delete_stream_checkpoint(&original_claim)
            .await
            .expect("stale checkpoint delete is a fenced outcome"),
        awaken_run_ingress::SettleOutcome::Fenced,
        "R2"
    );
    let replacement_claim = RunClaim::from(&reclaimed.lease);
    remote_checkpoint.put(checkpoint("stale-via-fence")).await;
    remote_checkpoint.delete("run-A").await;
    assert_eq!(
        recovery_queue
            .load_stream_checkpoint(&replacement_claim)
            .await
            .expect("replacement reads retained checkpoint"),
        Some(checkpoint("current")),
        "R2"
    );
    assert_eq!(
        recovery_queue
            .put_stream_checkpoint(&replacement_claim, checkpoint("replacement"))
            .await
            .expect("replacement overwrites checkpoint"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R3"
    );
    let replacement_checkpoint =
        FencedStreamCheckpointStore::new(None, recovery_queue.clone(), replacement_claim.clone());
    assert_eq!(
        replacement_checkpoint.get("run-A").await,
        Some(checkpoint("replacement")),
        "R3 inner=None reads through replacement authority"
    );
    assert_eq!(
        recovery_queue
            .delete_stream_checkpoint(&replacement_claim)
            .await
            .expect("replacement deletes checkpoint"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R3"
    );
    assert_eq!(
        recovery_queue
            .load_stream_checkpoint(&replacement_claim)
            .await
            .expect("deleted checkpoint is absent"),
        None,
        "R3"
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
        recovery_queue
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
            .claim("worker-1", 30_000, 60_000, &Default::default())
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
            &Default::default(),
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
            &Default::default(),
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
}

/// Receipt transport cause graph:
///
/// authenticated exact owner + current claim + exact attempt binding -> durable
/// receipt. A mechanism mismatch is rejected and recovery fences the old receipt.
///
/// | Rule | owner | epoch | mechanism | Result |
/// |---|---|---:|---|---|
/// | R1 | exact | current | exact | applied; replay applied |
/// | R2 | exact | current | mismatch | rejected |
/// | R3 | old owner | stale | exact | fenced |
#[tokio::test(flavor = "multi_thread")]
async fn credential_receipt_is_verified_and_fenced_over_real_http() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let clock = Arc::new(ManualWorkerClock::new(0));
    let (dispatch, capabilities) = credential_dispatch("credential-run", "credential-thread");
    let service = Arc::new(
        WorkerDispatchService::new(
            mem,
            Arc::new(HeaderWorkerAuthenticator),
            clock.clone(),
            Arc::new(FixedWorkerLeasePolicy::new(1_000)),
        )
        .with_local_credential_capabilities(capabilities),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, dispatch_transport_router_with_service(service))
            .await
            .unwrap();
    });
    let first_worker = HttpDispatchQueue::new(
        format!("http://{addr}"),
        WorkerIdentity::new("worker-1", "boot-1", 1),
    );
    let recovery_worker = HttpDispatchQueue::new(
        format!("http://{addr}"),
        WorkerIdentity::new("worker-2", "boot-2", 1),
    );
    first_worker.enqueue(dispatch).await.unwrap();
    let first = first_worker
        .claim("ignored", 30_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("credential run claims");
    let first_claim = RunClaim::from(&first.lease);
    let binding = first
        .credential_bindings
        .first()
        .expect("claim carries exact attempt binding");
    let receipt = CredentialRealizationReceipt::new(
        binding,
        CredentialRealizationKind::WorkerProviderAdapter,
    )
    .expect("exact receipt builds");
    assert!(
        first_worker
            .record_credential_realization(&first_claim, receipt.clone())
            .await
            .expect("R1 receipt crosses HTTP")
            .applied()
    );
    assert!(
        first_worker
            .record_credential_realization(&first_claim, receipt.clone())
            .await
            .expect("R1 retry is idempotent")
            .applied()
    );
    let mut wrong = receipt.clone();
    wrong.actual_realization_kind = CredentialRealizationKind::WorkerRelay;
    assert!(
        first_worker
            .record_credential_realization(&first_claim, wrong)
            .await
            .is_err(),
        "R2 mechanism mismatch is rejected over HTTP"
    );

    clock.set(first.lease.expires_ms + 1);
    let recovered = recovery_worker
        .claim(
            "ignored",
            30_000,
            first.lease.expires_ms + 1,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("recovery advances the fence");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert!(
        !first_worker
            .record_credential_realization(&first_claim, receipt)
            .await
            .expect("R3 stale receipt returns a verdict")
            .applied(),
        "R3 stale receipt is fenced over HTTP"
    );
}
