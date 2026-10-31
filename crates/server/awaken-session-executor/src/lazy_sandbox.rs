//! First-Sandbox-tool materialization for Native Awaken Sessions.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use awaken_tool_contract::tool::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability,
};

/// Held in the Session's installed Session, so it reaches its one owner, the
/// realization executor, weakly: the installed Session must not keep the
/// executor that owns the installed-Session registry alive. It reads its own
/// Session through that executor's view of the registry; it holds no second
/// handle to the registry itself.
pub(crate) struct DeferredSandboxExecutor {
    realization_executor: Weak<crate::realization_executor::RealizationExecutor>,
    thread: String,
}

/// Bind the deferred Sandbox executor an on-tool-use Environment needs: the
/// one installation, for a co-located Session preparation and a Worker's
/// claimed resolve alike. An installed executor is kept.
pub(crate) fn bind_deferred_executor(
    host: &crate::RunAssembly,
    thread: &str,
) -> Result<(), crate::realization_error::RealizationLock> {
    let needed = host
        .installed_sessions
        .read(thread, |installed| {
            installed.run.deferred_executor.is_none()
                && installed
                    .inputs
                    .environment_projection
                    .as_ref()
                    .is_some_and(|env| {
                        env.provisioning
                            == awaken_session_configuration_contract::SandboxProvisioning::OnToolUse
                    })
        })?
        .unwrap_or(false);
    if needed {
        let executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor> = Arc::new(
            DeferredSandboxExecutor::new(&host.realization_executor, thread),
        );
        host.installed_sessions
            .modify_run(thread, |run| run.deferred_executor = Some(executor))?;
    }
    Ok(())
}

impl DeferredSandboxExecutor {
    /// Borrow the owner; the executor decides the edge is weak.
    pub(crate) fn new(
        realization_executor: &Arc<crate::realization_executor::RealizationExecutor>,
        thread: impl Into<String>,
    ) -> Self {
        Self {
            realization_executor: Arc::downgrade(realization_executor),
            thread: thread.into(),
        }
    }

    /// Serve a pinned Skill `read` from this Session's own projection, read
    /// through the owner's view of the installed Sessions. `None` when the
    /// Session is absent or the path is not a pinned Skill file; a released
    /// owner or a poisoned registry refuses.
    fn read_pinned_skill(
        &self,
        args: &awaken_ext_builtin_tools::ReadArgs,
    ) -> Result<Option<Result<String, ToolError>>, ToolError> {
        Ok(self
            .realization_executor()?
            .sessions()
            .modify_session(&self.thread, |session| {
                crate::skills::read_pinned_skill(session, args)
            })
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?
            .flatten())
    }

    fn realization_executor(
        &self,
    ) -> Result<Arc<crate::realization_executor::RealizationExecutor>, ToolError> {
        self.realization_executor
            .upgrade()
            .ok_or_else(|| ToolError::DefinitiveFailure("Session host is unavailable".into()))
    }
}

#[async_trait]
impl ToolExecutor for DeferredSandboxExecutor {
    async fn execution_domain(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, ToolError> {
        if call.tool_id == "read" {
            let args = awaken_tool_contract::tool::parse_tool_args(call.arguments.clone())?;
            if self.read_pinned_skill(&args)?.is_some() {
                return Ok(awaken_tool_contract::tool_batch::ToolExecutionDomain::worker());
            }
        }
        let realization_executor = self.realization_executor()?;
        let environment = realization_executor
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))?;
        let (_, origin) = realization_executor
            .environments()
            .resident_environment_activity_identity(&self.thread, &environment)
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))?;
        environment
            .agent_tools()
            .tool_execution_domain()
            .await
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))?
            .bind_resource_origin(origin.as_ref())
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))
    }
    fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
        // An absent or poisoned registry reports no resident Environment, so
        // the call is classified non-recoverable instead of panicking.
        self.realization_executor
            .upgrade()
            .and_then(|realization| {
                realization
                    .sessions()
                    .read_realization(&self.thread, |r| r.environment().owner.resident())
                    .ok()
                    .flatten()
                    .flatten()
            })
            .map_or(ToolRecoveryCapability::NonRecoverable, |environment| {
                environment
                    .agent_tools()
                    .tool_executor()
                    .recovery_capability(tool_id)
            })
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if call.tool_id == "read" {
            let args: awaken_ext_builtin_tools::ReadArgs =
                awaken_tool_contract::tool::parse_tool_args(call.arguments.clone())?;
            if let Some(result) = self.read_pinned_skill(&args)? {
                return result.map(|content| ToolOutput::ok(&call.call_id, content));
            }
        }
        let environment = self
            .realization_executor()?
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?;
        environment.agent_tools().tool_executor().invoke(call).await
    }
}

#[cfg(test)]
mod tests;
