use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use genai::Client;

use crate::composition::{
    AgentDefinition, AgentOsWiringError, AgentRegistry, AgentToolsConfig, BehaviorRegistry,
    ModelRegistry, ProviderRegistry, StopPolicyRegistry, SystemWiring, ToolRegistry,
};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::storage::{ThreadHead, ThreadStore, ThreadStoreError, VersionPrecondition};
use crate::contracts::thread::CheckpointReason;
use crate::contracts::thread::Message;
use crate::contracts::thread::Thread;
use crate::contracts::RunContext;
use crate::contracts::{AgentEvent, RunRequest, ToolCallDecision};
#[cfg(feature = "skills")]
use crate::extensions::skills::SkillRegistry;
use crate::loop_runtime::loop_runner::{
    Agent, AgentLoopError, RunCancellationToken, StateCommitError, StateCommitter,
};

pub(crate) mod agent_tools;
mod composite_behavior;
mod context_window_plugin;
mod policy;
pub(crate) mod resolve;
mod run;
mod stop_policy_plugin;
pub(crate) mod thread_run;

#[cfg(test)]
mod tests;

use agent_tools::SubAgentHandleTable;
pub use composite_behavior::compose_behaviors;
pub use context_window_plugin::{ContextWindowPlugin, CONTEXT_WINDOW_PLUGIN_ID};
pub use stop_policy_plugin::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopPolicy, StopPolicyInput,
    StopPolicyPlugin, StopPolicyStats, StopOnTool, Timeout, TokenBudget,
};
pub use thread_run::ForwardedDecision;

pub use crate::loop_runtime::loop_runner::ResolvedRun;

#[derive(Clone)]
struct AgentStateStoreStateCommitter {
    agent_state_store: Arc<dyn ThreadStore>,
    persist_run_mapping: bool,
}

impl AgentStateStoreStateCommitter {
    fn new(agent_state_store: Arc<dyn ThreadStore>, persist_run_mapping: bool) -> Self {
        Self {
            agent_state_store,
            persist_run_mapping,
        }
    }
}

#[async_trait::async_trait]
impl StateCommitter for AgentStateStoreStateCommitter {
    async fn commit(
        &self,
        thread_id: &str,
        mut changeset: crate::contracts::ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<u64, StateCommitError> {
        if !self.persist_run_mapping {
            changeset.run_meta = None;
        }
        self.agent_state_store
            .append(thread_id, &changeset, precondition)
            .await
            .map(|committed| committed.version)
            .map_err(|e| StateCommitError::new(format!("checkpoint append failed: {e}")))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsResolveError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("provider not found: {provider_id} (for model id: {model_id})")]
    ProviderNotFound {
        provider_id: String,
        model_id: String,
    },

    #[error(transparent)]
    RunConfig(#[from] crate::contracts::RunConfigError),

    #[error(transparent)]
    Wiring(#[from] AgentOsWiringError),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsRunError {
    #[error(transparent)]
    Resolve(#[from] AgentOsResolveError),

    #[error(transparent)]
    RunConfig(#[from] crate::contracts::RunConfigError),

    #[error(transparent)]
    Loop(#[from] AgentLoopError),

    #[error("agent state store error: {0}")]
    ThreadStore(#[from] ThreadStoreError),

    #[error("agent state store not configured")]
    AgentStateStoreNotConfigured,
}

/// Result of [`AgentOs::run_stream`]: an event stream plus metadata.
///
/// Checkpoint persistence is handled internally in stream order — callers only
/// consume the event stream and use the IDs for protocol encoding.
///
/// The final thread is **not** exposed here; storage is updated incrementally
/// via `ThreadChangeSet` appends.
pub struct RunStream {
    /// Resolved thread ID (may have been auto-generated).
    pub thread_id: String,
    /// Resolved run ID (may have been auto-generated).
    pub run_id: String,
    /// Sender for runtime interaction decisions (approve/deny payloads).
    ///
    /// The receiver is owned by the running loop. Sending a decision while the
    /// run is active allows mid-run resolution of suspended tool calls.
    pub decision_tx: tokio::sync::mpsc::UnboundedSender<ToolCallDecision>,
    /// The agent event stream.
    pub events: Pin<Box<dyn Stream<Item = AgentEvent> + Send>>,
}

impl RunStream {
    /// Submit one interaction decision to the active run.
    pub fn submit_decision(
        &self,
        decision: ToolCallDecision,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<ToolCallDecision>> {
        self.decision_tx.send(decision)
    }
}

/// Fully prepared run payload ready for execution.
///
/// This separates request preprocessing from stream execution so preprocessing
/// can be unit-tested deterministically.
pub struct PreparedRun {
    /// Resolved thread ID (may have been auto-generated).
    pub thread_id: String,
    /// Resolved run ID (may have been auto-generated).
    pub run_id: String,
    agent: Arc<dyn Agent>,
    tools: HashMap<String, Arc<dyn Tool>>,
    run_ctx: RunContext,
    execution_ctx: crate::loop_runtime::loop_runner::RunExecutionContext,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_tx: tokio::sync::mpsc::UnboundedSender<ToolCallDecision>,
    decision_rx: tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>,
}

impl PreparedRun {
    /// Attach a cooperative cancellation token for this prepared run.
    ///
    /// This keeps loop cancellation wiring outside protocol/UI layers:
    /// transport code can own token lifecycle and inject it before execution.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: RunCancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }
}

#[derive(Clone)]
pub struct AgentOs {
    pub(crate) default_client: Client,
    pub(crate) agents: Arc<dyn AgentRegistry>,
    pub(crate) base_tools: Arc<dyn ToolRegistry>,
    pub(crate) behaviors: Arc<dyn BehaviorRegistry>,
    pub(crate) providers: Arc<dyn ProviderRegistry>,
    pub(crate) models: Arc<dyn ModelRegistry>,
    pub(crate) stop_policies: Arc<dyn StopPolicyRegistry>,
    #[cfg(feature = "skills")]
    pub(crate) skills_registry: Option<Arc<dyn SkillRegistry>>,
    pub(crate) system_wirings: Vec<Arc<dyn SystemWiring>>,
    pub(crate) sub_agent_handles: Arc<SubAgentHandleTable>,
    pub(crate) active_runs: Arc<thread_run::ActiveThreadRunRegistry>,
    pub(crate) agent_tools: AgentToolsConfig,
    pub(crate) agent_state_store: Option<Arc<dyn ThreadStore>>,
}
