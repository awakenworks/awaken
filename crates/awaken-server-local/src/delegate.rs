//! Delegation fulfillment: `agent_run` as a host-fulfilled parking tool.
//!
//! A delegate call parks (no executor is registered for `agent_run`); the host
//! fulfills the park by running the sub-agent and resuming the parent with its
//! result. The sub-agent runs either locally (a fresh rooted sub-run) or over A2A
//! (a remote agent, through an injected [`A2aTransport`]). Because the parent's
//! park is durable, a crash mid-delegation is recovered on the next drive.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_protocol_a2a::{SendMessageResponse, TaskState};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::ToolOutput;
use awaken_sandbox_local::{SandboxProvider, SandboxSpec};
use serde_json::json;

use crate::host::{
    BASE_SEQ, HostError, SessionCtx, SharedHost, build_runtime, latest_assistant_text,
    server_config,
};

/// The delegation tool id: a call to it parks and the host fulfills it.
pub(crate) const AGENT_RUN: &str = "agent_run";

/// The `(agent_id, input)` a delegate `agent_run` call carries.
fn delegate_args(arguments: &serde_json::Value) -> (String, String) {
    let field = |key: &str| {
        arguments
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (field("agent_id"), field("input"))
}

/// Sends an A2A `message:send` to a remote agent and returns the raw response
/// bytes. The composition root supplies the transport (HTTP, or an in-process
/// router for tests) — the host never names the wire mechanism.
#[async_trait]
pub trait A2aTransport: Send + Sync {
    async fn message_send(&self, body: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// Fulfill a delegate call over A2A: build a `message:send`, post it through the
/// transport, and read the remote agent's reply off the returned `Task`. The
/// stable `context_id` lets the remote keep this delegation's history across
/// turns. A non-completed task (working/input-required/failed) is surfaced as a
/// tool error, since this seam runs the delegate to completion.
async fn delegate_over_a2a(
    transport: &dyn A2aTransport,
    agent_id: &str,
    input: &str,
    context_id: &str,
) -> Result<String, HostError> {
    let request = json!({
        "agentId": agent_id,
        "message": {
            "messageId": format!("m-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst)),
            "contextId": context_id,
            "role": "ROLE_USER",
            "parts": [{ "text": input }],
        }
    });
    let body = serde_json::to_vec(&request).map_err(|e| HostError::internal(e.to_string()))?;
    let bytes = transport
        .message_send(body)
        .await
        .map_err(HostError::internal)?;
    let response: SendMessageResponse =
        serde_json::from_slice(&bytes).map_err(|e| HostError::internal(e.to_string()))?;
    let task = response.task;
    match task.status.state {
        TaskState::Completed => Ok(task
            .status
            .message
            .map(|message| message.text())
            .unwrap_or_default()),
        TaskState::InputRequired => Err(HostError::bad_request(
            "remote A2A agent requires further input",
        )),
        TaskState::Working => Err(HostError::internal("remote A2A agent is still working")),
        TaskState::Failed => Err(HostError::internal("remote A2A agent failed")),
    }
}

impl SharedHost {
    /// Register a delegate agent fulfilled over A2A: `agent_run` calls naming it
    /// are routed to `transport` (a remote agent) instead of a local sub-run. The
    /// id joins the advertised roster so the model can delegate to it.
    pub fn with_remote_a2a(
        mut self,
        agent_id: impl Into<String>,
        transport: Arc<dyn A2aTransport>,
    ) -> Self {
        let agent_id = agent_id.into();
        self.delegates.insert(agent_id.clone());
        self.remote_agents.insert(agent_id, transport);
        self
    }

    /// Run a delegate to completion and return its last reply, failing closed when
    /// the target is not in the roster. A remote agent is fulfilled over A2A; a
    /// local one is a fresh rooted sub-run with no delegation tool (no recursion).
    pub(crate) async fn run_delegate(
        &self,
        agent_id: &str,
        input: &str,
    ) -> Result<String, HostError> {
        if let Some(transport) = self.remote_agents.get(agent_id) {
            let context_id = format!("deleg-{agent_id}");
            return delegate_over_a2a(transport.as_ref(), agent_id, input, &context_id).await;
        }
        if !self.delegates.contains(agent_id) {
            return Err(HostError::bad_request(format!(
                "delegate agent {agent_id:?} is not in the roster"
            )));
        }
        let n = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let env = self
            .provider
            .create(&SandboxSpec::new(format!("{agent_id}-sub-{n}")))
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        let runtime = build_runtime(self.llm.clone(), env.root);
        let config = server_config(&self.model_ref, &HashSet::new(), &HashSet::new(), &[]);
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let thread = format!("sub-thread-{n}");
        let ctx = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone());
        runtime
            .run_to_completion(&config, thread.clone(), input, ctx, |_| {
                ResumeResult::allow()
            })
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(latest_assistant_text(
            &commit.committed_messages(&ThreadId(thread)),
        ))
    }

    /// Fulfill delegate `agent_run` parks in place: while the run is parked on a
    /// delegate call, run the sub-agent and resume the parent with its result, so
    /// delegation is transparent to the caller. The parent's park is durable, so a
    /// crash mid-delegation recovers here on the next drive. Non-delegate parks
    /// (client tools, HITL) are returned untouched for the caller to answer.
    pub(crate) async fn fulfill_delegations(
        &self,
        ctx: &SessionCtx,
        run_id: &RunId,
        mut phase: Phase,
    ) -> Result<Phase, HostError> {
        loop {
            if !matches!(phase, Phase::Waiting) {
                return Ok(phase);
            }
            let Some(ticket) = ctx.commit.waiting_ticket(run_id) else {
                return Ok(phase);
            };
            let pending = match &ticket.pending_tool {
                Some(tool) if tool.tool_id == AGENT_RUN => tool.clone(),
                _ => return Ok(phase),
            };
            let call_id = ticket.call_id.clone().unwrap_or_default();
            let (agent_id, input) = delegate_args(&pending.arguments);
            let output = match self.run_delegate(&agent_id, &input).await {
                Ok(text) => ToolOutput::ok(&call_id, text),
                Err(err) => ToolOutput::error(&call_id, err.to_string()),
            };
            let command = ResumeCommand::from_ticket(&ticket, ResumeResult::ToolResult(output), 0);
            phase = ctx
                .runtime
                .resume(command, &*ctx.commit, ctx.context())
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        }
    }
}
