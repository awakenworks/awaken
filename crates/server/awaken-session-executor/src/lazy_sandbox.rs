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
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))?;
        let (_, origin) = host
            .realizer
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
                    .read_realization(&self.thread, |r| r.environment_owner.resident())
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
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?;
        environment.agent_tools().tool_executor().invoke(call).await
    }
}
