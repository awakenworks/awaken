//! Adapter-layer erasure for the neutral typed [`Tool`] contract.

use std::sync::Arc;

use crate::tool::{
    RawTool, Tool, ToolCall, ToolConcurrency, ToolError, ToolExecutionTarget, ToolOutput,
    ToolRecoveryCapability, parse_tool_args, render_tool_output,
};
use async_trait::async_trait;

/// The sole adapter from a typed [`Tool`] to the dynamic [`RawTool`] registry.
pub struct Erased<T> {
    tool: T,
    target: ToolExecutionTarget,
}

pub fn erase<T: Tool + 'static>(tool: T) -> Arc<dyn RawTool> {
    erase_for(tool, ToolExecutionTarget::Brain)
}

pub fn erase_for<T: Tool + 'static>(tool: T, target: ToolExecutionTarget) -> Arc<dyn RawTool> {
    Arc::new(Erased { tool, target })
}

#[async_trait]
impl<T: Tool> RawTool for Erased<T> {
    fn id(&self) -> &str {
        T::ID
    }

    fn execution_target(&self) -> ToolExecutionTarget {
        self.target
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.tool.recovery_capability()
    }

    fn concurrency(&self, arguments: &serde_json::Value) -> ToolConcurrency {
        parse_tool_args::<T::Args>(arguments.clone()).map_or_else(
            |_| ToolConcurrency::Serial,
            |args| self.tool.concurrency(&args),
        )
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse_tool_args::<T::Args>(call.arguments)?;
        let output = self.tool.call(args).await?;
        Ok(ToolOutput::ok(call.call_id, render_tool_output(&output)?))
    }
}
