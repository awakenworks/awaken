use awaken_contract::StateError;
use awaken_contract::registry_spec::AgentSpec;

use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::MutationBatch;

use super::A2UI_PLUGIN_ID;

/// A2UI plugin that provides the render tool and prompt instructions.
pub struct A2uiPlugin {
    instructions: String,
}

impl A2uiPlugin {
    /// Create with a specific catalog ID.
    pub fn with_catalog_id(catalog_id: &str) -> Self {
        Self {
            instructions: build_instructions(catalog_id, None),
        }
    }

    /// Create with a catalog ID and custom examples.
    pub fn with_catalog_and_examples(catalog_id: &str, examples: &str) -> Self {
        Self {
            instructions: build_instructions(catalog_id, Some(examples)),
        }
    }

    /// Create with fully custom instructions.
    pub fn with_custom_instructions(instructions: String) -> Self {
        Self { instructions }
    }

    /// Returns the instructions that will be injected.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl Plugin for A2uiPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: A2UI_PLUGIN_ID,
        }
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // A2UI plugin is stateless — instructions are injected via the phase hook system
        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
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
