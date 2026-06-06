---
title: "Add a Tool"
description: "Use this when you need to expose a custom capability to the agent by implementing the Tool trait."
---

Use this when you need to expose a custom capability to the agent by implementing the `Tool` trait.

## Purpose

Tools are the code boundary for actions the model may request but must not implement itself. Keeping capabilities behind typed descriptors, argument validation, and `ToolOutput` is better than prompt-only instructions because the runtime can validate inputs, stream results, and commit state through one controlled channel.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- `async-trait` and `serde_json` available

## Steps

1. Implement the `Tool` trait.

```rust
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};

async fn fetch_weather(_city: &str) -> Result<String, ToolError> {
    Ok("Sunny, 22°C".to_string())
}

pub struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_weather", "Get Weather", "Fetch current weather for a city")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["city"]
            }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let city = args["city"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'city'".into()))?;

        let weather = fetch_weather(city).await?;

        Ok(ToolResult::success("get_weather", json!({ "forecast": weather })).into())
    }
}
```

2. Optionally override argument validation.

```rust
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult};

pub struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_weather", "Get Weather", "Fetch current weather for a city")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["city"]
            }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        if !args.get("city").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
            return Err(ToolError::InvalidArguments("'city' must be a non-empty string".into()));
        }
        Ok(())
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("get_weather", json!({})).into())
    }
}
```

`validate_args` runs before `execute` and lets you reject malformed input early.

3. Register the tool with the builder.

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentRuntimeBuilder, AgentSpec};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult};

pub struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_weather", "Get Weather", "Fetch current weather for a city")
            .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("get_weather", json!({})).into())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = AgentSpec::new("assistant").with_model_id("claude-sonnet");

    let runtime = AgentRuntimeBuilder::new()
        .with_tool("get_weather", Arc::new(WeatherTool))
        .with_agent_spec(spec)
        .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
        .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"))
        .build()?;

    let _runtime = runtime;
    Ok(())
}
```

The string ID passed to `with_tool` must match the `id` in `ToolDescriptor::new`.

4. Register via a plugin (alternative).

   Tools can also be registered inside a `Plugin::register` method through the `PluginRegistrar`:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::{Plugin, PluginDescriptor, PluginRegistrar, StateError};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult};

pub struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_weather", "Get Weather", "Fetch current weather for a city")
            .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("get_weather", json!({})).into())
    }
}

pub struct WeatherPlugin;

impl Plugin for WeatherPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "weather" }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool("get_weather", Arc::new(WeatherTool))?;
        Ok(())
    }
}
```

Plugin-registered tools are scoped to agents that activate that plugin.

## Gate which tools an agent can call

`with_tool` puts the tool in the runtime *registry* — every running agent can potentially call it. *Which* tools a given agent is actually allowed to call is set by two `AgentSpec` fields, `allowed_tools` (whitelist) and `excluded_tools` (blacklist). You can set them in **code**, over the **REST config API**, or in the **admin console UI** — they are the same two fields everywhere, and config overrides the code default on the next run.

Gating only selects among things already in the registries, so both halves must be registered first:

- **The tool** — via `with_tool`, a plugin's `registrar.register_tool`, or MCP auto-registration. Unregistered tools cannot be called at all.
- **The agent** — via `with_agent_spec` in code, or by publishing its spec through config (the runtime merges local and published agents). The `assistant` referenced below must already exist this way to be resolvable by id; see [Build an Agent](/awaken/how-to/build-an-agent/).

Registration into these registries is the runtime's core wiring — the same model covers tools, agents, models, providers, and plugins. See [Agent Resolution](/awaken/explanation/agent-resolution/).

### In code

Bake a default into the `AgentSpec` you build. The fields are plain `Option<Vec<String>>`:

```rust
use awaken::AgentSpec;

let mut spec = AgentSpec::new("assistant")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("You help with weather questions.");
spec.allowed_tools = Some(vec!["get_weather".into()]); // None = all registered tools
spec.excluded_tools = None;
// builder.with_agent_spec(spec)
```

### Dynamically, over config

In server mode the runtime resolves the *managed* spec at call time, so publishing a change overrides the code-built default with no rebuild and no restart:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "assistant",
    "model_id": "gpt-4o-mini",
    "system_prompt": "You help with weather questions.",
    "allowed_tools": ["get_weather"],
    "excluded_tools": []
  }'
```

### In the admin console UI

Open the agent editor and use the **Tools** section: choose **All tools** or a **Custom selection**, narrow built-in / plugin / MCP tools with source filters, and validate a preview before saving. Step-by-step: [Configure Agent Behavior → Narrow the tool catalog](/awaken/how-to/configure-agent-behavior/#narrow-the-tool-catalog); editor tour: [Use the Admin Console](/awaken/how-to/use-admin-console/).

Whichever surface you use, `allowed_tools` whitelists and `excluded_tools` blacklists, and the change applies on the next run. Add a tool in code once; gate it per agent in code, config, or the UI.

For finer per-call control (allow/deny/ask on argument shape, not just tool name), use the [Permission plugin](/awaken/how-to/enable-tool-permission-hitl/).

## Verify

Send a message that should trigger the tool. Inspect the run result to confirm the tool was called and returned the expected output.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| `ToolError::InvalidArguments` | The LLM passed malformed JSON | Tighten the JSON Schema in `with_parameters` to guide the model |
| Tool never called | Descriptor `id` does not match the registered ID | Ensure the ID in `ToolDescriptor::new` and `with_tool` are identical |
| `ToolError::ExecutionFailed` | Runtime error inside `execute` | Return a descriptive error; the agent will see it and may retry |

## Related Example

`examples/src/research/tools.rs` -- `SearchTool` and `WriteReportTool` implementations.

## Key Files

- `crates/awaken-runtime-contract/src/contract/tool.rs` -- `Tool` trait, `ToolDescriptor`, `ToolResult`, `ToolError`
- `crates/awaken-runtime/src/builder.rs` -- `with_tool` registration

## Related

- [Build an Agent](/awaken/how-to/build-an-agent/)
- [Add a Plugin](/awaken/how-to/add-a-plugin/)
