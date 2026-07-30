//! First-Sandbox-tool materialization for Native Awaken Sessions.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
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
            .ok_or_else(|| ToolError::Execution("Session host is unavailable".into()))
    }
}

#[async_trait]
impl ToolExecutor for DeferredSandboxExecutor {
    fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
        self.host
            .upgrade()
            .and_then(|host| {
                host.session_slots
                    .read(&self.thread, |slot| slot.environment.clone())
                    .flatten()
            })
            .map_or(ToolRecoveryCapability::NonRecoverable, |environment| {
                environment.tool_executor().recovery_capability(tool_id)
            })
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let host = self.host()?;
        let environment = host
            .ensure_session_environment_for_tool(&self.thread)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        environment.tool_executor().invoke(call).await
    }
}
