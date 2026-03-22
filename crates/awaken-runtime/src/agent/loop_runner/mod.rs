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
pub(crate) mod truncation_recovery;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use crate::runtime::{AgentResolver, CancellationToken, ExecutionEnv, PhaseRuntime};
use crate::state::MutationBatch;
use awaken_contract::StateError;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::ToolResult;

use super::state::{RunLifecycle, ToolCallStates};

// Re-export submodule items used by external callers
pub use orchestrator::run_agent_loop_controlled;
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
        use super::state::{
            AccumulatedOverrides, AccumulatedToolExclusions, AccumulatedToolInclusions,
            ContextMessageStore, ContextThrottleState,
        };
        use crate::state::StateKeyOptions;

        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions::default())?;
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

/// Build an execution environment for the agent loop.
///
/// Adds internal plugins (stop conditions, default permission) and registers
/// built-in request transforms (context truncation when a policy is provided).
/// Build an execution environment. Prefer `AgentRuntime::run()` for production use.
pub fn build_agent_env(
    plugins: &[Arc<dyn crate::plugins::Plugin>],
    agent: &super::config::AgentConfig,
) -> Result<ExecutionEnv, StateError> {
    use super::compaction::ContextTransform;
    use super::stop_conditions::MaxRoundsPlugin;
    use super::tool_permission::AllowAllToolsPlugin;

    let mut all_plugins: Vec<Arc<dyn crate::plugins::Plugin>> = plugins.to_vec();
    all_plugins.push(Arc::new(MaxRoundsPlugin::new(agent.max_rounds)));
    all_plugins.push(Arc::new(AllowAllToolsPlugin));
    all_plugins.push(Arc::new(actions::LoopActionHandlersPlugin));

    let mut env = ExecutionEnv::from_plugins(&all_plugins)?;

    // Register built-in context truncation transform when policy is set
    if let Some(ref policy) = agent.context_policy {
        env.request_transforms
            .push(Arc::new(ContextTransform::new(policy.clone())));
    }

    Ok(env)
}

/// Agent loop implementation. Prefer `AgentRuntime::run()` for production use.
///
/// Handles both fresh runs and resumed runs (state-driven detection).
/// Supports dynamic agent handoff via `ActiveAgentIdKey` re-resolve at step boundaries.
/// Cooperative cancellation via `CancellationToken`.
pub async fn run_agent_loop(
    resolver: &dyn AgentResolver,
    initial_agent_id: &str,
    runtime: &PhaseRuntime,
    sink: &dyn EventSink,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
    cancellation_token: Option<CancellationToken>,
) -> Result<AgentRunResult, AgentLoopError> {
    run_agent_loop_controlled(
        resolver,
        initial_agent_id,
        runtime,
        sink,
        checkpoint_store,
        initial_messages,
        run_identity,
        cancellation_token,
        None,
        None,
    )
    .await
}
