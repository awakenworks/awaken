//! The delegation resolver: run a sub-agent behind the `agent_run` tool.
//!
//! This is the composition-root adapter of the kernel's [`AgentResolver`] port.
//! The kernel routes the delegation tool to it; here, native (in-process sub-run)
//! and remote (A2A) agents are *peer* implementations chosen by `agent_id`. All
//! delegation orchestration — running the sub-agent, polling a remote task, and
//! parking for remote input — is owned by the kernel via this port; the host only
//! injects the resolver.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_protocol_a2a::{AgentCard, SendMessageResponse, Task, TaskState};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::{AgentError, AgentRequest, AgentResolver, AgentStep};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_sandbox_local::{LocalSandboxProvider, SandboxProvider, SandboxSpec};
use serde_json::{Value, json};

use crate::host::{BASE_SEQ, SharedHost, build_runtime, latest_assistant_text, server_config};

/// The delegation tool id the resolver backs. Model-visible; not named by the
/// kernel (the kernel matches on `AgentResolver::tool_id`).
pub(crate) const AGENT_RUN: &str = "agent_run";

/// The `(agent_id, input)` a delegate `agent_run` call carries.
fn delegate_args(arguments: &Value) -> (String, String) {
    let field = |key: &str| {
        arguments
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (field("agent_id"), field("input"))
}

// ── Transport port ───────────────────────────────────────────────────────────

/// A raw A2A HTTP+JSON response: the status code and the body bytes.
pub struct A2aResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Performs an A2A HTTP+JSON request against a remote agent. The composition root
/// supplies the transport (HTTP with credentials, or an in-process router for
/// tests) — neither host nor kernel names the wire mechanism. `path` is the A2A
/// route (e.g. `/v1/a2a/message:send`); the transport prepends the remote base and
/// any auth.
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

// ── A2A client helpers ───────────────────────────────────────────────────────

const MESSAGE_SEND_PATH: &str = "/v1/a2a/message:send";
const MAX_TASK_POLLS: usize = 600;
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

fn read_send_response(body: &[u8]) -> Result<Task, AgentError> {
    let response: SendMessageResponse =
        serde_json::from_slice(body).map_err(|e| AgentError::new(e.to_string()))?;
    Ok(response.task)
}

fn read_task(body: &[u8]) -> Result<Task, AgentError> {
    if let Ok(response) = serde_json::from_slice::<SendMessageResponse>(body) {
        return Ok(response.task);
    }
    serde_json::from_slice::<Task>(body).map_err(|e| AgentError::new(e.to_string()))
}

fn ok_status(response: &A2aResponse, what: &str) -> Result<(), AgentError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(AgentError::new(format!(
            "remote A2A {what} failed: HTTP {}",
            response.status
        )))
    }
}

