//! The host delegation executor: run a child Agent behind the `agent_run` tool.
//!
//! This is the composition-root implementation of the kernel's [`DelegationExecutor`].
//! The kernel routes the delegation tool to it; here, native (in-process Agent Run)
//! and remote agents are *peer* implementations chosen by `agent_id`. A remote agent
//! is reached through the neutral [`RemoteAgent`] interface, so the wire (message shape,
//! task polling, discovery card) lives entirely in the adapter that implements it
//! (e.g. `awaken-run-executor-a2a`); this module owns only the *dispatch* — routing an
//! `agent_id` to its native or remote Agent execution — and names no protocol.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationExecutor, DelegationRequest, DelegationResume,
    DelegationStep, RemoteAgent,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::subagent::AgentRunSandbox;
use serde_json::Value;

use crate::host::{HostError, SharedHost};

/// The host's delegate agents (local + A2A-remote), behind one type owning their
/// shared invariant: `remotes` (agents fulfilled over A2A) is a *subset* of `ids`
/// (every advertised delegate). [`Self::add_remote`] maintains it by registering into
/// both; [`Self::native_ids`] derives the local delegates as `ids − remotes`. Keeping
/// the pair split let a caller register a remote delegate without also advertising
/// the delegate, silently breaking the `multiagent` roster.
///
/// `remotes` holds the neutral [`RemoteAgent`] interface, not a wire type: the A2A
/// transport + poll loop live in the adapter that implements it (Phase 2), so host
/// state names no protocol.
#[derive(Clone, Default)]
pub(crate) struct Delegates {
    /// All delegate agent ids (advertised as the agent's `multiagent` roster).
    ids: HashSet<String>,
    /// The subset fulfilled by a remote peer (agent id → the neutral delegate port)
    /// instead of a local Agent Run. Invariant: every key is also in `ids`.
    remotes: HashMap<String, Arc<dyn RemoteAgent>>,
    /// Per-Agent delegation rosters. Absence means that Agent has no delegation
    /// capability; being initiated by another Agent never implicitly copies the
    /// initiator's roster.
    agent_rosters: HashMap<String, HashSet<String>>,
}

impl Delegates {
    /// An empty roster.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add local delegate Agents (fulfilled by an in-process child Run).
    pub(crate) fn add_local(&mut self, ids: HashSet<String>) {
        self.ids.extend(ids);
    }

    /// Register a remote delegate: advertised in the roster AND routed to its neutral
    /// [`RemoteAgent`] interface. Maintains the `remotes ⊆ ids` invariant via both.
    pub(crate) fn add_remote(&mut self, agent_id: String, delegate: Arc<dyn RemoteAgent>) {
        self.ids.insert(agent_id.clone());
        self.remotes.insert(agent_id, delegate);
    }

