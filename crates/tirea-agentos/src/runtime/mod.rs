use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(feature = "skills")]
use std::time::Duration;

use futures::Stream;
use genai::Client;

use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;
use crate::contracts::storage::{ThreadHead, ThreadStore, ThreadStoreError, VersionPrecondition};
use crate::contracts::thread::CheckpointReason;
use crate::contracts::thread::Message;
use crate::contracts::thread::Thread;
use crate::contracts::RunContext;
use crate::contracts::{AgentEvent, RunRequest, ToolCallDecision};
#[cfg(feature = "skills")]
use crate::extensions::skills::{
    CompositeSkillRegistry, InMemorySkillRegistry, Skill, SkillError, SkillRegistry,
    SkillRegistryError, SkillRegistryManagerError,
};
use crate::loop_runtime::loop_runner::{
    Agent, AgentLoopError, BaseAgent, RunCancellationToken, StateCommitError, StateCommitter,
};

#[path = "../composition/agent_definition.rs"]
mod agent_definition;
pub(crate) mod agent_tools;
#[path = "../composition/builder.rs"]
mod builder;
mod composite_behavior;
mod context_window_plugin;
mod policy;
mod run;
mod stop_policy_plugin;
#[path = "../composition/registry_set.rs"]
mod registry_set;
#[path = "../composition/system_wiring.rs"]
pub(crate) mod system_wiring;
mod thread_run;
#[path = "../composition/wiring.rs"]
mod wiring;

#[cfg(test)]
mod tests;

pub use agent_definition::{AgentDefinition, ToolExecutionMode};
use agent_tools::{
    AgentOutputTool, AgentRecoveryPlugin, AgentRunTool, AgentStopTool, AgentToolsPlugin,
    SubAgentHandleTable,
};
pub use composite_behavior::compose_behaviors;
pub use registry_set::{
    AgentRegistry, AgentRegistryError, BehaviorRegistry, BehaviorRegistryError, BundleComposeError,
    BundleComposer, BundleRegistryAccumulator, BundleRegistryKind, CompositeAgentRegistry,
    CompositeBehaviorRegistry, CompositeModelRegistry, CompositeProviderRegistry,
    CompositeStopPolicyRegistry, CompositeToolRegistry, InMemoryAgentRegistry,
    InMemoryBehaviorRegistry, InMemoryModelRegistry, InMemoryProviderRegistry,
    InMemoryStopPolicyRegistry, InMemoryToolRegistry, ModelDefinition, ModelRegistry,
    ModelRegistryError, ProviderRegistry, ProviderRegistryError, RegistryBundle, RegistrySet,
    StopPolicyRegistry, StopPolicyRegistryError, ToolBehaviorBundle, ToolRegistry,
    ToolRegistryError,
};
pub use context_window_plugin::{ContextWindowPlugin, CONTEXT_WINDOW_PLUGIN_ID};
pub use stop_policy_plugin::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopConditionSpec, StopOnTool,
    StopPolicy, StopPolicyInput, StopPolicyPlugin, StopPolicyStats, Timeout, TokenBudget,
};
pub use thread_run::ForwardedDecision;

pub use system_wiring::{SystemWiring, WiringContext};

pub use crate::loop_runtime::loop_runner::{tool_map, tool_map_from_arc, ResolvedRun};

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

/// Configuration for the skills subsystem.
///
/// - `enabled: false` → no skills tools or plugins are registered.
/// - `enabled: true, advertise_catalog: true` → discovery plugin injects skills catalog
///   before inference so the model knows which skills are available.
/// - `enabled: true, advertise_catalog: false` → tools are registered but the catalog
///   is not injected (the model must be told about skills through other means).
#[cfg(feature = "skills")]
#[derive(Debug, Clone)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub advertise_catalog: bool,
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

