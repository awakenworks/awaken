---
title: "Build an Agent"
description: "Use this when you need to assemble an agent with tools, persistence, and a provider into a running AgentRuntime."
---

Use this when you need to assemble an agent with tools, persistence, and a provider into a running `AgentRuntime`.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- An `LlmExecutor` implementation (provider) available
- Familiarity with `AgentSpec` and `AgentRuntimeBuilder`

## Steps

1. Define the agent spec.

```rust
use awaken::AgentSpec;

let spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_max_rounds(10);
```

2. Register tools.

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult};
use awaken::{AgentRuntimeBuilder, AgentSpec};

struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("search", "Search", "Search documents")
            .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("search", json!([])).into())
    }
}

struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("calculator", "Calculator", "Evaluate expressions")
            .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("calculator", json!({"result": 0})).into())
    }
}

let spec = AgentSpec::new("assistant");

let builder = AgentRuntimeBuilder::new()
    .with_agent_spec(spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_tool("calculator", Arc::new(CalculatorTool));
```

3. Register a provider and a model.

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::AgentRuntimeBuilder;

let builder = AgentRuntimeBuilder::new();

let builder = builder
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"));
```

4. Attach persistence.

```rust
use std::sync::Arc;
use awaken::stores::InMemoryStore;
use awaken::AgentRuntimeBuilder;

let builder = AgentRuntimeBuilder::new();

let store = Arc::new(InMemoryStore::new());
let builder = builder.with_thread_run_store(store);
```

5. Build and validate.

```rust
use std::sync::Arc;
use awaken::engine::MockLlmExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentRuntimeBuilder, AgentSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let builder = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec::new("assistant").with_model_id("mock"))
        .with_provider("mock", Arc::new(MockLlmExecutor::new()))
        .with_model(ModelSpec::new("mock", "mock", "mock"));

    let runtime = builder.build()?;
    let _runtime = runtime;
    Ok(())
}
```

`build` resolves every registered agent and catches missing models, providers, or plugins at startup rather than at request time.

6. Tune agent behavior through config.

`AgentSpec` is the runtime config object for an agent. The fields and sections
below are the same data edited by `/v1/config/agents` and the admin console:

```rust
use serde_json::json;
use awaken::AgentSpec;

let mut spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a careful coding assistant.")
    .with_hook_filter("reminder")
    .with_section("reminder", json!({
        "rules": [{
            "tool": "*",
            "output": "any",
            "message": {
                "target": "suffix_system",
                "content": "Prefer verifying code changes before final answers.",
                "cooldown_turns": 3
            }
        }]
    }));
spec.plugin_ids.push("reminder".into());
```

Use `system_prompt` for the base prompt. Use plugin sections such as
`reminder`, `generative-ui`, `permission`, and `deferred_tools` for behavior
that should be validated, saved, edited in the page, and applied to later runs.
Future prompt semantic hooks should follow the same typed section pattern.

7. Execute a run.

```rust
use awaken::engine::MockLlmExecutor;
use awaken::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec::new("assistant").with_model_id("mock"))
        .with_provider("mock", Arc::new(MockLlmExecutor::new()))
        .with_model(ModelSpec::new("mock", "mock", "mock"))
        .build()?;
    let user_message = Message::user("Hello");

    let request = RunActivation::new("thread-1", vec![user_message])
        .with_agent_id("assistant");

    // Use runtime.run(..., sink) when callers need streaming events.
    let result = runtime.run_to_completion(request).await?;
    let _result = result;
    Ok(())
}
```

## Verify

Call the `/health` endpoint (if using the server feature) or inspect the returned `AgentRunResult` to confirm the agent loop completed without errors.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| `BuildError::ValidationFailed` | Agent spec references a model or provider not registered in the builder | Register the missing model/provider before calling `build` |
| `BuildError::State` | Duplicate state key registration across plugins | Ensure each `StateKey` is registered by exactly one plugin |
| `RuntimeError` at run time | Provider returns an inference error | Check provider credentials and model ID |

## Related Example

`examples/src/research/` -- a research agent with search and report-writing tools.

## Key Files

- `crates/awaken-runtime/src/builder.rs` -- `AgentRuntimeBuilder`
- `crates/awaken-contract/src/registry_spec.rs` -- `AgentSpec`
- `crates/awaken-runtime/src/runtime/agent_runtime/mod.rs` -- `AgentRuntime`

## Related

- [Add a Tool](/awaken/how-to/add-a-tool/)
- [Add a Plugin](/awaken/how-to/add-a-plugin/)
- [Use File Store](/awaken/how-to/use-file-store/)
- [Expose HTTP with SSE](/awaken/how-to/expose-http-sse/)
