//! Cancellation produces a terminal Cancelled outcome and DirectRunIngress is
//! the direct delivery seam; durable-only operations fail closed (G5).

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::{DirectRunIngress, RunIngress, RunService, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error, RunAttemptExecutor, RunExecutor};
use awaken_runtime_contract::live_inbox::{LiveInbox, Offer};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cancel_steers_an_in_flight_run() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(token);

    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

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
    let ingress = DirectRunIngress::with_attempt_executor(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pause_awaits_an_in_flight_run_at_the_next_boundary() {
    use awaken_runtime_contract::pause::PauseSignal;

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));

    let context = RuntimeRunContext::new().with_pause(PauseSignal::new());
    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

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
    let started = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(HangingLlm {
        started: started.clone(),
    })));

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(token);

    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

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
    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(run, context).await });

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
async fn direct_ingress_runs_inline_and_rejects_durable() {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let ingress = DirectRunIngress::new(runtime);

    let outcome = ingress
        .start(activation(), RuntimeRunContext::new())
        .await
        .expect("inline run");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // Durable submission fails closed on direct ingress (G5).
    assert!(matches!(
        ingress.submit_background(activation()).await,
        Err(Error::Execution(_))
    ));
}

#[tokio::test]
async fn direct_ingress_cancel_on_unknown_run_is_not_active() {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let ingress = DirectRunIngress::new(runtime);
    assert_eq!(
        ingress.cancel(&RunId("ghost".to_string())).await,
        Err(ControlError::NotActive)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_on_an_active_run_is_accepted() {
    // Test design — Causes: C1 an execution has crossed into a live gated model
    // call; C2 a Wake targets that exact active Run. Effects: delivery returns
    // Ok and, after release, the same Run completes naturally.
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
    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

    started.notified().await;
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

    let old_inbox = LiveInbox::new();
    let old = runtime.track_active_attempt(
        &run_id,
        &thread_id,
        &RuntimeRunContext::new().with_live_inbox(old_inbox.clone()),
    );

    let ownership = Arc::new(SwitchableOwnership(AtomicUsize::new(0)));
    let replacement_inbox = LiveInbox::new();
    let replacement_pause = PauseSignal::new();
    let replacement = runtime.track_active_attempt(
        &run_id,
        &thread_id,
        &RuntimeRunContext::new()
            .with_live_inbox(replacement_inbox.clone())
            .with_pause(replacement_pause.clone())
            .with_ownership(ownership.clone()),
    );
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
    assert!(old_inbox.list().is_empty(), "A1/E2 exact replacement inbox");
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
