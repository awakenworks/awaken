//! Description-override layer for the tool registry.
//!
//! Wraps an existing `Arc<dyn ToolRegistry>` and rewrites each looked-up
//! tool's `descriptor().description` to the operator-supplied override.
//! The underlying `Tool::execute` is forwarded unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::registry::ToolRegistry;
use awaken_server_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput,
};
use serde_json::Value;

pub(crate) struct DescriptionOverrideTool {
    inner: Arc<dyn Tool>,
    description: String,
}

impl DescriptionOverrideTool {
    pub fn new(inner: Arc<dyn Tool>, description: String) -> Self {
        Self { inner, description }
    }
}

#[async_trait]
impl Tool for DescriptionOverrideTool {
    fn descriptor(&self) -> ToolDescriptor {
        let mut d = self.inner.descriptor();
        d.description = self.description.clone();
        d
    }
    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        self.inner.validate_args(args)
    }
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.inner.execute(args, ctx).await
    }
}

pub(crate) struct DescriptionOverrideRegistry {
    base: Arc<dyn ToolRegistry>,
    /// tool_id -> override description
    overrides: HashMap<String, String>,
}

impl DescriptionOverrideRegistry {
    pub fn new(base: Arc<dyn ToolRegistry>, overrides: HashMap<String, String>) -> Self {
        Self { base, overrides }
    }
}

impl ToolRegistry for DescriptionOverrideRegistry {
    fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>> {
        let inner = self.base.get_tool(id)?;
        match self.overrides.get(id) {
            Some(desc) => Some(Arc::new(DescriptionOverrideTool::new(inner, desc.clone()))),
            None => Some(inner),
        }
    }
    fn tool_ids(&self) -> Vec<String> {
        self.base.tool_ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::tool::{ToolDescriptor, ToolResult};
    use serde_json::json;

    struct StaticTool;

    #[async_trait]
    impl Tool for StaticTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "Echo", "stock description")
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("echo", json!({})).into())
        }
    }

    /// Lightweight in-test ToolRegistry over a single tool.
    struct OneToolRegistry {
        id: String,
        tool: Arc<dyn Tool>,
    }

    impl ToolRegistry for OneToolRegistry {
        fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>> {
            if id == self.id {
                Some(Arc::clone(&self.tool))
            } else {
                None
            }
        }
        fn tool_ids(&self) -> Vec<String> {
            vec![self.id.clone()]
        }
    }

    fn base_registry() -> Arc<dyn ToolRegistry> {
        Arc::new(OneToolRegistry {
            id: "echo".into(),
            tool: Arc::new(StaticTool),
        })
    }

    #[test]
    fn override_replaces_description_for_matching_id() {
        let mut overrides = HashMap::new();
        overrides.insert("echo".into(), "patched".into());
        let reg = DescriptionOverrideRegistry::new(base_registry(), overrides);
        let tool = reg.get_tool("echo").unwrap();
        assert_eq!(tool.descriptor().description, "patched");
        assert_eq!(tool.descriptor().id, "echo");
    }

    #[test]
    fn passes_through_when_no_override_for_id() {
        let reg = DescriptionOverrideRegistry::new(base_registry(), HashMap::new());
        let tool = reg.get_tool("echo").unwrap();
        assert_eq!(tool.descriptor().description, "stock description");
    }

    #[test]
    fn tool_ids_passes_through_to_base() {
        let reg = DescriptionOverrideRegistry::new(base_registry(), HashMap::new());
        assert_eq!(reg.tool_ids(), vec!["echo".to_string()]);
    }

    #[test]
    fn unknown_id_returns_none() {
        let reg = DescriptionOverrideRegistry::new(base_registry(), HashMap::new());
        assert!(reg.get_tool("nope").is_none());
    }
}
