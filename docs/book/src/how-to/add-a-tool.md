# Add a Tool

Use this when you need to expose a custom capability to the agent by implementing the `Tool` trait.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- `async-trait` and `serde_json` available

## Steps

1. Implement the `Tool` trait.

```rust,no_run
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};

# async fn fetch_weather(_city: &str) -> Result<String, ToolError> {
#     Ok("Sunny, 22°C".to_string())
# }
#
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

```rust,ignore
fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
    if !args.get("city").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
        return Err(ToolError::InvalidArguments("'city' must be a non-empty string".into()));
    }
    Ok(())
}
```

`validate_args` runs before `execute` and lets you reject malformed input early.

3. Register the tool with the builder.

```rust,ignore
use std::sync::Arc;
use awaken::AgentRuntimeBuilder;

let runtime = AgentRuntimeBuilder::new()
    .with_tool("get_weather", Arc::new(WeatherTool))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

The string ID passed to `with_tool` must match the `id` in `ToolDescriptor::new`.

4. Register via a plugin (alternative).

   Tools can also be registered inside a `Plugin::register` method through the `PluginRegistrar`:

```rust,ignore
fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
    registrar.register_tool("get_weather", Arc::new(WeatherTool))?;
    Ok(())
}
```

Plugin-registered tools are scoped to agents that activate that plugin.

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

- `crates/awaken-contract/src/contract/tool.rs` -- `Tool` trait, `ToolDescriptor`, `ToolResult`, `ToolError`
- `crates/awaken-runtime/src/builder.rs` -- `with_tool` registration

## Related

- [Build an Agent](./build-an-agent.md)
- [Add a Plugin](./add-a-plugin.md)
