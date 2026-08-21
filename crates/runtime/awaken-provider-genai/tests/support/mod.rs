use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{Tool, ToolError};

/// One small typed fixture used by every live compatibility surface. Keeping it
/// here proves the Rust authoring path itself: the tests never maintain a JSON
/// Schema beside the executable argument type.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityArgs {
    /// Marker naming the deterministic fixture requested by the test.
    pub marker: String,
}

pub struct CompatibilityTool;

#[async_trait]
impl Tool for CompatibilityTool {
    type Args = CompatibilityArgs;
    type Output = String;
    const ID: &'static str = "read_compatibility_fixture";
    const DESCRIPTION: &'static str = "Read the fixed compatibility marker requested by the user.";

    async fn call(&self, args: CompatibilityArgs) -> Result<String, ToolError> {
        Ok(args.marker)
    }
}

pub fn compatibility_tool() -> ToolDescriptor {
    ToolDescriptor::for_tool::<CompatibilityTool>("live-fixture-v1")
}
