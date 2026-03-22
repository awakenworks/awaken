//! Tool behavior bundle for grouping tools and plugin references.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use super::tool::Tool;

/// Errors from bundle composition.
#[derive(Debug, Error)]
pub enum BundleComposeError {
    /// A bundle contributed a duplicate tool name.
    #[error("bundle '{bundle_id}' contributed duplicate tool: {tool_name}")]
    DuplicateTool {
        bundle_id: String,
        tool_name: String,
    },
    /// A bundle contributed an empty tool name.
    #[error("bundle '{bundle_id}' contributed empty tool name")]
    EmptyToolName { bundle_id: String },
}

/// Lightweight bundle carrying tools and plugin ID references.
///
/// Since the Plugin trait lives in awaken-runtime (not awaken-contract),
/// plugins are referenced by ID string rather than trait object.
#[derive(Clone, Default)]
pub struct ToolBehaviorBundle {
    id: String,
    tools: HashMap<String, Arc<dyn Tool>>,
    plugin_ids: Vec<String>,
}

impl ToolBehaviorBundle {
    /// Create a new bundle with the given identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// Add a tool to this bundle (keyed by descriptor ID).
    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        let id = tool.descriptor().id;
        self.tools.insert(id, tool);
        self
    }

    /// Add multiple tools to this bundle.
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        for tool in tools {
            let id = tool.descriptor().id;
            self.tools.insert(id, tool);
        }
        self
    }

    /// Add a plugin reference by ID.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_ids.push(plugin_id.into());
        self
    }

    /// Get the bundle identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the tool map.
    pub fn tools(&self) -> &HashMap<String, Arc<dyn Tool>> {
        &self.tools
    }

    /// Get the plugin ID references.
    pub fn plugin_ids(&self) -> &[String] {
        &self.plugin_ids
    }
}

impl std::fmt::Debug for ToolBehaviorBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolBehaviorBundle")
            .field("id", &self.id)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("plugin_ids", &self.plugin_ids)
            .finish()
    }
}

/// Merges multiple [`ToolBehaviorBundle`]s, deduplicating tools by name.
pub struct BundleComposer;

impl BundleComposer {
    /// Merge bundles into a single tool map, erroring on duplicates.
    pub fn merge(
        bundles: &[ToolBehaviorBundle],
    ) -> Result<HashMap<String, Arc<dyn Tool>>, BundleComposeError> {
        let mut merged: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        for bundle in bundles {
            for (name, tool) in &bundle.tools {
                if name.trim().is_empty() {
                    return Err(BundleComposeError::EmptyToolName {
                        bundle_id: bundle.id.clone(),
                    });
                }
                if merged.contains_key(name) {
                    return Err(BundleComposeError::DuplicateTool {
                        bundle_id: bundle.id.clone(),
                        tool_name: name.clone(),
                    });
                }
                merged.insert(name.clone(), Arc::clone(tool));
            }
        }

        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::tool::{ToolCallContext, ToolDescriptor, ToolError, ToolResult};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    struct MockTool {
        descriptor: ToolDescriptor,
    }

    impl MockTool {
        fn new(id: &str) -> Self {
            Self {
                descriptor: ToolDescriptor::new(id, id, format!("{id} tool")),
            }
        }
    }

    #[async_trait]
    impl Tool for MockTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(&self.descriptor.id, json!(null)))
        }
    }

    #[test]
    fn bundle_creation_and_accessors() {
        let bundle = ToolBehaviorBundle::new("test-bundle")
            .with_tool(Arc::new(MockTool::new("search")))
            .with_tool(Arc::new(MockTool::new("calc")))
            .with_plugin_id("my-plugin");

        assert_eq!(bundle.id(), "test-bundle");
        assert_eq!(bundle.tools().len(), 2);
        assert!(bundle.tools().contains_key("search"));
        assert!(bundle.tools().contains_key("calc"));
        assert_eq!(bundle.plugin_ids(), &["my-plugin"]);
    }

    #[test]
    fn bundle_with_tools_batch() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool::new("a")),
            Arc::new(MockTool::new("b")),
            Arc::new(MockTool::new("c")),
        ];
        let bundle = ToolBehaviorBundle::new("batch").with_tools(tools);
        assert_eq!(bundle.tools().len(), 3);
    }

    #[test]
    fn bundle_default_is_empty() {
        let bundle = ToolBehaviorBundle::default();
        assert_eq!(bundle.id(), "");
        assert!(bundle.tools().is_empty());
        assert!(bundle.plugin_ids().is_empty());
    }

    #[test]
    fn bundle_debug_format() {
        let bundle = ToolBehaviorBundle::new("dbg").with_tool(Arc::new(MockTool::new("t1")));
        let debug = format!("{bundle:?}");
        assert!(debug.contains("dbg"));
        assert!(debug.contains("t1"));
    }

    #[test]
    fn composer_merge_disjoint_bundles() {
        let b1 = ToolBehaviorBundle::new("b1").with_tool(Arc::new(MockTool::new("search")));
        let b2 = ToolBehaviorBundle::new("b2").with_tool(Arc::new(MockTool::new("calc")));

        let merged = BundleComposer::merge(&[b1, b2]).unwrap();
        assert_eq!(merged.len(), 2);
        assert!(merged.contains_key("search"));
        assert!(merged.contains_key("calc"));
    }

    #[test]
    fn composer_merge_duplicate_errors() {
        let b1 = ToolBehaviorBundle::new("b1").with_tool(Arc::new(MockTool::new("search")));
        let b2 = ToolBehaviorBundle::new("b2").with_tool(Arc::new(MockTool::new("search")));

        let result = BundleComposer::merge(&[b1, b2]);
        let err = result.err().expect("should be an error");
        assert!(err.to_string().contains("duplicate tool"));
        assert!(err.to_string().contains("search"));
    }

    #[test]
    fn composer_merge_empty_bundles() {
        let merged = BundleComposer::merge(&[]).unwrap();
        assert!(merged.is_empty());
    }

    #[test]
    fn composer_merge_single_bundle() {
        let b = ToolBehaviorBundle::new("b1")
            .with_tool(Arc::new(MockTool::new("tool-a")))
            .with_tool(Arc::new(MockTool::new("tool-b")));

        let merged = BundleComposer::merge(&[b]).unwrap();
        assert_eq!(merged.len(), 2);
    }
}
