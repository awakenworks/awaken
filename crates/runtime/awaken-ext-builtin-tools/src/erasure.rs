//! Adapter-layer erasure for the neutral typed [`Tool`] contract.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolCall, ToolConcurrency, ToolError, ToolExecutionTarget, ToolOutput,
    ToolRecoveryCapability, parse_tool_args, render_tool_output,
};

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

#[cfg(test)]
mod tests {
    use awaken_runtime_contract::tool::{ToolResource, ToolResourceAccess};
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize, JsonSchema)]
    struct WriteArgs {
        path: String,
    }

    struct TypedWrite;

    #[async_trait]
    impl Tool for TypedWrite {
        type Args = WriteArgs;
        type Output = String;

        const ID: &'static str = "typed_write";
        const DESCRIPTION: &'static str = "write one path";

        fn concurrency(&self, args: &Self::Args) -> ToolConcurrency {
            ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
                "workspace-path",
                &args.path,
            ))])
        }

        async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
            Ok(args.path)
        }
    }

    #[test]
    fn typed_concurrency_classifier_uses_parsed_arguments_and_fails_closed() {
        // Boundary rule: valid wire JSON is classified from `WriteArgs`, giving
        // authors field access and compiler checks; malformed JSON never reaches
        // the classifier and remains Serial.
        let tool = erase(TypedWrite);
        assert_eq!(
            tool.concurrency(&serde_json::json!({"path": "src/lib.rs"})),
            ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
                "workspace-path",
                "src/lib.rs"
            ))])
        );
        assert_eq!(
            tool.concurrency(&serde_json::json!({"path": 42})),
            ToolConcurrency::Serial
        );
    }
}
