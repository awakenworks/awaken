//! Independent public-API tasks: typed tools, outcomes, approval and recovery.

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_lease_testkit::ManualClock;
use awaken_runtime::tools::{Tool, ToolError, ToolRegistration};
use awaken_runtime::{ApprovalError, ApprovalOptions, RunOutcome, Runtime};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_time::WallClockExt;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    text: String,
}
struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for Echo {
    type Args = EchoArgs;
    type Output = String;
    const ID: &'static str = "app.echo";
    const DESCRIPTION: &'static str = "Echo text";
    async fn call(&self, args: EchoArgs) -> Result<String, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("echoed: {}", args.text))
    }
}

#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    longest_history: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for Model {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.longest_history
            .fetch_max(request.messages.len(), Ordering::SeqCst);
        let output = if request
            .messages
            .iter()
            .any(|message| message.role == awaken_agent_contract::agent::message::Role::Tool)
        {
            AssistantOutput::text("complete")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "echo-1".into(),
                tool_id: Echo::ID.into(),
                arguments: serde_json::json!({"text":"hello"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
struct ApprovalGate;
#[async_trait::async_trait]
impl ToolGateHook for ApprovalGate {
    async fn gate(
        &self,
        _: &ToolCall,
        _: &awaken_agent_contract::agent::state::Store,
    ) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "approve-echo".into(),
        })
    }
}

fn setup(
    gated: bool,
    clock: Arc<dyn awaken_time::WallClock>,
) -> (
    Runtime,
    ExecutableAgentSnapshot,
    Arc<Model>,
    Arc<AtomicUsize>,
    Arc<MemoryCommitCoordinator>,
) {
    let model = Arc::new(Model::default());
    let hits = Arc::new(AtomicUsize::new(0));
    let registration = ToolRegistration::new("app", Echo(hits.clone())).unwrap();
    let mut runtime = registration
        .install(
            Runtime::new()
                .with_clock(clock.clone())
                .with_llm(model.clone()),
        )
        .unwrap();
    let descriptor = registration.descriptor().clone();
    assert_eq!(descriptor.id, Echo::ID);
    assert!(descriptor.parameters.to_string().contains("text"));
    let snapshot = ExecutableAgentSnapshot::builder("sdk")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(descriptor)
        .max_steps(8)
        .build();
    if gated {
        runtime = runtime.with_gate(Arc::new(ApprovalGate));
    }
    let store = Arc::new(MemoryCommitCoordinator::new(clock));
    (runtime, snapshot, model, hits, store)
}

