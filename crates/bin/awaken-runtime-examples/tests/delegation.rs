//! Kernel-level guard for the delegation port: the runtime executes the delegation
//! tool via an injected [`AgentResolver`] (not the tool registry), and parks/resumes
//! it — no host orchestration. Proves delegation is a runtime concern.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime_contract::agent_resolver::{AgentError, AgentRequest, AgentResolver, AgentStep};
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
impl AgentResolver for DoneResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn run(&self, request: AgentRequest) -> Result<AgentStep, AgentError> {
        let agent = request.arguments["agent_id"].as_str().unwrap_or_default();
        Ok(AgentStep::Done {
            text: format!("reply from {agent}"),
            usage: ThreadUsage::default(),
        })
    }
    async fn resume(
        &self,
        _handle: &serde_json::Value,
        _input: &str,
        _cancellation: Option<&awaken_runtime_contract::CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        unreachable!("DoneResolver never parks")
    }
}

/// A resolver that finishes and reports the delegate spent tokens — so a test can
/// prove the kernel folds a sub-agent's usage into the parent thread's tally.
struct UsageResolver;

#[async_trait::async_trait]
impl AgentResolver for UsageResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn run(&self, _request: AgentRequest) -> Result<AgentStep, AgentError> {
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
        Ok(AgentStep::Done {
            text: "reply from sub".into(),
            usage,
        })
    }
    async fn resume(
        &self,
        _handle: &serde_json::Value,
        _input: &str,
        _cancellation: Option<&awaken_runtime_contract::CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        unreachable!("UsageResolver never parks")
    }
}

/// A resolver that parks once (needing input), then finishes on resume.
struct ParkingResolver;

#[async_trait::async_trait]
impl AgentResolver for ParkingResolver {
    fn tool_id(&self) -> &str {
        "agent_run"
    }
    async fn run(&self, _request: AgentRequest) -> Result<AgentStep, AgentError> {
        Ok(AgentStep::Parked {
            handle: serde_json::json!({ "task": "t-1" }),
        })
    }
    async fn resume(
        &self,
        handle: &serde_json::Value,
        input: &str,
        _cancellation: Option<&awaken_runtime_contract::CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        assert_eq!(handle["task"], "t-1", "the durable handle round-trips");
        Ok(AgentStep::Done {
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
    PermissionGate::new(Arc::new(RulePermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Allow,
        mode: Mode::Default,
        rules: Vec::new(),
    })))
}

#[tokio::test]
async fn the_kernel_runs_agent_run_through_the_resolver() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CoordinatorLlm))
        .with_gate(Arc::new(allow_all()))
        .with_resolver(Arc::new(DoneResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let phase = runtime
        .run(&config(), "delegate please", ctx)
        .await
        .expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

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
        .with_resolver(Arc::new(UsageResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let phase = runtime
        .run(&config(), "delegate please", ctx)
        .await
        .expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    // The coordinator model reports no usage, so the parent thread's committed
    // tally is exactly the delegate's spend — proving it rolled up rather than
    // vanishing with the sub-run's isolated store.
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
async fn a_parked_delegation_resumes_through_the_resolver() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CoordinatorLlm))
        .with_gate(Arc::new(allow_all()))
        .with_resolver(Arc::new(ParkingResolver));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    // The delegate parks: the run waits, holding the durable handle in its ticket.
    let (run_id, phase) = runtime
        .start_run(&config(), "delegate please", "delegate please", ctx)
        .await
        .expect("start");
    assert_eq!(phase, Phase::Waiting, "the parked delegation waits");
    let ticket = commit
        .waiting_ticket(&run_id)
        .expect("a delegation ticket is committed");

    // Resume with the user's input; the resolver finishes and the run completes.
    let resume_ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input("the detail".into()), 0);
    let phase = runtime
        .resume(command, &*commit, resume_ctx)
        .await
        .expect("resume");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    assert!(
        commit.committed().messages.iter().any(|m| m
            .text_content()
            .contains("coordinator: finished with: the detail")),
        "the resumed delegation's reply reached the coordinator"
    );
}
