//! Independent public-API tasks: typed tools, outcomes, approval and recovery.

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::tools::{Tool, ToolError};
use awaken_runtime::{ApprovalError, ApprovalOptions, RunOutcome, Runtime};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_store_inmem::MemoryCommitCoordinator;
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
) -> (
    Runtime,
    ExecutableAgentSnapshot,
    Arc<Model>,
    Arc<AtomicUsize>,
    Arc<MemoryCommitCoordinator>,
) {
    let model = Arc::new(Model::default());
    let hits = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new().with_llm(model.clone());
    let descriptor = runtime.register_tool("app", Echo(hits.clone())).unwrap();
    assert_eq!(descriptor.id, Echo::ID);
    assert!(descriptor.parameters.to_string().contains("text"));
    let snapshot = ExecutableAgentSnapshot::builder("sdk")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(descriptor)
        .max_steps(8)
        .build();
    if gated {
        runtime = runtime.with_gate(Arc::new(ApprovalGate));
    }
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    (runtime, snapshot, model, hits, store)
}

#[tokio::test]
async fn typed_tool_and_named_outcome_work_without_builtin_dependencies() {
    let (runtime, snapshot, model, hits, store) = setup(false);
    let first = runtime
        .start_run_with_outcome(
            &snapshot,
            "conversation",
            "first",
            RuntimeRunContext::new().with_store(store.clone()),
        )
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
        .start_run_with_outcome(
            &snapshot,
            "conversation",
            "second",
            RuntimeRunContext::new().with_store(store.clone()),
        )
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
        let (runtime, snapshot, _, hits, store) = setup(true);
        let state = runtime
            .run_to_completion_async(
                &snapshot,
                "approval",
                "hello",
                RuntimeRunContext::new().with_store(store.clone()),
                move |ticket| async move {
                    assert_eq!(ticket.correlation_id, "approve-echo");
                    tokio::task::yield_now().await;
                    Ok(if allow {
                        ResumeResult::allow()
                    } else {
                        ResumeResult::deny(Some("declined".into()))
                    })
                },
            )
            .await
            .unwrap();
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
    let (runtime, snapshot, model, hits, store) = setup(true);
    let error = runtime
        .run_to_completion_async_with_options(
            &snapshot,
            "recover",
            "hello",
            RuntimeRunContext::new().with_store(store.clone()),
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
    let mut rebuilt = Runtime::new()
        .with_llm(model.clone())
        .with_gate(Arc::new(ApprovalGate));
    rebuilt.register_tool("app", Echo(hits.clone())).unwrap();
    let context = RuntimeRunContext::new().with_store(store.clone());
    let state = rebuilt
        .run_to_completion_with_id_async(
            &snapshot,
            record.id.clone(),
            "recover",
            "hello",
            context.clone(),
            |_| async { Ok(ResumeResult::allow()) },
        )
        .await
        .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let calls = model.calls.load(Ordering::SeqCst);
    let replay = rebuilt
        .run_to_completion_with_id_async(
            &snapshot,
            record.id,
            "recover",
            "ignored replay input",
            context,
            |_| async { panic!("terminal replay never asks again") },
        )
        .await
        .unwrap();
    assert_eq!(replay, state);
    assert_eq!(model.calls.load(Ordering::SeqCst), calls);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_approval_commits_cancelled_without_running_the_tool() {
    let (runtime, snapshot, _, hits, store) = setup(true);
    let token = tokio_util::sync::CancellationToken::new();
    let callback_token = token.clone();
    let state = runtime
        .run_to_completion_async(
            &snapshot,
            "cancel",
            "hello",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_cancellation(token),
            move |_| {
                let token = callback_token.clone();
                async move {
                    token.cancel();
                    std::future::pending().await
                }
            },
        )
        .await
        .unwrap();
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
    let (runtime, snapshot, _, hits, store) = setup(true);
    let error = runtime
        .run_to_completion_async(
            &snapshot,
            "failure",
            "hello",
            RuntimeRunContext::new().with_store(store.clone()),
            |_| async {
                Err(ApprovalError::failed(std::io::Error::other(
                    "approval service offline",
                )))
            },
        )
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
    let (runtime, snapshot, _, hits, store) = setup(true);
    let context = RuntimeRunContext::new().with_store(store.clone());
    let error = runtime
        .run_to_completion_async(&snapshot, "race", "hello", context.clone(), |ticket| {
            let context = context.clone();
            let runtime = &runtime;
            let store = store.clone();
            async move {
                let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0);
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
