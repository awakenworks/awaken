# Use Generative UI (A2UI)

Use this when you want the agent to send declarative UI messages to the frontend instead of describing the UI in prose.

## Prerequisites

- A working awaken agent runtime (see [Build an Agent](./build-an-agent.md))
- A frontend that can render A2UI messages from the event stream
- A frontend-side component catalog that matches the names the model is allowed to use

```toml
[dependencies]
awaken = { version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Register the A2UI plugin.

```rust,ignore
use std::sync::Arc;

use awaken::{AgentRuntimeBuilder, Plugin};
use awaken::ext_generative_ui::{A2uiPlugin, DEFAULT_A2UI_CATALOG_ID};

let plugin = A2uiPlugin::default();
assert_eq!(
    plugin.instructions().contains(DEFAULT_A2UI_CATALOG_ID),
    true
);

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_plugin("generative-ui", Arc::new(plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

The plugin does two things:

- registers the `render_a2ui` tool
- injects A2UI protocol guidance into the inference request before each model call

The tool validates the payload. Frontends normally render from the tool **input** (the A2UI message the model sent), while the tool **output** is only an acknowledgement such as `{"rendered": true, "count": 3}`.

2. Understand the current A2UI message contract.

Each tool call must provide either:

- one top-level A2UI message key: `surfaceUpdate`, `dataModelUpdate`, `beginRendering`, or `deleteSurface`
- or a legacy `messages` array containing those single-message objects

The recommended flow is:

1. `surfaceUpdate`
2. `dataModelUpdate`
3. `beginRendering`

3. Send `surfaceUpdate` with a flat component list.

```rust,ignore
let args = serde_json::json!({
    "surfaceUpdate": {
        "surfaceId": "ops_request",
        "components": [
            {"id": "root", "component": {"Card": {"child": "content"}}},
            {"id": "content", "component": {"Column": {"children": {"explicitList": ["title", "requester", "submit_label", "submit_button"]}}}},
            {"id": "title", "component": {"Text": {"usageHint": "h2", "text": {"literalString": "Operations request"}}}},
            {"id": "requester", "component": {"TextField": {"label": {"literalString": "Requester"}, "text": {"path": "/request/requester"}, "textFieldType": "shortText"}}},
            {"id": "submit_label", "component": {"Text": {"text": {"literalString": "Submit"}}}},
            {"id": "submit_button", "component": {"Button": {"child": "submit_label", "primary": true, "action": {"name": "ops_request.submit", "context": [{"key": "requester", "value": {"path": "/request/requester"}}]}}}}
        ]
    }
});
```

Rules:

- components are a flat list
- every component must include `id` and `component`
- `component` must be an object such as `{"Text": {...}}`
- parent/child relationships are expressed by component IDs inside nested props

4. Send `dataModelUpdate` for the bound values.

```rust,ignore
let args = serde_json::json!({
    "dataModelUpdate": {
        "surfaceId": "ops_request",
        "path": "/request",
        "contents": [
            {"key": "requester", "valueString": ""},
            {"key": "status", "valueString": "draft"}
        ]
    }
});
```

5. Activate the surface with `beginRendering`.

```rust,ignore
let args = serde_json::json!({
    "beginRendering": {
        "surfaceId": "ops_request",
        "root": "root"
    }
});
```

6. Remove the surface when the workflow is complete.

```rust,ignore
let args = serde_json::json!({
    "deleteSurface": {
        "surfaceId": "ops_request"
    }
});
```

7. Override the catalog hint or injected instructions through config.

You can set plugin defaults when constructing the plugin:

```rust,ignore
use awaken::ext_generative_ui::A2uiPlugin;

let plugin = A2uiPlugin::with_catalog_and_examples(
    "catalog://ops-ui",
    "Example: build a request intake card with a submit button."
);
```

Or override them per agent with typed config:

```rust,ignore
use awaken::registry_spec::AgentSpec;
use awaken::ext_generative_ui::{A2uiPromptConfig, A2uiPromptConfigKey};

let agent = AgentSpec::new("a2ui")
    .with_model("default")
    .with_system_prompt("You create operational UIs with render_a2ui.")
    .with_config::<A2uiPromptConfigKey>(A2uiPromptConfig {
        catalog_id: Some("catalog://ops-ui".into()),
        examples: Some("Example: render an approval form.".into()),
        instructions: None,
    })?;
```

If `instructions` is set, it fully replaces the default injected guidance for that agent.

8. Optional: override agent prompts and renderer catalogs from a config file.

The starter backend example accepts `AGENT_CONFIG=/path/to/agents.json`. Every agent — including generative UI renderers — is configured under `agents.<id>`:

```json
{
  "agents": {
    "default": {
      "system_prompt": "You are a concise assistant for the starter backend."
    },
    "a2ui": {
      "system_prompt": "You build procurement UIs with render_a2ui.",
      "catalog": "Catalog component hints:\n- TextField: Single-line text input for requester, ticket ID, or budget code\n- MultipleChoice: Selection input for priority, approval state, or department\n\nCatalog examples:\n- Use TextField for requester and budget code.\n- Use a nearby Text component as the visible label for MultipleChoice."
    },
    "json-render": {
      "system_prompt": "Use render_json_ui when the user asks for dashboards, forms, or review workspaces."
    },
    "json-render-ui": {
      "catalog": "{\n  \"Card\": \"Container for approval summaries\",\n  \"Table\": \"Tabular records such as request line items\",\n  \"Badge\": \"Short status label such as Pending or Approved\"\n}"
    },
    "openui": {
      "system_prompt": "Use render_openui_ui for operational review panels and forms."
    },
    "openui-ui": {
      "catalog": "Available components:\n- Card(children, variant?) — Groups a logical section\n- Tag(text, variant?) — Status pill\n- Buttons(buttons, direction?) — Action bar for workflow steps"
    }
  }
}
```

- `system_prompt` — fully replaces the agent's default system prompt.
- `catalog` — free-form text injected into the renderer's prompt template. The format depends on the renderer (JSON object for json-render, DSL signatures for openui, component hints for a2ui). When `system_prompt` is also set, it takes priority.

## Verify

1. Run an agent with the `generative-ui` plugin enabled.
2. Ask it to create a form, workflow screen, or review panel.
3. Confirm the model calls `render_a2ui` with valid `surfaceUpdate` / `dataModelUpdate` / `beginRendering` payloads.
4. Confirm the frontend renders the surface from the tool input.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `expected at least one A2UI message key` | The tool call had no supported top-level A2UI key | Send one of `surfaceUpdate`, `dataModelUpdate`, `beginRendering`, or `deleteSurface` |
| `A2UI validation failed: message[0]: multiple message types...` | A single object included more than one message key | Send exactly one A2UI message per object |
| `components[0].id is required` | A component is missing `id` | Add an `id` to every component |
| `components[0].component is required` | A component is missing its payload object | Add a payload such as `{"Text": {...}}` |
| `beginRendering.root is required` | `beginRendering` did not reference a root component | Provide the root component ID |
| The model does not call the tool | The plugin is not active for the agent, or prompt guidance was overridden incorrectly | Verify the agent activates the `generative-ui` plugin and check `A2uiPromptConfigKey` overrides |

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-generative-ui/src/a2ui/plugin.rs` | `A2uiPlugin`, injected prompt guidance, and typed config override support |
| `crates/awaken-ext-generative-ui/src/a2ui/tool.rs` | `render_a2ui` schema, normalization, validation, and execution |
| `crates/awaken-ext-generative-ui/src/a2ui/types.rs` | A2UI payload structs |
| `crates/awaken-ext-generative-ui/src/a2ui/validation.rs` | Structural validation for A2UI messages |
| `examples/src/starter_backend/generative_ui_config.rs` | Starter backend agent config loader (system prompt and catalog overrides) |

## Related

- [Integrate CopilotKit / AG-UI](./integrate-copilotkit-ag-ui.md)
- [Integrate AI SDK Frontend](./integrate-ai-sdk-frontend.md)
- [Add a Plugin](./add-a-plugin.md)
