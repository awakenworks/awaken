// @formal evidence remote_tool.controls_never_admit_invocation
// @formal evidence session_execution.one_physical_attempt_per_thread
// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Real Native Runtime/Hand journal and exact Dispatch retirement over the existing mock API.
use super::*;
use awaken_dispatch::{DispatchQueue, WorkerDirectory};
use awaken_runtime_contract::{RunExecutor, RunRecoverySource};
use awaken_tool_contract::tool::{ToolCall, ToolError, ToolExecutor, ToolOutput};

struct OneToolModel;
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for OneToolModel {
    fn effect_scope(
        &self,
        _: &awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::model_operation::ModelEffectScope {
        awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly
    }
    async fn infer(
        &self,
        _: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        Ok(awaken_runtime_contract::llm::ChatResponse {
            output: awaken_runtime_contract::llm::AssistantOutput::from_tool_calls(vec![
                ToolCall {
                    call_id: "hold".into(),
                    tool_id: "hold".into(),
                    arguments: json!({}),
                },
            ]),
            usage: None,
            stop_reason: None,
        })
    }
}
struct HoldingTool {
    entered: tokio::sync::Notify,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl awaken_tool_contract::tool::RawTool for HoldingTool {
    fn id(&self) -> &str {
        "hold"
    }
    async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
}
struct BrainTrap;
#[async_trait::async_trait]
impl awaken_tool_contract::tool::RawTool for BrainTrap {
    fn id(&self) -> &str {
        "hold"
    }
    fn execution_target(&self) -> awaken_tool_contract::tool::ToolExecutionTarget {
        awaken_tool_contract::tool::ToolExecutionTarget::Sandbox
    }
    async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("must cross Hand")
    }
}
struct EnrolledHand {
    executor: Arc<dyn ToolExecutor>,
    domain: awaken_tool_contract::tool_batch::ToolExecutionDomain,
}
#[async_trait::async_trait]
impl ToolExecutor for EnrolledHand {
    async fn execution_domain(
        &self,
        _: &ToolCall,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, ToolError> {
        Ok(self.domain.clone())
    }
    fn recovery_capability(&self, id: &str) -> awaken_tool_contract::tool::ToolRecoveryCapability {
        self.executor.recovery_capability(id)
    }
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        self.executor.invoke(call).await
    }
}

#[tokio::test]
async fn lost_hand_result_keeps_opaque_occupied_but_exact_contained_exit_retires_the_same_slot() {
    // DT-NH: C1 actual Native journal before one Hand executor entry; C2 exact
    // Dispatch mutation lease expires and cancellation is signalled; C3 Hand
    // control cancels the same operation; C4 enrolled domain opaque/contained;
    // C5 provider process evidence pending/exact. NH1 C1+C2+C3 -> committed
    // unknown tool result, closed local entry, old physical slot occupied.
    // NH2 opaque -> no Pod mutation and no successor entry even after a local
    // Pod exit. NH3 contained+pending -> retain. NH4 contained+exact -> retire
    // only the original slot; result stays unknown and invocation count is one.
    // This uses the production Runtime, Hand factory, footprint classifier,
    // stop source and Dispatch CAS. The API is a deterministic fixture, not
    // proof of Kubernetes/CNI enforcement or Session-realization renewal timing.
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    use awaken_agent_contract::agent::state::{StateKey, Store};
    use awaken_runtime_contract::resolved::ToolDescriptor;
    use awaken_tool_contract::tool_batch::{ActiveToolBatch, ToolExecutionDomain};
    for contained in [false, true] {
        let (transport, facts, client) = fixture_with_client();
        let directory = awaken_worker_registry::MemoryWorkerDirectory::new();
        let registered = directory
            .register(
                awaken_dispatch::WorkerRegistration {
                    worker_id: "k8s:workers:worker-a:uid-a".into(),
                    incarnation_id: "uid-a".into(),
                    manifest: manifest(),
                    create_ledger: None,
                },
                100,
                10_000,
            )
            .await
            .unwrap();
        let worker_binding = transport
            .binding_source()
            .unwrap()
            .binding_for(&registered.snapshot.identity)
            .await
            .unwrap();
        let mut sandbox = running();
        sandbox["metadata"]["name"] = json!("sandbox-a");
        sandbox["metadata"]["uid"] = json!("sandbox-uid");
        *facts.pod.lock().unwrap() = (StatusCode::OK, sandbox);
        let sandbox_binding = PodProcessObserver::new(client, "workers");
        // The fixture freezes its exact sandbox binding through the same observer.
        let sandbox_binding = sandbox_binding
            .unwrap()
            .bind_pod_process("sandbox-a", "sandbox-uid")
            .await
            .unwrap();
        let domain = if contained {
            ToolExecutionDomain::ControlledProcess {
                provider_ref: "kubernetes-sandbox".into(),
                execution_ref: serde_json::to_string(&sandbox_binding).unwrap(),
            }
        } else {
            ToolExecutionDomain::Opaque {}
        };
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
        let store = awaken_dispatch::MemoryDispatchStore::new(clock.clone());
        let mut activation = model_only_activation(if contained {
            "contained-hand"
        } else {
            "opaque-hand"
        });
        activation.snapshot.resolved_spec.tool_descriptors = vec![ToolDescriptor::pinned(
            "test",
            "hold",
            "hold",
            json!({"type":"object"}),
        )];
        activation.snapshot.recompute_fingerprint().unwrap();
        let run = activation.run_id.clone();
        let thread = activation.thread_id.clone();
        store
            .enqueue(
                awaken_dispatch::RunDispatch::new(activation.clone())
                    .with_placement(awaken_dispatch::PlacementRequirements::remote_required()),
            )
            .await
            .unwrap();
        let mut snapshot = registered.snapshot;
        snapshot.state = awaken_dispatch::WorkerState::Ready;
        let old = awaken_dispatch::RunClaim::from(
            &store
                .claim_compatible(&snapshot, 50)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        store
            .begin_attempt(&old, Some(&worker_binding))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(awaken_tool_relay::FsOperationLedger::open(dir.path()).unwrap());
        let holding = Arc::new(HoldingTool {
            entered: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        let peer = tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::new(
                [holding.clone() as Arc<dyn awaken_tool_contract::tool::RawTool>],
                ledger.clone(),
                awaken_time::system_clock(),
            ),
        ));
        let bound = awaken_worker::relay_hand_executor_factory().bind(
            Box::new(brain),
            "hand-scope",
            awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest,
        );
        let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        ));
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        let context = awaken_runtime_contract::RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone())
            .with_cancellation(cancellation.clone())
            .with_tool_executor(Arc::new(EnrolledHand {
                executor: bound.executor.clone(),
                domain,
            }));
        let latch = context.physical_quiescence.clone();
        let runtime = awaken_runtime::Runtime::new()
            .with_llm(Arc::new(OneToolModel))
            .with_tool(Arc::new(BrainTrap));
        let drive = tokio::spawn(async move { runtime.execute(activation, context).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            holding.entered.notified(),
        )
        .await
        .unwrap();
        clock.set(200);
        cancellation.cancel();
        let (brain, hand) = tokio::io::duplex(64 * 1024);
        let control_peer = tokio::spawn(awaken_tool_relay::serve_hand(
            hand,
            awaken_tool_relay::HandSession::new([], ledger, awaken_time::system_clock()),
        ));
        bound
            .recovery
            .as_ref()
            .unwrap()
            .recover(Box::new(brain), true)
            .await
            .unwrap();
        control_peer.await.unwrap().unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), drive)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                ended,
                RunState::Ended(EndCause::Cancelled | EndCause::Indeterminate)
            ),
            "NH1"
        );
        assert!(!latch.close_and_wait().await, "NH1 no ordinary ACK");
        assert!(latch.local_entry_closed());
        store.close_attempt(&old).await.unwrap();
        let recovery = commit.recovery_snapshot(&thread, &run).await.unwrap();
        let mut state = Store::new();
        for c in &recovery.state {
            state.apply(c);
        }
        let batch = ActiveToolBatch::load(&state).unwrap().unwrap();
        assert!(
            batch.calls().iter().any(|c| matches!(
                c.phase,
                awaken_tool_contract::tool_batch::ToolCallPhase::Indeterminate { .. }
            )),
            "NH1 actual unknown journal"
        );
        let successor = directory
            .register(
                awaken_dispatch::WorkerRegistration {
                    worker_id: "next".into(),
                    incarnation_id: "next".into(),
                    manifest: manifest(),
                    create_ledger: None,
                },
                200,
                10_000,
            )
            .await
            .unwrap();
        let mut next_snapshot = successor.snapshot;
        next_snapshot.state = awaken_dispatch::WorkerState::Ready;
        let next = awaken_dispatch::RunClaim::from(
            &store
                .claim_compatible(&next_snapshot, 50)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let stops = transport.stop_source().unwrap();
        awaken_dispatch::stop_expired_predecessor(
            &store,
            &directory,
            stops.as_ref(),
            Some(commit.as_ref()),
            &next,
            200,
        )
        .await
        .unwrap();
        assert!(
            store.load_physical_attempt(&run).await.unwrap().is_some(),
            "NH2/NH3"
        );
        if !contained {
            assert!(
                facts.deletes.lock().unwrap().is_empty(),
                "NH2 no destructive guess"
            );
        }
        {
            let mut pod = facts.pod.lock().unwrap();
            pod.1["status"]["phase"] = json!("Failed");
            pod.1["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
                "exitCode":137,"reason":"Error","containerID":"containerd://exact", "startedAt":"2026-09-09T00:00:00Z","finishedAt":"2026-09-09T00:01:00Z"
            }});
        }
        awaken_dispatch::stop_expired_predecessor(
            &store,
            &directory,
            stops.as_ref(),
            Some(commit.as_ref()),
            &next,
            200,
        )
        .await
        .unwrap();
        assert_eq!(
            store.load_physical_attempt(&run).await.unwrap().is_none(),
            contained,
            "NH2/NH4"
        );
        assert_eq!(
            store.begin_attempt(&next, None).await.unwrap()
                == awaken_dispatch::AttemptAdmission::Applied,
            contained,
            "NH4"
        );
        assert_eq!(holding.calls.load(Ordering::SeqCst), 1);
        drop(bound);
        match peer.await.unwrap() {
            Ok(()) => {}
            Err(awaken_tool_relay::ServeError::Io(message)) if message.contains("broken pipe") => {
                // The cancelled consumer may close before the original cached
                // reply is delivered. The committed outcome/closure above,
                // rather than this transport teardown, owns every assertion.
            }
            other => panic!("unexpected original Hand teardown: {other:?}"),
        }
    }
}
