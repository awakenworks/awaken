# First Agent

## Goal

Run one agent end-to-end and confirm you receive a complete event stream.

## Prerequisites

```toml
[dependencies]
tirea = "0.3.0-alpha.4"
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
futures = "0.3"
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

```rust,ignore
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tirea::contracts::{AgentEvent, Message, RunOrigin, RunRequest, ToolCallContext};
use tirea::composition::{tool_map, AgentDefinition, AgentOsBuilder};

struct EchoTool;

#[async_trait]
impl tirea::contracts::Tool for EchoTool {
    fn descriptor(&self) -> tirea::contracts::ToolDescriptor {
        tirea::contracts::ToolDescriptor::new("echo", "Echo", "Echo input")
            .with_parameters(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<tirea::contracts::ToolResult, tirea::contracts::ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(tirea::contracts::ToolResult::success("echo", json!({ "text": text })))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let os = AgentOsBuilder::new()
        .with_tools(tool_map([EchoTool]))
        .with_agent(
            "assistant",
            AgentDefinition::with_id("assistant", "gpt-4o-mini")
                .with_system_prompt("You are a helpful assistant.")
                .with_allowed_tools(vec!["echo".to_string()]),
        )
        .build()?;

    let run = os
        .run_stream(RunRequest {
            agent_id: "assistant".to_string(),
            thread_id: Some("thread-1".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user("Say hello using the echo tool")],
            initial_decisions: vec![],
        })
        .await?;

    let events: Vec<_> = run.events.collect().await;
    println!("events: {}", events.len());

    let finished = events.iter().any(|e| matches!(e, AgentEvent::RunFinish { .. }));
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

## What You Created

This example creates an in-process `AgentOs` and runs one request immediately.

That means the agent is already usable in three ways:

1. Call `os.run_stream(...)` from your own Rust application code.
2. Start it as a local CLI-style binary with `cargo run`.
3. Mount the same `AgentOs` into an HTTP server so browser or remote clients can call it.

The tutorial shows option 1 and 2. Production integrations usually move to option 3.

## How To Use It After Creation

The object you actually use is:

```rust,ignore
let os = AgentOsBuilder::new()
    .with_tools(tool_map([EchoTool]))
    .with_agent(...)
    .build()?;
```

After that, the normal entrypoint is:

```rust,ignore
let run = os.run_stream(RunRequest { ... }).await?;
```

Common usage patterns:

- one-shot CLI program: construct `RunRequest`, collect events, print result
- application service: wrap `os.run_stream(...)` inside your own app logic
- HTTP server: store `Arc<AgentOs>` in app state and expose protocol routes

## How To Start It

For this tutorial, the binary entrypoint is `main()`, so startup is simply:

```bash
cargo run
```

If the agent is in a package inside a workspace, use:

```bash
cargo run -p your-package-name
```

If startup succeeds, your process:

- builds the tool registry
- registers the agent definition
- sends one `RunRequest`
- streams events until completion
- exits

So this tutorial is a runnable smoke test, not a long-lived server process.

## How To Turn It Into A Server

To expose the same agent over HTTP, keep the `AgentOsBuilder` wiring and move it into server state:

```rust,ignore
use std::sync::Arc;
use tirea_agentos_server::service::AppState;
use tirea_agentos_server::{http, protocol};

let agent_os = AgentOsBuilder::new()
    .with_tools(tool_map([EchoTool]))
    .with_agent(
        "assistant",
        AgentDefinition::with_id("assistant", "gpt-4o-mini")
            .with_system_prompt("You are a helpful assistant.")
            .with_allowed_tools(vec!["echo".to_string()]),
    )
    .build()?;

let app = axum::Router::new()
    .merge(http::health_routes())
    .merge(http::thread_routes())
    .merge(http::run_routes())
    .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
    .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
    .with_state(AppState {
        os: Arc::new(agent_os),
        read_store,
    });
```

Then run the server with an Axum listener instead of immediately calling `run_stream(...)`.

## Which Doc To Read Next

Use the next page based on what you want:

- keep calling the agent from Rust code: [Build an Agent](../how-to/build-an-agent.md)
- expose the agent to browsers or remote clients: [Expose HTTP SSE](../how-to/expose-http-sse.md)
- connect it to AI SDK or CopilotKit: [Integrate AI SDK Frontend](../how-to/integrate-ai-sdk-frontend.md) and [Integrate CopilotKit (AG-UI)](../how-to/integrate-copilotkit-ag-ui.md)

## Common Errors

- Model/provider mismatch: `gpt-4o-mini` requires a compatible OpenAI-style provider setup.
- Missing key: set `OPENAI_API_KEY` or `DEEPSEEK_API_KEY` before `cargo run`.
- Tool not selected: ensure prompt explicitly asks to use `echo`.

## Next

- [First Tool](./first-tool.md)
- [Build an Agent](../how-to/build-an-agent.md)
- [Expose HTTP SSE](../how-to/expose-http-sse.md)
- [Events Reference](../reference/events.md)
