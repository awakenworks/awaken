//! Delegation fulfillment: `agent_run` as a host-fulfilled parking tool.
//!
//! A delegate call parks (no executor is registered for `agent_run`); the host
//! fulfills the park by running the sub-agent and resuming the parent with its
//! result. The sub-agent runs either locally (a fresh rooted sub-run) or over A2A
//! (a remote agent, through an injected [`A2aTransport`]). Because the parent's
//! park is durable, a crash mid-delegation is recovered on the next drive.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_protocol_a2a::{AgentCard, SendMessageResponse, Task, TaskState};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::ToolOutput;
use awaken_sandbox_local::{SandboxProvider, SandboxSpec};
use serde_json::json;

use crate::host::{
    BASE_SEQ, HostError, SessionCtx, SharedHost, build_runtime, latest_assistant_text,
    sanitize_thread, server_config,
};

/// The delegation tool id: a call to it parks and the host fulfills it.
pub(crate) const AGENT_RUN: &str = "agent_run";

/// The result of running a delegate one step.
pub(crate) enum DelegateOutcome {
    /// The delegate finished with this reply.
    Done(String),
    /// A remote delegate needs more input (its A2A task is `input-required`); the
    /// parent parks for the user to supply it, delivered as a follow-up turn.
    NeedsInput,
}

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

/// Submit a delegation turn to a remote agent (`message:send`) and return the
/// initial `Task`. The stable `context_id` lets the remote keep this delegation's
/// history across turns.
async fn submit_a2a(
    transport: &dyn A2aTransport,
    agent_id: &str,
    input: &str,
    context_id: &str,
) -> Result<Task, HostError> {
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
    read_send_response(&response.body)
}

/// Fetch a remote task by id (`tasks/get`) — used to reattach to an in-flight task
/// after a restart, instead of resubmitting.
async fn fetch_task(transport: &dyn A2aTransport, task_id: &str) -> Result<Task, HostError> {
    let path = format!("/v1/a2a/tasks/{task_id}");
    let response = transport
        .request("GET", &path, None)
        .await
        .map_err(HostError::internal)?;
    ok_status(&response, "tasks/get")?;
    read_task(&response.body)
}

