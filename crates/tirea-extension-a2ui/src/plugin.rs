//! A2UI behavior plugin for system prompt injection.
//!
//! Implements [`AgentBehavior`] to inject A2UI schema instructions into the
//! system prompt before each LLM inference call. This tells the LLM about
//! the `render_a2ui` tool's expected JSON format, available components, and
//! usage patterns.
//!
//! Mirrors Google ADK's `_SendA2uiJsonToClientTool.process_llm_request()`,
//! which appends A2UI schema and examples to the system instructions.

use async_trait::async_trait;
use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea_contract::runtime::AgentBehavior;

/// A2UI behavior plugin that injects A2UI schema and usage instructions
/// into the system prompt before LLM inference.
///
/// # Usage
///
/// ```ignore
/// use tirea_extension_a2ui::A2uiPlugin;
///
/// let plugin = A2uiPlugin::with_catalog_id(
///     "https://a2ui.org/specification/v0_9/basic_catalog.json",
/// );
/// // Register as behavior on the agent
/// ```
pub struct A2uiPlugin {
    instructions: String,
}

impl A2uiPlugin {
    /// Create a plugin with a specific catalog ID and optional extra examples.
    ///
    /// The generated instructions tell the LLM how to call `render_a2ui` with
    /// A2UI v0.9 messages using the given catalog.
    pub fn with_catalog_id(catalog_id: &str) -> Self {
        Self {
            instructions: build_instructions(catalog_id, None),
        }
    }

    /// Create a plugin with a catalog ID and custom examples block.
    ///
    /// `examples` is appended verbatim after the schema instructions. Use this
    /// to provide domain-specific A2UI examples that guide the LLM.
    pub fn with_catalog_and_examples(catalog_id: &str, examples: &str) -> Self {
        Self {
            instructions: build_instructions(catalog_id, Some(examples)),
        }
    }

    /// Create a plugin with fully custom instructions.
    ///
    /// Use this when you need complete control over the injected prompt.
    pub fn with_custom_instructions(instructions: String) -> Self {
        Self { instructions }
    }

    /// Returns the instructions that will be injected.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

#[async_trait]
impl AgentBehavior for A2uiPlugin {
    fn id(&self) -> &str {
        "a2ui"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddSystemContext(
            self.instructions.clone(),
        ))
    }
}

fn build_instructions(catalog_id: &str, examples: Option<&str>) -> String {
    let mut s = String::from(A2UI_SCHEMA_INSTRUCTIONS);
    s = s.replace("{{CATALOG_ID}}", catalog_id);
    if let Some(examples) = examples {
        s.push_str("\n\n---BEGIN A2UI EXAMPLES---\n");
        s.push_str(examples);
        s.push_str("\n---END A2UI EXAMPLES---");
    }
    s
}

const A2UI_SCHEMA_INSTRUCTIONS: &str = r#"
## A2UI Declarative UI

You have access to the `render_a2ui` tool to send declarative UI to the client.
Call this tool with an array of A2UI v0.9 messages. Each message must be a JSON
object with `"version": "v0.9"` and exactly one of:

### Message Types

1. **createSurface** — Initialize a UI surface:
   ```json
   {"version": "v0.9", "createSurface": {"surfaceId": "<id>", "catalogId": "{{CATALOG_ID}}"}}
   ```

2. **updateComponents** — Define the component tree (flat adjacency list):
   ```json
   {"version": "v0.9", "updateComponents": {"surfaceId": "<id>", "components": [
     {"id": "root", "component": "Card", "child": "col"},
     {"id": "col", "component": "Column", "children": ["title", "input"]},
     {"id": "title", "component": "Text", "text": "Hello"},
     {"id": "input", "component": "TextField", "label": "Name", "value": {"path": "/name"}}
   ]}}
   ```

3. **updateDataModel** — Populate data for the surface:
   ```json
   {"version": "v0.9", "updateDataModel": {"surfaceId": "<id>", "path": "/", "value": {"name": ""}}}
   ```

4. **deleteSurface** — Remove a surface:
   ```json
   {"version": "v0.9", "deleteSurface": {"surfaceId": "<id>"}}
   ```

### Rules
- Always send `createSurface` first, then `updateComponents`, then `updateDataModel`.
- Components are a flat list; use `children` (array of IDs) or `child` (single ID) to form a tree.
- One component must have `"id": "root"`.
- Each component needs `"id"` and `"component"` (the type name from the catalog).
- Use `{"path": "/key"}` for data binding in component properties.
- Use `"action": {"event": {"name": "...", "context": {...}}}` on Button for user interactions.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    const TEST_CATALOG: &str = "https://a2ui.org/specification/v0_9/basic_catalog.json";

    #[tokio::test]
    async fn injects_schema_instructions_before_inference() {
        let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = plugin.before_inference(&ctx).await;
        assert_eq!(actions.len(), 1);

        let action = actions.into_iter().next().unwrap();
        match action {
            BeforeInferenceAction::AddSystemContext(text) => {
                assert!(text.contains("render_a2ui"), "should mention tool name");
                assert!(
                    text.contains("createSurface"),
                    "should mention createSurface"
                );
                assert!(
                    text.contains("updateComponents"),
                    "should mention updateComponents"
                );
                assert!(
                    text.contains("updateDataModel"),
                    "should mention updateDataModel"
                );
                assert!(
                    text.contains("deleteSurface"),
                    "should mention deleteSurface"
                );
                assert!(text.contains(TEST_CATALOG), "should contain catalog ID");
                assert!(
                    !text.contains("{{CATALOG_ID}}"),
                    "template should be resolved"
                );
            }
            _ => panic!("expected AddSystemContext"),
        }
    }

    #[tokio::test]
    async fn includes_custom_examples() {
        let examples = r#"
[
  {"version": "v0.9", "createSurface": {"surfaceId": "demo", "catalogId": "test"}},
  {"version": "v0.9", "updateComponents": {"surfaceId": "demo", "components": [
    {"id": "root", "component": "Text", "text": "Demo"}
  ]}}
]"#;
        let plugin = A2uiPlugin::with_catalog_and_examples(TEST_CATALOG, examples);

        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = plugin.before_inference(&ctx).await;
        let action = actions.into_iter().next().unwrap();
        match action {
            BeforeInferenceAction::AddSystemContext(text) => {
                assert!(text.contains("---BEGIN A2UI EXAMPLES---"));
                assert!(text.contains("---END A2UI EXAMPLES---"));
                assert!(text.contains("Demo"));
            }
            _ => panic!("expected AddSystemContext"),
        }
    }

    #[test]
    fn plugin_id() {
        let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
        assert_eq!(plugin.id(), "a2ui");
    }

    #[test]
    fn custom_instructions() {
        let plugin = A2uiPlugin::with_custom_instructions("Use A2UI.".into());
        assert_eq!(plugin.instructions(), "Use A2UI.");
    }
}
