//! Durable ingress over the in-memory dispatch store and commit coordinator.
//!
//! These prove the durable slice end to end without a database: a durable submit
//! persists then runs a fresh run; the durable-only operation fails closed on
//! direct ingress (G5); enqueue and pending append are idempotent; an awaiting run
//! resumes through delivered input (#4); and an expired lease is recovered.

mod harness;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_builtin_tools::{MessageSendRequest, MessageSender};
use awaken_run_ingress::{
    DispatchOutcome, DispatchQueue, DispatchWorker, DurableRunIngress, Inbox, ManualClock,
    MemoryDispatchStore, Outbox, OutboxMessageSender, PendingInput, RunDispatch,
    RunIngressCapabilities,
};
use awaken_runtime::{DirectRunIngress, RunIngress, RunService};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    FP, SNAP, THREAD, TICKET, activation, input_echo_runtime, schedule_runtime, text_runtime,
    tool_runtime,
};

fn send_request(target: &str, content: &str, operation_id: &str) -> MessageSendRequest {
    MessageSendRequest {
        target_thread: target.to_string(),
        content: content.to_string(),
        idempotency_key: None,
        source_run_id: "source-run".to_string(),
        operation_id: operation_id.to_string(),
    }
}

#[derive(Default)]
struct RecordingAttemptExecutor {
    executes: AtomicUsize,
    resumes: AtomicUsize,
    cancels: AtomicUsize,
}

struct OwnershipCheckingAttemptExecutor {
    verified: AtomicUsize,
}

#[async_trait::async_trait]
impl RunExecutor for OwnershipCheckingAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        context
            .ownership
            .as_ref()
            .expect("claimed attempt receives ownership authority")
            .verify_current()
            .await
            .expect("fresh exact claim remains current");
        self.verified.fetch_add(1, Ordering::SeqCst);
        RecordingAttemptExecutor::finish(&activation, &context).await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for OwnershipCheckingAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        _command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.execute(activation, context).await
    }

    async fn cancel(
        &self,
        _activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        context
            .ownership
            .as_ref()
            .expect("claimed cancellation receives ownership authority")
            .verify_current()
            .await
            .expect("fresh exact claim remains current");
        self.verified.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl RecordingAttemptExecutor {
    async fn finish(
        activation: &RunActivation,
        context: &RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let disposition = awaken_agent_contract::thread::commit::RunDisposition::ended(
            activation.run_id.clone(),
            EndCause::NaturalEnd,
        );
        if let Some(commit) = &context.commit {
            awaken_agent_contract::thread::commit::commit_run(
                commit.as_ref(),
                &activation.thread_id,
                disposition,
                Vec::new(),
                Vec::new(),
            )
            .await
            .map_err(|error| ExecutionError::Commit(error.to_string()))?;
        }
        Ok(RunState::Ended(EndCause::NaturalEnd))
    }
}

#[async_trait::async_trait]
impl RunExecutor for RecordingAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        Self::finish(&activation, &context).await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for RecordingAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        _command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.resumes.fetch_add(1, Ordering::SeqCst);
        Self::finish(&activation, &context).await
    }

    async fn cancel(
        &self,
        _activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
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
        },
    )
}

fn allow_command() -> ResumeCommand {
    ResumeCommand {
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms: 0,
    }
}

/// Pending input answering the gate's ticket (the common case).
fn pending(message_id: &str, run: &str, result: ResumeResult) -> PendingInput {
    pending_for(message_id, run, TICKET, result)
}

/// Pending input answering a specific ticket correlation.
fn pending_for(
    message_id: &str,
    run: &str,
    correlation: &str,
    result: ResumeResult,
) -> PendingInput {
    harness::pending(message_id, run, correlation, result)
}

#[tokio::test]
async fn durable_submit_persists_then_runs_to_completion() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("durable submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // Committed truth holds the run: the user turn then the assistant reply.
    // Per-step durability: the input commits at the first step boundary, the
    // terminal turn through finish.
    assert_eq!(commit.commit_count(), 2);
    let messages = commit.committed().messages;
    assert_eq!(messages[0].text_content(), "go");
    assert_eq!(messages.last().unwrap().text_content(), "done");
    assert_eq!(store.dispatch_count(), 0, "a finished dispatch is removed");
}

