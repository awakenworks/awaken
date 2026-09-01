//! Cancellation produces a terminal Cancelled outcome and DirectAttemptDriver
//! is the concrete queue-less delivery seam.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{
    EndCause, Failure, Id as RunId, Record as RunRecord, RunState,
};
use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, Key as StateKey, MergePolicy, Scope,
    StateKey as TypedStateKey, Store as StateStore,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::{DirectAttemptDriver, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error, LiveInput, RunAttemptExecutor, RunExecutor};
use awaken_runtime_contract::live_inbox::Offer;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolCallPhase};
use awaken_store_inmem::MemoryCommitCoordinator;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct TextLlm;

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Blocks the first inference until released, so a live cancel can land mid-run.
struct GatedLlm {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct BlockingExternalAttempt {
    entered: Arc<Notify>,
}

struct ConcurrencyTrackingExternalAttempt {
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    entered: tokio::sync::mpsc::UnboundedSender<RunId>,
    release: Arc<tokio::sync::Semaphore>,
}

impl ConcurrencyTrackingExternalAttempt {
    async fn cross(&self, run_id: RunId) -> Result<RunState, Error> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        self.entered
            .send(run_id)
            .expect("test admission receiver remains live");
        self.release
            .acquire()
            .await
            .expect("test release remains open")
            .forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(RunState::Ended(EndCause::NaturalEnd))
    }
}

