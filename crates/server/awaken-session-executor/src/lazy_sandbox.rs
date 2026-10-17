//! First-Sandbox-tool materialization for Native Awaken Sessions.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use awaken_tool_contract::tool::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability,
};

pub(crate) struct DeferredSandboxExecutor {
    host: Weak<crate::SharedHost>,
    thread: String,
}

/// Bind the deferred Sandbox executor an on-tool-use Environment needs: the
/// one installation, for a co-located Session preparation and a Worker's
/// claimed resolve alike. An installed executor is kept.
pub(crate) fn bind_deferred_executor(host: &Arc<crate::SharedHost>, thread: &str) {
    let needed = host
        .installed_sessions
        .read(thread, |slot| {
            slot.run.deferred_executor.is_none()
                && slot
                    .inputs
                    .environment_projection
                    .as_ref()
                    .is_some_and(|env| {
                        env.provisioning
                            == awaken_session_configuration_contract::SandboxProvisioning::OnToolUse
                    })
        })
        .unwrap_or(false);
    if needed {
        let executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor> =
            Arc::new(DeferredSandboxExecutor::new(Arc::downgrade(host), thread));
        host.installed_sessions
            .update_run(thread, |run| run.deferred_executor = Some(executor));
    }
}

impl DeferredSandboxExecutor {
    pub(crate) fn new(host: Weak<crate::SharedHost>, thread: impl Into<String>) -> Self {
        Self {
            host,
            thread: thread.into(),
        }
    }

    fn host(&self) -> Result<Arc<crate::SharedHost>, ToolError> {
        self.host
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
        let host = self.host()?;
        if call.tool_id == "read" {
            let args = awaken_tool_contract::tool::parse_tool_args(call.arguments.clone())?;
            if host
                .installed_sessions
                .read(&self.thread, |slot| {
                    crate::skills::read_pinned_skill(slot, &args)
                })
                .flatten()
                .is_some()
            {
                return Ok(awaken_tool_contract::tool_batch::ToolExecutionDomain::worker());
            }
        }
        let environment = host
            .realization_executor
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))?;
        let (_, origin) = host
            .realization_executor
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
        self.host
            .upgrade()
            .and_then(|host| {
                host.installed_sessions
                    .realizations()
                    .read_realization(&self.thread, |r| r.environment().owner.resident())
                    .expect("session runtime slots mutex poisoned")
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
        let host = self.host()?;
        if call.tool_id == "read" {
            let args: awaken_ext_builtin_tools::ReadArgs =
                awaken_tool_contract::tool::parse_tool_args(call.arguments.clone())?;
            if let Some(result) = host
                .installed_sessions
                .read(&self.thread, |slot| {
                    crate::skills::read_pinned_skill(slot, &args)
                })
                .flatten()
            {
                return result.map(|content| ToolOutput::ok(&call.call_id, content));
            }
        }
        let environment = host
            .realization_executor
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?;
        environment.agent_tools().tool_executor().invoke(call).await
    }
}
