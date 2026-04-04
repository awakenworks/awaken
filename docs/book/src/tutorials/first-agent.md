# First Agent

## Goal

Run one agent end-to-end and confirm you receive a complete event stream.

## Prerequisites

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
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
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::VecEventSink;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::AgentSpec;
use awaken::registry::ModelEntry;
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
        .with_model("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant. Use the echo tool when asked.")
        .with_max_rounds(5);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model("gpt-4o-mini", ModelEntry {
            provider: "openai".into(),
            model_name: "gpt-4o-mini".into(),
        })
        .build()?;

    let request = RunRequest::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    let sink = Arc::new(VecEventSink::new());
    runtime.run(request, sink.clone()).await?;

    let events = sink.take();
    println!("events: {}", events.len());

    let finished = events
        .iter()
        .any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    println!("run_finish_seen: {}", finished);

    Ok(())
}
```

## 2. Run

```bash
cargo run
```

## 3. Verify

Expected output includes:

- `events: <n>` where `n > 0`
- `run_finish_seen: true`

The event stream will contain at least `RunStart`, one or more `TextDelta` or `ToolCallStart`/`ToolCallDone` events, and a final `RunFinish`.

## What You Created

This example creates an in-process `AgentRuntime` and runs one request immediately.

The core object is:

```rust,ignore
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("echo", Arc::new(EchoTool))
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model("gpt-4o-mini", ModelEntry {
        provider: "openai".into(),
        model_name: "gpt-4o-mini".into(),
    })
    .build()?;
```

After that, the normal entry point is:

```rust,ignore
let sink = Arc::new(VecEventSink::new());
runtime.run(request, sink.clone()).await?;
let events = sink.take();
```

Common usage patterns:

- one-shot CLI program: construct `RunRequest`, collect events via `VecEventSink`, print result
- application service: wrap `runtime.run(...)` inside your own app logic
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
- No `RunFinish` event: check that `with_max_rounds` is set high enough for the model to complete.

## Next

- [First Tool](./first-tool.md)
- [Events](../reference/events.md)
- [Expose HTTP SSE](../how-to/expose-http-sse.md)
