# First Agent

## Goal

Run one agent end-to-end and inspect the final result.

## Prerequisites

```toml
[dependencies]
awaken = { version = "0.4.0-dev" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

Set one model provider key before running:

```bash
# OpenAI-compatible models (for gpt-4o-mini)
export OPENAI_API_KEY=<your-key>

# Or DeepSeek models
export DEEPSEEK_API_KEY=<your-key>
```

## 1. Create `src/main.rs`

```rust,no_run
use std::sync::Arc;
use serde_json::{json, Value};
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::AgentSpec;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, RunRequest};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller")
            .with_parameters(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent_spec = AgentSpec::new("assistant")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant. Use the echo tool when asked.")
        .with_max_rounds(5);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model_binding("gpt-4o-mini", ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        })
        .build()?;

    let request = RunRequest::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    // This tutorial only needs the final result. Use run(..., sink) when
    // streaming events to SSE, WebSocket, protocol adapters, or tests.
    let result = runtime.run_to_completion(request).await?;
    println!("response: {}", result.response);
    println!("termination: {:?}", result.termination);

    Ok(())
}
```

## 2. Run

```bash
cargo run
```

## 3. Verify

Expected output includes:

- `response: ...`
- `termination: NaturalEnd`

## What You Created

This example creates an in-process `AgentRuntime` and runs one request immediately.

The core object is:

```rust,ignore
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("echo", Arc::new(EchoTool))
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding("gpt-4o-mini", ModelBinding {
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    })
    .build()?;
```

After that, the normal entry point is:

```rust,ignore
let result = runtime.run_to_completion(request).await?;
```

Common usage patterns:

- one-shot CLI program: construct `RunRequest`, call `runtime.run_to_completion(...)`, print the result
- application service: use `runtime.run(...)` with an `EventSink` when callers need streaming events
- HTTP server: store `Arc<AgentRuntime>` in app state and expose protocol routes

## Which Doc To Read Next

Use the next page based on what you want:

- add typed state and stateful tools: [First Tool](./first-tool.md)
- learn how events map to the agent loop: [Events](../reference/events.md)
- expose the agent over HTTP: [Expose HTTP SSE](../how-to/expose-http-sse.md)

## Common Errors

- Model/provider mismatch: `gpt-4o-mini` requires a compatible OpenAI-style provider setup.
- Missing key: set `OPENAI_API_KEY` or `DEEPSEEK_API_KEY` before `cargo run`.
- Tool not selected: ensure the prompt explicitly asks to use `echo`.
- Early termination: check that `with_max_rounds` is high enough for the model to complete.

## Next

- [First Tool](./first-tool.md)
- [Events](../reference/events.md)
- [Expose HTTP SSE](../how-to/expose-http-sse.md)
