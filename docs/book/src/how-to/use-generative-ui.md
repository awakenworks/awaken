# Use Generative UI (A2UI)

Use this when you want the agent to send declarative UI components to a frontend -- for example, rendering a form, a data table, or an interactive card without the frontend knowing the layout in advance.

## Prerequisites

- A working awaken agent runtime (see [Build an Agent](./build-an-agent.md))
- A frontend that consumes A2UI messages from the event stream (e.g. a CopilotKit or AI SDK integration)
- A component catalog registered on the frontend that defines available UI components

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Register the A2UI plugin.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_generative_ui::A2uiPlugin;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let plugin = A2uiPlugin::with_catalog_id("my-catalog");
let mut agent_spec = AgentSpec::new("ui-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Render structured UI when visual output helps.")
    .with_hook_filter("generative-ui");
agent_spec.plugin_ids.push("generative-ui".into());

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding(
        "gpt-4o-mini",
        ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin("generative-ui", Arc::new(plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

The plugin registers a tool called `render_a2ui` that the LLM can invoke. When the LLM calls this tool with A2UI messages, the tool validates the message structure and returns the validated payload, which flows through the event stream to the frontend.

`plugin_ids` loads the plugin for the agent. `with_hook_filter("generative-ui")`
keeps the A2UI prompt-injection hook active when the agent also loads other
plugins.

2. Understand the A2UI protocol.

   Awaken's A2UI tool uses the v0.8 server-to-client message keys directly. A
   tool call can pass exactly one message object or a legacy `messages` array.

| Message Type | Purpose |
|-------------|---------|
| `surfaceUpdate` | Define or update the component tree |
| `dataModelUpdate` | Populate or change data values |
| `beginRendering` | Select the root component and start rendering |
| `deleteSurface` | Remove a surface |

For a new surface, send `surfaceUpdate`, then `dataModelUpdate` when data is
needed, then `beginRendering`. Use `deleteSurface` when the workflow is complete.

3. Define the component tree.

   Components are a flat list. Each component has an `id` and a v0.8 component
   payload object with exactly one component type:

```rust,ignore
// The LLM sends this via the render_a2ui tool:
let message = serde_json::json!({
    "surfaceUpdate": {
        "surfaceId": "order-form-1",
        "components": [
            {
                "id": "root",
                "component": { "Card": { "child": "title" } }
            },
            {
                "id": "title",
                "component": {
                    "Text": { "text": { "literalString": "New Order" } }
                }
            }
        ]
    }
});
```

Rules for component lists:

- Every component requires `id` and `component`.
- `component` must be an object with one key, such as `{ "Text": {...} }`.
- Relationships are expressed by component IDs inside component props, such as
  `child` or `children.explicitList`.

4. Bind data with data model entries.

   `dataModelUpdate.contents` is an array of key/value entries. Supported value
   fields are `valueString`, `valueNumber`, `valueBoolean`, and `valueMap`.

```rust,ignore
let message = serde_json::json!({
    "dataModelUpdate": {
        "surfaceId": "order-form-1",
        "path": "/order",
        "contents": [
            { "key": "customer", "valueString": "" },
            { "key": "quantity", "valueNumber": 1.0 }
        ]
    }
});
```

5. Start rendering.

```rust,ignore
let message = serde_json::json!({
    "beginRendering": {
        "surfaceId": "order-form-1",
        "root": "root"
    }
});
```

6. Delete a surface.

```rust,ignore
let message = serde_json::json!({
    "deleteSurface": {
        "surfaceId": "order-form-1"
    }
});
```

7. Send multiple messages in one tool call.

   The `render_a2ui` tool accepts a `messages` array for multi-message calls:

```rust,ignore
let args = serde_json::json!({
    "messages": [
        { "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                { "id": "root", "component": { "Text": {
                    "text": { "literalString": "Hello" }
                }}}
            ]
        }},
        { "beginRendering": { "surfaceId": "s1", "root": "root" }}
    ]
});
```

8. Customize plugin instructions.

   The plugin injects prompt instructions that teach the LLM how to use the
   `render_a2ui` tool. Defaults can be set on the plugin, and per-agent
   overrides can be saved in the `generative-ui` config section.

```rust,ignore
// With catalog ID and custom examples appended to the default instructions
let plugin = A2uiPlugin::with_catalog_and_examples(
    "my-catalog",
    "Example: create a card with a title and a button..."
);

// With fully custom instructions (replaces the default instructions entirely)
let plugin = A2uiPlugin::with_custom_instructions(
    "You can render UI by calling render_a2ui...".to_string()
);

// Per-agent config, equivalent to editing the generative-ui section in the admin console
let agent_spec = agent_spec.with_section("generative-ui", serde_json::json!({
    "catalog_id": "my-catalog",
    "examples": "Example: render a compact order summary."
}));
```

## Verify

1. Register the A2UI plugin and run the agent with a prompt that asks it to display information visually.
2. The agent should call the `render_a2ui` tool with valid A2UI messages.
3. Check the tool result in the event stream -- a successful call returns `{"a2ui": [...], "rendered": true}`.
4. On the frontend, confirm the surface appears with the expected components.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `expected at least one A2UI message key` | Tool called without a direct message key or `messages` array | Send one of `surfaceUpdate`, `dataModelUpdate`, `beginRendering`, `deleteSurface`, or `{"messages": [...]}` |
| `messages array must not be empty` | Empty `messages` array | Include at least one A2UI message |
| `surfaceUpdate.components is required` | Component update has no component list | Add a non-empty `components` array |
| `component must contain exactly one component payload` | `component` is not shaped like `{ "Text": {...} }` | Use one v0.8 component type per component |
| `dataModelUpdate.contents must not be empty` | Data model update has no entries | Add `contents` entries with `key` and `valueString`/`valueNumber`/`valueBoolean`/`valueMap` |
| `beginRendering.root is required` | Render start did not specify the root component | Set `root` to an existing component ID |
| LLM does not call the tool | Plugin not loaded or hook filtered out | Add `"generative-ui"` to `plugin_ids` and include `with_hook_filter("generative-ui")` when using hook filters |

## Related Example

- `crates/awaken-ext-generative-ui/src/a2ui/tests.rs` -- validation and tool execution test cases

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-generative-ui/src/a2ui/mod.rs` | A2UI module root, constants, re-exports |
| `crates/awaken-ext-generative-ui/src/a2ui/plugin.rs` | `A2uiPlugin` registration and prompt instructions |
| `crates/awaken-ext-generative-ui/src/a2ui/tool.rs` | `A2uiRenderTool` -- validation and execution |
| `crates/awaken-ext-generative-ui/src/a2ui/types.rs` | `A2uiMessage`, `A2uiComponent`, and related structs |
| `crates/awaken-ext-generative-ui/src/a2ui/validation.rs` | `validate_a2ui_messages` structural checks |

## Related

- [Integrate CopilotKit / AG-UI](./integrate-copilotkit-ag-ui.md)
- [Integrate AI SDK Frontend](./integrate-ai-sdk-frontend.md)
- [Add a Plugin](./add-a-plugin.md)
