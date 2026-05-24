//! Helper function for running a streaming sub-agent.
//!
//! Thin composition of [`awaken_runtime::run_child_agent`] and
//! [`awaken_runtime::StreamingPassthroughSink`] — kept for backward
//! compatibility with downstream generative-UI integrations.

use std::sync::Arc;

use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{ToolCallContext, ToolError};
use awaken_runtime::AgentResolver;
use awaken_runtime::backend::{BackendParentContext, BackendRunStatus};
use awaken_runtime::child_agent::{ChildAgentParams, StreamingPassthroughSink, run_child_agent};

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

    let result = run_child_agent(
        ChildAgentParams::new(
            resolver,
            agent_id,
            vec![Message::user(prompt)],
            BackendParentContext {
                parent_run_id: Some(ctx.run_identity.run_id.clone()),
                parent_thread_id: Some(ctx.run_identity.thread_id.clone()),
                parent_tool_call_id: Some(ctx.call_id.clone()),
            },
            sink,
        )
        .with_cancellation_token(ctx.cancellation_token.clone()),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use awaken_contract::CancellationToken;
    use awaken_contract::contract::event_sink::VecEventSink;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
    use awaken_contract::contract::inference::{StopReason, StreamResult};
    use awaken_contract::contract::message::ToolCall;
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput,
    };
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
            !stream_deltas.is_empty(),
            "parent sink should receive ToolCallStreamDelta events"
        );
    }

    struct CancellationToolLlm;

    #[async_trait::async_trait]
    impl LlmExecutor for CancellationToolLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    "cancel-call",
                    "wait_for_cancel",
                    serde_json::json!({}),
                )],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "cancellation-tool"
        }
    }

    struct WaitForCancelTool;

    #[async_trait::async_trait]
    impl Tool for WaitForCancelTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "wait_for_cancel",
                "wait_for_cancel",
                "wait until cancellation is propagated",
            )
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let token = ctx
                .cancellation_token
                .as_ref()
                .expect("child tool should receive parent cancellation token");
            token.cancelled().await;
            Err(ToolError::ExecutionFailed("child tool cancelled".into()))
        }
    }

    #[tokio::test]
    async fn streaming_subagent_propagates_parent_cancellation() {
        let llm = Arc::new(CancellationToolLlm);
        let mut agent = ResolvedAgent::new("sub-agent", "mock-model", "sys", llm);
        agent
            .tools
            .insert("wait_for_cancel".into(), Arc::new(WaitForCancelTool));
        let resolver = SingleAgentResolver { agent };

        let token = CancellationToken::new();
        let mut ctx = make_ctx(None);
        ctx.cancellation_token = Some(token.clone());

        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_streaming_subagent(&resolver, "sub-agent", "go", &ctx),
        )
        .await
        .expect("child run should observe parent cancellation promptly");

        let err = result.expect_err("cancelled child should fail the streaming helper");
        match err {
            ToolError::ExecutionFailed(message) => {
                assert!(
                    message.to_ascii_lowercase().contains("cancel"),
                    "expected cancellation error, got: {message}"
                );
            }
            other => panic!("expected ExecutionFailed, got: {other:?}"),
        }
    }
}
