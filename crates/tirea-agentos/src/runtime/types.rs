use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use genai::Client;

use crate::composition::{
    AgentRegistry, AgentToolsConfig, BehaviorRegistry, ModelRegistry, ProviderRegistry,
    RegistrySet, StopPolicyRegistry, SystemWiring, ToolRegistry,
};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::RunIdentity;
use crate::contracts::storage::{ThreadStore, VersionPrecondition};
use crate::contracts::{AgentEvent, RunContext, ToolCallDecision};
#[cfg(feature = "skills")]
use crate::extensions::skills::SkillRegistry;
use crate::loop_runtime::loop_runner::{
    Agent, RunCancellationToken, StateCommitError, StateCommitter,
};

use super::background_tasks::{BackgroundTaskManager, TaskStore};
use super::thread_run;

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
    pub(crate) agent: Arc<dyn Agent>,
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
    pub(crate) run_ctx: RunContext,
    pub(crate) cancellation_token: Option<RunCancellationToken>,
    pub(crate) state_committer: Option<Arc<dyn StateCommitter>>,
    pub(crate) decision_tx: tokio::sync::mpsc::UnboundedSender<ToolCallDecision>,
    pub(crate) decision_rx: tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>,
}

impl PreparedRun {
    /// Resolved thread ID (may have been auto-generated).
    pub fn thread_id(&self) -> &str {
        self.run_ctx.thread_id()
    }

    /// Resolved run ID (may have been auto-generated).
    pub fn run_id(&self) -> &str {
        self.run_ctx
            .run_identity()
            .run_id_opt()
            .expect("prepared runs always carry a run id")
    }

    /// Strongly typed identity for the prepared run.
    pub fn run_identity(&self) -> &RunIdentity {
        self.run_ctx.run_identity()
    }

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
    pub(crate) background_task_manager: Arc<BackgroundTaskManager>,
    pub(crate) active_runs: Arc<thread_run::ActiveThreadRunRegistry>,
    pub(crate) agent_tools: AgentToolsConfig,
    pub(crate) agent_state_store: Option<Arc<dyn ThreadStore>>,
}

pub(crate) struct RuntimeServices {
    pub default_client: Client,
    pub system_wirings: Vec<Arc<dyn SystemWiring>>,
    pub agent_tools: AgentToolsConfig,
    pub agent_state_store: Option<Arc<dyn ThreadStore>>,
}

impl AgentOs {
    pub(crate) fn from_registry_set(registries: RegistrySet, services: RuntimeServices) -> Self {
        let background_task_manager =
            if let Some(agent_state_store) = services.agent_state_store.clone() {
                let task_store = Arc::new(TaskStore::new(agent_state_store));
                Arc::new(BackgroundTaskManager::with_task_store(Some(task_store)))
            } else {
                Arc::new(BackgroundTaskManager::new())
            };
        Self {
            default_client: services.default_client,
            agents: registries.agents,
            base_tools: registries.tools,
            behaviors: registries.behaviors,
            providers: registries.providers,
            models: registries.models,
            stop_policies: registries.stop_policies,
            #[cfg(feature = "skills")]
            skills_registry: registries.skills,
            system_wirings: services.system_wirings,
            background_task_manager,
            active_runs: Arc::new(thread_run::ActiveThreadRunRegistry::default()),
            agent_tools: services.agent_tools,
            agent_state_store: services.agent_state_store,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentStateStoreStateCommitter {
    agent_state_store: Arc<dyn ThreadStore>,
    persist_run_mapping: bool,
}

impl AgentStateStoreStateCommitter {
    pub(crate) fn new(agent_state_store: Arc<dyn ThreadStore>, persist_run_mapping: bool) -> Self {
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
