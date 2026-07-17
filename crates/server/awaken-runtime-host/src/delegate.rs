//! The delegation resolver: run a sub-agent behind the `agent_run` tool.
//!
//! This is the composition-root adapter of the kernel's [`AgentResolver`] port.
//! The kernel routes the delegation tool to it; here, native (in-process sub-run)
//! and remote (A2A) agents are *peer* implementations chosen by `agent_id`. The
//! A2A *wire* (routes, message shape, task polling) lives in the
//! [`awaken_protocol_a2a::client`] bounded context; this module owns only the
//! *delegation semantics* — dispatch, the poll loop with cancellation, and mapping
//! a task's state to an [`AgentStep`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{AgentCard, Task, TaskState};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::{AgentError, AgentRequest, AgentResolver, AgentStep};
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::subagent::SubrunSandbox;
use serde_json::{Value, json};

use crate::host::{BASE_SEQ, HostError, SharedHost};

/// Bound on task polling before giving up, so a stuck remote cannot hang a
/// delegation forever.
const MAX_TASK_POLLS: usize = 600;
/// Delay between task polls while a remote task is still `working`.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// The host's delegate agents (local + A2A-remote), behind one type owning their
/// shared invariant: `remotes` (agents fulfilled over A2A) is a *subset* of `ids`
/// (every advertised delegate). [`Self::add_remote`] maintains it by registering into
/// both; [`Self::native_ids`] derives the local delegates as `ids − remotes`. Keeping
/// the pair split let a caller register a remote transport without also advertising
/// the delegate, silently breaking the `multiagent` roster.
///
/// NOTE (protocol leak, tracked separately): `remotes` holds
/// [`awaken_protocol_a2a`]'s `Transport` directly — a wire type on host state.
/// Replacing it with a neutral remote-agent port is a later step, not this pass.
#[derive(Default)]
pub(crate) struct Delegates {
    /// All delegate agent ids (advertised as the agent's `multiagent` roster).
    ids: HashSet<String>,
    /// The subset fulfilled over A2A (agent id → transport) instead of a local
    /// sub-run. Invariant: every key is also in `ids` (see [`Self::add_remote`]).
    remotes: HashMap<String, Arc<dyn Transport>>,
}

impl Delegates {
    /// An empty roster.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add local delegate agents (fulfilled by an in-process sub-run).
    pub(crate) fn add_local(&mut self, ids: HashSet<String>) {
        self.ids.extend(ids);
    }

    /// Register a remote (A2A) delegate: advertised in the roster AND routed to its
    /// transport. Maintains the `remotes ⊆ ids` invariant by inserting into both.
    pub(crate) fn add_remote(&mut self, agent_id: String, transport: Arc<dyn Transport>) {
        self.ids.insert(agent_id.clone());
        self.remotes.insert(agent_id, transport);
    }

    /// Whether the roster is empty (no delegates configured at all).
    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// All delegate ids (the `multiagent` advertisement).
    pub(crate) fn ids(&self) -> Vec<String> {
        self.ids.iter().cloned().collect()
    }

    /// The advertised id set, for callers that pass it by reference (config build).
    pub(crate) fn ids_set(&self) -> &HashSet<String> {
        &self.ids
    }

    /// The transport for a remote delegate, or `None` if `agent_id` is local/unknown.
    pub(crate) fn remote_transport(&self, agent_id: &str) -> Option<&Arc<dyn Transport>> {
        self.remotes.get(agent_id)
    }

    /// The native (in-process) delegate ids: the roster minus the remotes.
    pub(crate) fn native_ids(&self) -> HashSet<String> {
        self.ids
            .iter()
            .filter(|id| !self.remotes.contains_key(*id))
            .cloned()
            .collect()
    }

    /// A clone of the remote transport map, for injection into the delegation resolver.
    pub(crate) fn remotes(&self) -> HashMap<String, Arc<dyn Transport>> {
        self.remotes.clone()
    }
}

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
        // A remote (A2A) delegate runs on another host: its token spend is not
        // observable over the A2A wire, so no usage rolls into the parent tally.
        TaskState::Completed => Ok(AgentStep::Done {
            text: completed_reply(&task),
            usage: ThreadUsage::default(),
        }),
        TaskState::InputRequired | TaskState::AuthRequired => Ok(AgentStep::Parked {
            handle: json!({ "agent_id": agent_id, "task_id": task.id }),
        }),
        TaskState::Failed => Err(AgentError::new("remote A2A agent failed")),
        TaskState::Canceled => Err(AgentError::new("remote A2A task was canceled")),
        TaskState::Working => Err(AgentError::new(
            "remote A2A task did not reach a terminal state in time",
        )),
    }
}

