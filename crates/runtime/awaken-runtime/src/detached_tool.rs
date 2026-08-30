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
use awaken_runtime_contract::tool::{
    ToolCall, ToolConcurrency, ToolOutput, ToolRecoveryPolicy, ToolTaskHandle, ToolTaskPoll,
    ToolTaskStart,
};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};

use crate::Runtime;
use crate::engine::convert::executable_tool_descriptors;
use crate::engine::tool_execution::{
    DetachedToolAction, ToolExecutionOrigin, ToolExecutorOutcome, execute_detached_tool_action,
    execute_tool, gate_decision, spill_tool_output, tool_concurrency, tool_recovery_capability,
};

/// Trusted execution facts frozen before a detached effect starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToolExecution {
    pub kind: ToolKind,
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
            .filter(|descriptor| {
                matches!(descriptor.kind, ToolKind::Regular | ToolKind::DetachedOnly)
            })
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
            kind: descriptor.kind,
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

    /// Start the canonical ordinary tool through its negotiated durable-task
    /// port. Authorization is evaluated exactly once here; subsequent poll and
    /// cancellation calls continue the already-authorized request coordinates.
    pub async fn start_task(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        operation_id: String,
        call: &ToolCall,
        state: &Store,
    ) -> Result<ToolTaskStart, DetachedToolError> {
        match gate_decision(self.runtime.as_ref(), call, &self.env, state, &self.context).await {
            GateOutcome::Allow => {}
            outcome => {
                return Err(DetachedToolError::NotAuthorized {
                    tool_id: call.tool_id.clone(),
                    reason: outcome.decision_label().to_string(),
                });
            }
        }
        let outcome = execute_detached_tool_action(
            self.runtime.as_ref(),
            &self.env,
            &self.resolved,
            call,
            &self.context,
            ToolExecutionOrigin {
                run_id,
                thread_id,
                operation_id,
            },
            state,
            DetachedToolAction::Start,
        )
        .await
        .map_err(|error| DetachedToolError::Execution(error.to_string()))?
        .map_err(|error| DetachedToolError::Execution(error.to_string()))?;
        match outcome {
            ToolExecutorOutcome::Started(ToolTaskStart::Completed(output)) => {
                spill_tool_output(&self.context, run_id, output)
                    .await
                    .map(ToolTaskStart::Completed)
                    .map_err(|error| DetachedToolError::Execution(error.to_string()))
            }
            ToolExecutorOutcome::Started(started) => Ok(started),
            _ => Err(DetachedToolError::Execution(
                "detached start returned an invalid executor outcome".into(),
            )),
        }
    }

    /// Poll one durable request using the exact resolved adapter that created
    /// its opaque handle. This is continuation, not a second permission event.
    pub async fn poll_task(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        operation_id: String,
        call: &ToolCall,
        task: &ToolTaskHandle,
        state: &Store,
    ) -> Result<ToolTaskPoll, DetachedToolError> {
        self.continue_task(
            run_id,
            thread_id,
            operation_id,
            call,
            task,
            state,
            DetachedToolAction::Poll(task),
        )
        .await
    }

    /// Cancel one durable request. A successful return reports the remote
    /// protocol's observed state; it does not invent local terminal truth.
    pub async fn cancel_task(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        operation_id: String,
        call: &ToolCall,
        task: &ToolTaskHandle,
        state: &Store,
    ) -> Result<ToolTaskPoll, DetachedToolError> {
        self.continue_task(
            run_id,
            thread_id,
            operation_id,
            call,
            task,
            state,
            DetachedToolAction::Cancel(task),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn continue_task(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        operation_id: String,
        call: &ToolCall,
        _task: &ToolTaskHandle,
        state: &Store,
        action: DetachedToolAction<'_>,
    ) -> Result<ToolTaskPoll, DetachedToolError> {
        let outcome = execute_detached_tool_action(
            self.runtime.as_ref(),
            &self.env,
            &self.resolved,
            call,
            &self.context,
            ToolExecutionOrigin {
                run_id,
                thread_id,
                operation_id,
            },
            state,
            action,
        )
        .await
        .map_err(|error| DetachedToolError::Execution(error.to_string()))?
        .map_err(|error| DetachedToolError::Execution(error.to_string()))?;
        match outcome {
            ToolExecutorOutcome::Polled(ToolTaskPoll::Completed(output)) => {
                spill_tool_output(&self.context, run_id, output)
                    .await
                    .map(ToolTaskPoll::Completed)
                    .map_err(|error| DetachedToolError::Execution(error.to_string()))
            }
            ToolExecutorOutcome::Polled(polled) => Ok(polled),
            _ => Err(DetachedToolError::Execution(
                "detached continuation returned an invalid executor outcome".into(),
            )),
        }
    }
}