/// Poll `task` to a terminal state while it is `working` (an async A2A backend
/// returns a task before it is done), bounded so a stuck remote cannot hang
/// forever. A parent interrupt cancels the wait and best-effort cancels the remote
/// task. A non-completed terminal (input-required/failed) maps accordingly.
async fn poll_to_terminal(
    transport: &dyn A2aTransport,
    mut task: Task,
    cancellation: Option<&CancellationToken>,
) -> Result<DelegateOutcome, HostError> {
    let mut polls = 0usize;
    while matches!(task.status.state, TaskState::Working) {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            cancel_remote_task(transport, &task.id).await;
            return Err(HostError::internal("remote A2A delegation was cancelled"));
        }
        if polls >= MAX_TASK_POLLS {
            return Err(HostError::internal(
                "remote A2A task did not reach a terminal state in time",
            ));
        }
        polls += 1;
        task = fetch_task(transport, &task.id).await?;
        if matches!(task.status.state, TaskState::Working) {
            match cancellation {
                Some(token) => {
                    tokio::select! {
                        _ = tokio::time::sleep(POLL_INTERVAL) => {}
                        _ = token.cancelled() => {
                            cancel_remote_task(transport, &task.id).await;
                            return Err(HostError::internal("remote A2A delegation was cancelled"));
                        }
                    }
                }
                None => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    match task.status.state {
        TaskState::Completed => Ok(DelegateOutcome::Done(completed_reply(&task))),
        // The remote agent asked for more input: park the parent so the user can
        // supply it (delivered as a follow-up `message:send` on the same context).
        TaskState::InputRequired => Ok(DelegateOutcome::NeedsInput),
        TaskState::Failed => Err(HostError::internal("remote A2A agent failed")),
        TaskState::Working => unreachable!("loop exits only on a terminal state"),
    }
}

/// The reply text of a completed task: the status message plus any artifact text
/// the remote produced (A2A `TextAndArtifacts`).
fn completed_reply(task: &Task) -> String {
    let mut reply = task
        .status
        .message
        .as_ref()
        .map(|message| message.text())
        .unwrap_or_default();
    for artifact in &task.artifacts {
        let text = artifact.text();
        if !text.is_empty() {
            if !reply.is_empty() {
                reply.push('\n');
            }
            reply.push_str(&text);
        }
    }
    reply
}

/// Best-effort cancel of a remote A2A task (A2A `tasks:cancel`). Failures are
/// ignored — the local delegation already ended cancelled.
async fn cancel_remote_task(transport: &dyn A2aTransport, task_id: &str) {
    let path = format!("/v1/a2a/tasks/{task_id}:cancel");
    let _ = transport.request("POST", &path, None).await;
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

    /// The durable file that records a thread's in-flight remote task id, so a
    /// crash mid-delegation reattaches instead of resubmitting. `None` (no store
    /// dir) means no reattach — an in-memory session cannot survive a restart.
    fn remote_handle_path(&self, thread: &str) -> Option<PathBuf> {
        self.store_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.remote", sanitize_thread(thread))))
    }

    /// Persist a thread's in-flight remote `(agent_id, task_id)`.
    fn persist_remote_handle(&self, thread: &str, agent_id: &str, task_id: &str) {
        if let Some(path) = self.remote_handle_path(thread) {
            let record = json!({ "agent_id": agent_id, "task_id": task_id }).to_string();
            let _ = std::fs::write(path, record);
        }
    }

    /// The persisted in-flight task id for `thread` if it matches `agent_id`.
    fn load_remote_handle(&self, thread: &str, agent_id: &str) -> Option<String> {
        let path = self.remote_handle_path(thread)?;
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        if value.get("agent_id")?.as_str()? != agent_id {
            return None;
        }
        value.get("task_id")?.as_str().map(str::to_string)
    }

    /// Clear a thread's in-flight remote handle (the task reached a terminal state).
    fn clear_remote_handle(&self, thread: &str) {
        if let Some(path) = self.remote_handle_path(thread) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Fulfill a remote delegation: reattach to a persisted in-flight task if one
    /// exists (after a restart), else submit a fresh `message:send` and persist its
    /// task id before polling — so a crash mid-poll reattaches rather than
    /// resubmitting (at-most-once submission). The handle is cleared on a terminal.
    async fn remote_fulfill(
        &self,
        thread: &str,
        agent_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegateOutcome, HostError> {
        let transport = self.remote_agents.get(agent_id).ok_or_else(|| {
            HostError::internal(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        let context_id = format!("deleg-{agent_id}");
        let task = match self.load_remote_handle(thread, agent_id) {
            Some(task_id) => fetch_task(transport.as_ref(), &task_id).await?,
            None => {
                let task = submit_a2a(transport.as_ref(), agent_id, input, &context_id).await?;
                self.persist_remote_handle(thread, agent_id, &task.id);
                task
            }
        };
        let outcome = poll_to_terminal(transport.as_ref(), task, cancellation).await;
        self.clear_remote_handle(thread);
        outcome
    }

    /// Fetch a remote delegate's A2A agent card (discovery): its advertised name,
    /// version, capabilities, and skills. Fails if the agent is not a registered
    /// remote.
    pub async fn remote_agent_card(&self, agent_id: &str) -> Result<AgentCard, HostError> {
        let transport = self.remote_agents.get(agent_id).ok_or_else(|| {
            HostError::internal(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        let response = transport
            .request("GET", "/v1/a2a/agent-card", None)
            .await
            .map_err(HostError::internal)?;
        ok_status(&response, "agent-card")?;
        serde_json::from_slice::<AgentCard>(&response.body)
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// Run a delegate to completion and return its last reply, failing closed when
    /// the target is not in the roster. A remote agent is fulfilled over A2A; a
    /// local one is a fresh rooted sub-run with no delegation tool (no recursion).
    pub(crate) async fn run_delegate(
        &self,
        thread: &str,
        agent_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegateOutcome, HostError> {
        if self.remote_agents.contains_key(agent_id) {
            return self
                .remote_fulfill(thread, agent_id, input, cancellation)
                .await;
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
        Ok(DelegateOutcome::Done(latest_assistant_text(
            &commit.committed_messages(&ThreadId(thread)),
        )))
    }

    /// Deliver the user's `content` to a remote A2A agent that previously asked for
    /// input (a follow-up `message:send` on the same context), returning the next
    /// step. Errors if the agent is not a registered remote.
    pub(crate) async fn deliver_remote_input(
        &self,
        thread: &str,
        agent_id: &str,
        content: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegateOutcome, HostError> {
        self.remote_fulfill(thread, agent_id, content, cancellation)
            .await
    }

    /// Fulfill delegate `agent_run` parks in place: while the run is parked on a
    /// delegate call, run the sub-agent and resume the parent with its result, so
    /// delegation is transparent to the caller. The parent's park is durable, so a
    /// crash mid-delegation recovers here on the next drive. Non-delegate parks
    /// (client tools, HITL) are returned untouched for the caller to answer.
    ///
    /// Returns the terminal/parked phase and whether the run is now parked awaiting
    /// *remote input*: a remote delegate asked for more input, so the parent stays
    /// parked for the user to supply it (via `resume` with a client result).
    pub(crate) async fn fulfill_delegations(
        &self,
        ctx: &SessionCtx,
        run_id: &RunId,
        mut phase: Phase,
    ) -> Result<(Phase, bool), HostError> {
        loop {
            if !matches!(phase, Phase::Waiting) {
                return Ok((phase, false));
            }
            let Some(ticket) = ctx.commit.waiting_ticket(run_id) else {
                return Ok((phase, false));
            };
            let pending = match &ticket.pending_tool {
                Some(tool) if tool.tool_id == AGENT_RUN => tool.clone(),
                _ => return Ok((phase, false)),
            };
            let call_id = ticket.call_id.clone().unwrap_or_default();
            let (agent_id, input) = delegate_args(&pending.arguments);
            // Guard the delegation with a cancel token so a concurrent `interrupt`
            // aborts it (and cancels the remote task).
            let token = ctx.register_cancel();
            let output = match self
                .run_delegate(&ctx.thread_id.0, &agent_id, &input, Some(&token))
                .await
            {
                Ok(DelegateOutcome::Done(text)) => ToolOutput::ok(&call_id, text),
                // The remote needs input: leave the parent parked for the user.
                Ok(DelegateOutcome::NeedsInput) => return Ok((phase, true)),
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