/// Run one remote-agent turn: `message:send`, poll `working` to a terminal state
/// (bounded, cancellation-aware; a parent interrupt cancels the remote task), then
/// map the task to a step.
async fn remote_run(
    transport: &dyn Transport,
    agent_id: &str,
    input: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<AgentStep, AgentError> {
    let context_id = format!("deleg-{agent_id}");
    let message_id = format!("m-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst));
    let mut task = a2a::send_message(transport, Some(agent_id), &context_id, &message_id, input)
        .await
        .map_err(|e| AgentError::new(e.to_string()))?;

    let mut polls = 0usize;
    while matches!(task.status.state, TaskState::Working) {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            a2a::cancel_task(transport, &task.id).await;
            return Err(AgentError::new("remote A2A delegation was cancelled"));
        }
        if polls >= MAX_TASK_POLLS {
            break;
        }
        polls += 1;
        task = a2a::get_task(transport, &task.id)
            .await
            .map_err(|e| AgentError::new(e.to_string()))?;
        if matches!(task.status.state, TaskState::Working) {
            match cancellation {
                Some(token) => {
                    tokio::select! {
                        _ = tokio::time::sleep(POLL_INTERVAL) => {}
                        _ = token.cancelled() => {
                            a2a::cancel_task(transport, &task.id).await;
                            return Err(AgentError::new("remote A2A delegation was cancelled"));
                        }
                    }
                }
                None => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
    step_from_task(agent_id, task)
}

/// Runs delegates behind `agent_run`: local agents as fresh rooted sub-runs, and
/// A2A agents as remote turns — peers chosen by `agent_id`.
pub(crate) struct DelegationResolver {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalProvider,
    /// The parent agent's sandbox, shared with a native delegate by default so the two
    /// collaborate in one workspace (`默认共用`); bypassed when `reuse_sandbox` is off.
    sandbox: Arc<LocalSandbox>,
    /// Whether a native delegate reuses the parent sandbox (default) or gets a fresh,
    /// isolated one. The per-subagent knob behind "new sandbox vs. shared".
    reuse_sandbox: bool,
    /// Local (native) delegate ids.
    roster: HashSet<String>,
    /// Remote (A2A) delegate ids → transport.
    remotes: HashMap<String, Arc<dyn Transport>>,
}

impl DelegationResolver {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        provider: LocalProvider,
        sandbox: Arc<LocalSandbox>,
        reuse_sandbox: bool,
        roster: HashSet<String>,
        remotes: HashMap<String, Arc<dyn Transport>>,
    ) -> Self {
        Self {
            llm,
            model_ref,
            provider,
            sandbox,
            reuse_sandbox,
            roster,
            remotes,
        }
    }

    /// Run a native (in-process) delegate: a rooted sub-run over the same model with no
    /// delegation tool (a delegate cannot recurse). By default it shares the parent's
    /// sandbox (`默认共用`); with `reuse_sandbox` off it gets a fresh, isolated one.
    async fn native_run(
        &self,
        agent_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        let n = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let name = format!("{agent_id}-sub-{n}");
        let sandbox = if self.reuse_sandbox {
            SubrunSandbox::Shared(&self.sandbox)
        } else {
            SubrunSandbox::Fresh(&self.provider)
        };
        // The sub-run's usage rides back on the step so the kernel folds it into the
        // parent thread's tally (its own isolated store is dropped here) — turn work,
        // so it counts against the session.
        let (text, usage) = crate::subagent::run_subagent(
            self.llm.clone(),
            &self.model_ref,
            sandbox,
            &name,
            input,
            cancellation.cloned(),
            crate::subagent::UsageRollup::FoldIntoParent,
        )
        .await
        .map_err(AgentError::new)?;
        Ok(AgentStep::Done { text, usage })
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
        self.native_run(&agent_id, &input, request.cancellation.as_ref())
            .await
    }

    async fn resume(
        &self,
        handle: &Value,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        // The handle names the remote agent whose task parked for input; deliver
        // the user's input as a follow-up turn on the same context.
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
    pub async fn remote_agent_card(&self, agent_id: &str) -> Result<AgentCard, HostError> {
        let transport = self.delegates.remote_transport(agent_id).ok_or_else(|| {
            HostError::bad_request(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        a2a::agent_card(transport.as_ref())
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }
}
