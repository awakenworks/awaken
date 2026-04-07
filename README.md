# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io](https://img.shields.io/crates/v/awaken-agent.svg?label=crates.io)](https://crates.io/crates/awaken-agent) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange)

Production AI agent runtime for Rust — type-safe state, multi-protocol serving, plugin extensibility.

Published on crates.io as `awaken-agent`; keep importing it in Rust as `awaken`.

Docs: [GitHub Pages](https://awakenworks.github.io/awaken/) | [Chinese docs](https://awakenworks.github.io/awaken/zh-CN/)

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken demo — tool call + LLM streaming" width="800">
</p>

## 30-second mental model

1. **Tools** — typed functions your agent can call; JSON schema is generated at compile time
2. **Agents** — each agent has a system prompt, a model, and a set of allowed tools; the LLM drives orchestration through natural language — no predefined graphs
3. **State** — typed and scoped (`thread` / `run`), with merge strategies for safe concurrent writes and immutable snapshots
4. **Plugins** — lifecycle hooks for permissions, observability, context management, skills, MCP, and more

Your agent picks tools, calls them, reads and updates state, and repeats — all orchestrated by the runtime through 8 typed phases. Every state change is committed atomically after the gather phase.

## Try it in 5 minutes

Prerequisites:

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1.1" }
tokio = { version = "1.51.0", features = ["full"] }
async-trait = "0.1.89"
serde_json = "1.0.149"
```

```bash
export OPENAI_API_KEY=<your-key>
```

Copy this into `src/main.rs` and run `cargo run`:

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

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
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

    let finished = events.iter().any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    println!("run_finish_seen: {}", finished);

    Ok(())
}
```

## Serve over any protocol

Start the built-in server and connect from React, Next.js, or another agent — no code changes:

```rust,no_run
use awaken::prelude::*;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use std::sync::Arc;

let store = Arc::new(InMemoryStore::new());
let runtime = Arc::new(runtime);
let mailbox = Arc::new(Mailbox::new(
    runtime.clone(),
    Arc::new(InMemoryMailboxStore::new()),
    "default-consumer".into(),
    MailboxConfig::default(),
));

let state = AppState::new(
    runtime.clone(),
    mailbox,
    store,
    runtime.resolver_arc(),
    ServerConfig::default(),
);
serve(state).await?;
```

#### Frontend protocols

| Protocol | Endpoint | Frontend |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/chat` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/run` | CopilotKit `<CopilotKit>` |
| A2A | `POST /v1/a2a/message:send` | Other agents |

**React + AI SDK v6:**

```typescript
import { useChat } from "ai/react";

const { messages, input, handleSubmit } = useChat({
  api: "http://localhost:3000/v1/ai-sdk/chat",
});
```

**Next.js + CopilotKit:**

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui/run">
  <YourApp />
</CopilotKit>
```

## Built-in plugins

All features are enabled by default via the `full` feature. Use `default-features = false` to opt out.

| Plugin | What it does | Feature flag |
|---|---|---|
| **Permission** | Firewall-style tool access control with Deny/Allow/Ask rules, glob/regex matching, and HITL suspension via mailbox. | `permission` |
| **Reminder** | Injects system or conversation-level context messages when tool calls match configured patterns. | `reminder` |
| **Observability** | OpenTelemetry telemetry aligned with GenAI Semantic Conventions; supports OTLP, file, and in-memory export. | `observability` |
| **MCP** | Connects to external MCP servers and registers their tools as native Awaken tools. | `mcp` |
| **Skills** | Discovers skill packages and injects a catalog before inference so the LLM can activate skills on demand. | `skills` |
| **Generative UI** | Streams declarative UI components to frontends via the A2UI protocol. | `generative-ui` |

`awaken-ext-deferred-tools` provides lazy tool loading; add it as a direct dependency if needed — it is not included in the `full` feature.

## Why Awaken

- One backend serves every frontend protocol — React (AI SDK v6), Next.js (AG-UI), other agents (A2A), and tool servers (MCP) from the same binary.
- The LLM orchestrates — define each agent's identity and tool access; no hand-coded DAGs or state machines.
- Type-safe state with compile-time checks, scoped lifetimes, and merge strategies for safe concurrent writes.
- Production-ready: circuit breaker, exponential backoff, graceful shutdown, Prometheus metrics, and health probes included.
- Zero `unsafe` — the entire workspace forbids `unsafe` and relies on the Rust compiler for memory safety.

## When to use Awaken

- You want a **Rust backend** for AI agents with compile-time safety
- You need to serve **multiple frontend or agent protocols** from one backend
- Your tools need to **safely share state** during concurrent execution
- You need **auditable thread history**, checkpoints, and resumable control paths
- You are comfortable wiring your own tools, providers, and model registry instead of relying on batteries-included defaults

## When NOT to use Awaken

- You need **built-in file/shell/web tools** out of the box — consider OpenAI Agents SDK, Dify, or CrewAI
- You want a **visual workflow builder** — consider Dify, LangGraph Studio
- You want **Python** and rapid prototyping — consider LangGraph, AG2, PydanticAI
- You need a **stable, slow-moving surface area** more than an evolving runtime platform
- You need **LLM-managed memory** (agent decides what to remember) — consider Letta

## Architecture

Awaken is split into three runtime layers. `awaken-contract` defines the shared contracts: agent specs, model/provider specs, tools, events, transport traits, and the typed state model. `awaken-runtime` resolves an `AgentSpec` into a `ResolvedAgent`, builds an `ExecutionEnv` from plugins, executes the phase loop, and manages active runs plus external control such as cancellation and HITL decisions. `awaken-server` exposes that same runtime through HTTP routes, SSE replay, mailbox-backed background execution, and protocol adapters for AI SDK v6, AG-UI, A2A, and MCP.

Around those layers sit storage and extensions. `awaken-stores` provides memory, file, and PostgreSQL backends for threads and runs. `awaken-ext-*` crates extend the runtime at phase and tool boundaries.

```text
awaken                   Facade crate with feature flags
├─ awaken-contract       Contracts: specs, tools, events, transport, state model
├─ awaken-runtime        Resolver, phase engine, loop runner, runtime control
├─ awaken-server         Routes, mailbox, SSE transport, protocol adapters
├─ awaken-stores         Memory, file, and PostgreSQL persistence
├─ awaken-tool-pattern   Glob/regex matching used by extensions
└─ awaken-ext-*          Optional runtime extensions
```

## Examples and learning paths

| Example | What it shows |
|---|---|
| [`live_test`](./crates/awaken/examples/live_test.rs) | Basic LLM integration |
| [`multi_turn`](./crates/awaken/examples/multi_turn.rs) | Multi-turn with persistent threads |
| [`tool_call_live`](./crates/awaken/examples/tool_call_live.rs) | Tool calling with calculator |
| [`ai-sdk-starter`](./examples/ai-sdk-starter/) | React + AI SDK v6 full-stack |
| [`copilotkit-starter`](./examples/copilotkit-starter/) | Next.js + CopilotKit full-stack |

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken-agent --example multi_turn

cd examples/ai-sdk-starter && npm install && npm run dev
```

| Goal | Start with | Then |
|---|---|---|
| Build your first agent | [Get Started](https://awakenworks.github.io/awaken/get-started.html) | [Build Agents](https://awakenworks.github.io/awaken/build-agents.html) |
| See a full-stack app | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| Explore the API | [Reference docs](https://awakenworks.github.io/awaken/reference/overview.html) | `cargo doc --workspace --no-deps --open` |
| Understand the runtime | [Architecture](https://awakenworks.github.io/awaken/explanation/architecture.html) | [Run Lifecycle and Phases](https://awakenworks.github.io/awaken/explanation/run-lifecycle-and-phases.html) |
| Migrate from tirea | [Migration guide](https://awakenworks.github.io/awaken/appendix/migration-from-tirea.html) | |

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) and [DEVELOPMENT.md](./DEVELOPMENT.md) for setup details.

[Good first issues](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) are a great entry point. Quick contribution flow: fork → create a branch → write tests → open a PR.

Areas where contributions are especially welcome:

- Additional storage backends (Redis, SQLite)
- Built-in tool implementations (file read/write, web search)
- Token cost tracking and budget enforcement
- Model fallback/degradation chains

Join the conversation on [GitHub Discussions](https://github.com/AwakenWorks/awaken/discussions).

---

Awaken is a ground-up rewrite of [tirea](../../tree/tirea-0.5); it is not backwards-compatible. The tirea 0.5 codebase is archived on the [`tirea-0.5`](../../tree/tirea-0.5) branch.

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE).