/// Submit a delegation turn (`message:send`) and return the initial task.
async fn submit_a2a(
    transport: &dyn A2aTransport,
    agent_id: &str,
    input: &str,
    context_id: &str,
) -> Result<Task, AgentError> {
    let request = json!({
        "agentId": agent_id,
        "message": {
            "messageId": format!("m-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst)),
            "contextId": context_id,
            "role": "ROLE_USER",
            "parts": [{ "text": input }],
        }
    });
    let body = serde_json::to_vec(&request).map_err(|e| AgentError::new(e.to_string()))?;
    let response = transport
        .request("POST", MESSAGE_SEND_PATH, Some(body))
        .await
        .map_err(AgentError::new)?;
    ok_status(&response, "message:send")?;
    read_send_response(&response.body)
}

/// Fetch a remote task by id (`tasks/get`).
async fn fetch_task(transport: &dyn A2aTransport, task_id: &str) -> Result<Task, AgentError> {
    let path = format!("/v1/a2a/tasks/{task_id}");
    let response = transport
        .request("GET", &path, None)
        .await
        .map_err(AgentError::new)?;
    ok_status(&response, "tasks/get")?;
    read_task(&response.body)
}

/// Best-effort cancel of a remote A2A task (A2A `tasks:cancel`).
async fn cancel_remote_task(transport: &dyn A2aTransport, task_id: &str) {
    let path = format!("/v1/a2a/tasks/{task_id}:cancel");
    let _ = transport.request("POST", &path, None).await;
}

/// Poll `task` inline while it is `working`, bounded so a stuck remote cannot hang
/// forever. A parent interrupt cancels the wait and best-effort cancels the remote
/// task. Returns the task in a non-working state.
async fn poll_working(
    transport: &dyn A2aTransport,
    mut task: Task,
    cancellation: Option<&CancellationToken>,
) -> Result<Task, AgentError> {
    let mut polls = 0usize;
    while matches!(task.status.state, TaskState::Working) {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            cancel_remote_task(transport, &task.id).await;
            return Err(AgentError::new("remote A2A delegation was cancelled"));
        }
        if polls >= MAX_TASK_POLLS {
            return Err(AgentError::new(
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
                            return Err(AgentError::new("remote A2A delegation was cancelled"));
                        }
                    }
                }
                None => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
    Ok(task)
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

/// Map a non-working task to a delegation step: completed → done; input/auth
/// required → parked (the parent parks for the user, resumed via the handle);
/// failed → error.
fn step_from_task(agent_id: &str, task: Task) -> Result<AgentStep, AgentError> {
    match task.status.state {
        TaskState::Completed => Ok(AgentStep::Done {
            text: completed_reply(&task),
        }),
        TaskState::InputRequired | TaskState::AuthRequired => Ok(AgentStep::Parked {
            handle: json!({ "agent_id": agent_id, "task_id": task.id }),
        }),
        TaskState::Failed => Err(AgentError::new("remote A2A agent failed")),
        TaskState::Working => Err(AgentError::new(
            "remote A2A task did not reach a terminal state in time",
        )),
    }
}

/// Run one remote-agent turn: `message:send`, poll to a terminal, map to a step.
async fn remote_run(
    transport: &dyn A2aTransport,
    agent_id: &str,
    input: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<AgentStep, AgentError> {
    let context_id = format!("deleg-{agent_id}");
    let task = submit_a2a(transport, agent_id, input, &context_id).await?;
    let task = poll_working(transport, task, cancellation).await?;
    step_from_task(agent_id, task)
}

// ── The resolver ─────────────────────────────────────────────────────────────

/// Runs delegates behind `agent_run`: local agents as fresh rooted sub-runs, and
/// A2A agents as remote turns — peers chosen by `agent_id`.
pub(crate) struct DelegationResolver {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalSandboxProvider,
    /// Local (native) delegate ids.
    roster: HashSet<String>,
    /// Remote (A2A) delegate ids → transport.
    remotes: HashMap<String, Arc<dyn A2aTransport>>,
}

impl DelegationResolver {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        provider: LocalSandboxProvider,
        roster: HashSet<String>,
        remotes: HashMap<String, Arc<dyn A2aTransport>>,
    ) -> Self {
        Self {
            llm,
            model_ref,
            provider,
            roster,
            remotes,
        }
    }

    /// Run a native (in-process) delegate: a fresh rooted sub-run over the same
    /// model with no delegation tool (a delegate cannot recurse).
    async fn native_run(&self, agent_id: &str, input: &str) -> Result<AgentStep, AgentError> {
        let n = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let env = self
            .provider
            .create(&SandboxSpec::new(format!("{agent_id}-sub-{n}")))
            .await
            .map_err(|e| AgentError::new(e.to_string()))?;
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
            .map_err(|e| AgentError::new(e.to_string()))?;
        Ok(AgentStep::Done {
            text: latest_assistant_text(&commit.committed_messages(&ThreadId(thread))),
        })
    }
}

#[async_trait]
impl AgentResolver for DelegationResolver {
    fn tool_id(&self) -> &str {
        AGENT_RUN
    }

    async fn run(&self, request: AgentRequest) -> Result<AgentStep, AgentError> {
        let (agent_id, input) = delegate_args(&request.arguments);
        if let Some(transport) = self.remotes.get(&agent_id) {
            return remote_run(
                transport.as_ref(),
                &agent_id,
                &input,
                request.cancellation.as_ref(),
            )
            .await;
        }
        if !self.roster.contains(&agent_id) {
            return Err(AgentError::new(format!(
                "delegate agent {agent_id:?} is not in the roster"
            )));
        }
        self.native_run(&agent_id, &input).await
    }

    async fn resume(
        &self,
        handle: &Value,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        // The handle names the remote agent whose task parked for input; deliver
        // the user's input as a follow-up `message:send` on the same context.
        let agent_id = handle
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::new("delegation handle is missing agent_id"))?;
        let transport = self
            .remotes
            .get(agent_id)
            .ok_or_else(|| AgentError::new(format!("agent {agent_id:?} is not a remote agent")))?;
        remote_run(transport.as_ref(), agent_id, input, cancellation).await
    }
}

impl SharedHost {
    /// Fetch a remote delegate's A2A agent card (outbound discovery). Fails if the
    /// agent is not a registered remote.
    pub async fn remote_agent_card(&self, agent_id: &str) -> Result<AgentCard, String> {
        let transport = self
            .remote_agents
            .get(agent_id)
            .ok_or_else(|| format!("agent {agent_id:?} is not a remote agent"))?;
        let response = transport.request("GET", "/v1/a2a/agent-card", None).await?;
        if !(200..300).contains(&response.status) {
            return Err(format!("agent-card failed: HTTP {}", response.status));
        }
        serde_json::from_slice::<AgentCard>(&response.body).map_err(|e| e.to_string())
    }
}