#[tokio::test]
async fn typed_tool_and_named_outcome_work_without_builtin_dependencies() {
    let (runtime, snapshot, model, hits, store) = setup(false, awaken_time::system_clock());
    let first = runtime
        .session(
            &snapshot,
            "conversation",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run("first")
        .await
        .unwrap();
    assert_eq!(first.end_cause(), Some(&EndCause::NaturalEnd));
    assert!(
        first
            .transcript(store.as_ref())
            .unwrap()
            .iter()
            .any(|message| message.text_content().contains("echoed: hello"))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let second = runtime
        .session(
            &snapshot,
            "conversation",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run("second")
        .await
        .unwrap();
    assert_eq!(first.thread_id(), second.thread_id());
    assert_ne!(first.run_id(), second.run_id());
    assert!(model.longest_history.load(Ordering::SeqCst) >= 5);
    assert!(first.resume_ticket(store.as_ref()).unwrap().is_none());
    assert!(
        RunOutcome::read(
            store.as_ref(),
            first.run_id(),
            &awaken_agent_contract::agent::thread::Id("another".into())
        )
        .is_err()
    );
}

#[tokio::test]
async fn asynchronous_approval_and_denial_use_the_same_durable_resume_path() {
    for allow in [true, false] {
        let (runtime, snapshot, _, hits, store) = setup(true, awaken_time::system_clock());
        let state = runtime
            .session(
                &snapshot,
                "approval",
                RuntimeRunContext::new().with_store(store.clone()),
            )
            .unwrap()
            .run_to_completion_async("hello", move |ticket| async move {
                assert_eq!(ticket.correlation_id, "approve-echo");
                tokio::task::yield_now().await;
                Ok(if allow {
                    ResumeResult::allow()
                } else {
                    ResumeResult::deny(Some("declined".into()))
                })
            })
            .await
            .unwrap()
            .state()
            .clone();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(hits.load(Ordering::SeqCst), usize::from(allow));
        assert!(
            store
                .latest_run(&awaken_agent_contract::agent::thread::Id("approval".into()))
                .unwrap()
                .state
                .is_terminal()
        );
    }
}

#[tokio::test]
async fn approval_timeout_preserves_ticket_and_rebuilt_runtime_can_resume_once() {
    let clock = awaken_time::system_clock();
    let (runtime, snapshot, model, hits, store) = setup(true, clock.clone());
    let error = runtime
        .session(
            &snapshot,
            "recover",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run_to_completion_async_with_options(
            "hello",
            ApprovalOptions::default().with_timeout(Duration::ZERO),
            |_| std::future::pending(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "approval_timeout");
    let record = store
        .latest_run(&awaken_agent_contract::agent::thread::Id("recover".into()))
        .unwrap();
    assert_eq!(record.state, RunState::Awaiting);
    assert!(store.resume_ticket(&record.id).is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let registration = ToolRegistration::new("app", Echo(hits.clone())).unwrap();
    let rebuilt = registration
        .install(
            Runtime::new()
                .with_clock(clock)
                .with_llm(model.clone())
                .with_gate(Arc::new(ApprovalGate)),
        )
        .unwrap();
    let context = RuntimeRunContext::new().with_store(store.clone());
    let state = rebuilt
        .session(&snapshot, "recover", context.clone())
        .unwrap()
        .resume_with_id_async(record.id.clone(), "hello", |_| async {
            Ok(ResumeResult::allow())
        })
        .await
        .unwrap()
        .state()
        .clone();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let calls = model.calls.load(Ordering::SeqCst);
    let replay = rebuilt
        .session(&snapshot, "recover", context)
        .unwrap()
        .resume_with_id_async(record.id, "ignored replay input", |_| async {
            panic!("terminal replay never asks again")
        })
        .await
        .unwrap()
        .state()
        .clone();
    assert_eq!(replay, state);
    assert_eq!(model.calls.load(Ordering::SeqCst), calls);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_approval_commits_cancelled_without_running_the_tool() {
    let (runtime, snapshot, _, hits, store) = setup(true, awaken_time::system_clock());
    let token = tokio_util::sync::CancellationToken::new();
    let callback_token = token.clone();
    let state = runtime
        .session(
            &snapshot,
            "cancel",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_cancellation(token),
        )
        .unwrap()
        .run_to_completion_async("hello", move |_| {
            let token = callback_token.clone();
            async move {
                token.cancel();
                std::future::pending().await
            }
        })
        .await
        .unwrap()
        .state()
        .clone();
    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    let record = store
        .latest_run(&awaken_agent_contract::agent::thread::Id("cancel".into()))
        .unwrap();
    assert_eq!(record.state, state);
    assert!(store.resume_ticket(&record.id).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn callback_failure_keeps_cause_and_does_not_consume_the_ticket() {
    let (runtime, snapshot, _, hits, store) = setup(true, awaken_time::system_clock());
    let error = runtime
        .session(
            &snapshot,
            "failure",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run_to_completion_async("hello", |_| async {
            Err(ApprovalError::failed(std::io::Error::other(
                "approval service offline",
            )))
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "approval_failed");
    assert!(error.to_string().contains("approval service offline"));
    assert!(std::error::Error::source(&error).is_some());
    let record = store
        .latest_run(&awaken_agent_contract::agent::thread::Id("failure".into()))
        .unwrap();
    assert_eq!(record.state, RunState::Awaiting);
    assert!(store.resume_ticket(&record.id).is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_concurrent_reply_cannot_execute_the_same_tool_twice() {
    let clock = awaken_time::system_clock();
    let (runtime, snapshot, _, hits, store) = setup(true, clock.clone());
    let context = RuntimeRunContext::new().with_store(store.clone());
    let error = runtime
        .session(&snapshot, "race", context.clone())
        .unwrap()
        .run_to_completion_async("hello", |ticket| {
            let context = context.clone();
            let runtime = &runtime;
            let store = store.clone();
            let clock = clock.clone();
            async move {
                let command =
                    ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), clock.now_ms());
                runtime
                    .resume(command, store.as_ref(), context)
                    .await
                    .unwrap();
                Ok(ResumeResult::allow())
            }
        })
        .await
        .unwrap_err();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(error.code(), "run_not_awaiting");
}

#[tokio::test]
async fn cold_recovery_rejects_each_identity_mismatch_before_approval_or_effects() {
    let (runtime, snapshot, model, hits, store) = setup(true, awaken_time::system_clock());
    let context = RuntimeRunContext::new().with_store(store.clone());
    let outcome = runtime
        .session(&snapshot, "identity", context.clone())
        .unwrap()
        .run("hello")
        .await
        .unwrap();
    assert_eq!(outcome.state(), &RunState::Awaiting);
    let commits = store.commit_count();
    let calls = model.calls.load(Ordering::SeqCst);
    for mismatch in ["thread", "snapshot", "fingerprint"] {
        let mut candidate = snapshot.clone();
        let thread = if mismatch == "thread" {
            "other"
        } else {
            "identity"
        };
        let code = match mismatch {
            "thread" => "run_identity_conflict",
            "snapshot" => {
                candidate.id.0 = "another-snapshot".into();
                "resume_snapshot_mismatch"
            }
            _ => {
                candidate.fingerprint.0 = "another-catalog".into();
                "resume_fingerprint_mismatch"
            }
        };
        let error = runtime
            .session(&candidate, thread, context.clone())
            .unwrap()
            .resume_with_id_async(outcome.run_id().clone(), "ignored", |_| async {
                panic!("an identity mismatch must not ask for approval")
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), code);
        assert_eq!(store.commit_count(), commits);
        assert_eq!(model.calls.load(Ordering::SeqCst), calls);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(store.resume_ticket(outcome.run_id()).is_some());
    }
}

#[tokio::test]
async fn recovery_enforces_ticket_deadline_using_the_supplied_owner_clock() {
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};

    // A1 one composition injects the SAME ManualClock into Runtime and store.
    // A2/A3 its reading at/beyond the deadline refuses the existing resume kernel
    // without a tool effect/ticket-consumption commit; A1 before expiry resumes.
    // The helper samples after callback, not the ticket's original commit time.
    for now in [19, 20, 21] {
        let clock = Arc::new(ManualClock::new(0));
        let (runtime, snapshot, _, hits, store) = setup(true, clock.clone());
        let context = RuntimeRunContext::new().with_store(store.clone());
        let session = runtime.session(&snapshot, "deadline", context).unwrap();
        let outcome = session.run("hello").await.unwrap();
        let ticket = outcome
            .resume_ticket(store.as_ref())
            .unwrap()
            .unwrap()
            .with_deadline(Some(20));
        store
            .commit(ThreadCommit::assemble(
                outcome.thread_id().clone(),
                RunDisposition::awaiting(ticket),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        let commits = store.commit_count();
        clock.set(now);
        let result = session
            .resume_with_id_async(outcome.run_id().clone(), "ignored", |_| async {
                Ok(ResumeResult::allow())
            })
            .await;
        if now < 20 {
            assert_eq!(result.unwrap().end_cause(), Some(&EndCause::NaturalEnd));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(result.unwrap_err().code(), "resume_expired");
            assert_eq!(hits.load(Ordering::SeqCst), 0);
            assert_eq!(store.commit_count(), commits);
            assert!(store.resume_ticket(outcome.run_id()).is_some());
        }
    }
}

#[tokio::test]
async fn missing_owner_clock_refuses_approval_before_callback_or_resume() {
    // M1 a real committed Awaiting ticket + no injected clock -> typed
    // InvalidConfiguration/missing_clock, zero callback/tool/resume commit.
    // No deadline is required: an actual ResumeCommand always carries an
    // observation. Initial execution may commit Awaiting; it never consumes it.
    let clock = Arc::new(ManualClock::new(10));
    let (runtime, snapshot, model, hits, store) = setup(true, clock);
    let context = RuntimeRunContext::new().with_store(store.clone());
    let outcome = runtime
        .session(&snapshot, "missing-clock", context.clone())
        .unwrap()
        .run("hello")
        .await
        .unwrap();
    let ticket = store.resume_ticket(outcome.run_id()).unwrap();
    let commits = store.commit_count();
    let model_calls = model.calls.load(Ordering::SeqCst);
    let callbacks = Arc::new(AtomicUsize::new(0));
    let without_clock = ToolRegistration::new("app", Echo(hits.clone()))
        .unwrap()
        .install(
            Runtime::new()
                .with_llm(model.clone())
                .with_gate(Arc::new(ApprovalGate)),
        )
        .unwrap();
    let error = without_clock
        .session(&snapshot, "missing-clock", context)
        .unwrap()
        .resume_with_id_async(outcome.run_id().clone(), "ignored", |_| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(ResumeResult::allow()))
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "missing_clock");
    assert_eq!(
        error.kind(),
        awaken_runtime_contract::diagnostic::RuntimeErrorKind::InvalidConfiguration
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 0, "M1 no callback");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "M1 no tool");
    assert_eq!(model.calls.load(Ordering::SeqCst), model_calls);
    assert_eq!(store.commit_count(), commits, "M1 no resume");
    assert_eq!(store.resume_ticket(outcome.run_id()), Some(ticket));
}

#[tokio::test]
async fn callback_elapsed_time_cannot_borrow_an_earlier_clock_reading() {
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};

    // A4 callback starts at19, returns at20: the original expiry law rejects
    // at the inclusive boundary. One shared ManualClock supplies store and
    // Runtime; sampling before callback would incorrectly execute the tool.
    let clock = Arc::new(ManualClock::new(19));
    let (runtime, snapshot, _, hits, store) = setup(true, clock.clone());
    let context = RuntimeRunContext::new().with_store(store.clone());
    let session = runtime.session(&snapshot, "elapsed", context).unwrap();
    let outcome = session.run("hello").await.unwrap();
    let ticket = store
        .resume_ticket(outcome.run_id())
        .unwrap()
        .with_deadline(Some(20));
    store
        .commit(ThreadCommit::assemble(
            outcome.thread_id().clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let commits = store.commit_count();
    let error = session
        .resume_with_id_async(outcome.run_id().clone(), "ignored", |_| {
            clock.set(20);
            std::future::ready(Ok(ResumeResult::allow()))
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "resume_expired", "A4");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(store.commit_count(), commits);
    assert_eq!(store.resume_ticket(outcome.run_id()), Some(ticket));
}

#[tokio::test]
async fn committed_terminal_replay_requires_neither_clock_nor_callback() {
    // M2 after real completion, a fresh Runtime with no clock/model returns the
    // exact committed terminal before accessing approval dependencies. Identity
    // and terminal delivery remain the existing authority, not a time fallback.
    let (runtime, snapshot, _, hits, store) = setup(true, Arc::new(ManualClock::new(10)));
    let context = RuntimeRunContext::new().with_store(store.clone());
    let first = runtime
        .session(&snapshot, "terminal-clock", context.clone())
        .unwrap()
        .run_to_completion_async("hello", |_| std::future::ready(Ok(ResumeResult::allow())))
        .await
        .unwrap();
    let commits = store.commit_count();
    let replay_runtime = Runtime::new();
    let replay = replay_runtime
        .session(&snapshot, "terminal-clock", context)
        .unwrap()
        .resume_with_id_async(first.run_id().clone(), "ignored", |_| async {
            panic!("M2 terminal replay cannot call approval")
        })
        .await
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(store.commit_count(), commits);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_typed_tool_schema_is_a_fallible_registration_error() {
    struct InvalidTool;
    #[async_trait::async_trait]
    impl Tool for InvalidTool {
        type Args = String;
        type Output = String;
        const ID: &'static str = "invalid";
        const DESCRIPTION: &'static str = "invalid root argument schema";
        async fn call(&self, _: String) -> Result<String, ToolError> {
            panic!("a rejected tool must never execute")
        }
    }
    let error = ToolRegistration::new("app", InvalidTool).err().unwrap();
    assert!(error.to_string().contains("object"));
}