#[async_trait::async_trait]
impl RunExecutor for ConcurrencyTrackingExternalAttempt {
    async fn execute(
        &self,
        activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        self.cross(activation.run_id).await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for ConcurrencyTrackingExternalAttempt {
    async fn resume(
        &self,
        activation: RunActivation,
        _command: ResumeCommand,
        _context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        self.cross(activation.run_id).await
    }
}

#[async_trait::async_trait]
impl RunExecutor for BlockingExternalAttempt {
    async fn execute(
        &self,
        _activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        let cancellation = context
            .cancellation
            .ok_or_else(|| Error::Execution("external attempt has no cancellation".into()))?;
        self.entered.notify_one();
        cancellation.cancelled().await;
        Ok(RunState::Ended(EndCause::Cancelled))
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for BlockingExternalAttempt {
    async fn resume(
        &self,
        _activation: RunActivation,
        _command: ResumeCommand,
        _context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        Err(Error::Execution(
            "fresh direct-ingress test must not resume".into(),
        ))
    }
}

#[async_trait::async_trait]
impl LlmExecutor for GatedLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ChatResponse {
            output: AssistantOutput::text("late".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "p".to_string(),
                        model_ref: "m".to_string(),
                        backend_ref: "b".to_string(),
                    },
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

fn activation_on(run_id: &str, thread_id: &str) -> RunActivation {
    let mut value = activation();
    value.run_id = RunId(run_id.to_string());
    value.thread_id = ThreadId(thread_id.to_string());
    value
}

/// Test design for cancellation command handling.
/// Decision rules: R1 pre-cancelled -> no model call and committed Cancelled;
/// R2 cancellation during inference -> discard late output and end Cancelled;
/// R3 no cancellation -> ordinary execution. This case covers R1; the following
/// `live_cancel_steers_an_in_flight_run` covers R2, while execution tests cover R3.
#[tokio::test]
async fn pre_cancelled_run_commits_a_terminal_cancelled_outcome() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_cancellation(token);

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::Cancelled));
    assert_eq!(
        commit.committed().latest_run.unwrap().state,
        RunState::Ended(EndCause::Cancelled)
    );
}

#[tokio::test]
async fn claimed_cancellation_commits_unpublished_activation_input_with_terminal_state() {
    // Cause/effect graph: C1 admission has accepted one exact User message; C2
    // its execution owner is fenced before the first commit; C3 a cancellation
    // claim receives the immutable activation. Effects: E1 C3 commits C1 once;
    // E2 the same commit ends the Run Cancelled. Invariant: a processed public
    // receipt cannot lose its User message merely because interrupt won the
    // execution lease. The real-process ACP matrix owns old-owner fencing and
    // no-late-output; this unit isolates the replacement claim's commit payload.
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let outcome = runtime
        .cancel_activation(activation(), context)
        .await
        .expect("C3 cancellation claim commits");

    assert_eq!(outcome, RunState::Ended(EndCause::Cancelled), "E2");
    let committed = commit.committed();
    assert_eq!(committed.messages.len(), 1, "E1");
    assert_eq!(committed.messages[0].id, MessageId("m1".to_string()), "E1");
    assert_eq!(
        committed.latest_run.expect("E2 terminal Run").state,
        RunState::Ended(EndCause::Cancelled),
        "E2",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cancel_steers_an_in_flight_run() {
    // Decision rule D1: C1 a direct attempt is active and C2 cancellation names
    // its exact Run => E1 the single ActiveAttemptScope receives Cancel and E2
    // the attempt ends Cancelled. C3 no scope would instead yield NotActive.
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(token);

    let driver = DirectAttemptDriver::new(runtime.clone());
    let handle = tokio::spawn(async move { driver.start(activation(), context).await });

    // Wait until the first inference is in-flight, then cancel via live control.
    started.notified().await;
    runtime
        .deliver(LiveCommand::Cancel {
            run_id: RunId("run-1".to_string()),
        })
        .expect("cancel delivered");
    release.notify_one();

    let outcome = handle.await.expect("join").expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::Cancelled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_ingress_tracks_external_attempt_for_live_cancellation() {
    // Cause/effect graph: C1 Direct ingress selects an external executor; C2 its
    // attempt context carries a cancellation token; C3 cancel addresses the exact
    // active Run; C4 the executor returns. Effects: E1 Runtime exposes that token
    // through its one active-attempt registry; E2 cancel reaches the blocked
    // executor; E3 the Run ends Cancelled; E4 return removes the registration.
    // Constraint: the ingress must reuse Runtime tracking, not add an ACP/private
    // cancellation table. Decision rule D1=C1+C2+C3+C4 -> E1+E2+E3+E4.
    let entered = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new());
    let ingress = DirectAttemptDriver::with_attempt_executor(
        runtime.clone(),
        Arc::new(BlockingExternalAttempt {
            entered: entered.clone(),
        }),
    );
    let context = RuntimeRunContext::new().with_cancellation(CancellationToken::new());
    let running = tokio::spawn({
        let ingress = ingress.clone();
        async move { ingress.start(activation(), context).await }
    });

    entered.notified().await;
    ingress
        .cancel(&RunId("run-1".into()))
        .await
        .expect("D1/E1+E2 exact live cancellation");
    let state = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .expect("D1/E2 external attempt observes cancellation")
        .expect("D1 executor task joins")
        .expect("D1 executor returns a state");
    assert_eq!(state, RunState::Ended(EndCause::Cancelled), "D1/E3");
    assert_eq!(
        runtime.deliver(LiveCommand::Cancel {
            run_id: RunId("run-1".into()),
        }),
        Err(ControlError::NotActive),
        "D1/E4",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_ingress_never_enters_two_external_attempts_for_one_thread() {
    // Cause/effect graph and decision table:
    // C1=same Thread, C2=different Thread, C3=first external future blocked,
    // C4=first future returned. E1=second has not entered, E2=second enters,
    // E3=max physical concurrency is one, E4=different Threads may overlap.
    // D1 C1+C3+!C4 -> E1; D2 C1+C4 -> E2+E3; D3 C2+C3 -> E4.
    // This case executes D1/D2 at the actual injected RunAttemptExecutor seam;
    // the existing cross-Thread dispatch-pool test owns D3. The local gate is
    // the sole Direct authority; active-attempt controls remain observation.
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let runtime = Arc::new(Runtime::new());
    let ingress = DirectAttemptDriver::with_attempt_executor(
        runtime,
        Arc::new(ConcurrencyTrackingExternalAttempt {
            active: active.clone(),
            maximum: maximum.clone(),
            entered: entered_tx,
            release: release.clone(),
        }),
    );

    let first = tokio::spawn({
        let ingress = ingress.clone();
        async move {
            ingress
                .start(
                    activation_on("direct-1", "shared-thread"),
                    RuntimeRunContext::new(),
                )
                .await
        }
    });
    assert_eq!(
        entered_rx.recv().await.expect("D1 first enters"),
        RunId("direct-1".into())
    );
    let second = tokio::spawn({
        let ingress = ingress.clone();
        async move {
            ingress
                .resume(
                    activation_on("direct-2", "shared-thread"),
                    ResumeCommand {
                        correlation_id: "direct-correlation".into(),
                        run_id: RunId("direct-2".into()),
                        thread_id: ThreadId("shared-thread".into()),
                        snapshot_id: ExecutableAgentSnapshotId("snapshot-1".into()),
                        catalog_fingerprint: CatalogFingerprint("catalog-a".into()),
                        result: ResumeResult::allow(),
                        operation_id: None,
                        context_messages: Vec::new(),
                        now_ms: 1,
                    },
                    RuntimeRunContext::new(),
                )
                .await
        }
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), entered_rx.recv())
            .await
            .is_err(),
        "D1/E1 the second executor must not enter while the first future is live"
    );
    release.add_permits(1);
    first
        .await
        .expect("D2 first task joins")
        .expect("D2 first ends");
    assert_eq!(
        entered_rx.recv().await.expect("D2 second enters after ACK"),
        RunId("direct-2".into())
    );
    release.add_permits(1);
    second
        .await
        .expect("D2 second task joins")
        .expect("D2 second ends");
    assert_eq!(maximum.load(Ordering::SeqCst), 1, "D2/E3");
    assert_eq!(active.load(Ordering::SeqCst), 0, "all attempts returned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pause_awaits_an_in_flight_run_at_the_next_boundary() {
    use awaken_runtime_contract::pause::PauseSignal;

    // Decision rule D1: C1 a direct attempt is active, C2 Pause names that Run,
    // and C3 inference reaches the next safe boundary => E1 the exact scope
    // observes Pause and E2 commits Awaiting rather than a terminal result.
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));

    let context = RuntimeRunContext::new().with_pause(PauseSignal::new());
    let driver = DirectAttemptDriver::new(runtime.clone());
    let handle = tokio::spawn(async move { driver.start(activation(), context).await });

    // Once the first inference is in-flight (so the run is registered), pause it via
    // live control; it awaits at the boundary after the step completes.
    started.notified().await;
    runtime
        .deliver(LiveCommand::Pause {
            run_id: RunId("run-1".to_string()),
        })
        .expect("pause delivered");
    release.notify_one();

    let outcome = handle.await.expect("join").expect("runs");
    assert_eq!(outcome, RunState::Awaiting, "a paused run awaits, not ends");
}

/// Signals when inference starts, then never returns — a provider that hangs.
struct HangingLlm {
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl LlmExecutor for HangingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("a hung inference never completes")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cancel_aborts_a_hung_inference() {
    // Decision rule D1: C1 a direct attempt is active, C2 inference is pending,
    // and C3 exact live Cancel arrives => E1 cancellation aborts the provider
    // future and E2 the Run ends Cancelled without waiting for a loop boundary.
    let started = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(HangingLlm {
        started: started.clone(),
    })));

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(token);

    let driver = DirectAttemptDriver::new(runtime.clone());
    let handle = tokio::spawn(async move { driver.start(activation(), context).await });

    // The provider never returns, so the cancel must abort inference in
    // flight — a step-boundary check alone would hang forever.
    started.notified().await;
    runtime
        .deliver(LiveCommand::Cancel {
            run_id: RunId("run-1".to_string()),
        })
        .expect("cancel delivered");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("cancel aborts the hung inference")
        .expect("join")
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::Cancelled));
}

struct ToolCallingLlm;

#[async_trait::async_trait]
impl LlmExecutor for ToolCallingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![
                awaken_runtime_contract::llm::ToolCall {
                    call_id: "call-hang".into(),
                    tool_id: "hang".into(),
                    arguments: serde_json::json!({}),
                },
            ]),
            usage: None,
            stop_reason: None,
        })
    }
}

