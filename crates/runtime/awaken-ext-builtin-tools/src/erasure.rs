//! Adapter-layer erasure for the neutral typed [`Tool`] contract.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolCall, ToolError, ToolOutput, ToolRecoveryCapability, parse_tool_args,
    render_tool_output,
};

/// The sole adapter from a typed [`Tool`] to the dynamic [`RawTool`] registry.
pub struct Erased<T>(pub T);

pub fn erase<T: Tool + 'static>(tool: T) -> Arc<dyn RawTool> {
    Arc::new(Erased(tool))
}

#[async_trait]
impl<T: Tool> RawTool for Erased<T> {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.0.recovery_capability()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse_tool_args::<T::Args>(call.arguments)?;
        let output = self.0.call(args).await?;
        Ok(ToolOutput::ok(call.call_id, render_tool_output(&output)?))
    }
}
