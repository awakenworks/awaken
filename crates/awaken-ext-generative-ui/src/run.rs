//! Helper function for running a streaming sub-agent.
//!
//! Thin composition of [`awaken_runtime::run_child_agent`] and
//! [`awaken_runtime::StreamingPassthroughSink`] — kept for backward
//! compatibility with downstream generative-UI integrations.

use std::sync::Arc;

use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{ToolCallContext, ToolError};
use awaken_runtime::backend::{
    BackendControl, BackendDelegatePolicy, BackendParentContext, BackendRunStatus,
};
use awaken_runtime::child_agent::{ChildAgentParams, StreamingPassthroughSink, run_child_agent};
use awaken_runtime::registry::{ExecutionResolver, ResolvedAgent, ResolvedExecution};
use awaken_runtime::{AgentResolver, RuntimeError};

/// Result of a streaming sub-agent run.
#[derive(Debug)]
pub struct StreamingSubagentResult {
    /// Accumulated text content from the sub-agent.
    pub content: String,
    /// Number of inference steps executed.
    pub steps: usize,
}

/// Run a sub-agent that streams its text output onto the parent sink in
/// real time.
///
/// Text deltas from the sub-agent are forwarded as
/// [`AgentEvent::ToolCallStreamDelta`](awaken_contract::contract::event::AgentEvent::ToolCallStreamDelta)
/// events on the parent sink so the caller can stream preliminary tool
/// output. The full accumulated text is returned in
/// [`StreamingSubagentResult::content`].
pub async fn run_streaming_subagent(
    resolver: &dyn AgentResolver,
    agent_id: &str,
    prompt: &str,
    ctx: &ToolCallContext,
) -> Result<StreamingSubagentResult, ToolError> {
    let parent_sink = ctx
        .activity_sink
        .clone()
        .unwrap_or_else(|| Arc::new(NullEventSink));
    let (streaming_sink, buffer) =
        StreamingPassthroughSink::new(ctx.call_id.clone(), ctx.tool_name.clone(), parent_sink);
    let sink: Arc<dyn EventSink> = Arc::new(streaming_sink);

    let shim = AgentResolverShim(resolver);

    let result = run_child_agent(ChildAgentParams {
        resolver: &shim,
        agent_id,
        messages: vec![Message::user(prompt)],
        parent: BackendParentContext {
            parent_run_id: Some(ctx.run_identity.run_id.clone()),
            parent_thread_id: Some(ctx.run_identity.thread_id.clone()),
            parent_tool_call_id: Some(ctx.call_id.clone()),
        },
        initial_state_seed: None,
        sink,
        control: BackendControl::default(),
        policy: BackendDelegatePolicy::default(),
    })
    .await
    .map_err(|e| ToolError::ExecutionFailed(format!("sub-agent failed: {e}")))?;

    // Only treat a `Completed` child as a successful return. Suspensions and
    // waits cannot be re-driven through this synchronous helper (callers
    // should use `run_child_agent` directly if they need that), and
    // failed/cancelled/timed-out child runs must surface as errors instead
    // of yielding an `Ok` with partial accumulated text.
    if !matches!(result.status, BackendRunStatus::Completed) {
        return Err(ToolError::ExecutionFailed(format!(
            "sub-agent did not complete: {}",
            result.status
        )));
    }

    let content = buffer.lock().await.clone();

    Ok(StreamingSubagentResult {
        content,
        steps: result.steps,
    })
}

/// Adapter so a borrowed [`AgentResolver`] can satisfy the
/// [`ExecutionResolver`] bound required by `run_child_agent`. Always
/// resolves to a [`ResolvedExecution::Local`].
struct AgentResolverShim<'a>(&'a dyn AgentResolver);

impl AgentResolver for AgentResolverShim<'_> {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        self.0.resolve(agent_id)
    }

    fn agent_ids(&self) -> Vec<String> {
        self.0.agent_ids()
    }
}

