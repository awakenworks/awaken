//! Helper function for running a streaming sub-agent.

use std::sync::Arc;

use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{ToolCallContext, ToolError};
use awaken_runtime::AgentResolver;

use crate::sink::StreamingSubagentSink;

/// Result of a streaming sub-agent run.
pub struct StreamingSubagentResult {
    /// Accumulated text content from the sub-agent.
    pub content: String,
    /// Number of inference steps executed.
    pub steps: usize,
}

/// Run a sub-agent that streams its text output to the parent sink in real-time.
///
/// Text deltas from the sub-agent are forwarded as [`AgentEvent::ActivityDelta`]
/// events on the parent activity sink, while the full accumulated text is
/// returned in [`StreamingSubagentResult::content`].
pub async fn run_streaming_subagent(
    resolver: &dyn AgentResolver,
    agent_id: &str,
    prompt: &str,
    activity_type: &str,
    ctx: &ToolCallContext,
) -> Result<StreamingSubagentResult, ToolError> {
    let parent_sink = ctx
        .activity_sink
        .clone()
        .unwrap_or_else(|| Arc::new(awaken_contract::contract::event_sink::NullEventSink));
    let (streaming_sink, buffer) =
        StreamingSubagentSink::new(ctx.call_id.clone(), activity_type.to_string(), parent_sink);
    let sink: Arc<dyn EventSink> = Arc::new(streaming_sink);

    let store = awaken_runtime::StateStore::new();
    store
        .install_plugin(awaken_runtime::loop_runner::LoopStatePlugin)
        .map_err(|e| ToolError::ExecutionFailed(format!("state setup: {e}")))?;
    let phase_runtime = awaken_runtime::PhaseRuntime::new(store.clone())
        .map_err(|e| ToolError::ExecutionFailed(format!("phase setup: {e}")))?;

    let sub_run_id = uuid::Uuid::now_v7().to_string();
    let sub_identity = RunIdentity::new(
        sub_run_id.clone(),
        Some(ctx.run_identity.thread_id.clone()),
        sub_run_id,
        Some(ctx.run_identity.run_id.clone()),
        agent_id.to_string(),
        RunOrigin::Subagent,
    );

    let result =
        awaken_runtime::loop_runner::run_agent_loop(awaken_runtime::loop_runner::AgentLoopParams {
            resolver,
            agent_id,
            runtime: &phase_runtime,
            sink,
            checkpoint_store: None,
            messages: vec![Message::user(prompt)],
            run_identity: sub_identity,
            cancellation_token: None,
            decision_rx: None,
            overrides: None,
            frontend_tools: Vec::new(),
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("sub-agent failed: {e}")))?;

    let content = buffer.lock().await.clone();

    Ok(StreamingSubagentResult {
        content,
        steps: result.steps,
    })
}
