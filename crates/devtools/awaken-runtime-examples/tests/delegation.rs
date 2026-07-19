//! Kernel-level guard for the delegation port: the runtime executes the delegation
//! tool via an injected [`RunDelegationService`] (not the tool registry), and awaits/resumes
//! it — no host orchestration. Proves delegation is a runtime concern.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    RunDelegationService,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, THREAD_USAGE_STATE_KEY, ThreadUsage,
    TokenUsage, ToolCall,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_examples::prelude::*;

/// A coordinator model: it calls `agent_run` once, then replies with the delegate's
/// result text (so the test can see the result flow back through the tool).
struct CoordinatorLlm;

#[async_trait::async_trait]
impl LlmExecutor for CoordinatorLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count();
        let output = if tool_results == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "d1".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({ "agent_id": "sub", "input": "go" }),
            }])
        } else {
            // Echo the last tool result so the assertion can read the delegate reply.
            let reply = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .map(|m| m.content.iter().map(block_text).collect::<String>())
                .unwrap_or_default();
            AssistantOutput::text(format!("coordinator: {reply}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

fn block_text(block: &awaken_agent_contract::agent::content::ContentBlock) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::ToolResult { content, .. } => content.iter().map(block_text).collect(),
        _ => String::new(),
    }
}

/// A resolver that always finishes with a fixed reply.
struct DoneResolver;

#[async_trait::async_trait]
impl RunDelegationService for DoneResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let agent = request.arguments["agent_id"].as_str().unwrap_or_default();
        Ok(DelegationStep::Ended {
            text: format!("reply from {agent}"),
            usage: ThreadUsage::default(),
        })
    }
    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("DoneResolver never awaits")
    }
}

/// A resolver that finishes and reports the delegate spent tokens — so a test can
/// prove the kernel folds a sub-agent's usage into the parent thread's tally.
struct UsageResolver;

#[async_trait::async_trait]
impl RunDelegationService for UsageResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn start(
        &self,
        _request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let mut usage = ThreadUsage::default();
        usage.record(
            "sub-model",
            TokenUsage {
                prompt_tokens: 13,
                completion_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        Ok(DelegationStep::Ended {
            text: "reply from sub".into(),
            usage,
        })
    }
    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("UsageResolver never awaits")
    }
}

/// A resolver that awaits once (needing input), then finishes on resume.
struct AwaitingResolver;

#[async_trait::async_trait]
impl RunDelegationService for AwaitingResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn start(
        &self,
        _request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        Ok(DelegationStep::Awaiting {
            continuation: serde_json::json!({ "task": "t-1" }),
        })
    }
    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        assert_eq!(
            request.continuation["task"], "t-1",
            "the durable continuation round-trips"
        );
        let input = match request.result {
            ResumeResult::Input(text) => text,
            ResumeResult::ToolResult(output) => output.content,
            ResumeResult::Decision { allow, note } => {
                note.unwrap_or_else(|| if allow { "allow" } else { "deny" }.into())
            }
        };
        Ok(DelegationStep::Ended {
            text: format!("finished with: {input}"),
            usage: ThreadUsage::default(),
        })
    }
}

fn config() -> RunnableConfig {
    RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(ToolDescriptor::pinned(
            "demo",
            "agent_run",
            "Delegate to a sub-agent",
            serde_json::json!({ "type": "object" }),
        ))
        .max_steps(8)
        .build()
}

fn allow_all() -> PermissionGate {
    PermissionGate::new(Arc::new(RuleBasedToolPermissionPolicy::new(
        PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Allow,
            mode: Mode::Default,
            rules: Vec::new(),
        },
    )))
}

#[tokio::test]
async fn the_kernel_runs_agent_run_through_the_resolver() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CoordinatorLlm))
        .with_gate(Arc::new(allow_all()))
        .with_run_delegation(Arc::new(DoneResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let state = runtime
        .run(&config(), "delegate please", ctx)
        .await
        .expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // The resolver ran the delegate (no tool named `agent_run` is registered) and
    // its reply reached the coordinator.
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content().contains("coordinator: reply from sub")),
        "the resolver's reply flowed back through the delegation tool"
    );
}

#[tokio::test]
async fn a_delegates_usage_folds_into_the_parent_thread_tally() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CoordinatorLlm))
        .with_gate(Arc::new(allow_all()))
        .with_run_delegation(Arc::new(UsageResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let state = runtime
        .run(&config(), "delegate please", ctx)
        .await
        .expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // The coordinator model reports no usage, so the parent thread's committed
    // tally is exactly the delegate's spend — proving it rolled up rather than
    // while the child also retains its own committed tally.
    let thread_id = commit
        .committed()
        .thread_id
        .expect("the turn committed a thread");
    let mut tally = ThreadUsage::default();
    for cmd in commit.committed_state(&thread_id) {
        use awaken_agent_contract::agent::state::{Action, Scope};
        if cmd.scope == Scope::Thread
            && cmd.key.0 == THREAD_USAGE_STATE_KEY
            && let Action::Set(value) = &cmd.action
        {
            tally = serde_json::from_value(value.clone()).expect("thread usage");
        }
    }
    let total = tally.total();
    assert_eq!(total.prompt_tokens, 13, "delegate input tokens rolled up");
    assert_eq!(
        total.completion_tokens, 5,
        "delegate output tokens rolled up"
    );
    assert_eq!(
        tally.by_model.get("sub-model").map(|u| u.prompt_tokens),
        Some(13),
        "the delegate's model is attributed in the parent tally"
    );
}

#[tokio::test]
async fn an_awaiting_delegation_resumes_through_the_resolver() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CoordinatorLlm))
        .with_gate(Arc::new(allow_all()))
        .with_run_delegation(Arc::new(AwaitingResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    // The delegate awaits: the run waits, holding the durable handle in its ticket.
    let (run_id, state) = runtime
        .start_run(&config(), "delegate please", "delegate please", ctx)
        .await
        .expect("start");
    assert_eq!(state, RunState::Awaiting, "the awaiting delegation waits");
    let ticket = commit
        .resume_ticket(&run_id)
        .expect("a delegation ticket is committed");

    // Resume with the user's input; the resolver finishes and the run completes.
    let resume_ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input("the detail".into()), 0);
    let state = runtime
        .resume(command, &*commit, resume_ctx)
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    assert!(
        commit.committed().messages.iter().any(|m| m
            .text_content()
            .contains("coordinator: finished with: the detail")),
        "the resumed delegation's reply reached the coordinator"
    );
}
