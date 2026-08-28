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
use awaken_agent_contract::audit::kind::Kind as AuditKind;
use awaken_ext_builtin_tools::{MessageSendRequest, MessageSender};
use awaken_run_ingress::PlacementRequirements;
use awaken_run_ingress::{
    DEFAULT_LEASE_MS, DispatchOutcome, DispatchQueue, DispatchSettlementError,
    DispatchSettlementObserver, DispatchWorker, DurableRunIngress, Inbox, ManualClock,
    MemoryDispatchStore, Outbox, OutboxMessageSender, PendingInput, RunClaim, RunDispatch,
    RunIngressCapabilities, SessionChildAdmission,
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

struct RecordingSettlementObserver {
    fail_remaining: AtomicUsize,
    observed: Mutex<Vec<(String, String, RunState, bool)>>,
}

impl RecordingSettlementObserver {
    fn new(fail_remaining: usize) -> Self {
        Self {
            fail_remaining: AtomicUsize::new(fail_remaining),
            observed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for RecordingSettlementObserver {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        self.observed.lock().unwrap().push((
            dispatch.run_id().0.clone(),
            claim.run_id.0.clone(),
            committed_state.clone(),
            cancellation_requested,
        ));
        if self
            .fail_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(DispatchSettlementError("injected report outage".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn coordinated_settlement_observer_follows_the_durable_boundary_decision_table() {
    // Cause/effect graph: C1 dispatch has/omits an activity epoch; C2 committed
    // boundary is Awaiting/Ended; C3 observer succeeds/fails; C4 failed claim is
    // recovered after lease expiry; C5 an Awaiting coordinated Run is cancelled.
    // Effects: E1 ordinary Runs bypass the hook;
    // E2 coordinated boundaries deliver trusted dispatch+claim+committed state
    // before queue settlement; E3 observer failure retains the leased row and
    // committed Run evidence; E4 recovery redelivers without re-execution and
    // only then removes Done; E5 cancellation is observed as trusted provenance,
    // the Awaiting tool receives the fixed interruption error, the model
    // is not resumed, and the row reaches Done. The row plus committed Run are the sole retry
    // evidence—there is no CompletionSink task or observer receipt map.
    //
    // | Rule | Epoch | State | Cancel | Observer/recovery | Effect |
    // |---|---|---|---|---|---|
    // | S1 | none | Ended | no | would fail/no | E1, Done |
    // | S2 | some | Awaiting | no | success/no | E2, Awaiting |
    // | S3 | some | Ended | no | fail/no | E2+E3, Leased |
    // | S4 | some | Ended committed | no | success/yes | E4, Done |
    // | S5 | some | Awaiting | yes | success/no | E5, Done |
    // Constraint/Invariant: the dispatch row plus committed Run are the sole
    // retry evidence; no observer receipt or completion task is authoritative.
    // Decision rule: S1-S5 cover epoch presence, both boundaries, observer
    // success/failure, recovery, and cancellation.
    let ordinary_store = Arc::new(MemoryDispatchStore::new());
    let ordinary_observer = Arc::new(RecordingSettlementObserver::new(1));
    let ordinary = DispatchWorker::new(
        text_runtime(),
        ordinary_store.clone(),
        Arc::new(MemoryCommitCoordinator::new()),
        "ordinary-worker",
    )
    .with_settlement_observer(ordinary_observer.clone());
    assert!(
        ordinary
            .start_run(
                RunDispatch::new(activation("ordinary-observer")),
                harness::clock(0),
            )
            .await
            .expect("S1 ordinary drive")
            .is_some()
    );
    assert!(
        ordinary_observer.observed.lock().unwrap().is_empty(),
        "S1/E1"
    );
    assert!(
        ordinary
            .start_run(
                RunDispatch::new(activation("ordinary-session-child"))
                    .for_session(ThreadId("parent-session".into())),
                harness::clock(1),
            )
            .await
            .expect("S1 ordinary session-affined child drive")
            .is_some()
    );
    assert!(
        ordinary_observer.observed.lock().unwrap().is_empty(),
        "S1/E1 session affinity alone does not classify coordination"
    );

    let awaiting_store = Arc::new(MemoryDispatchStore::new());
    let awaiting_observer = Arc::new(RecordingSettlementObserver::new(0));
    let awaiting_clock = Arc::new(ManualClock::new(0));
    let (awaiting_runtime, awaiting_tool_runs) = tool_runtime();
    let awaiting_commit = Arc::new(MemoryCommitCoordinator::new());
    let awaiting = DispatchWorker::new(
        awaiting_runtime,
        awaiting_store.clone(),
        awaiting_commit.clone(),
        "awaiting-worker",
    )
    .with_settlement_observer(awaiting_observer.clone());
    awaiting_store
        .enqueue_session_child(
            RunDispatch::new(activation("coordinated-awaiting"))
                .for_session(ThreadId("parent-session".into()))
                .with_session_activity_epoch(7),
            SessionChildAdmission::new(25, Vec::new()),
        )
        .await
        .expect("S2 admit child");
    let awaiting_result = awaiting
        .tick(awaiting_clock.clone())
        .await
        .expect("S2 coordinated drive")
        .expect("S2 applied");
    assert_eq!(awaiting_result.1, RunState::Awaiting, "S2/E2");
    assert_eq!(awaiting_observer.observed.lock().unwrap().len(), 1, "S2/E2");
    assert_eq!(
        awaiting_store.list_dispatches().await.unwrap()[0].state,
        awaken_run_ingress::DispatchState::Awaiting,
        "S2/E2"
    );
    assert_eq!(
        awaiting_store
            .cancel(&RunId("coordinated-awaiting".into()))
            .await
            .expect("S5 durable cancellation"),
        Some(ThreadId(THREAD.into())),
        "S5 cancellation targets the trusted dispatch Thread"
    );
    awaiting_clock.set(1);
    let interrupted = awaiting
        .tick(awaiting_clock.clone())
        .await
        .expect("S5 cancellation drive")
        .expect("S5 applied");
    assert_eq!(
        interrupted.1,
        RunState::Ended(EndCause::NaturalEnd),
        "S5/E5 Awaiting-tool interruption ends without resampling"
    );
    assert_eq!(awaiting_tool_runs.load(Ordering::SeqCst), 0, "S5/E5");
    assert!(
        awaiting_store.list_dispatches().await.unwrap().is_empty(),
        "S5/E5"
    );
    let interrupted_results = awaiting_commit
        .committed()
        .messages
        .into_iter()
        .flat_map(|message| message.content)
        .filter_map(|block| match block {
            awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                content,
                is_error,
                ..
            } => Some((
                awaken_agent_contract::agent::content::extract_text(&content),
                is_error,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        interrupted_results,
        vec![(
            "Tool execution was interrupted before completion. Please retry.".into(),
            true,
        )],
        "S5/E5 fixed ToolResult"
    );
    {
        let awaiting_observed = awaiting_observer.observed.lock().unwrap();
        assert_eq!(awaiting_observed.len(), 2, "S2+S5");
        assert!(!awaiting_observed[0].3, "S2/E2");
        assert!(awaiting_observed[1].3, "S5/E5 trusted cancellation");
    }

    let terminal_store = Arc::new(MemoryDispatchStore::new());
    let terminal_commit = Arc::new(MemoryCommitCoordinator::new());
    let terminal_observer = Arc::new(RecordingSettlementObserver::new(1));
    let executor = Arc::new(RecordingAttemptExecutor::default());
    let terminal_clock = Arc::new(ManualClock::new(0));
    let terminal = DispatchWorker::new(
        text_runtime(),
        terminal_store.clone(),
        terminal_commit,
        "terminal-worker",
    )
    .with_settlement_observer(terminal_observer.clone());
    terminal.install_attempt_executor(executor.clone());
    let request = RunDispatch::new(activation("coordinated-terminal"))
        .for_session(ThreadId("parent-session".into()))
        .with_session_activity_epoch(8);
    terminal_store
        .enqueue_session_child(request, SessionChildAdmission::new(25, Vec::new()))
        .await
        .expect("S3 admit child");
    assert!(
        terminal.tick(terminal_clock.clone()).await.is_err(),
        "S3/E3"
    );
    let retained = terminal_store.list_dispatches().await.unwrap();
    assert_eq!(retained.len(), 1, "S3/E3");
    assert_eq!(
        retained[0].state,
        awaken_run_ingress::DispatchState::Leased,
        "S3/E3"
    );

    terminal_clock.set(30_001);
    let recovered = terminal
        .tick(terminal_clock.clone())
        .await
        .expect("S4 recovery")
        .expect("S4 redelivered terminal");
    assert!(matches!(recovered.1, RunState::Ended(_)), "S4/E4");
    assert!(
        terminal_store.list_dispatches().await.unwrap().is_empty(),
        "S4/E4"
    );
    assert_eq!(executor.executes.load(Ordering::SeqCst), 1, "S4 no replay");
    let observed = terminal_observer.observed.lock().unwrap();
    assert_eq!(observed.len(), 2, "S3+S4 exact redelivery");
    assert!(
        observed
            .iter()
            .all(|(dispatch_run, claim_run, state, cancelled)| {
                dispatch_run == claim_run && matches!(state, RunState::Ended(_)) && !cancelled
            })
    );
}

#[tokio::test]
async fn an_unclaimable_parent_mediated_run_remains_scheduled_for_a_compatible_worker() {
    // Cause/effect graph and decision table:
    // C1 placement is locally claimable -> E1 start_run claims and drives it;
    // C2 placement is RemoteRequired -> E2 local start returns None and the one
    // durable queue retains the exact pending Run for a registered Worker;
    // C3 the same stable request is retried -> E3 enqueue remains idempotent.
    // R1=C1/E1 is covered by `a_persisted_frozen_candidate_routes_through_the_gateway`.
    // R2=C2/E2 and R3=C2+C3/E2+E3 are owned here. FMECA: dropping C2 after a
    // failed local claim leaves a parent waiting for a child boundary that no
    // Worker can ever observe; the pending-row assertion detects that loss.
    // Constraint/Invariant: inability to claim locally cannot delete or fork the
    // one durable dispatch. Decision rule: execute R2 and its idempotent retry R3;
    // R1 remains covered by the named local-routing test.
    let store = Arc::new(MemoryDispatchStore::new());
    let worker = DispatchWorker::new(
        text_runtime(),
        store.clone(),
        Arc::new(MemoryCommitCoordinator::new()),
        "parent-worker",
    );
    let request = RunDispatch::new(activation("remote-child"))
        .with_placement(PlacementRequirements::remote_required());

    assert!(
        worker
            .start_run(request.clone(), harness::clock(0))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        worker
            .start_run(request, harness::clock(1))
            .await
            .unwrap()
            .is_none()
    );

    let rows = store.list_dispatches().await.unwrap();
    assert_eq!(rows.len(), 1, "R2+R3/E2+E3");
    assert_eq!(rows[0].run_id.0, "remote-child");
    assert_eq!(rows[0].state, awaken_run_ingress::DispatchState::Pending);
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

struct ExpiringOwnershipAttemptExecutor {
    clock: Arc<ManualClock>,
    effects: AtomicUsize,
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

#[async_trait::async_trait]
impl RunExecutor for ExpiringOwnershipAttemptExecutor {
    async fn execute(
        &self,
        _activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.clock.set(DEFAULT_LEASE_MS + 1);
        awaken_runtime_contract::execution::verify_attempt_ownership(context.ownership.as_deref())
            .await?;
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(RunState::Ended(EndCause::NaturalEnd))
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for ExpiringOwnershipAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        _command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.execute(activation, context).await
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
    .expect("coherent durable provider candidate")
}

fn allow_command() -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
        result: ResumeResult::allow(),
        context_messages: Vec::new(),
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

    // Committed truth holds the Run: the user message then the assistant reply.
    // Per-step durability: the input commits at the first step boundary, the
    // terminal Step through finish.
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
    // Clock-authority cause/effect table. C1 is the one edge ManualClock, C2 is
    // claim eligibility, and C3 is its value at the pre-effect verifier. E1 is
    // claim+verify on one timeline, E2 is one external effect, and E3 is a
    // fail-closed attempt with no effect.
    //
    // | Rule | Claim time | Verify time | Expected effect |
    // |---|---|---|---|
    // | CA1 | 7 | 7 (live) | E1+E2 |
    // | CA2 | 0 | lease+1 (expired) | E1+E3 |
    //
    // CA1 specifically prevents a Worker-private SystemClock from judging a
    // deterministic edge claim as already expired. CA2 below proves the inverse:
    // advancing that same source is observed by the verifier before any effect.
    // Constraint/Invariant: claim and pre-effect verification read the same live
    // Clock authority. Decision rule: this test owns CA1; the adjacent expiry
    // test owns complementary rule CA2.
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let clock = Arc::new(ManualClock::new(7));
    let selected = Arc::new(OwnershipCheckingAttemptExecutor {
        verified: AtomicUsize::new(0),
    });
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit, "worker");
    worker.install_attempt_executor(selected.clone());
    store
        .enqueue(RunDispatch::new(activation("ownership-run")))
        .await
        .expect("dispatch enqueued");

    assert_eq!(
        worker.tick(clock).await.expect("claimed attempt completes"),
        Some((
            RunId("ownership-run".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(selected.verified.load(Ordering::SeqCst), 1, "CA1/E1+E2");
}

#[tokio::test]
async fn advancing_the_drive_clock_expires_ownership_before_any_effect() {
    // Causes: CA2 claims at time zero, then the executor advances the exact Clock
    // past lease expiry. Effects: the claim-bound verifier rejects and the
    // external-effect count remains zero. Constraint/Invariant: verification and
    // claim share one Clock authority. Decision rule: execute complementary CA2
    // from the adjacent clock table and require fail-closed pre-effect behavior.
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let clock = Arc::new(ManualClock::new(0));
    let selected = Arc::new(ExpiringOwnershipAttemptExecutor {
        clock: clock.clone(),
        effects: AtomicUsize::new(0),
    });
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit, "worker");
    worker.install_attempt_executor(selected.clone());
    store
        .enqueue(RunDispatch::new(activation("expired-ownership-run")))
        .await
        .expect("dispatch enqueued");

    let error = worker
        .tick(clock)
        .await
        .expect_err("CA2 expired ownership fails closed");
    assert!(
        error
            .to_string()
            .contains("no longer owns external execution"),
        "CA2/E3: {error}"
    );
    assert_eq!(selected.effects.load(Ordering::SeqCst), 0, "CA2/E3");
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
    // Test design. Causes: C1 the Worker has no provider secret; C2 the frozen
    // candidate carries snapshot-pinned gateway access; C3 a resolver observes
    // that access. Effects: E1 C2+C3 materializes the gateway executor; E2 the
    // Run returns gateway output without ambient credentials. Constraint/
    // Invariant: credentials come only from the frozen access descriptor, never
    // Worker environment fallback. Decision rule: exercise the secretless,
    // descriptor-present branch and assert both observed access and output.
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
        .start_run(request, harness::clock(0))
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
    // This proves the per-Run switch reaches the materialization seam without
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
    // Test design. Causes: C1 a Run commits Awaiting with an exact correlation;
    // C2 matching input is delivered; C3 the Worker ticks again. Effects: E1 no
    // tool runs before C2; E2 C2+C3 resumes once and commits terminal output; E3
    // dispatch and pending input settle away. Constraint/Invariant: committed
    // ticket identity, not queue presence alone, admits resume. Decision rule:
    // cover pre-input Awaiting and matching-input terminal transitions.
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
            pending("msg-1", "run-1", ResumeResult::allow()),
            harness::clock(0),
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
    // Test design. Causes: C1 the native first attempt commits Awaiting; C2 an
    // alternate attempt executor is installed before matching input arrives.
    // Effects: E1 the retained dispatch/ticket selects resume on C2; E2 only the
    // installed executor records the resumed attempt. Constraint/Invariant:
    // replacing execution does not replace durable resume authority. Decision rule:
    // execute one attempt before and one after replacement, then distinguish
    // execute versus resume counters.
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
                pending("resume-selected", "run-1", ResumeResult::allow(),),
                harness::clock(1),
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
    // Cause/effect graph: C1 a worker dies after its first claim and before any
    // Thread commit; C2 the lease expires; C3 a replacement claim executes the
    // same Run. Effects: E1 recovery durably records one neutral reschedule
    // observation before execution; E2 input/running and terminal each commit
    // exactly once; E3 the dispatch is removed; E4 the successful logical model
    // request is recorded once in the terminal commit. Decision rule
    // R1=C1+C2+C3 => E1+E2+E3+E4. The audit-kind order distinguishes the
    // intentional recovery and model observation from duplicate execution, so
    // this test does not use a stale commit count as a proxy for exactly-once
    // behavior.
    // Constraint/Invariant: the same durable Run is recovered; lifecycle and
    // request audit facts remain the sole evidence. Decision rule: R1 is the
    // crash-before-first-commit partition and must yield E1-E4 exactly once.
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
    let processed = ingress
        .recover(harness::clock(2_000))
        .await
        .expect("recover");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        commit.commit_count(),
        3,
        "R1/E1-E2: reschedule observation + input + terminal commits"
    );
    assert_eq!(
        commit
            .committed()
            .events
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            AuditKind::RunRescheduled,
            AuditKind::RunStateChanged,
            AuditKind::RunStateChanged,
            AuditKind::ModelRequestCompleted,
        ],
        "R1/E1-E2+E4: recovery and the logical model request are observed once"
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
    // Test design. Causes: C1 a correlated resume commits and clears its ticket;
    // C2 the Worker crashes before settling, leaving the row Running and input
    // present; C3 recovery reclaims after expiry. Effects: E1 C3 settles from
    // committed terminal truth; E2 the tool is not run again; E3 pending input is
    // consumed. Constraint/Invariant: cleared committed ticket dominates stale
    // queue payload. Decision rule: reproduce C1+C2+C3 and require one tool run.
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
        .append(pending("msg-1", "run-1", ResumeResult::allow()))
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
    let processed = ingress
        .recover(harness::clock(2_000))
        .await
        .expect("recover");
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
/// Decision rule: execute R1-R4 together so a nonterminal prefix cannot consume
/// the one-repair limit and both Awaiting and expired-Running terminals repair.
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
        .reconcile_committed_terminals(harness::clock(2_000), 1)
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
    // Test design. Causes: C1 a Run owns committed ticket correlation A; C2 input
    // arrives with stale correlation B; C3 matching A later arrives. Effects: E1
    // C2 is dropped and the Run remains Awaiting; E2 C3 resumes once. Constraint/
    // Invariant: only exact committed correlation can authorize delivery.
    // Decision rule: exercise stale and matching partitions in that order.
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
            pending_for("stale", "run-1", "some-old-ticket", ResumeResult::allow()),
            harness::clock(0),
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
            pending("good", "run-1", ResumeResult::allow()),
            harness::clock(0),
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
    // Test design. Causes: C1 a Run is Awaiting; C2 cross-Thread input is staged
    // in the authoritative Outbox; C3 relay runs. Effects: E1 C2 alone is not yet
    // pending; E2 C3 moves it once into pending; E3 the Run resumes and settles.
    // Constraint/Invariant: Outbox-to-Inbox relay is the only cross-Thread path.
    // Decision rule: observe state before relay, after relay, and after drive.
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
        .stage_cross_thread(pending("x1", "run-1", ResumeResult::allow()))
        .await
        .unwrap();
    assert!(staged);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);

    // Relay moves it to pending and drives the run to completion.
    let processed = ingress
        .relay_outbox(harness::clock(0))
        .await
        .expect("relay");
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
    harness::assert_scheduled_due(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn millis_boundaries_store_spec() {
    harness::assert_millis_boundaries(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn dead_letter_budget_store_spec() {
    harness::assert_dead_letter(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
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
    let record =
        awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run(
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
    let record =
        awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run(
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
    // Cause/effect graph: C1 one queued activation owns accepted input; C2 a
    // durable cancel is claimed before ordinary execution; C3 the selected
    // backend cancel succeeds. Effects: E1 C3 runs once; E2 input and Cancelled
    // commit together; E3 the model is never needed. This integration guards
    // the Worker wiring; Runtime's control suite owns the commit primitive.
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
    assert_eq!(commit.committed().messages.len(), 1, "C1/E2");
    assert_eq!(commit.committed().messages[0].id.0, "m1", "C1/E2");
    assert_eq!(
        awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run(
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
    // Test design. Causes: C1 cancel intent is claimed and commits Cancelled; C2
    // the Worker crashes before settling; C3 recovery reclaims after expiry.
    // Effects: E1 C3 settles Done from committed cancellation; E2 no second cancel
    // commit or model execution occurs. Constraint/Invariant: committed terminal
    // truth dominates the still-leased dispatch. Decision rule: reproduce the
    // exact post-commit/pre-settle crash and assert one terminal fact.
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
        ingress
            .recover(harness::clock(1_001))
            .await
            .expect("recovery"),
        vec![(run.clone(), RunState::Ended(EndCause::Cancelled))]
    );
    assert_eq!(store.dispatch_count(), 0);
    let record =
        awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run(
            commit.as_ref(),
            &run,
        )
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
        awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run(
            commit.as_ref(),
            &run
        )
        .unwrap()
        .state,
        RunState::Ended(EndCause::Cancelled)
    );
}

#[tokio::test]
async fn send_message_cannot_approve_a_threads_pending_tool() {
    // Test design. Causes: C1 a Thread awaits a structured tool decision; C2 the
    // generic send_message path delivers plain input to that Thread. Effects: E1
    // C2 cannot satisfy the decision ticket; E2 the Run remains Awaiting and the
    // tool does not execute. Constraint/Invariant: approval requires the typed,
    // correlation-bound decision path. Decision rule: exercise generic delivery
    // against a permission ticket and require fail-closed non-execution.
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
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
    let sender = OutboxMessageSender::new(
        store.clone(),
        commit.clone() as Arc<dyn CommittedThreadView>,
    );
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
    let processed = ingress
        .relay_outbox(harness::clock(0))
        .await
        .expect("relay");
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
async fn send_message_does_not_bind_to_a_client_execution_tool_wait() {
    // Test design. Causes: C1 the committed target is a ClientExecution tool
    // call (whose legacy flattened reason is the same ExternalEvent value used
    // by remote input); C2 generic send_message addresses the awaiting Thread.
    // Effects: E1 the message remains unbound; E2 it becomes neither a bound
    // Input nor a Permission/ToolResult resume; E3 the committed ticket and
    // Awaiting dispatch remain unchanged. K: the closed AwaitTarget variant,
    // never its lossy flattened reason, is the sole binding authority.
    //
    // | Rule | AwaitTarget | send_message binding |
    // |---|---|---|
    // | R1 | RemoteInput | admit as bound Input |
    // | R2 | Pause(Manual) | admit as bound Input |
    // | R3 | ToolCall(Delegation) | admit as bound Input |
    // | R4 | ToolCall(Permission) | reject; keep unbound |
    // | R5 | ToolCall(ScheduledAction) | reject; keep unbound |
    // | R6 | ToolCall(ClientExecution) | reject; keep unbound |
    //
    // R4 is covered by the adjacent permission regression. R6 is the minimal
    // additional rule because it alone collides with an input-accepting legacy
    // ExternalEvent reason; duplicating the remaining typed partitions here
    // would not add another decision or persistence boundary.
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason,
    };
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;

    let run_id = RunId("client-execution-wait".to_string());
    let thread_id = ThreadId(THREAD.to_string());
    let ticket = ResumeTicket::new(
        "client-call",
        run_id.clone(),
        thread_id.clone(),
        SNAP,
        FP,
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::ClientExecution,
            call_id: "client-call".to_string(),
            tool: PendingTool {
                tool_id: "client-tool".to_string(),
                arguments: serde_json::json!({"value": 1}),
            },
        },
    );
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    awaken_agent_contract::thread::commit::commit_run(
        commit.as_ref(),
        &thread_id,
        RunDisposition::awaiting(ticket.clone()),
        Vec::new(),
        Vec::new(),
    )
    .await
    .expect("commit client-execution wait");

    store
        .enqueue(RunDispatch::new(activation("client-execution-wait")))
        .await
        .expect("enqueue awaiting dispatch");
    let claim = store
        .claim("setup", 1_000, 0, &Default::default())
        .await
        .expect("claim awaiting dispatch")
        .expect("dispatch is runnable");
    store
        .settle(&run_id, claim.lease.epoch, DispatchOutcome::Awaiting, &[])
        .await
        .expect("settle dispatch awaiting");

    let sender = OutboxMessageSender::new(
        store.clone(),
        commit.clone() as Arc<dyn CommittedThreadView>,
    );
    sender
        .send(send_request(
            THREAD,
            "ordinary message",
            "client-execution-send",
        ))
        .await
        .expect("stage ordinary message");
    assert_eq!(store.relay().await.expect("relay staged message"), 1);

    let queued = store.list(&thread_id).await.expect("list thread input");
    assert_eq!(queued.len(), 1);
    assert!(
        queued.iter().all(|record| {
            record.input.run_id.0.is_empty()
                && record.input.correlation_id.is_empty()
                && matches!(&record.input.result, ResumeResult::Input(_))
        }),
        "R6/E1+E2: ordinary input stays unbound and cannot answer the tool call"
    );
    assert_eq!(
        commit.resume_ticket_for(&run_id),
        Some(ticket),
        "R6/E3: client-execution ticket remains committed authority"
    );
    assert_eq!(
        store.awaiting_run(&thread_id).await.expect("read dispatch"),
        Some(run_id),
        "R6/E3: the dispatch remains Awaiting"
    );
}

#[tokio::test]
async fn priority_dedupe_gc_store_spec() {
    harness::assert_priority_dedupe_gc(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn lease_renewal_store_spec() {
    harness::assert_lease_renewal(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
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

    // A crashed run explicitly quarantined through the ingress API.
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
    assert_eq!(ingress.quarantine_retry_exhausted(0, 100).await.unwrap(), 1);
    assert_eq!(
        ingress.dead_letters().await.unwrap(),
        vec![RunId("run-1".to_string())]
    );

    // Requeue, quarantine again, then GC through the ingress API.
    assert!(ingress.requeue(&RunId("run-1".to_string())).await.unwrap());
    assert!(ingress.dead_letters().await.unwrap().is_empty());
    assert!(
        store
            .claim("w", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ingress.quarantine_retry_exhausted(0, 200).await.unwrap(), 1);
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
    // Test design. Causes: C1 a ScheduledAction is committed Awaiting; C2 the
    // dispatch crashes Running before performing it; C3 recovery claims after
    // lease expiry. Effects: E1 C3 reconstructs and performs the committed action
    // once; E2 the row settles Done. Constraint/Invariant: the committed request,
    // not lost process memory, owns scheduled recovery. Decision rule: execute
    // the C1+C2+C3 recovery partition and assert one effect plus no dispatch.
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
    let processed = worker.tick(harness::clock(100)).await.unwrap();
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
    // Test design. Causes: C1 send_message targets a Thread with no active Run;
    // C2 Outbox relay runs; C3 the Thread's next Run starts. Effects: E1 C1 is
    // staged unbound; E2 C2 exposes it to the Thread inbox; E3 C3 consumes it as
    // new input once. Constraint/Invariant: idle delivery remains unbound until
    // the next Run claim freezes it. Decision rule: exercise C1-C3 in order and
    // assert the next transcript contains the message once.
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
    ingress
        .relay_outbox(harness::clock(0))
        .await
        .expect("relay");
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
    harness::assert_settle_fences_stale_epoch(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn a_superseded_owners_commit_is_fenced_while_the_current_owners_lands() {
    // Cause/effect graph: C1 claim epoch is current/stale; C2 commit Run matches
    // the guarded dispatch; C3 commit Thread matches it; C4 a dispatch exists.
    // Effects: E1 apply exactly one commit; E2 reject before durable commit.
    // Constraint: the queue's CommitEpochGuard remains held across validation
    // and commit, so recovery cannot change the authoritative dispatch mid-write.
    //
    // | Rule | C1      | C2 | C3 | C4 | effect |
    // | R1   | stale   | -  | -  | Y  | E2     |
    // | R2   | current | N  | Y  | Y  | E2     |
    // | R3   | current | Y  | N  | Y  | E2     |
    // | R4   | current | Y  | Y  | Y  | E1     |
    // | R5   | any     | -  | -  | N  | E2     |
    // Decision rule: R1-R5 exhaust epoch, Run, Thread, and dispatch-presence
    // validation while the one guard is held.
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

    let plan = |run: &RunId, thread: &str| {
        ThreadCommit::assemble(
            ThreadId(thread.to_string()),
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
        fenced_a.commit(plan(&run, THREAD)).await.is_err(),
        "R1/E2 a superseded owner's commit must be fenced"
    );
    assert_eq!(
        inner.commit_count(),
        0,
        "R1/E2 the fenced commit never reached the boundary"
    );

    // B has the current epoch, but its authority is still limited to the exact
    // Run and logical Thread recorded by the guarded dispatch.
    let fenced_b = ClaimedCommitCoordinator::new(service.clone(), RunClaim::from(&b.lease));
    assert!(
        fenced_b
            .commit(plan(&RunId("other-run".into()), THREAD))
            .await
            .is_err(),
        "R2/E2 a live claim cannot commit another Run"
    );
    assert!(
        fenced_b.commit(plan(&run, "other-thread")).await.is_err(),
        "R3/E2 a live claim cannot commit another Thread"
    );
    assert_eq!(inner.commit_count(), 0, "R2/R3 reject before commit");

    fenced_b
        .commit(plan(&run, THREAD))
        .await
        .expect("R4 the exact current owner commits");
    assert_eq!(inner.commit_count(), 1, "R4/E1");

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
    assert!(fenced_ghost.commit(plan(&ghost, THREAD)).await.is_err());
    assert_eq!(
        inner.commit_count(),
        1,
        "R5/E2 the unauthorized commit was fenced"
    );
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_memory() {
    harness::assert_concurrent_recovery_yields_one_winner(
        std::sync::Arc::new(MemoryDispatchStore::new()),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn awaiting_settle_fences_stale_epoch_store_spec() {
    harness::assert_awaiting_settle_fences_stale_epoch(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn submit_superseding_abandons_prior_thread_work() {
    // Cause/effect graph: C1=an older Run is Awaiting; C2=a foreground
    // superseding submit claims the same Thread; C3=both attempts use the live
    // claim Clock. Effects: E1=the old Run is superseded; E2=only the new Run is
    // driven; E3=recovery cannot wake the old Run. Decision rule
    // R1=C1+C2+C3 -> E1+E2+E3. This also owns the superseding half of the
    // foreground-clock boundary; the Any/SQLite test owns ordinary submit.
    // Constraint/Invariant: supersession mutates the existing queue authority and
    // cannot leave the old Awaiting Run wakeable. Decision rule: R1 covers the
    // only same-Thread superseding branch and requires E1-E3.
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
            .recover(harness::clock(0))
            .await
            .unwrap()
            .iter()
            .any(|(run, _)| run.0 == "old"),
        "a superseded run is not claimable"
    );
}

#[tokio::test]
async fn dead_letter_ttl_gc_store_spec() {
    harness::assert_dead_letter_ttl_gc(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn renew_owned_leases_store_spec() {
    harness::assert_renew_owned_leases(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
}

#[tokio::test]
async fn relinquish_claim_store_spec() {
    harness::assert_relinquish_claim(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn renew_skips_far_from_expiry_store_spec() {
    harness::assert_renew_skips_far_from_expiry(
        &MemoryDispatchStore::new(),
        &awaken_run_ingress_testkit::LogicalCommandClock,
    )
    .await;
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
    assert_eq!(
        ingress.quarantine_retry_exhausted(0, 1_000).await.unwrap(),
        1
    );
    assert_eq!(ingress.purge_dead_letters_before(999).await.unwrap(), 0);
    assert_eq!(ingress.purge_dead_letters_before(1_000).await.unwrap(), 1);
    assert!(ingress.list_dispatches().await.unwrap().is_empty());
}