struct MultiClientToolLlm(AtomicUsize);

#[async_trait::async_trait]
impl LlmExecutor for MultiClientToolLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![
                awaken_runtime_contract::llm::ToolCall {
                    call_id: "call-a".into(),
                    tool_id: "client-a".into(),
                    arguments: serde_json::json!({}),
                },
                awaken_runtime_contract::llm::ToolCall {
                    call_id: "call-b".into(),
                    tool_id: "client-b".into(),
                    arguments: serde_json::json!({}),
                },
            ]),
            usage: None,
            stop_reason: None,
        })
    }
}

/// One mutually exclusive fault injected over the real commit authority.
/// Keeping all partitions in this enum avoids parallel fake stores and makes
/// the recovery decision table exhaustive by construction.
#[derive(Clone, Copy)]
enum AwaitingDamage {
    MissingTicket,
    MissingBatch,
    UnreadableBatch,
    IncoherentBatch,
    IncoherentTicketOwner,
}

/// Fault-injection read view over the real commit authority. It changes only
/// the selected corrupt projection and delegates every other fact unchanged.
struct DamagedAwaitingView {
    inner: Arc<MemoryCommitCoordinator>,
    damage: AwaitingDamage,
}

impl CommittedThreadView for DamagedAwaitingView {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.inner.committed_messages(thread_id)
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.inner.run(run_id)
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.inner.latest_run(thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        match self.damage {
            AwaitingDamage::MissingTicket => None,
            AwaitingDamage::IncoherentTicketOwner => {
                self.inner.resume_ticket(run_id).map(|mut ticket| {
                    ticket.thread_id = ThreadId("another-thread".into());
                    ticket
                })
            }
            AwaitingDamage::MissingBatch
            | AwaitingDamage::UnreadableBatch
            | AwaitingDamage::IncoherentBatch => self.inner.resume_ticket(run_id),
        }
    }

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let latest = self.latest_run(thread_id)?;
        let ticket = self.resume_ticket(&latest.id)?;
        (ticket.thread_id == *thread_id).then_some((latest.id, ticket))
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        let mut commands = self.inner.committed_state(thread_id);
        match self.damage {
            AwaitingDamage::MissingBatch => {
                commands.retain(|command| command.key.0 != "runtime.active_tool_batch.v1");
            }
            AwaitingDamage::UnreadableBatch => {
                commands.push(StateCommand {
                    key: StateKey("runtime.active_tool_batch.v1".into()),
                    scope: Scope::Run,
                    merge: MergePolicy::Disjoint,
                    run_id: Some(RunId("run-1".into())),
                    action: StateAction::Set(serde_json::json!({"invalid": "tool batch"})),
                });
            }
            AwaitingDamage::IncoherentBatch => {
                let store = StateStore::rebuild(&commands);
                let mut batch = <ActiveToolBatch as TypedStateKey>::load(&store)
                    .expect("fixture batch is readable")
                    .expect("fixture batch exists");
                let call_id = batch
                    .calls()
                    .iter()
                    .find_map(|entry| {
                        matches!(entry.phase, ToolCallPhase::Awaiting { .. })
                            .then_some(entry.call.call_id.clone())
                    })
                    .expect("fixture has an external wait");
                batch
                    .complete(ToolOutput::error(
                        &call_id,
                        "fixture removes the only external wait",
                    ))
                    .expect("fixture completes its external wait");
                commands.push(<ActiveToolBatch as TypedStateKey>::write(&Some(batch)));
            }
            AwaitingDamage::MissingTicket | AwaitingDamage::IncoherentTicketOwner => {}
        }
        commands
    }
}

