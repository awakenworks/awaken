//! Prepared execution of one ordinary tool outside the model loop.
//!
//! The API is deliberately tool-agnostic. Product adapters such as a committed
//! background-task observer may reuse the Runtime's exact plugin resolution,
//! permission, placement, recovery, concurrency, State capability, and panic
//! isolation without teaching the kernel why the invocation is detached.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Store;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::permission::GateOutcome;
use awaken_runtime_contract::plugin::ResolvedExecutionEnv;
use awaken_runtime_contract::resolved::{ResolvedRun, ToolKind};
use awaken_runtime_contract::resolver::RunResolver;
use awaken_runtime_contract::tool::{ToolCall, ToolConcurrency, ToolOutput, ToolRecoveryPolicy};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};

use crate::Runtime;
use crate::engine::convert::executable_tool_descriptors;
use crate::engine::tool_execution::{
    ToolExecutionOrigin, execute_tool, gate_decision, spill_tool_output, tool_concurrency,
    tool_recovery_capability,
};

/// Trusted execution facts frozen before a detached effect starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToolExecution {
    pub recovery: ToolRecoveryPolicy,
    pub concurrency: ToolConcurrency,
}

#[derive(Debug, thiserror::Error)]
pub enum DetachedToolError {
    #[error("resolve detached tool environment: {0}")]
    Resolve(String),
    #[error("tool `{0}` is not an executable ordinary tool in this publication")]
    NotExecutable(String),
    #[error("tool `{tool_id}` is not pre-authorized for detached execution: {reason}")]
    NotAuthorized { tool_id: String, reason: String },
    #[error("execute detached tool: {0}")]
    Execution(String),
}

/// One immutable Runtime resolution reused by claim and invocation.
///
/// Construction requires an `Arc<Runtime>` so the prepared value can safely be
/// moved into a product-owned task without copying a parallel tool registry.
pub struct PreparedToolExecutor {
    runtime: Arc<Runtime>,
    resolved: ResolvedRun,
    env: ResolvedExecutionEnv,
    context: RuntimeRunContext,
}

impl PreparedToolExecutor {
    /// Resolve the same authored and Session plugins used by a foreground Run.
    pub fn new(
        runtime: Arc<Runtime>,
        snapshot: &ExecutableAgentSnapshot,
        context: RuntimeRunContext,
    ) -> Result<Self, DetachedToolError> {
        let resolved = runtime
            .resolve(snapshot)
            .map_err(|error| DetachedToolError::Resolve(error.to_string()))?;
        let env = runtime
            .resolve_plugin_env_with(&resolved.spec, &context.session_plugins)
            .map_err(|error| DetachedToolError::Resolve(error.to_string()))?;
        Ok(Self {
            runtime,
            resolved,
            env,
            context,
        })
    }

    /// Resolve policy and concurrency from the canonical descriptor/executor.
    pub fn execution(&self, call: &ToolCall) -> Result<ResolvedToolExecution, DetachedToolError> {
        let descriptors =
            executable_tool_descriptors(&self.resolved.spec, &self.env.dynamic_descriptors());
        let descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.id == call.tool_id)
            .filter(|descriptor| descriptor.kind == ToolKind::Regular)
            .ok_or_else(|| DetachedToolError::NotExecutable(call.tool_id.clone()))?;
        descriptor
            .recovery_policy
            .validate(tool_recovery_capability(
                self.runtime.as_ref(),
                &self.env,
                &self.context,
                &call.tool_id,
            ))
            .map_err(|error| DetachedToolError::Execution(error.to_string()))?;
        Ok(ResolvedToolExecution {
            recovery: descriptor.recovery_policy.clone(),
            concurrency: tool_concurrency(self.runtime.as_ref(), &self.env, &self.context, call),
        })
    }

    /// Invoke through the Runtime's one authorization and execution confluence.
    pub async fn invoke(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        operation_id: String,
        call: &ToolCall,
        state: &Store,
    ) -> Result<ToolOutput, DetachedToolError> {
        match gate_decision(self.runtime.as_ref(), call, &self.env, state, &self.context).await {
            GateOutcome::Allow => {}
            outcome => {
                return Err(DetachedToolError::NotAuthorized {
                    tool_id: call.tool_id.clone(),
                    reason: outcome.decision_label().to_string(),
                });
            }
        }
        let output = execute_tool(
            self.runtime.as_ref(),
            Some(&self.env),
            Some(&self.resolved),
            call,
            &self.context,
            ToolExecutionOrigin {
                run_id,
                thread_id,
                operation_id,
            },
            state,
        )
        .await
        .map_err(|error| DetachedToolError::Execution(error.to_string()))?;
        spill_tool_output(&self.context, run_id, output)
            .await
            .map_err(|error| DetachedToolError::Execution(error.to_string()))
    }
}
