# Awaken

**A production-ready AI agent runtime for Rust — type-safe state, phase-based execution, multi-protocol serving.**

Build production AI agents with compile-time guarantees, deterministic phase execution, and built-in observability. Define your agent logic once and serve it over AI SDK, AG-UI, A2A, and MCP from a single binary.

> **Note:** Awaken is a ground-up rewrite of [tirea](../../tree/tirea-0.5), redesigned for simplicity and production reliability. The tirea 0.5 codebase is archived on the [`tirea-0.5`](../../tree/tirea-0.5) branch for reference. Awaken is **not** backwards-compatible with tirea.

## Why Awaken

Building production AI agents in Rust means solving the same problems repeatedly: state management across conversation turns, tool execution with permission gates, multi-protocol serving, crash recovery, and observability. Awaken provides these as composable building blocks so you can focus on your agent's logic.

```rust
use awaken::prelude::*;

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(AgentSpec::new("assistant", "gpt-4o"))
    .with_tool("search", Arc::new(SearchTool))
    .with_plugin("permissions", Arc::new(PermissionPlugin::new(rules)))
    .build()?;

let result = runtime.run_streaming(request, sink).await?;
```

### What it gives you

- **Type-safe state** with scoping (thread / run / tool-call), merge strategies, and snapshot isolation
- **Phase-based execution** -- gather hooks, execute actions, commit state -- deterministic and replayable
- **7 built-in extensions** for permission, observability, MCP, skills, reminders, generative UI, and deferred tool loading
- **5 protocol adapters** -- AI SDK v6, AG-UI, A2A, ACP/MCP -- from one HTTP server
- **Durable mailbox** with lease-based claim, crash recovery, and human-in-the-loop support
- **Any LLM provider** via [genai](https://crates.io/crates/genai) (OpenAI, Anthropic, Gemini, DeepSeek, Ollama, etc.)
- **Zero `unsafe` code** across the entire workspace

## Architecture

```
awaken                   Facade crate with feature flags
  awaken-contract        Types, traits, state model, agent specs
  awaken-runtime         Phase execution engine, plugin system, agent loop
  awaken-server          Axum HTTP server, SSE, protocol adapters, mailbox
  awaken-stores          Memory, file, and PostgreSQL storage backends
  awaken-tool-pattern    Glob/regex tool matching
  awaken-ext-*           Extensions:
    permission           Allow / deny / ask policies with glob rules
    observability        OpenTelemetry GenAI semantic conventions
    mcp                  Model Context Protocol client
    skills               YAML-based skill package discovery
    reminder             Rule-based context message injection
    generative-ui        Server-driven UI components (A2UI)
    deferred-tools       Lazy tool loading with ToolSearch and DiscBeta model
```

## Quick start

```toml
[dependencies]
awaken = "0.1"
```

With minimal features (no server, no extensions):

```toml
[dependencies]
awaken = { version = "0.1", default-features = false }
```

### Minimal agent

```rust
use awaken::prelude::*;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec::new("chat", "gpt-4o"))
        .build()?;

    let request = RunRequest::new("thread-1", vec![Message::user("Hello!")]);
    let result = runtime.run(request).await?;
    println!("{}", result.text());
    Ok(())
}
```

### With tools and streaming

```rust
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(
        AgentSpec::new("assistant", "gpt-4o")
            .with_tool("calculator")
            .with_plugin("permissions"),
    )
    .with_tool("calculator", Arc::new(CalculatorTool))
    .with_plugin("permissions", Arc::new(
        PermissionPlugin::new(PolicySet::allow_all()),
    ))
    .build()?;

let sink = Arc::new(ChannelEventSink::new(tx));
let result = runtime.run_streaming(request, sink).await?;
```

### Multi-protocol server

```rust
use awaken::prelude::*;

let state = AppState::new(runtime, mailbox, store, resolver, ServerConfig::default());
serve(state).await?;
// Now serving:
//   POST /v1/threads/:id/runs    (native API)
//   POST /v1/ai-sdk/chat         (AI SDK v6)
//   POST /v1/ag-ui/run           (AG-UI / CopilotKit)
//   POST /v1/a2a/tasks/send      (Agent-to-Agent)
//   GET  /health                  (readiness probe)
//   GET  /health/live             (liveness probe)
//   GET  /metrics                 (Prometheus)
```

## Feature flags

All features enabled by default. Use `default-features = false` to opt out.

| Feature | Default | Description |
|---|:---:|---|
| `permission` | yes | Allow / deny / ask tool policies |
| `observability` | yes | OpenTelemetry tracing with GenAI semantic conventions |
| `mcp` | yes | Model Context Protocol client |
| `skills` | yes | YAML skill package discovery and activation |
| `reminder` | yes | Rule-based context message injection |
| `generative-ui` | yes | Server-driven UI components |
| `server` | yes | Multi-protocol HTTP server with mailbox |
| `full` | yes | All of the above |

## Examples

### Runtime examples

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken --example live_test
cargo run --package awaken --example multi_turn
cargo run --package awaken --example tool_call_live
```

### Full-stack server demos

```bash
# AI SDK v6 frontend + backend
cd examples/ai-sdk-starter && npm install && npm run dev

# CopilotKit / AG-UI frontend + backend
cd examples/copilotkit-starter && npm install && npm run dev
```

## Documentation

| Resource | Description |
|---|---|
| [`docs/book/`](./docs/book/) | User guide -- tutorials, how-to, reference, explanation |
| [`docs/adr/`](./docs/adr/) | 19 Architecture Decision Records |
| [`DEVELOPMENT.md`](./DEVELOPMENT.md) | Build, test, and contribution guide |

## Project status

Awaken is in active development. The API is not yet stable.

| Component | Status |
|---|---|
| Contract types | Stable -- breaking changes unlikely |
| Runtime engine | Maturing -- API may evolve |
| Server / protocols | Maturing -- AI SDK v6 and AG-UI well-tested |
| Storage backends | Stable (memory, file) / beta (PostgreSQL) |
| Extensions | Stable (permission, observability) / beta (others) |

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE).
