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
use awaken_protocol_a2a::{SendMessageResponse, Task, TaskState};
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

/// A raw A2A HTTP+JSON response: the status code and the body bytes.
pub struct A2aResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Performs an A2A HTTP+JSON request against a remote agent. The composition root
/// supplies the transport (HTTP with credentials, or an in-process router for
/// tests) — the host never names the wire mechanism. `path` is the A2A route
/// (e.g. `/v1/a2a/message:send`); the transport prepends the remote base and any
/// auth.
#[async_trait]
pub trait A2aTransport: Send + Sync {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<A2aResponse, String>;
}

/// A real HTTP `A2aTransport` to a remote A2A agent, with an optional bearer
/// token. `ureq` is synchronous, so each request runs on a blocking thread; a
/// non-2xx status is returned (not raised) so A2A's HTTP-status error envelope
/// reaches the caller.
pub struct HttpA2aTransport {
    base_url: String,
    bearer: Option<String>,
}

impl HttpA2aTransport {
    /// A transport to the remote agent at `base_url` (e.g. `https://host`); A2A
    /// paths are appended to it.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: None,
        }
    }

    /// Authenticate every request with `Authorization: Bearer <token>`.
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }
}

#[async_trait]
impl A2aTransport for HttpA2aTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<A2aResponse, String> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let method = method.to_string();
        let bearer = self.bearer.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut req = ureq::request(&method, &url);
            if let Some(token) = &bearer {
                req = req.set("authorization", &format!("Bearer {token}"));
            }
            let result = match body {
                Some(bytes) => req
                    .set("content-type", "application/json")
                    .send_bytes(&bytes),
                None => req.call(),
            };
            // A non-2xx status is a normal A2A error envelope, not a transport
            // failure — capture its status and body.
            let (status, response) = match result {
                Ok(response) => (response.status(), response),
                Err(ureq::Error::Status(code, response)) => (code, response),
                Err(err) => return Err(err.to_string()),
            };
            let mut buffer = Vec::new();
            response
                .into_reader()
                .read_to_end(&mut buffer)
                .map_err(|e| e.to_string())?;
            Ok(A2aResponse {
                status,
                body: buffer,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// A2A route the outbound client posts a turn to.
const MESSAGE_SEND_PATH: &str = "/v1/a2a/message:send";
/// Bound on task polling before giving up, so a stuck remote cannot hang a
/// delegation forever.
const MAX_TASK_POLLS: usize = 600;
/// Delay between task polls while a remote task is still `working`.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Read a `Task` from a `message:send` response (a `{ "task": ... }` envelope).
fn read_send_response(body: &[u8]) -> Result<Task, HostError> {
    let response: SendMessageResponse =
        serde_json::from_slice(body).map_err(|e| HostError::internal(e.to_string()))?;
    Ok(response.task)
}

/// Read a `Task` from a `tasks/get` response, accepting either a bare task or a
/// `{ "task": ... }` envelope.
fn read_task(body: &[u8]) -> Result<Task, HostError> {
    if let Ok(response) = serde_json::from_slice::<SendMessageResponse>(body) {
        return Ok(response.task);
    }
    serde_json::from_slice::<Task>(body).map_err(|e| HostError::internal(e.to_string()))
}

/// Fail on a non-2xx A2A response.
fn ok_status(response: &A2aResponse, what: &str) -> Result<(), HostError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(HostError::internal(format!(
            "remote A2A {what} failed: HTTP {}",
            response.status
        )))
    }
}

/// Fulfill a delegate call over A2A: post a `message:send`, poll the returned
/// `Task` to a terminal state while it is `working`, and read the remote agent's
/// reply. The stable `context_id` lets the remote keep this delegation's history
/// across turns. A non-completed terminal (input-required/failed) is surfaced as a
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
    let response = transport
        .request("POST", MESSAGE_SEND_PATH, Some(body))
        .await
        .map_err(HostError::internal)?;
    ok_status(&response, "message:send")?;
    let mut task = read_send_response(&response.body)?;

    // Poll while the remote task is still working (an async A2A backend returns a
    // task before it is done), bounded so a stuck remote cannot hang forever.
    let mut polls = 0usize;
    while matches!(task.status.state, TaskState::Working) {
        if polls >= MAX_TASK_POLLS {
            return Err(HostError::internal(
                "remote A2A task did not reach a terminal state in time",
            ));
        }
        polls += 1;
        let path = format!("/v1/a2a/tasks/{}", task.id);
        let response = transport
            .request("GET", &path, None)
            .await
            .map_err(HostError::internal)?;
        ok_status(&response, "tasks/get")?;
        task = read_task(&response.body)?;
        if matches!(task.status.state, TaskState::Working) {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    match task.status.state {
        TaskState::Completed => Ok(task
            .status
            .message
            .map(|message| message.text())
            .unwrap_or_default()),
        TaskState::InputRequired => Err(HostError::bad_request(
            "remote A2A agent requires further input",
        )),
        TaskState::Failed => Err(HostError::internal("remote A2A agent failed")),
        TaskState::Working => unreachable!("loop exits only on a terminal state"),
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