    /// Configure the delegation targets available when `agent_id` itself runs.
    /// This is independent from whether `agent_id` appears in the root Agent's
    /// roster: each Agent owns its ordinary capability set.
    pub(crate) fn set_agent_roster(&mut self, agent_id: String, targets: HashSet<String>) {
        self.agent_rosters.insert(agent_id, targets);
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

    /// The remote-delegate port for `agent_id`, or `None` if it is local/unknown.
    pub(crate) fn remote(&self, agent_id: &str) -> Option<&Arc<dyn RemoteAgent>> {
        self.remotes.get(agent_id)
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

/// Runs delegates behind `agent_run`: local and remote Agents share the same
/// first-class child Run identity and lifecycle; only placement differs.
#[derive(Clone)]
pub(crate) struct HostDelegationExecutor {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: Arc<LocalProvider>,
    /// The parent agent's sandbox, shared with a native delegate by default so the two
    /// collaborate in one workspace (`默认共用`); bypassed when `reuse_sandbox` is off.
    sandbox: Arc<LocalSandbox>,
    /// Whether a native delegate reuses the parent sandbox (default) or gets a fresh,
    /// isolated one. Sandbox placement does not change child Run semantics.
    reuse_sandbox: bool,
    /// Delegates this Agent may call, regardless of local/remote placement.
    roster: HashSet<String>,
    /// All configured Agent identities, per-Agent rosters, and remote adapters.
    /// A child receives its own roster, never an implicit copy of its initiator's.
    delegates: Delegates,
}

impl HostDelegationExecutor {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        provider: Arc<LocalProvider>,
        sandbox: Arc<LocalSandbox>,
        reuse_sandbox: bool,
        delegates: Delegates,
    ) -> Self {
        let roster = delegates.ids.clone();
        Self {
            llm,
            model_ref,
            provider,
            sandbox,
            reuse_sandbox,
            roster,
            delegates,
        }
    }

    /// Run a native delegate as an ordinary Agent under a first-class child Run id.
    /// It receives that Agent's configured delegation roster; it may recursively
    /// delegate when its own config allows it. Sandbox placement is the only local
    /// execution choice made here.
    async fn native_run(
        &self,
        agent_id: &str,
        child_run_id: &awaken_agent_contract::agent::run::Id,
        input: &str,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let sandbox = if self.reuse_sandbox {
            AgentRunSandbox::Shared(&self.sandbox)
        } else {
            AgentRunSandbox::Fresh(&self.provider)
        };
        // The child keeps its own committed usage; the returned value additionally
        // rolls that usage into the initiating Run's accounting projection.
        let delegates = self
            .delegates
            .agent_rosters
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        let delegation_executor = if delegates.is_empty() {
            None
        } else {
            let mut child_executor = self.clone();
            child_executor.roster.clone_from(&delegates);
            Some(Arc::new(child_executor) as Arc<dyn DelegationExecutor>)
        };
        let (text, usage) = crate::subagent::run_agent(
            self.llm.clone(),
            crate::subagent::AgentExecution {
                agent_id,
                model_ref: &self.model_ref,
                delegates: &delegates,
                delegation_executor,
                context: Some(context),
            },
            sandbox,
            crate::subagent::AgentRunIdentity::child(child_run_id),
            input,
            None,
            crate::subagent::UsageRollup::FoldIntoParent,
        )
        .await
        .map_err(DelegationExecutionError::new)?;
        Ok(DelegationStep::Ended { text, usage })
    }
}

#[async_trait]
impl DelegationExecutor for HostDelegationExecutor {
    fn tool_id(&self) -> &str {
        AGENT_RUN
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let (agent_id, input) = delegate_args(&request.arguments);
        if !self.roster.contains(&agent_id) {
            return Err(DelegationExecutionError::new(format!(
                "delegate agent {agent_id:?} is not in this Agent's roster"
            )));
        }
        if let Some(remote) = self.delegates.remotes.get(&agent_id) {
            return remote
                .run(&agent_id, &input, request.context.cancellation.as_ref())
                .await;
        }
        self.native_run(&agent_id, &request.child_run_id, &input, request.context)
            .await
    }

    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        // The handle names the remote agent whose task awaiting for input; deliver
        // the user's input as a follow-up turn on the same context.
        let agent_id = request
            .continuation
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DelegationExecutionError::new("delegation continuation is missing agent_id")
            })?;
        let remote = self.delegates.remotes.get(agent_id).ok_or_else(|| {
            DelegationExecutionError::new(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        remote
            .run(
                agent_id,
                &request.input,
                request.context.cancellation.as_ref(),
            )
            .await
    }
}

impl SharedHost {
    /// Fetch a remote delegate's discovery card (outbound discovery) as neutral JSON.
    /// Fails if the agent is not a registered remote. The wire card shape lives in the
    /// adapter behind the [`RemoteAgent`] interface; the host only echoes the value.
    pub async fn remote_agent_card(&self, agent_id: &str) -> Result<Value, HostError> {
        let remote = self.delegates.remote(agent_id).ok_or_else(|| {
            HostError::bad_request(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        remote
            .card(agent_id)
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }
}