#[tokio::test]
async fn installed_attempt_executor_drives_a_fresh_durable_run() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(text_runtime(), store, commit);
    let selected = Arc::new(RecordingAttemptExecutor::default());
    ingress.install_attempt_executor(selected.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-selected"))
            .await
            .expect("selected executor completes"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(selected.executes.load(Ordering::SeqCst), 1);
    assert_eq!(selected.resumes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn claimed_attempt_receives_live_exact_ownership_authority() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let clock = Arc::new(ManualClock::new(7));
    let selected = Arc::new(OwnershipCheckingAttemptExecutor {
        verified: AtomicUsize::new(0),
    });
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit, "worker")
        .with_ownership_clock(clock);
    worker.install_attempt_executor(selected.clone());
    store
        .enqueue(RunDispatch::new(activation("ownership-run")))
        .await
        .expect("dispatch enqueued");

    assert_eq!(
        worker.tick(7).await.expect("claimed attempt completes"),
        Some((
            RunId("ownership-run".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(selected.verified.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unified_foreground_service_uses_the_installed_attempt_executor() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(text_runtime(), store, commit);
    let selected = Arc::new(RecordingAttemptExecutor::default());
    ingress.install_attempt_executor(selected.clone());

    RunService::start(
        &ingress,
        activation("foreground-start"),
        RuntimeRunContext::new(),
    )
    .await
    .expect("foreground start routes through the selected executor");
    RunService::resume(
        &ingress,
        activation("run-1"),
        allow_command(),
        RuntimeRunContext::new(),
    )
    .await
    .expect("foreground resume routes through the selected executor");

    assert_eq!(selected.executes.load(Ordering::SeqCst), 1);
    assert_eq!(selected.resumes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_worker_routes_inference_through_the_resolved_model_executor() {
    // Cause: the worker carries a model resolver that maps the run's binding model_ref
    // to a labeled executor. Effect: the drive routes inference through THAT executor,
    // not the runtime's bound default — the committed reply is the resolved model's
    // ("RESOLVED"), never the runtime default ("done"). This is the per-run provider
    // seam a database-less worker uses to run the run's own configured model.
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

    struct Labeled;
    #[async_trait::async_trait]
    impl LlmExecutor for Labeled {
        async fn infer(
            &self,
            _r: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("RESOLVED"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let resolver: awaken_run_ingress::InferenceMaterializerFn =
        Arc::new(|_activation, _context| Ok(Some(Arc::new(Labeled) as Arc<dyn LlmExecutor>)));
    let ingress = DurableRunIngress::with_owner_and_resolver(
        text_runtime(), // its bound model would reply "done"
        store.clone(),
        commit.clone(),
        "owner",
        None,
        Some(resolver),
    );

    let state = ingress
        .submit_background(activation("run-resolved"))
        .await
        .expect("durable submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    assert_eq!(
        messages.last().unwrap().text_content(),
        "RESOLVED",
        "the worker ran the resolved model, not the runtime's bound default"
    );
}

#[tokio::test]
async fn a_secretless_worker_reads_the_snapshot_pinned_access() {
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

    struct Gateway;
    #[async_trait::async_trait]
    impl LlmExecutor for Gateway {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("GATEWAY"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let capture = seen.clone();
    let resolver: awaken_run_ingress::InferenceMaterializerFn =
        Arc::new(move |activation, _context| {
            *capture.lock().expect("grant capture mutex") =
                Some(activation.snapshot.resolved_spec.model_binding.clone());
            Ok(Some(Arc::new(Gateway) as Arc<dyn LlmExecutor>))
        });
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let ingress = DurableRunIngress::with_owner_and_resolver(
        text_runtime(),
        store,
        commit.clone(),
        "secretless-worker",
        None,
        Some(resolver),
    )
    .with_local_credential_capabilities(
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
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
    );
    let candidate = provider_candidate("grant-17");
    let mut activation = activation("run-gateway");
    activation.snapshot.resolved_spec.model_binding = candidate.clone();
    let request = RunDispatch::new(activation).with_inference_plaintext_holder(holder);
    let (_, state) = ingress
        .worker()
        .start_run(request, 0)
        .await
        .expect("worker drive succeeds")
        .expect("new run is claimed");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(*seen.lock().expect("grant capture mutex"), Some(candidate));
    assert_eq!(
        commit
            .committed()
            .messages
            .last()
            .expect("assistant reply")
            .text_content(),
        "GATEWAY"
    );
}

#[tokio::test]
async fn a_per_run_model_override_routes_the_worker_to_the_overridden_model() {
    // R5, end to end on the worker path: a run selects the already-published `alt`
    // candidate and resolves it to a different executor than the primary binding.
    // This proves the per-turn switch reaches the materialization seam without
    // allowing an arbitrary model ref outside the immutable snapshot.
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

    struct Fixed(&'static str);
    #[async_trait::async_trait]
    impl LlmExecutor for Fixed {
        async fn infer(
            &self,
            _r: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let resolver: awaken_run_ingress::InferenceMaterializerFn =
        Arc::new(
            |activation, _context| match activation.effective_model_ref() {
                "alt" => Ok(Some(Arc::new(Fixed("ALT")) as Arc<dyn LlmExecutor>)),
                _ => Ok(Some(Arc::new(Fixed("BOUND")) as Arc<dyn LlmExecutor>)),
            },
        );
    let ingress = DurableRunIngress::with_owner_and_resolver(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "owner",
        None,
        Some(resolver),
    );

    let mut over = activation("run-override");
    over.snapshot.resolved_spec.model_candidates.push(
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new("alt", "alt", "stub"),
        ),
    );
    let over = over.with_model_ref_override(Some("alt".into()));
    let state = ingress
        .submit_background(over)
        .await
        .expect("durable submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    assert_eq!(
        messages.last().unwrap().text_content(),
        "ALT",
        "the per-run override selected the model, overriding the snapshot binding"
    );
}

#[tokio::test]
async fn durable_run_drains_live_inbox_steer_at_the_boundary() {
    // ADR-0054 P2: a steer message offered into the durable ingress's per-session
    // inbox is drained by the worker-driven run at its safe loop boundary — steer
    // reaches a durable (worker-driven) run, not only the direct native path.
    use awaken_agent_contract::agent::message::{Id as MessageId, Message};
    use awaken_runtime_contract::live_inbox::MessageOrigin;

    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    // Queue a steer before the run is driven; the worker drains the *same* inbox.
    let _ = ingress.live_inbox().offer_as(
        MessageOrigin::External,
        Message::text(MessageId("client-id".into()), Role::User, "steer me"),
    );

    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("durable submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    let steer = messages
        .iter()
        .find(|m| m.id.0 == "run-1-inbox-0")
        .expect("steer drained + re-identified into the durable transcript");
    assert_eq!(steer.text_content(), "steer me");
    // The caller-supplied id never reaches the committed transcript.
    assert!(messages.iter().all(|m| m.id.0 != "client-id"));
}

#[tokio::test]
async fn direct_ingress_fails_durable_submit_closed_while_durable_does_not() {
    // G5: the durable-only operation (submit_background) fails closed on direct
    // ingress and succeeds on durable ingress.
    let runtime = text_runtime();
    let direct = DirectRunIngress::new(runtime.clone());
    let err = direct
        .submit_background(activation("run-1"))
        .await
        .expect_err("direct has no durable submit");
    assert!(matches!(
        err,
        awaken_runtime_contract::execution::Error::Execution(_)
    ));

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let durable = DurableRunIngress::new(runtime, store, commit);
    assert_eq!(durable.capabilities(), RunIngressCapabilities::DURABLE);
    assert!(durable.submit_background(activation("run-1")).await.is_ok());
}

#[tokio::test]
async fn durable_submit_is_idempotent_per_run() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    ingress
        .submit_background(activation("run-1"))
        .await
        .expect("first submit");
    // Re-submitting the same run id is a no-op: the dispatch was already settled,
    // and re-enqueue does not create a second run or a second commit.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("second submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // Two commits (input + terminal) from the first submit; the re-submit
    // adds none.
    assert_eq!(commit.commit_count(), 2, "the run committed exactly once");
}

#[tokio::test]
async fn awaiting_run_resumes_through_delivered_input() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // A durable submit awaits on the gate.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit awaits");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "tool must not run while awaiting"
    );
    assert_eq!(
        store.dispatch_count(),
        1,
        "the awaiting dispatch is retained"
    );

    // Delivering an allow decision wakes and resumes the run to completion.
    let resumed = ingress
        .deliver_resume(
            pending(
                "msg-1",
                "run-1",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("resume");
    assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "allow runs the pending tool once"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the finished dispatch is removed"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("echoed"))
    );
}

#[tokio::test]
async fn installed_attempt_executor_drives_the_durable_resume_path() {
    let (runtime, _) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit);

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .expect("native first attempt awaits"),
        RunState::Awaiting
    );

    // Model a worker/backend replacement between attempts. The retained dispatch
    // and ticket remain authoritative; only attempt execution is replaced.
    let selected = Arc::new(RecordingAttemptExecutor::default());
    ingress.install_attempt_executor(selected.clone());
    assert_eq!(
        ingress
            .deliver_resume(
                pending(
                    "resume-selected",
                    "run-1",
                    ResumeResult::Decision {
                        allow: true,
                        note: None,
                    },
                ),
                1,
            )
            .await
            .expect("selected executor resumes"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(selected.executes.load(Ordering::SeqCst), 0);
    assert_eq!(selected.resumes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_pending_delivery_is_idempotent() {
    let store = MemoryDispatchStore::new();
    let input = pending("msg-1", "run-1", ResumeResult::Input("hi".to_string()));
    assert!(
        store.append(input.clone()).await.unwrap(),
        "first append stores"
    );
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op"
    );
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 1);
}

#[tokio::test]
async fn expired_lease_is_reclaimable_for_recovery() {
    let store = MemoryDispatchStore::new();
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // First claim takes a 1000ms lease at t=0.
    assert!(
        store
            .claim("worker-a", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "a fresh run is claimable"
    );
    // While the lease holds, the run is not re-claimable.
    assert!(
        store
            .claim("worker-b", 1_000, 500, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a held lease blocks a second claim"
    );
    // After the lease expires, recovery reclaims it.
    let recovered = store
        .claim("worker-b", 1_000, 1_001, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("worker-b".to_string()),
        "an expired lease is reclaimed by the next worker"
    );
}

#[tokio::test]
async fn worker_recovery_runs_a_crashed_dispatch_to_completion() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Simulate a worker that claimed a run, then crashed before executing it:
    // enqueue and claim directly, leaving a held lease and no committed run.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("dead-worker", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        commit.commit_count(),
        0,
        "the crashed attempt committed nothing"
    );

    // Recovery after the lease expires reclaims and completes the run.
    let processed = ingress.recover(2_000).await.expect("recover");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        commit.commit_count(),
        2,
        "recovery ran the run exactly once (input + terminal commits)"
    );
    assert_eq!(store.dispatch_count(), 0);
}

#[tokio::test]
async fn settle_done_clears_pending_and_dispatch() {
    // A store-level invariant: settling Done removes the dispatch and any pending.
    let store = MemoryDispatchStore::new();
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    store
        .append(pending(
            "msg-1",
            "run-1",
            ResumeResult::Input("x".to_string()),
        ))
        .await
        .unwrap();
    // Settle is authority-bearing: obtain the exact claim epoch first. Epoch 0
    // means "never claimed" and is not execution authority.
    let claimed = store
        .claim_run(
            &RunId("run-1".to_string()),
            "settle-test",
            1_000,
            0,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("claim before settle");
    store
        .settle(
            &RunId("run-1".to_string()),
            claimed.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);
}

#[tokio::test]
async fn committed_resume_is_not_reapplied_after_a_crash(/* M1 */) {
    // Model the crash window: a resume commits, but the worker dies before it
    // settles. The dispatch is left 'running' with the pending input still
    // present and the ticket already cleared. Recovery must NOT re-run the tool.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime.clone(), store.clone(), commit.clone());

    // Await, then deliver input WITHOUT driving (just append).
    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
    );
    store
        .append(pending(
            "msg-1",
            "run-1",
            ResumeResult::Decision {
                allow: true,
                note: None,
            },
        ))
        .await
        .unwrap();

    // Worker got partway: it claimed (took a lease) and committed the resume,
    // then crashed before settle. Drive those two steps by hand.
    let _claimed = store
        .claim("dead-worker", 1_000, 0, &Default::default())
        .await
        .unwrap();
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(allow_command(), commit.as_ref(), context)
        .await
        .expect("resume commits");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the tool ran once before the crash"
    );

    // Recovery after the lease expires: the committed run is terminal, so the
    // worker settles it without re-running the tool, and clears the pending.
    let processed = ingress.recover(2_000).await.expect("recover");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "recovery did not re-apply the resume"
    );
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);
}

/// Cause/effect decision table for a legacy quiescent delivery row:
///
/// | Rule | Dispatch | Committed Run | Expected effect |
/// |------|----------|---------------|-----------------|
/// | R1 | Awaiting, no input | absent/nonterminal | keep row; no completion |
/// | R2 | Awaiting, no input | Ended | fenced Done; emit tombstone; never execute |
/// | R3 | R1 precedes R2 and limit=1 | mixed | skip R1 and still repair one R2 |
/// | R4 | expired Running recovery claim | Ended | reclaim; fenced Done; never execute |
///
/// Constraints: committed Run truth is the only terminal cause; queue order is
/// not outcome evidence; the limit bounds repaired terminals, not inspected rows;
/// a reconciliation-process crash cannot strand its own expired claim.
#[tokio::test]
async fn reconciliation_repairs_committed_terminal_awaiting_and_expired_rows() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime.clone(), store.clone(), commit.clone());

    // R1 is deliberately first so it cannot consume the one-repair budget.
    let mut control = activation("run-control");
    control.thread_id = ThreadId("thread-control".to_string());
    store
        .enqueue(RunDispatch::new(control))
        .await
        .expect("enqueue nonterminal control");
    let control_claim = store
        .claim("setup", 1_000, 0, &Default::default())
        .await
        .expect("claim nonterminal control")
        .expect("control is runnable");
    store
        .settle(
            &control_claim.lease.run_id,
            control_claim.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("make control quiescent");

    assert_eq!(
        ingress
            .submit_background(activation("run-terminal"))
            .await
            .expect("terminal candidate reaches await"),
        RunState::Awaiting
    );
    let mut command = allow_command();
    command.run_id = RunId("run-terminal".to_string());
    let state = runtime
        .resume(
            command,
            commit.as_ref(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("model historical commit-before-settle gap");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    store
        .claim_for_terminal_recovery(
            &RunId("run-terminal".to_string()),
            "crashed-reconciler",
            10,
            1_000,
        )
        .await
        .expect("claim terminal before modeled crash")
        .expect("terminal awaiting row is claimable");

    let reconciled = ingress
        .reconcile_committed_terminals(2_000, 1)
        .await
        .expect("reconcile committed terminal");
    assert_eq!(
        reconciled,
        vec![(
            RunId("run-terminal".to_string()),
            RunState::Ended(EndCause::NaturalEnd),
        )]
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "R2/R4 never re-execute");

    let rows = store.list_dispatches().await.expect("remaining rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, RunId("run-control".to_string()), "R1");
    let completions = store
        .completion_events_after(0, 10)
        .await
        .expect("completion tombstone");
    assert_eq!(completions.len(), 1);
    assert_eq!(
        completions[0].run_id,
        RunId("run-terminal".to_string()),
        "R2"
    );
}

#[tokio::test]
async fn input_for_a_superseded_ticket_is_not_delivered(/* M1 */) {
    // Input whose correlation does not match the run's committed ticket is stale;
    // it is dropped without delivery, and the run stays awaiting until the right
    // input arrives.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
    );

    // Stale input (wrong correlation) does not resume the run.
    let state = ingress
        .deliver_resume(
            pending_for(
                "stale",
                "run-1",
                "some-old-ticket",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("stale delivery");
    assert_eq!(
        state,
        RunState::Awaiting,
        "a stale input leaves the run awaiting"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "the tool did not run");
    assert_eq!(
        store.pending_count(&RunId("run-1".to_string())),
        0,
        "the stale input was dropped"
    );

    // The correctly-correlated input resumes the run.
    let state = ingress
        .deliver_resume(
            pending(
                "good",
                "run-1",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("good delivery");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_edit_and_retract_are_revision_guarded() {
    // M3a: the in-memory store is the spec for revision-guarded pending ops.
    harness::assert_pending_revision_cas(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cross_thread_outbox_store_spec() {
    harness::assert_cross_thread_outbox(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn message_idempotency_conflicts_store_spec() {
    harness::assert_message_idempotency_conflicts(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn staged_delivery_relays_and_resumes_an_awaiting_run() {
    // M3b end to end: an awaiting run is resumed by a cross-thread delivery that is
    // staged in the outbox and relayed to its pending input.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
    );

    // Stage the delivery (as if from another thread); it is not pending yet.
    let staged = ingress
        .stage_cross_thread(pending(
            "x1",
            "run-1",
            ResumeResult::Decision {
                allow: true,
                note: None,
            },
        ))
        .await
        .unwrap();
    assert!(staged);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);

    // Relay moves it to pending and drives the run to completion.
    let processed = ingress.relay_outbox(0).await.expect("relay");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scheduled_delivery_due_store_spec() {
    harness::assert_scheduled_due(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn millis_boundaries_store_spec() {
    harness::assert_millis_boundaries(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn dead_letter_budget_store_spec() {
    harness::assert_dead_letter(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cancel_store_spec() {
    harness::assert_cancel(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cancel_durable_commits_cancelled_for_an_awaiting_run() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
    );
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_some()
    );

    // Durable cancel commits a terminal Cancelled and clears the ticket.
    assert!(
        ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
    let record = awaken_agent_contract::thread::read::run_store::RunStore::get(
        commit.as_ref(),
        &RunId("run-1".to_string()),
    )
    .expect("run record");
    assert_eq!(record.state, RunState::Ended(EndCause::Cancelled));
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unified_run_service_cancel_is_durable_for_a_queued_run() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Enqueue without driving, then cancel: a terminal Cancelled is committed
    // even though the run never executed.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    RunService::cancel(&ingress, &RunId("run-1".to_string()))
        .await
        .expect("unified cancel");
    let record = awaken_agent_contract::thread::read::run_store::RunStore::get(
        commit.as_ref(),
        &RunId("run-1".to_string()),
    )
    .expect("run record");
    assert_eq!(record.state, RunState::Ended(EndCause::Cancelled));
    assert_eq!(store.dispatch_count(), 0);

    // Cancelling again fails closed as no longer active/queued.
    assert_eq!(
        RunService::cancel(&ingress, &RunId("run-1".to_string())).await,
        Err(awaken_runtime_contract::control::Error::NotActive)
    );
}

#[tokio::test]
async fn durable_cancel_invokes_the_selected_executor_before_terminal_commit() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());
    let selected = Arc::new(RecordingAttemptExecutor::default());
    ingress.install_attempt_executor(selected.clone());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    RunService::cancel(&ingress, &RunId("run-1".to_string()))
        .await
        .expect("durable cancellation");

    assert_eq!(selected.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(
        awaken_agent_contract::thread::read::run_store::RunStore::get(
            commit.as_ref(),
            &RunId("run-1".to_string()),
        )
        .expect("terminal record")
        .state,
        RunState::Ended(EndCause::Cancelled)
    );
}

#[tokio::test]
async fn committed_cancel_is_settled_without_duplicate_after_crash() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("cancel-crash".to_string());

    store
        .enqueue(RunDispatch::new(activation("cancel-crash")))
        .await
        .unwrap();
    store.cancel(&run).await.unwrap().expect("intent persisted");
    let crashed = store
        .claim("dead-worker", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("intent claimed");
    assert!(crashed.cancellation_requested);

    // Model the exact crash window: the fenced worker commits Cancelled, but the
    // process dies before settling/removing the dispatch row.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    assert_eq!(
        runtime
            .cancel_run(run.clone(), ThreadId(THREAD.to_string()), context)
            .await
            .expect("terminal cancel commit"),
        RunState::Ended(EndCause::Cancelled)
    );
    assert_eq!(store.dispatch_count(), 1, "crash left intent for recovery");

    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());
    assert_eq!(
        ingress.recover(1_001).await.expect("recovery"),
        vec![(run.clone(), RunState::Ended(EndCause::Cancelled))]
    );
    assert_eq!(store.dispatch_count(), 0);
    let record =
        awaken_agent_contract::thread::read::run_store::RunStore::get(commit.as_ref(), &run)
            .expect("single terminal record");
    assert_eq!(record.state, RunState::Ended(EndCause::Cancelled));
}

#[tokio::test]
async fn cancellation_does_not_materialize_the_model_or_credentials() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let materializations = Arc::new(AtomicUsize::new(0));
    let seen = materializations.clone();
    let resolver: awaken_run_ingress::InferenceMaterializerFn =
        Arc::new(move |_activation, _context| {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        });
    let ingress = DurableRunIngress::with_owner_and_resolver(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "cancel-owner",
        None,
        Some(resolver),
    );
    let run = RunId("cancel-with-provider-down".to_string());
    store
        .enqueue(RunDispatch::new(activation("cancel-with-provider-down")))
        .await
        .unwrap();

    assert!(ingress.cancel_durable(&run).await.unwrap());
    assert_eq!(
        materializations.load(Ordering::SeqCst),
        0,
        "terminal control must not depend on the unavailable execution provider"
    );
    assert_eq!(
        awaken_agent_contract::thread::read::run_store::RunStore::get(commit.as_ref(), &run)
            .unwrap()
            .state,
        RunState::Ended(EndCause::Cancelled)
    );
}

#[tokio::test]
async fn send_message_cannot_approve_a_threads_pending_tool() {
    use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
    use awaken_run_ingress::OutboxMessageSender;

    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // A run awaits on thread-1 for a structured tool-permission decision.
    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
    );

    // The send_message host adapter, addressed by thread, stages a delivery.
    let sender = OutboxMessageSender::new(store.clone(), commit.clone() as Arc<dyn ThreadReader>);
    sender
        .send(send_request(
            "thread-1",
            "hello from another agent",
            "permission-send",
        ))
        .await
        .expect("send to an awaiting thread");
    // Sending to a thread with no awaiting run is staged unbound (ADR-0021), not
    // an error: it is held for that thread's next run.
    sender
        .send(send_request("thread-2", "for later", "idle-send"))
        .await
        .expect("idle-thread send is queued");

    // Relaying persists the message, but it cannot consume the permission ticket.
    let processed = ingress.relay_outbox(0).await.expect("relay");
    assert!(processed.is_empty());
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "an ordinary message does not approve the gated tool"
    );
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_some(),
        "the approval ticket remains authoritative"
    );
    let queued = store.list(&ThreadId("thread-1".to_string())).await.unwrap();
    assert!(queued.iter().any(|record| {
        record.input.run_id.0.is_empty()
            && matches!(
                &record.input.result,
                ResumeResult::Input(text) if text == "hello from another agent"
            )
    }));
}

#[tokio::test]
async fn priority_dedupe_gc_store_spec() {
    harness::assert_priority_dedupe_gc(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn lease_renewal_store_spec() {
    harness::assert_lease_renewal(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn local_wake_signal_delivers_a_held_hint() {
    use awaken_run_ingress::{LocalWakeSignal, WakeSignal};
    let wake = LocalWakeSignal::new();
    // A hint published before anyone waits is held (one permit), so wait returns.
    wake.publish().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.wait())
        .await
        .expect("the wake hint was delivered");
}

#[tokio::test]
async fn ingress_dead_letter_and_purge_ops() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    // A crashed run reaped through the ingress API.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ingress.reap(0, 100).await.unwrap(), 1);
    assert_eq!(
        ingress.dead_letters().await.unwrap(),
        vec![RunId("run-1".to_string())]
    );

    // Requeue, re-reap, then GC through the ingress API.
    assert!(ingress.requeue(&RunId("run-1".to_string())).await.unwrap());
    assert!(ingress.dead_letters().await.unwrap().is_empty());
    assert!(
        store
            .claim("w", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ingress.reap(0, 200).await.unwrap(), 1);
    assert_eq!(ingress.purge_dead_letters().await.unwrap(), 1);
    assert!(ingress.dead_letters().await.unwrap().is_empty());

    // Cancelling a run that no longer exists is false.
    assert!(
        !ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn daemon_performs_a_scheduled_action_to_completion() {
    // RS-SCH-001 over the dispatch: a durably-submitted run whose gate defers the
    // tool awaits on a committed ScheduledAction; the worker performs it in-process
    // (no external input) and the run settles Done.
    let (runtime, ran) = schedule_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the scheduled action ran once"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the run settled Done, not Awaiting"
    );
}

#[tokio::test]
async fn a_recovered_scheduled_action_is_performed() {
    // RS-SCH-006: a run that committed a ScheduledAction await and then crashed
    // before performing it (dispatch left 'running' with an expired lease) is
    // recovered by another worker and performed from the committed request.
    let (runtime, ran) = schedule_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // The run awaiting on a committed ScheduledAction (the action has not run).
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Its dispatch is a crashed in-flight claim: 'running', lease expired at 10,
    // never settled.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    store
        .claim("dead-worker", 10, 0, &Default::default())
        .await
        .unwrap();

    // A live worker recovers it after the lease expires and performs the action.
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "live-worker");
    let processed = worker.tick(100).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the recovered action ran once"
    );
    assert_eq!(store.dispatch_count(), 0, "settled Done after recovery");
}

#[tokio::test]
async fn send_message_to_an_idle_thread_feeds_the_next_run() {
    // ADR-0021: a message to a thread with no awaiting run is queued unbound, then
    // consumed by the thread's next run as new input.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Agent A messages a thread with no run in flight: it is staged unbound.
    let sender = OutboxMessageSender::new(store.clone(), commit.clone());
    sender
        .send(send_request(THREAD, "hello from A", "idle-next-run"))
        .await
        .expect("send to idle thread");

    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());
    ingress.relay_outbox(0).await.expect("relay");
    let listed = store.list(&ThreadId(THREAD.to_string())).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].input.run_id.0.is_empty(),
        "queued input is unbound"
    );

    // The thread's next run consumes the queued message as new input.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let assistant = commit
        .committed()
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply")
        .clone();
    assert!(
        assistant.text_content().contains("hello from A"),
        "the idle-thread message reached the run input"
    );
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .is_empty(),
        "the unbound input is consumed"
    );
}

#[tokio::test]
async fn send_message_identity_is_stable_across_sender_instances_and_optional_keys() {
    // CE-SM4..SM7 decision rules:
    // - same operation across a replacement sender -> one durable message;
    // - distinct operations -> distinct messages;
    // - an optional caller key dedupes distinct operations in the same source Run;
    // - reusing that key with changed payload -> explicit conflict;
    // - the same caller key in another source Run -> a distinct message.
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let first = OutboxMessageSender::new(store.clone(), commit.clone());
    let replacement = OutboxMessageSender::new(store.clone(), commit);

    first
        .send(send_request(THREAD, "retry", "stable-op"))
        .await
        .unwrap();
    replacement
        .send(send_request(THREAD, "retry", "stable-op"))
        .await
        .unwrap();
    replacement
        .send(send_request(THREAD, "second", "different-op"))
        .await
        .unwrap();

    let keyed = |content: &str, operation: &str| MessageSendRequest {
        target_thread: THREAD.to_string(),
        content: content.to_string(),
        idempotency_key: Some("caller-key".to_string()),
        source_run_id: "source-run".to_string(),
        operation_id: operation.to_string(),
    };
    first.send(keyed("keyed", "key-op-1")).await.unwrap();
    replacement.send(keyed("keyed", "key-op-2")).await.unwrap();
    let conflict = replacement
        .send(keyed("changed", "key-op-3"))
        .await
        .expect_err("changed payload under one optional key must conflict");
    assert!(conflict.to_string().contains("idempotency key"));

    replacement
        .send(MessageSendRequest {
            target_thread: THREAD.to_string(),
            content: "cross-run".to_string(),
            idempotency_key: Some("caller-key".to_string()),
            source_run_id: "another-source-run".to_string(),
            operation_id: "key-op-4".to_string(),
        })
        .await
        .expect("the same caller key in another source Run is a distinct identity");

    assert_eq!(store.relay().await.unwrap(), 4);
    let messages = store.list(&ThreadId(THREAD.to_string())).await.unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                matches!(&message.input.result, ResumeResult::Input(text) if text == "retry")
            })
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                matches!(&message.input.result, ResumeResult::Input(text) if text == "keyed")
            })
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                matches!(&message.input.result, ResumeResult::Input(text) if text == "cross-run")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn supersession_store_spec() {
    harness::assert_supersession(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn settle_fences_stale_epoch_store_spec() {
    harness::assert_settle_fences_stale_epoch(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn a_superseded_owners_commit_is_fenced_while_the_current_owners_lands() {
    // dispatch-fencing-token-gap closed. A run commits per step while it executes, so a
    // stale owner — its lease lapsed and a peer re-claimed under a higher epoch — must
    // NOT be able to write to the thread, or both owners double-apply side effects. The
    // fenced commit boundary rejects the stale owner's write before it reaches the
    // durable boundary, while the current owner's write lands. This is the commit twin
    // of the already-covered settle fence.
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{
        ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit, RunClaim,
    };

    let store = Arc::new(MemoryDispatchStore::new());
    let inner = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Owner A claims (epoch 1); its lease lapses; owner B reclaims (epoch 2).
    let a = store
        .claim("owner-a", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    let b = store
        .claim("owner-b", 100, 200, &Default::default())
        .await
        .unwrap()
        .expect("B reclaims");
    assert_eq!((a.lease.epoch, b.lease.epoch), (1, 2));

    let plan = |run: &RunId| {
        ThreadCommit::assemble(
            ThreadId(THREAD.to_string()),
            RunDisposition::running(run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    };
    let run = RunId("run-1".to_string());

    // A (stale epoch 1) is fenced — nothing reaches the durable boundary.
    let service: Arc<dyn ClaimedRunCommit> =
        Arc::new(GuardedRunCommit::new(inner.clone(), store.clone()));
    let fenced_a = ClaimedCommitCoordinator::new(service.clone(), RunClaim::from(&a.lease));
    assert!(
        fenced_a.commit(plan(&run)).await.is_err(),
        "a superseded owner's commit must be fenced"
    );
    assert_eq!(
        inner.commit_count(),
        0,
        "the fenced commit never reached the boundary"
    );

    // B (current epoch 2) commits through.
    let fenced_b = ClaimedCommitCoordinator::new(service.clone(), RunClaim::from(&b.lease));
    fenced_b
        .commit(plan(&run))
        .await
        .expect("the current owner commits");
    assert_eq!(inner.commit_count(), 1, "the current owner's commit landed");

    // Fail-closed: a run with no live dispatch has no execution capability.
    let ghost = RunId("ghost".to_string());
    let fenced_ghost = ClaimedCommitCoordinator::new(
        service,
        RunClaim {
            run_id: ghost.clone(),
            owner: "ghost-owner".to_string(),
            epoch: 7,
        },
    );
    assert!(fenced_ghost.commit(plan(&ghost)).await.is_err());
    assert_eq!(
        inner.commit_count(),
        1,
        "the unauthorized commit was fenced"
    );
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_memory() {
    harness::assert_concurrent_recovery_yields_one_winner(std::sync::Arc::new(
        MemoryDispatchStore::new(),
    ))
    .await;
}

#[tokio::test]
async fn awaiting_settle_fences_stale_epoch_store_spec() {
    harness::assert_awaiting_settle_fences_stale_epoch(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn submit_superseding_abandons_prior_thread_work() {
    // ADR-0022 at the ingress: a superseding submit supersedes the thread's
    // awaiting run; only the newest run stays live.
    let (runtime, _ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // An older run awaits on the thread.
    assert_eq!(
        ingress.submit_background(activation("old")).await.unwrap(),
        RunState::Awaiting
    );
    // A superseding submit on the same thread abandons the awaiting run.
    ingress
        .submit_superseding(activation("new"))
        .await
        .expect("superseding submit");
    assert_eq!(
        ingress.superseded().await.unwrap(),
        vec![RunId("old".to_string())],
        "the prior awaiting run is superseded"
    );
    // The superseded run is never woken again: recovery does not process it.
    assert!(
        !ingress
            .recover(0)
            .await
            .unwrap()
            .iter()
            .any(|(run, _)| run.0 == "old"),
        "a superseded run is not claimable"
    );
}

#[tokio::test]
async fn dead_letter_ttl_gc_store_spec() {
    harness::assert_dead_letter_ttl_gc(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn renew_owned_leases_store_spec() {
    harness::assert_renew_owned_leases(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn renew_skips_far_from_expiry_store_spec() {
    harness::assert_renew_skips_far_from_expiry(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn list_dispatches_store_spec() {
    harness::assert_list_dispatches(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn dedupe_ignores_dead_lettered_store_spec() {
    harness::assert_dedupe_ignores_dead_lettered(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn wake_suppressed_while_thread_running_store_spec() {
    harness::assert_wake_suppressed_while_thread_running(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn ingress_lists_dispatches_and_purges_aged_dead_letters() {
    // Covers the DurableRunIngress query + time-windowed GC wrappers.
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let listed = ingress.list_dispatches().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, RunId("run-1".to_string()));

    // Dead-letter it at t=1000, then age it out through the ingress.
    assert!(
        store
            .claim("w", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ingress.reap(0, 1_000).await.unwrap(), 1);
    assert_eq!(ingress.purge_dead_letters_before(999).await.unwrap(), 0);
    assert_eq!(ingress.purge_dead_letters_before(1_000).await.unwrap(), 1);
    assert!(ingress.list_dispatches().await.unwrap().is_empty());
}
