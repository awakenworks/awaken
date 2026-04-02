# Build an Agent

Use this when you need to assemble an agent with tools, persistence, and a provider into a running `AgentRuntime`.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- An `LlmExecutor` implementation (provider) available
- Familiarity with `AgentSpec` and `AgentRuntimeBuilder`

## Steps

1. Define the agent spec.

```rust,ignore
use awaken::{AgentSpec, AgentRuntimeBuilder};

let spec = AgentSpec::new("assistant")
    .with_model("anthropic/claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_max_rounds(10);
```

2. Register tools.

```rust,ignore
use std::sync::Arc;

let builder = AgentRuntimeBuilder::new()
    .with_agent_spec(spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_tool("calculator", Arc::new(CalculatorTool));
```

3. Register a provider.

```rust,ignore
let builder = builder
    .with_provider("anthropic", Arc::new(my_provider));
```

4. Attach persistence.

```rust,ignore
use awaken::stores::InMemoryStore;

let store = Arc::new(InMemoryStore::new());
let builder = builder.with_thread_run_store(store);
```

5. Build and validate.

```rust,ignore
let runtime = builder.build()?;
```

`build` resolves every registered agent and catches missing models, providers, or plugins at startup rather than at request time.

6. Execute a run.

```rust,ignore
use std::sync::Arc;
use awaken::RunRequest;
use awaken::contract::event_sink::VecEventSink;

let request = RunRequest::new("thread-1", vec![user_message])
    .with_agent_id("assistant");

let sink = Arc::new(VecEventSink::new());
let handle = runtime.run(request, sink.clone()).await?;
```

## Verify

Call the `/health` endpoint (if using the server feature) or inspect the `RunHandle` result to confirm the agent loop completed without errors.

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

- [Add a Tool](./add-a-tool.md)
- [Add a Plugin](./add-a-plugin.md)
- [Use File Store](./use-file-store.md)
- [Expose HTTP with SSE](./expose-http-sse.md)
