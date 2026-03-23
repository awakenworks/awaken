//! Minimal sequential agent loop driven by state machines.
//!
//! Run lifecycle: RunLifecycle (Running → StepCompleted → Done/Waiting)
//! Tool call lifecycle: ToolCallStates (New → Running → Succeeded/Failed/Suspended)

pub(crate) mod actions;
mod checkpoint;
mod inference;
mod orchestrator;
pub mod parallel_merge;
mod resume;
mod setup;
mod step;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use crate::phase::{ExecutionEnv, PhaseRuntime};
use crate::registry::AgentResolver;
use crate::state::MutationBatch;
use awaken_contract::StateError;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolResult;
use futures::channel::mpsc;

use crate::agent::state::{RunLifecycle, ToolCallStates};

// Re-export submodule items used by external callers
pub use resume::prepare_resume;

/// Plugin that registers the core state keys required by the loop runner.
///
/// Must be installed on the `StateStore` before running the loop.
pub struct LoopStatePlugin;

impl crate::plugins::Plugin for LoopStatePlugin {
    fn descriptor(&self) -> crate::plugins::PluginDescriptor {
        crate::plugins::PluginDescriptor {
            name: "__loop_state",
        }
    }

    fn register(
        &self,
        r: &mut crate::plugins::PluginRegistrar,
    ) -> Result<(), awaken_contract::StateError> {
        use crate::agent::state::{
            AccumulatedOverrides, AccumulatedToolExclusions, AccumulatedToolInclusions,
            ContextMessageStore, ContextThrottleState,
        };
        use crate::state::{KeyScope, StateKeyOptions};

        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions {
            scope: KeyScope::Thread,
            persistent: true,
            ..StateKeyOptions::default()
        })?;
        r.register_key::<ContextThrottleState>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedOverrides>(StateKeyOptions::default())?;
        r.register_key::<ContextMessageStore>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedToolExclusions>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedToolInclusions>(StateKeyOptions::default())?;
        Ok(())
    }
}

/// Errors from the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    #[error("storage failed: {0}")]
    StorageError(String),
    #[error("phase error: {0}")]
    PhaseError(#[from] awaken_contract::StateError),
    #[error("runtime error: {0}")]
    RuntimeError(#[from] crate::error::RuntimeError),
    #[error("invalid resume: {0}")]
    InvalidResume(String),
}

impl From<awaken_contract::contract::executor::InferenceExecutionError> for AgentLoopError {
    fn from(e: awaken_contract::contract::executor::InferenceExecutionError) -> Self {
        Self::InferenceFailed(e.to_string())
    }
}

impl From<crate::execution::executor::ToolExecutorError> for AgentLoopError {
    fn from(e: crate::execution::executor::ToolExecutorError) -> Self {
        Self::InferenceFailed(e.to_string())
    }
}

/// Result of running the agent loop.
#[derive(Debug)]
pub struct AgentRunResult {
    pub response: String,
    pub termination: awaken_contract::contract::lifecycle::TerminationReason,
    pub steps: usize,
}

// -- Shared helpers --

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn commit_update<S: crate::state::StateKey>(
    store: &crate::state::StateStore,
    update: S::Update,
) -> Result<(), awaken_contract::StateError> {
    let mut patch = MutationBatch::new();
    patch.update::<S>(update);
    store.commit(patch)?;
    Ok(())
}

fn tool_result_to_content(result: &ToolResult) -> String {
    match &result.message {
        Some(msg) => msg.clone(),
        None => serde_json::to_string(&result.data).unwrap_or_default(),
    }
}

/// All parameters for executing the agent loop.
pub struct AgentLoopParams<'a> {
    /// Resolves agent IDs to config + execution environment.
    pub resolver: &'a dyn AgentResolver,
    /// Initial agent to resolve at loop start.
    pub agent_id: &'a str,
    /// Phase runtime (state store + hook executor).
    pub runtime: &'a PhaseRuntime,
    /// Event sink for streaming events to the caller.
    pub sink: Arc<dyn EventSink>,
    /// Optional persistent storage for checkpointing.
    pub checkpoint_store: Option<&'a dyn ThreadRunStore>,
    /// Messages to seed the conversation (history + new user input).
    pub messages: Vec<Message>,
    /// Run identity (thread, run, agent IDs).
    pub run_identity: RunIdentity,
    /// Cooperative cancellation token.
    pub cancellation_token: Option<CancellationToken>,
    /// Live decision channel for suspended tool calls (batched by sender).
    pub decision_rx: Option<mpsc::UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    /// Inference parameter overrides for this run.
    pub overrides: Option<InferenceOverride>,
}

/// Build an execution environment for the agent loop.
///
/// Injects runtime-required default plugins and conditionally adds
/// context truncation when a policy is provided. All transforms and hooks
/// flow through the standard plugin registration mechanism.
///
/// Prefer `AgentRuntime::run()` for production use.
pub fn build_agent_env(
    plugins: &[Arc<dyn crate::plugins::Plugin>],
    agent: &crate::agent::config::AgentConfig,
) -> Result<ExecutionEnv, StateError> {
    let mut all_plugins =
        crate::registry::resolve::inject_default_plugins(plugins.to_vec(), agent.max_rounds);

    if let Some(ref policy) = agent.context_policy {
        all_plugins.push(Arc::new(crate::context::ContextTransformPlugin::new(
            policy.clone(),
        )));
    }

    ExecutionEnv::from_plugins(&all_plugins)
}

/// Execute the agent loop. Prefer `AgentRuntime::run()` for production use.
///
/// Handles both fresh runs and resumed runs (state-driven detection).
/// Supports dynamic agent handoff via `ActiveAgentIdKey` re-resolve at step boundaries.
/// Cooperative cancellation via `CancellationToken`.
pub async fn run_agent_loop(params: AgentLoopParams<'_>) -> Result<AgentRunResult, AgentLoopError> {
    orchestrator::run_agent_loop_impl(params).await
}
