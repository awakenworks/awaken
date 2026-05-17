//! Smallest possible `Tool` impl — verifies the trait surface docs cite
//! in `how-to/add-a-tool` and `reference/tool-trait` still compiles.

use async_trait::async_trait;
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use serde_json::{Value, json};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "echo".into(),
            name: "echo".into(),
            description: "Echo the input message back.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"],
            }),
            category: None,
            metadata: Default::default(),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let msg = args["message"].as_str().unwrap_or_default().to_string();
        Ok(ToolResult::success("echo", json!({ "echo": msg })).into())
    }
}

fn main() {
    // Compile-only smoke. Constructing the tool exercises the trait.
    let _tool: Box<dyn Tool> = Box::new(EchoTool);
}
