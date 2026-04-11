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
let agent_spec = AgentSpec::new("ui-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Render structured UI when visual output helps.")
    .with_hook_filter("generative-ui");

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

The plugin registers a tool called `render_a2ui` that the LLM can invoke. When the LLM calls this tool with an array of A2UI messages, the tool validates the message structure and returns the validated payload, which flows through the event stream to the frontend.

2. Understand the A2UI protocol.

   A2UI v0.8 defines four message types. Each message is a JSON object with `"version": "v0.8"` and exactly one of the following keys:

| Message Type | Purpose |
|-------------|---------|
| `createSurface` | Initialize a new rendering surface |
| `updateComponents` | Define or update the component tree |
| `updateDataModel` | Populate or change data values |
| `deleteSurface` | Remove a surface |

Messages must be sent in order: create the surface first, then define components, then populate data.

3. Create a surface.

   Every UI starts by creating a surface with a unique ID and a reference to the frontend's component catalog:

```rust,ignore
// The LLM sends this via the render_a2ui tool:
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "createSurface": {
                "surfaceId": "order-form-1",
                "catalogId": "my-catalog"
            }
        }
    ]
});
```

The `catalogId` tells the frontend which set of component definitions to use for rendering.

4. Define the component tree.

   Components are specified as a flat list with adjacency references. Each component has a unique `id` and a `component` type name from the catalog. Parent-child relationships are expressed via `child` (single child) or `children` (multiple children):

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "updateComponents": {
                "surfaceId": "order-form-1",
                "components": [
                    {
                        "id": "root",
                        "component": "Card",
                        "child": "layout"
                    },
                    {
                        "id": "layout",
                        "component": "Column",
                        "children": ["title", "name-field", "submit-btn"]
                    },
                    {
                        "id": "title",
                        "component": "Text",
                        "text": "New Order"
                    },
                    {
                        "id": "name-field",
                        "component": "TextField",
                        "label": "Customer Name",
                        "value": { "path": "/customer/name" }
                    },
                    {
                        "id": "submit-btn",
                        "component": "Button",
                        "label": "Submit",
                        "action": {
                            "event": {
                                "name": "submit-order",
                                "context": { "formId": "order-form-1" }
                            }
                        }
                    }
                ]
            }
        }
    ]
});
```

Rules for the component list:
- One component must have `"id": "root"` -- this is the tree's entry point.
- Every component requires `"id"` and `"component"` fields.
- Use `"child"` for a single child reference or `"children"` for multiple.
- Additional properties (`text`, `label`, `action`, and any extra fields) are passed through to the frontend renderer.

5. Bind data with JSON paths.

   Use `{"path": "/json/pointer"}` in component properties to bind values to the surface's data model. The frontend resolves these paths against the data model at render time:

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "updateDataModel": {
                "surfaceId": "order-form-1",
                "path": "/",
                "value": {
                    "customer": {
                        "name": "",
                        "email": ""
                    },
                    "items": []
                }
            }
        }
    ]
});
```

The `path` field specifies which part of the data model to update. Use `"/"` to set the entire model, or a nested path like `"/customer/name"` to update a specific value.

6. Delete a surface.

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "deleteSurface": {
                "surfaceId": "order-form-1"
            }
        }
    ]
});
```

7. Send multiple messages in one tool call.

   The `render_a2ui` tool accepts an array of messages, so you can create a surface, define components, and populate data in a single call:

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "createSurface": { "surfaceId": "s1", "catalogId": "my-catalog" }
        },
        {
            "version": "v0.8",
            "updateComponents": {
                "surfaceId": "s1",
                "components": [
                    { "id": "root", "component": "Text", "text": "Hello" }
                ]
            }
        },
        {
            "version": "v0.8",
            "updateDataModel": { "surfaceId": "s1", "path": "/", "value": {} }
        }
    ]
});
```

8. Customize plugin instructions.

   The plugin injects prompt instructions that teach the LLM how to use the `render_a2ui` tool. You can customize these in several ways:

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
```

## Verify

1. Register the A2UI plugin and run the agent with a prompt that asks it to display information visually.
2. The agent should call the `render_a2ui` tool with valid A2UI messages.
3. Check the tool result in the event stream -- a successful call returns `{"a2ui": [...], "rendered": true}`.
4. On the frontend, confirm the surface appears with the expected components.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `missing required field "messages"` | Tool called without a `messages` array | Ensure the LLM sends `{"messages": [...]}` |
| `messages array must not be empty` | Empty messages array | Include at least one A2UI message |
| `unsupported version` | Version field is not `"v0.8"` | Set `"version": "v0.8"` on every message |
| `multiple message types in one object` | A single message contains more than one type key | Each message object must have exactly one of `createSurface`, `updateComponents`, `updateDataModel`, or `deleteSurface` |
| `components[N].id is required` | A component in `updateComponents` is missing `id` | Add `"id"` to every component object |
| `components[N].component is required` | A component is missing the type name | Add `"component"` with a valid catalog type |
| LLM does not call the tool | Plugin registered but instructions not reaching the LLM | Verify the plugin is activated on the agent spec |

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
