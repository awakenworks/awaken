//! TypedTool surface with schemars-derived Args — mirrors the canonical
//! pattern docs show in `how-to/add-a-tool` and `reference/tool-trait`.

use async_trait::async_trait;
use awaken::contract::tool::{ToolCallContext, ToolError, ToolOutput, ToolResult, TypedTool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
struct SumArgs {
    a: i64,
    b: i64,
}

struct SumTool;

#[async_trait]
impl TypedTool for SumTool {
    type Args = SumArgs;
    fn tool_id(&self) -> &str {
        "sum"
    }
    fn name(&self) -> &str {
        "sum"
    }
    fn description(&self) -> &str {
        "Return a + b."
    }

    async fn execute(
        &self,
        args: SumArgs,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("sum", json!({ "result": args.a + args.b })).into())
    }
}

fn main() {
    let _tool = SumTool;
}