async fn committed_client_tool_wait() -> (
    Runtime,
    Arc<MemoryCommitCoordinator>,
    Arc<MultiClientToolLlm>,
) {
    let llm = Arc::new(MultiClientToolLlm(AtomicUsize::new(0)));
    let runtime = Runtime::new().with_llm(llm.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let mut run = activation();
    run.snapshot.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::client_executed(
            "client-a",
            "client tool a",
            serde_json::json!({"type": "object"}),
        ),
        ToolDescriptor::client_executed(
            "client-b",
            "client tool b",
            serde_json::json!({"type": "object"}),
        ),
    ];
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    runtime
        .execute(run, context)
        .await
        .expect("commit awaiting client-tool batch");
    (runtime, commit, llm)
}

#[tokio::test]
async fn awaiting_tool_interrupt_resolves_the_whole_batch_without_inference() {
    // Cause/effect graph: C1 Run is Awaiting vs non-awaiting/terminal; C2 the
    // open ToolBatch has one or many unfinished calls; C3 command is exact
    // replay. Effects: E1 every unfinished call receives the fixed error in
    // original batch order; E2 the same commit finalizes the batch and ends with
    // NaturalEnd; E3 no model request occurs; E4 exact terminal replay is a
    // no-op; E5 non-awaiting input fails closed.
    //
    // | Rule | State | Pending | Replay | Effect |
    // | I1 | Awaiting | multiple | no | E1+E2+E3 |
    // | I2 | Ended by I1 | none | yes | E4 |
    // | I3 | absent/Running | any | no | E5 |
    // Constraints/invariants: one atomic terminal commit resolves the entire
    // ordered batch; interrupt never resamples the model or partially settles it.
    const INTERRUPTED: &str = "Tool execution was interrupted before completion. Please retry.";
    let llm = Arc::new(MultiClientToolLlm(AtomicUsize::new(0)));
    let runtime = Runtime::new().with_llm(llm.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let mut run = activation();
    run.snapshot.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::client_executed(
            "client-a",
            "client tool a",
            serde_json::json!({"type": "object"}),
        ),
        ToolDescriptor::client_executed(
            "client-b",
            "client tool b",
            serde_json::json!({"type": "object"}),
        ),
    ];
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    assert_eq!(
        runtime
            .execute(run, context.clone())
            .await
            .expect("I1 initial tool batch"),
        RunState::Awaiting
    );
    assert_eq!(llm.0.load(Ordering::SeqCst), 1, "I1 initial inference");

    let ended = runtime
        .interrupt_awaiting_tools(
            RunId("run-1".into()),
            ThreadId("thread-1".into()),
            context.clone(),
        )
        .await
        .expect("I1 interrupt");
    assert_eq!(ended, RunState::Ended(EndCause::NaturalEnd), "I2/E2");
    assert_eq!(llm.0.load(Ordering::SeqCst), 1, "I1/E3 no resample");

    let committed = commit.committed();
    let results = committed
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((
                tool_use_id.as_str(),
                awaken_agent_contract::agent::content::extract_text(content),
                *is_error,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![
            ("call-a", INTERRUPTED.to_string(), true),
            ("call-b", INTERRUPTED.to_string(), true),
        ],
        "I1/E1 ordered fixed errors"
    );

    let commits_before_replay = commit.commit_count();
    assert_eq!(
        runtime
            .interrupt_awaiting_tools(
                RunId("run-1".into()),
                ThreadId("thread-1".into()),
                context.clone(),
            )
            .await
            .expect("I2 exact replay"),
        RunState::Ended(EndCause::NaturalEnd),
        "I2/E4"
    );
    assert_eq!(commit.commit_count(), commits_before_replay, "I2/E4");
    assert!(
        runtime
            .interrupt_awaiting_tools(
                RunId("missing".into()),
                ThreadId("thread-1".into()),
                context,
            )
            .await
            .is_err(),
        "I3/E5"
    );
}

#[tokio::test]
async fn corrupt_awaiting_tool_facts_converge_to_an_audited_state_conflict() {
    // Cause/effect graph: C1=Run is durably Awaiting; C2=the ticket is valid or
    // missing; C3=the active ToolBatch is absent, semantically mismatched, or
    // unreadable; C4=a claim-fenced commit coordinator exists; C5=exact replay.
    // Effects: E1=fail closed as Error(StateConflict); E2=append the typed-cell
    // removal and existing RunStateChanged audit in one terminal ThreadCommit;
    // E3=consume the waiting row without direct deletion; E4=no inference or
    // tool effect; E5=replay returns the exact terminal state idempotently.
    //
    // | Rule | Awaiting | Ticket | Batch | Coordinator | Replay | Effect |
    // | Q1 | yes | any | valid external wait | yes | no | normal interruption |
    // | Q2a | yes | valid | unreadable batch | yes | no | E1+E2+E3+E4 |
    // | Q2b | yes | valid | missing batch | yes | no | E1+E2+E3+E4 |
    // | Q2c | yes | valid | no external wait | yes | no | E1+E2+E3+E4 |
    // | Q2d | yes | wrong owner | valid batch | yes | no | E1+E2+E3+E4 |
    // | Q2e | yes | missing | valid batch | yes | no | E1+E2+E3+E4 |
    // | Q3 | ended by Q2 | absent | any | yes | yes | E5 |
    // | Q4 | yes | any | corrupt/missing | no | no | fail without false terminal |
    // Constraint: corrupt bytes remain in the append-only log; only their active
    // materialization is quarantined. Q1 is covered above; this case covers every
    // Q2 partition plus Q3, while Runtime's durable-operation guard covers Q4.
    for (rule, damage) in [
        ("Q2a", AwaitingDamage::UnreadableBatch),
        ("Q2b", AwaitingDamage::MissingBatch),
        ("Q2c", AwaitingDamage::IncoherentBatch),
        ("Q2d", AwaitingDamage::IncoherentTicketOwner),
        ("Q2e", AwaitingDamage::MissingTicket),
    ] {
        let (runtime, commit, llm) = committed_client_tool_wait().await;
        let damaged = Arc::new(DamagedAwaitingView {
            inner: commit.clone(),
            damage,
        });
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(damaged);
        let commits_before = commit.commit_count();

        let state = runtime
            .interrupt_awaiting_tools(
                RunId("run-1".into()),
                ThreadId("thread-1".into()),
                context.clone(),
            )
            .await
            .unwrap_or_else(|error| panic!("{rule} quarantines through ThreadCommit: {error}"));
        assert_eq!(
            state,
            RunState::Ended(EndCause::Error(Failure::StateConflict)),
            "{rule}/E1"
        );
        assert_eq!(commit.commit_count(), commits_before + 1, "{rule}/E2");
        assert!(
            commit.resume_ticket_for(&RunId("run-1".into())).is_none(),
            "{rule}/E3"
        );
        assert_eq!(llm.0.load(Ordering::SeqCst), 1, "{rule}/E4");
        assert!(
            commit.committed().events.iter().any(|event| {
                event.run_id == RunId("run-1".into())
                    && event.kind == awaken_agent_contract::audit::kind::Kind::RunStateChanged
                    && serde_json::from_value::<RunState>(
                        event.payload.get("state").cloned().unwrap_or_default(),
                    )
                    .ok()
                    .is_some_and(|decoded| decoded == state)
            }),
            "{rule}/E2 existing lifecycle audit is observable"
        );

        let commits = commit.commit_count();
        assert_eq!(
            runtime
                .interrupt_awaiting_tools(
                    RunId("run-1".into()),
                    ThreadId("thread-1".into()),
                    context,
                )
                .await
                .unwrap_or_else(|error| panic!("{rule}/Q3 terminal replay: {error}")),
            state,
            "{rule}/Q3/E5"
        );
        assert_eq!(commit.commit_count(), commits, "{rule}/Q3/E5");
    }
}

struct HangingTool {
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct ToolFutureDrop(Arc<AtomicBool>);

impl Drop for ToolFutureDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl RawTool for HangingTool {
    fn id(&self) -> &str {
        "hang"
    }

    async fn invoke(
        &self,
        _call: awaken_runtime_contract::llm::ToolCall,
    ) -> Result<ToolOutput, ToolError> {
        let _drop = ToolFutureDrop(self.dropped.clone());
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("a hung tool never completes")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cancel_aborts_a_hung_tool_invocation() {
    // Decision rule D1: C1 a direct attempt is active, C2 its tool future is
    // pending, and C3 exact live Cancel arrives => E1 the future is dropped and
    // E2 the Run ends Cancelled. A non-current Run must remain NotActive.
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolCallingLlm))
            .with_tool(Arc::new(HangingTool {
                started: started.clone(),
                dropped: dropped.clone(),
            })),
    );
    let mut run = activation();
    run.snapshot.resolved_spec.tool_descriptors = vec![ToolDescriptor::pinned(
        "test",
        "hang",
        "hang until cancelled",
        serde_json::json!({"type": "object"}),
    )];
    let context = RuntimeRunContext::new().with_cancellation(CancellationToken::new());
    let driver = DirectAttemptDriver::new(runtime.clone());
    let handle = tokio::spawn(async move { driver.start(run, context).await });

    started.notified().await;
    runtime
        .deliver(LiveCommand::Cancel {
            run_id: RunId("run-1".to_string()),
        })
        .expect("cancel delivered");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("cancel aborts the hung tool")
        .expect("join")
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::Cancelled));
    assert!(dropped.load(Ordering::SeqCst), "tool future was dropped");
}