#[cfg(feature = "skills")]
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            // Skills are opt-in. If enabled, the caller must provide skills.
            enabled: false,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentToolsConfig {
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

impl Default for AgentToolsConfig {
    fn default() -> Self {
        Self {
            discovery_max_entries: 64,
            discovery_max_chars: 16 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsWiringError {
    #[error("reserved behavior id cannot be used: {0}")]
    ReservedBehaviorId(String),

    #[error("behavior not found: {0}")]
    BehaviorNotFound(String),

    #[error("stop condition not found: {0}")]
    StopConditionNotFound(String),

    #[error("behavior id already installed: {0}")]
    BehaviorAlreadyInstalled(String),

    #[cfg(feature = "skills")]
    #[error("skills tool id already registered: {0}")]
    SkillsToolIdConflict(String),

    #[cfg(feature = "skills")]
    #[error("skills behavior already installed: {0}")]
    SkillsBehaviorAlreadyInstalled(String),

    #[cfg(feature = "skills")]
    #[error("skills enabled but no skills configured")]
    SkillsNotConfigured,

    #[error("agent tool id already registered: {0}")]
    AgentToolIdConflict(String),

    #[error("agent tools behavior already installed: {0}")]
    AgentToolsBehaviorAlreadyInstalled(String),

    #[error("agent recovery behavior already installed: {0}")]
    AgentRecoveryBehaviorAlreadyInstalled(String),

    #[error("bundle '{bundle_id}' includes unsupported contribution in wiring: {kind}")]
    BundleUnsupportedContribution { bundle_id: String, kind: String },

    #[error("bundle '{bundle_id}' tool id already registered: {id}")]
    BundleToolIdConflict { bundle_id: String, id: String },

    #[error("bundle '{bundle_id}' behavior id mismatch: key={key} behavior.id()={behavior_id}")]
    BundleBehaviorIdMismatch {
        bundle_id: String,
        key: String,
        behavior_id: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsBuildError {
    #[error(transparent)]
    Agents(#[from] AgentRegistryError),

    #[error(transparent)]
    Bundle(#[from] BundleComposeError),

    #[error(transparent)]
    Tools(#[from] ToolRegistryError),

    #[error(transparent)]
    Behaviors(#[from] BehaviorRegistryError),

    #[error(transparent)]
    Providers(#[from] ProviderRegistryError),

    #[error(transparent)]
    Models(#[from] ModelRegistryError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    Skills(#[from] SkillError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    SkillRegistry(#[from] SkillRegistryError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    SkillRegistryManager(#[from] SkillRegistryManagerError),

    #[error(transparent)]
    StopPolicies(#[from] StopPolicyRegistryError),

    #[error("agent {agent_id} references an empty behavior id")]
    AgentEmptyBehaviorRef { agent_id: String },

    #[error("agent {agent_id} references reserved behavior id: {behavior_id}")]
    AgentReservedBehaviorId {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} references unknown behavior id: {behavior_id}")]
    AgentBehaviorNotFound {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} has duplicate behavior reference: {behavior_id}")]
    AgentDuplicateBehaviorRef {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} references an empty stop condition id")]
    AgentEmptyStopConditionRef { agent_id: String },

    #[error("agent {agent_id} references unknown stop condition id: {stop_condition_id}")]
    AgentStopConditionNotFound {
        agent_id: String,
        stop_condition_id: String,
    },

    #[error("agent {agent_id} has duplicate stop condition reference: {stop_condition_id}")]
    AgentDuplicateStopConditionRef {
        agent_id: String,
        stop_condition_id: String,
    },

    #[error("models configured but no ProviderRegistry configured")]
    ProvidersNotConfigured,

    #[error("provider not found: {provider_id} (for model id: {model_id})")]
    ProviderNotFound {
        provider_id: String,
        model_id: String,
    },

    #[cfg(feature = "skills")]
    #[error("skills enabled but no skills configured")]
    SkillsNotConfigured,
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
    default_client: Client,
    agents: Arc<dyn AgentRegistry>,
    base_tools: Arc<dyn ToolRegistry>,
    behaviors: Arc<dyn BehaviorRegistry>,
    providers: Arc<dyn ProviderRegistry>,
    models: Arc<dyn ModelRegistry>,
    stop_policies: Arc<dyn StopPolicyRegistry>,
    #[cfg(feature = "skills")]
    skills_registry: Option<Arc<dyn SkillRegistry>>,
    system_wirings: Vec<Arc<dyn SystemWiring>>,
    sub_agent_handles: Arc<SubAgentHandleTable>,
    active_runs: Arc<thread_run::ActiveThreadRunRegistry>,
    agent_tools: AgentToolsConfig,
    agent_state_store: Option<Arc<dyn ThreadStore>>,
}

pub struct AgentOsBuilder {
    client: Option<Client>,
    bundles: Vec<Arc<dyn RegistryBundle>>,
    agents: HashMap<String, AgentDefinition>,
    agent_registries: Vec<Arc<dyn AgentRegistry>>,
    base_tools: HashMap<String, Arc<dyn Tool>>,
    base_tool_registries: Vec<Arc<dyn ToolRegistry>>,
    behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
    behavior_registries: Vec<Arc<dyn BehaviorRegistry>>,
    stop_policies: HashMap<String, Arc<dyn StopPolicy>>,
    stop_policy_registries: Vec<Arc<dyn StopPolicyRegistry>>,
    providers: HashMap<String, Client>,
    provider_registries: Vec<Arc<dyn ProviderRegistry>>,
    models: HashMap<String, ModelDefinition>,
    model_registries: Vec<Arc<dyn ModelRegistry>>,
    #[cfg(feature = "skills")]
    skills: Vec<Arc<dyn Skill>>,
    #[cfg(feature = "skills")]
    skill_registries: Vec<Arc<dyn SkillRegistry>>,
    #[cfg(feature = "skills")]
    skills_refresh_interval: Option<Duration>,
    #[cfg(feature = "skills")]
    skills_config: SkillsConfig,
    system_wirings: Vec<Arc<dyn SystemWiring>>,
    agent_tools: AgentToolsConfig,
    agent_state_store: Option<Arc<dyn ThreadStore>>,
}
