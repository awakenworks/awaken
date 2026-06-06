---
title: "Build an Agent"
description: "Use this when you need to assemble an agent with tools, persistence, and a provider into a running AgentRuntime."
---

Use this when you need to assemble an agent with tools, persistence, and a provider into a running `AgentRuntime`.

## Purpose

This page defines the runtime boundary: what the agent can execute, which model it can call, where state is persisted, and which behavior remains configurable later. Putting these choices in the builder makes startup validation fail fast instead of letting a user discover missing tools or providers mid-run.

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

Each `with_*` call registers into one of the runtime's five registries (agents, tools, models, providers, plugins); an agent is resolved against them by id at call time, and in server mode the same registries are filled from published config. See [Agent Resolution](/awaken/explanation/agent-resolution/).

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

For a custom model client, implement `LlmExecutor` and register it with
`with_provider`. In server-managed config, provide a `ProviderExecutorFactory`
so `ProviderSpec` records can be materialized into live executors. Keep retry
and model failover outside the provider implementation: retry is applied during
resolution, and provider failover belongs in `ModelPoolSpec`.

4. Attach persistence.

```rust
use std::sync::Arc;
use awaken::contract::commit_coordinator::CommitCoordinator;
use awaken::stores::{InMemoryStore, MemoryCommitCoordinator};
use awaken::AgentRuntimeBuilder;

let builder = AgentRuntimeBuilder::new();

let store = Arc::new(InMemoryStore::new());
let coordinator = MemoryCommitCoordinator::wrap(store) as Arc<dyn CommitCoordinator>;
let builder = builder.with_commit_coordinator(coordinator);
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

8. Wire background work only when the agent needs it.

Use the background extension when a tool starts work that may finish after the current model step, or when a child agent should keep an inbox open for follow-up messages. This is better than spawning an untracked task because the task gets a stable ID, cancellation token, persisted status, parent lineage, and inbox events that the loop can resume from.

```rust
use std::sync::Arc;
use awaken::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, SendMessageTool,
};

let background = Arc::new(BackgroundTaskManager::new());
let background_plugin = Arc::new(BackgroundTaskPlugin::new(background.clone()));

let builder = builder
    .with_plugin("background_tasks", background_plugin)
    // Register SendMessageTool when your host provides a DurableMessageSink
    // for cross-thread or cross-process agent messages.
    .with_tool("send_message", Arc::new(SendMessageTool::new(background, durable_sink)));
```

Inside a tool, use `BackgroundTaskManager::spawn(...)` for ordinary background work and `spawn_agent_with_context(...)` when the background task runs another agent loop with its own inbox. Keep state exchange explicit: task status lives in `BackgroundTaskStateKey`; parent ↔ child domain state should still use typed `StateKey` seed/export rules from [Invoke a Sub-Agent from a Tool](/awaken/how-to/invoke-sub-agent-from-tool/).

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

## Code References

- `crates/awaken-doctest/examples/http_app_builder.rs` -- canonical `AgentRuntime` → `Mailbox` → `ServerState` wiring.
- `crates/awaken/tests/readme_quickstart.rs` -- small custom `LlmExecutor` used by the README path.
- `crates/awaken-server/tests/config_api.rs` and `crates/awaken-server/tests/config_backends.rs` -- `ProviderExecutorFactory`, managed provider config, and model-pool coverage.
- `crates/awaken-runtime/tests/background_task_lifecycle.rs` -- background task, background agent, inbox, cancellation, and status propagation.
- `crates/awaken-runtime/tests/child_agent_seed.rs` -- parent → child state seed and child → parent state export rules.

## Key Files

- `crates/awaken-runtime/src/builder.rs` -- `AgentRuntimeBuilder`
- `crates/awaken-runtime-contract/src/registry_spec.rs` -- `AgentSpec`
- `crates/awaken-runtime/src/runtime/agent_runtime/mod.rs` -- `AgentRuntime`

## Related

- [Add a Tool](/awaken/how-to/add-a-tool/)
- [Add a Plugin](/awaken/how-to/add-a-plugin/)
- [Use File Store](/awaken/how-to/use-file-store/)
- [Expose HTTP with SSE](/awaken/how-to/expose-http-sse/)