#[test]
fn cancel_on_unknown_run_is_not_active() {
    let runtime = Runtime::new();
    assert_eq!(
        runtime.deliver(LiveCommand::Cancel {
            run_id: RunId("ghost".to_string()),
        }),
        Err(ControlError::NotActive)
    );
}

#[test]
fn pause_on_unknown_run_is_not_active() {
    let runtime = Runtime::new();
    assert_eq!(
        runtime.deliver(LiveCommand::Pause {
            run_id: RunId("ghost".to_string()),
        }),
        Err(ControlError::NotActive)
    );
}

#[test]
fn wake_on_unknown_run_is_not_active() {
    // G5: a wake for a run with no live subscriber is a hard error, not a silent
    // no-op — the untested half of the Wake rule (an active run is the accepted one).
    let runtime = Runtime::new();
    assert_eq!(
        runtime.deliver(LiveCommand::Wake {
            run_id: RunId("ghost".to_string()),
            reason: "nudge".to_string(),
        }),
        Err(ControlError::NotActive)
    );
}

#[tokio::test]
async fn direct_attempt_driver_runs_inline() {
    // Cause D1: a prepared activation is explicitly assigned queue-less delivery.
    // Effect E1: the exact attempt runs inline and returns its committed terminal
    // state. Constraint: durable submission is absent from this concrete type,
    // making the former invalid direct/durable combination unrepresentable.
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let ingress = DirectAttemptDriver::new(runtime);

    let outcome = ingress
        .start(activation(), RuntimeRunContext::new())
        .await
        .expect("inline run");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn direct_ingress_cancel_on_unknown_run_is_not_active() {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let ingress = DirectAttemptDriver::new(runtime);
    assert_eq!(
        ingress.cancel(&RunId("ghost".to_string())).await,
        Err(ControlError::NotActive)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_on_an_active_run_is_accepted() {
    // Test design — Causes: C1 an execution has crossed into a live gated model
    // call; C2 a Wake targets that exact active Run; C3 the native executor
    // advertises safe-boundary input. Effects: delivery returns Ok, one fresh
    // inbox is discoverable, and after release the same Run completes naturally.
    // Constraints/invariants: Wake is an accepted no-op in this Runtime; it must
    // neither create another attempt nor terminate the live one. Decision rule
    // W1=C1+C2=>accepted delivery plus one unchanged terminal outcome.
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(token);
    let driver = DirectAttemptDriver::new(runtime.clone());
    let handle = tokio::spawn(async move { driver.start(activation(), context).await });

    started.notified().await;
    assert!(
        runtime
            .active_attempt_live_inbox(&ThreadId("thread-1".into()))
            .await
            .is_some(),
        "C1+C3 expose only the current attempt inbox"
    );
    // A wake on a live run is accepted (no-op in the MVP) rather than failing.
    assert_eq!(
        runtime.deliver(LiveCommand::Wake {
            run_id: RunId("run-1".to_string()),
            reason: "nudge".to_string(),
        }),
        Ok(())
    );
    release.notify_one();
    let outcome = handle.await.expect("join").expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}

struct SwitchableOwnership(AtomicUsize);

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for SwitchableOwnership {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        match self.0.load(Ordering::SeqCst) {
            0 => Ok(()),
            1 => Err(AttemptOwnershipError::Lost),
            _ => Err(AttemptOwnershipError::Unavailable(
                "test authority down".into(),
            )),
        }
    }
}

#[tokio::test]
async fn attempt_without_safe_boundary_never_advertises_a_live_inbox() {
    // Causes: C1 an exact local attempt is current; C2 its executor declares
    // LiveInput::None. Effects: E1 Run identity remains discoverable for neutral
    // control, while E2 Thread live-inbox lookup fails closed. Decision table:
    // C1+SafeBoundary is covered by the generation test below; C1+None => E1+E2.
    // Constraint: wait/resume capability cannot be used as a substitute signal.
    let runtime = Runtime::new();
    let run_id = RunId("no-live-run".into());
    let thread_id = ThreadId("no-live-thread".into());
    let attempt = runtime.begin_active_attempt(
        &run_id,
        &thread_id,
        RuntimeRunContext::new().with_cancellation(CancellationToken::new()),
        LiveInput::None,
    );

    assert_eq!(
        runtime.active_attempt_run_id(&thread_id).await,
        Some(run_id),
        "C1/E1"
    );
    assert!(
        runtime
            .active_attempt_live_inbox(&thread_id)
            .await
            .is_none(),
        "C1+C2/E2"
    );
    drop(attempt);
}

#[tokio::test]
async fn active_attempt_registry_is_exact_generation_owned_and_thread_addressed() {
    // Cause/effect graph: C1 an old attempt is registered for Run R/Thread T;
    // C2 a replacement claim registers the same R/T with a different inbox; C3
    // the old RAII guard drops; C4 replacement ownership is current, lost, or
    // unavailable; C5 lookup names T or another Thread. Effects: E1 C2+C3 keeps
    // the replacement registration; E2 current+T returns exactly its Run id,
    // inbox and live controls; E3 wrong Thread/lost/unavailable/removed all fail
    // closed; E4 a rejected lookup/control does not mutate either inbox or pause
    // signal.
    // Constraint: one registry is the source for run control and Thread lookup;
    // neither foreground state nor a second inbox map may establish liveness.
    //
    // | Rule | generation | ownership | Thread | effect |
    // |---|---|---|---|---|
    // | A1 | replacement after old drop | current | T | E1 + E2 |
    // | A2 | replacement | current | other | E3 + E4 |
    // | A3 | replacement | lost | T | E3 + E4 |
    // | A4 | replacement | unavailable | T | E3 + E4 |
    // | A5 | removed | n/a | T | E3 + E4 |
    let runtime = Runtime::new();
    let run_id = RunId("replacement-run".into());
    let thread_id = ThreadId("replacement-thread".into());
    let other_thread = ThreadId("other-thread".into());

    let old = runtime.begin_active_attempt(
        &run_id,
        &thread_id,
        RuntimeRunContext::new(),
        LiveInput::SafeBoundary,
    );
    let old_inbox = old.context().live_inbox.clone().expect("old attempt inbox");

    let ownership = Arc::new(SwitchableOwnership(AtomicUsize::new(0)));
    let replacement_pause = PauseSignal::new();
    let replacement = runtime.begin_active_attempt(
        &run_id,
        &thread_id,
        RuntimeRunContext::new()
            .with_pause(replacement_pause.clone())
            .with_ownership(ownership.clone()),
        LiveInput::SafeBoundary,
    );
    let replacement_inbox = replacement
        .context()
        .live_inbox
        .clone()
        .expect("replacement attempt inbox");
    drop(old);

    assert_eq!(
        runtime.active_attempt_run_id(&thread_id).await,
        Some(run_id.clone()),
        "A1/E1+E2 exact Run id"
    );
    let resolved = runtime
        .active_attempt_live_inbox(&thread_id)
        .await
        .expect("A1/E1+E2 replacement remains current");
    assert!(matches!(
        resolved.offer(Message::text(
            MessageId("replacement-input".into()),
            Role::User,
            "replacement"
        )),
        Offer::Accepted(_)
    ));
    assert!(
        matches!(
            old_inbox.offer(Message::text(
                MessageId("stale-old-input".into()),
                Role::User,
                "stale"
            )),
            Offer::Closed
        ),
        "A1/E2 stale inbox closes instead of carrying over"
    );
    assert_eq!(replacement_inbox.list().len(), 1, "A1/E2");
    assert!(
        runtime
            .active_attempt_live_inbox(&other_thread)
            .await
            .is_none(),
        "A2/E3"
    );
    assert!(
        runtime.active_attempt_run_id(&other_thread).await.is_none(),
        "A2/E3"
    );
    runtime
        .deliver_to_current_attempt(LiveCommand::Pause {
            run_id: run_id.clone(),
        })
        .await
        .expect("A1/E2 pause uses the same entry");
    assert!(replacement_pause.requested(), "A1/E2");

    ownership.0.store(1, Ordering::SeqCst);
    assert!(
        runtime
            .active_attempt_live_inbox(&thread_id)
            .await
            .is_none(),
        "A3/E3"
    );
    assert!(
        runtime.active_attempt_run_id(&thread_id).await.is_none(),
        "A3/E3"
    );
    assert_eq!(
        runtime
            .deliver_to_current_attempt(LiveCommand::Wake {
                run_id: run_id.clone(),
                reason: "stale".into(),
            })
            .await,
        Err(ControlError::NotActive),
        "A3/E3+E4"
    );

    ownership.0.store(2, Ordering::SeqCst);
    assert!(
        runtime
            .active_attempt_live_inbox(&thread_id)
            .await
            .is_none(),
        "A4/E3"
    );
    assert!(
        runtime.active_attempt_run_id(&thread_id).await.is_none(),
        "A4/E3"
    );

    drop(replacement);
    assert!(
        runtime
            .active_attempt_live_inbox(&thread_id)
            .await
            .is_none(),
        "A5/E3"
    );
    assert!(
        runtime.active_attempt_run_id(&thread_id).await.is_none(),
        "A5/E3"
    );
}