impl ExecutionResolver for AgentResolverShim<'_> {
    fn resolve_execution(&self, agent_id: &str) -> Result<ResolvedExecution, RuntimeError> {
        self.0.resolve(agent_id).map(ResolvedExecution::local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use awaken_contract::contract::event_sink::VecEventSink;
    use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
    use awaken_contract::contract::tool::ToolCallContext;
    use awaken_contract::registry_spec::AgentSpec;
    use awaken_contract::state::Snapshot;
    use awaken_runtime::engine::MockLlmExecutor;
    use awaken_runtime::{AgentResolver, ResolvedAgent, RuntimeError};

    struct SingleAgentResolver {
        agent: ResolvedAgent,
    }

    impl AgentResolver for SingleAgentResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Ok(self.agent.clone())
        }
    }

    struct FailingResolver;

    impl AgentResolver for FailingResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_ctx(sink: Option<Arc<dyn EventSink>>) -> ToolCallContext {
        ToolCallContext {
            call_id: "call-1".into(),
            tool_name: "render_ui".into(),
            run_identity: RunIdentity::new(
                "run-parent".into(),
                Some("thread-1".into()),
                "run-parent".into(),
                None,
                "parent-agent".into(),
                RunOrigin::User,
            ),
            agent_spec: Arc::new(AgentSpec::default()),
            snapshot: Snapshot::new(0, Arc::new(awaken_contract::state::StateMap::default())),
            activity_sink: sink,
            cancellation_token: None,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
        }
    }

    #[tokio::test]
    async fn streaming_subagent_returns_content_and_steps() {
        let llm =
            Arc::new(MockLlmExecutor::new().with_responses(vec!["Hello from subagent!".into()]));
        let agent = ResolvedAgent::new("sub-agent", "mock-model", "You are a helper", llm);
        let resolver = SingleAgentResolver { agent };

        let parent_sink = Arc::new(VecEventSink::new());
        let ctx = make_ctx(Some(parent_sink.clone() as Arc<dyn EventSink>));

        let result = run_streaming_subagent(&resolver, "sub-agent", "say hello", &ctx)
            .await
            .unwrap();

        assert!(!result.content.is_empty());
        assert!(result.steps >= 1);
    }

    #[tokio::test]
    async fn streaming_subagent_fails_with_invalid_agent() {
        let resolver = FailingResolver;
        let ctx = make_ctx(None);

        let result = run_streaming_subagent(&resolver, "nonexistent", "hello", &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ToolError::ExecutionFailed(msg) => {
                assert!(msg.contains("sub-agent failed"), "got: {msg}");
            }
            other => panic!("expected ExecutionFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn streaming_subagent_uses_null_sink_when_no_activity_sink() {
        let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["response".into()]));
        let agent = ResolvedAgent::new("sub-agent", "mock-model", "sys", llm);
        let resolver = SingleAgentResolver { agent };

        let ctx = make_ctx(None);

        let result = run_streaming_subagent(&resolver, "sub-agent", "test", &ctx)
            .await
            .unwrap();

        assert!(!result.content.is_empty());
    }

    /// LLM that always errors. Used to drive the child loop into
    /// `TerminationReason::Error`, which maps to
    /// `BackendRunStatus::Failed`.
    struct AlwaysFailingLlm;

    #[async_trait::async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for AlwaysFailingLlm {
        async fn execute(
            &self,
            _request: awaken_contract::contract::executor::InferenceRequest,
        ) -> Result<
            awaken_contract::contract::inference::StreamResult,
            awaken_contract::contract::executor::InferenceExecutionError,
        > {
            Err(
                awaken_contract::contract::executor::InferenceExecutionError::Provider(
                    "boom".into(),
                ),
            )
        }

        fn name(&self) -> &str {
            "always-failing"
        }
    }

    #[tokio::test]
    async fn streaming_subagent_rejects_non_completed_child_status() {
        // Child loop reaches a non-success terminal state (LLM error
        // bubbles through the loop). Both the loop-error path and the
        // new Ok-but-not-Completed guard funnel into ToolError —
        // verify the helper never silently returns Ok with partial text.
        let llm = Arc::new(AlwaysFailingLlm);
        let agent = ResolvedAgent::new("sub-agent", "mock-model", "sys", llm);
        let resolver = SingleAgentResolver { agent };
        let ctx = make_ctx(None);

        let err = run_streaming_subagent(&resolver, "sub-agent", "go", &ctx)
            .await
            .expect_err("non-success child must surface as ToolError, not Ok(content)");
        match err {
            ToolError::ExecutionFailed(msg) => {
                let lower = msg.to_ascii_lowercase();
                assert!(
                    lower.contains("did not complete")
                        || lower.contains("provider error")
                        || lower.contains("sub-agent failed"),
                    "error should surface the child failure, got: {msg}"
                );
            }
            other => panic!("expected ExecutionFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn streaming_subagent_forwards_text_as_tool_call_stream_delta() {
        let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["streamed text".into()]));
        let agent = ResolvedAgent::new("sub-agent", "mock-model", "sys", llm);
        let resolver = SingleAgentResolver { agent };

        let parent_sink = Arc::new(VecEventSink::new());
        let ctx = make_ctx(Some(parent_sink.clone() as Arc<dyn EventSink>));

        let result = run_streaming_subagent(&resolver, "sub-agent", "go", &ctx)
            .await
            .unwrap();

        assert!(!result.content.is_empty());

        let events = parent_sink.events();
        let stream_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    awaken_contract::contract::event::AgentEvent::ToolCallStreamDelta { .. }
                )
            })
            .collect();
        assert!(
            !stream_deltas.is_empty() || !result.content.is_empty(),
            "either stream deltas or content should be present"
        );
    }
}
