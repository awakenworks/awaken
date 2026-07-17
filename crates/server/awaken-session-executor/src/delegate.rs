//! The delegation resolver: run a sub-agent behind the `agent_run` tool.
//!
//! This is the composition-root adapter of the kernel's [`AgentResolver`] port.
//! The kernel routes the delegation tool to it; here, native (in-process sub-run)
//! and remote agents are *peer* implementations chosen by `agent_id`. A remote agent
//! is reached through the neutral [`RemoteDelegate`] port, so the wire (message shape,
//! task polling, discovery card) lives entirely in the adapter that implements it
//! (e.g. `awaken-run-executor-a2a`); this module owns only the *dispatch* — routing an
//! `agent_id` to its native sub-run or its remote delegate — and names no protocol.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::{
    AgentError, AgentRequest, AgentResolver, AgentStep, RemoteDelegate,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::subagent::SubrunSandbox;
use serde_json::Value;

use crate::host::{BASE_SEQ, HostError, SharedHost};

/// The host's delegate agents (local + A2A-remote), behind one type owning their
/// shared invariant: `remotes` (agents fulfilled over A2A) is a *subset* of `ids`
/// (every advertised delegate). [`Self::add_remote`] maintains it by registering into
/// both; [`Self::native_ids`] derives the local delegates as `ids − remotes`. Keeping
/// the pair split let a caller register a remote delegate without also advertising
/// the delegate, silently breaking the `multiagent` roster.
///
/// `remotes` holds the neutral [`RemoteDelegate`] port, not a wire type: the A2A
/// transport + poll loop live in the adapter that implements it (Phase 2), so host
/// state names no protocol.
#[derive(Default)]
pub(crate) struct Delegates {
    /// All delegate agent ids (advertised as the agent's `multiagent` roster).
    ids: HashSet<String>,
    /// The subset fulfilled by a remote peer (agent id → the neutral delegate port)
    /// instead of a local sub-run. Invariant: every key is also in `ids`.
    remotes: HashMap<String, Arc<dyn RemoteDelegate>>,
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

    /// Register a remote delegate: advertised in the roster AND routed to its neutral
    /// [`RemoteDelegate`] port. Maintains the `remotes ⊆ ids` invariant via both.
    pub(crate) fn add_remote(&mut self, agent_id: String, delegate: Arc<dyn RemoteDelegate>) {
        self.ids.insert(agent_id.clone());
        self.remotes.insert(agent_id, delegate);
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
    pub(crate) fn remote(&self, agent_id: &str) -> Option<&Arc<dyn RemoteDelegate>> {
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

    /// A clone of the remote-delegate map, for injection into the delegation resolver.
    pub(crate) fn remotes(&self) -> HashMap<String, Arc<dyn RemoteDelegate>> {
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
    /// Remote delegate ids → the neutral [`RemoteDelegate`] port.
    remotes: HashMap<String, Arc<dyn RemoteDelegate>>,
}

impl DelegationResolver {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        provider: LocalProvider,
        sandbox: Arc<LocalSandbox>,
        reuse_sandbox: bool,
        roster: HashSet<String>,
        remotes: HashMap<String, Arc<dyn RemoteDelegate>>,
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
        if let Some(remote) = self.remotes.get(&agent_id) {
            return remote
                .run(&agent_id, &input, request.cancellation.as_ref())
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
        let remote = self
            .remotes
            .get(agent_id)
            .ok_or_else(|| AgentError::new(format!("agent {agent_id:?} is not a remote agent")))?;
        remote.run(agent_id, input, cancellation).await
    }
}

impl SharedHost {
    /// Fetch a remote delegate's discovery card (outbound discovery) as neutral JSON.
    /// Fails if the agent is not a registered remote. The wire card shape lives in the
    /// adapter behind the [`RemoteDelegate`] port; the host only echoes the value.
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
