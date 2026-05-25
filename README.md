# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io awaken](https://img.shields.io/crates/v/awaken.svg?label=awaken)](https://crates.io/crates/awaken) [![crates.io awaken-agent](https://img.shields.io/crates/v/awaken-agent.svg?label=awaken-agent)](https://crates.io/crates/awaken-agent) [![Changelog](https://img.shields.io/badge/changelog-0.5-informational)](./CHANGELOG.md) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.93-orange)

A Rust agent runtime that serves AI SDK, CopilotKit, A2A, and MCP from the same backend, recovers from mid-stream LLM failures, and treats configuration as the control plane.

Docs: [Awaken docs](https://awakenworks.github.io/awaken) · [中文文档](https://awakenworks.github.io/awaken/zh-cn) · [Changelog](./CHANGELOG.md). MSRV: Rust 1.93. The published crate is `awaken`; `awaken-agent` is a compatibility republish from when the project shipped under that name (same import path either way).

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken demo — tool call + LLM streaming" width="800">
</p>

## What you get in 0.5

- **One backend, four protocols.** AI SDK v6, AG-UI / CopilotKit, A2A, and MCP all dispatch through the same `/v1/runs` — see [protocol table](#frontend-protocols).
- **Streaming survives transient failures.** Mid-stream interruptions and idle stalls trigger one of four typed recovery plans; `Retry-After` is honored and a `StreamCheckpointStore` extends recovery across process restarts. ([details](https://awakenworks.github.io/awaken/how-to/recover-streaming-llms))
- **Parent-child threads.** Sub-agent runs create child threads; deletion is explicit (`reject` / `detach` / `cascade`). Filters + cursors on `/v1/threads` make hierarchical UIs straightforward.
- **Secrets stay redacted.** `ProviderSpec.api_key` and bearer tokens use `RedactedString` — `Debug`/`Display` print `***`, the buffer zeroizes on drop, the JSON wire format is unchanged.
- **Type-safe state and tools.** Typed `StateKey`s with merge strategies, generated JSON Schema for `TypedTool`, atomic batched commits after each phase. `unsafe_code = "forbid"` workspace-wide.

## Mental model

Awaken separates **code you write once** from **config you tune continuously**.

**Code (Rust):**

1. **Tools** — implement `Tool` directly, or `TypedTool` with `schemars`-generated JSON Schema. This is the only part of an agent that you compile.
2. **State** — typed run/thread state plus persistent profile and shared state for cross-thread or cross-agent coordination.
3. **Plugins** — lifecycle hooks for permission, observability, context management, skills, MCP, generative UI.

**Config (declarative, hot-swappable):**

4. **Providers + Models** — credentials, adapters, and the `ModelSpec` entries agents reference (addressing + capabilities + pricing).
5. **Agents** — system prompt, `model_id`, allowed/excluded tools. The LLM orchestrates through natural language; there is no DAG.
6. **Skills** — discoverable packages that scope what tools and instructions an agent activates for a given task (`SkillSpec.allowed_tools`).

Tools are written once and stay stable. Models, agents, and skills are tuned **at runtime** through `/v1/config/*` or the [Admin Console](https://awakenworks.github.io/awaken/reference/admin-console/) — Validate → Save → preview-chat → adjust. That feedback loop *is* the optimization workflow.

The runtime drives 9 typed phases per round, including a pure `ToolGate` before tool execution. State mutations are batched and committed atomically.

## Quickstart

Prerequisites: Rust 1.93+ and an OpenAI-compatible API key.

```toml
[dependencies]
awaken = "0.5"
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

```bash
export OPENAI_API_KEY=<your-key>
```

`src/main.rs` (run with `cargo run`):

```rust,no_run
use awaken::engine::GenaiExecutor;
use awaken::prelude::*;
use async_trait::async_trait;
use serde_json::json;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller").with_parameters(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }))
    }

    async fn execute(&self, args: JsonValue, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(
            AgentSpec::new("assistant")
                .with_model_id("gpt-4o-mini")
                .with_system_prompt("You are helpful. Use the echo tool when asked.")
                .with_max_rounds(5),
        )
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
        .build()?;

    let request = RunActivation::new("thread-1", vec![Message::user("Say hello using the echo tool")])
        .with_agent_id("assistant");

    let result = runtime.run_to_completion(request).await?;
    println!("{}", result.response);
    Ok(())
}
```

Use `runtime.run(request, sink)` instead of `run_to_completion` when you need
to stream events to SSE, WebSocket, protocol adapters, or tests. For a
longer end-to-end example (multi-turn + persistent threads), see
[`crates/awaken/examples/multi_turn.rs`](./crates/awaken/examples/multi_turn.rs).

The quickstart path is covered without network access:

```bash
cargo test -p awaken --test readme_quickstart        # offline (scripted provider)
OPENAI_API_KEY=<key> cargo test -p awaken --test readme_live_provider -- --ignored  # live
```

## Serve over any protocol

Wrap the runtime in HTTP and the same agent serves React, Next.js, A2A peers,
and MCP clients — no code changes. Three pieces sit between the runtime and
the wire:

- `ThreadRunStore` — persists thread messages + run records (memory / file /
  PostgreSQL implementations ship in `awaken-stores`).
- `Mailbox` — durable run queue that decouples HTTP requests from agent
  execution (also pluggable: memory / SQLite / NATS).
- `ServerState` — the dependency bundle every route handler reads from.

```rust,no_run
use awaken::prelude::*;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};

let store = Arc::new(InMemoryStore::new());
let runtime = Arc::new(runtime);  // from the Quickstart above
let mailbox = Arc::new(Mailbox::new(
    runtime.clone(),
    Arc::new(InMemoryMailboxStore::new()),
    store.clone(),
    "default-consumer".into(),
    MailboxConfig::default(),
));
let state = ServerState::new(
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
| MCP | `POST /v1/mcp` | JSON-RPC 2.0 |

The optional admin console reads `/v1/capabilities` and writes through `/v1/config/*` to manage agents, models, providers, MCP servers, and plugin config sections. Saved changes publish a new registry snapshot that takes effect on the next `/v1/runs` request. OpenAI-compatible providers (including BigModel) use the `openai` adapter with their own `base_url`; non-secret extras go in `ProviderSpec.adapter_options`.

**React + AI SDK v6:**

```typescript
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const { messages, sendMessage } = useChat({
  transport: new DefaultChatTransport({
    api: "http://localhost:3000/v1/ai-sdk/chat",
  }),
});
```

**Next.js + CopilotKit:**

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui/run">
  <YourApp />
</CopilotKit>
```

#### Admin Console

Wire a `ConfigStore` into `ServerState` and the SPA in [`apps/admin-console`](./apps/admin-console/) gives you a browser UI for the same API (reads `VITE_BACKEND_URL` for the server base URL). It's a React 19 + Vite app on the Awaken brand: JetBrains Mono throughout, achromatic surfaces, sharp 2px corners; light by default with a Light/Dark/System cycle toggle in the topbar (also auto-switches to dark on `prefers-color-scheme: dark`). The dashboard surfaces live signal — **awaiting-decision** (HITL) gets warn-tinted hero treatment, plus rolling-window aggregates from the observability registry (inferences, errors, tokens, suspensions/handoffs/delegations) with top-N agents and tools — so an operator sees what needs attention in one glance.

<table>
  <tr>
    <td width="33%"><a href="./docs/assets/admin-console/01-dashboard.png"><img src="./docs/assets/admin-console/01-dashboard.png" alt="Dashboard — Live workload (awaiting decision, running, queued), Agent activity (inferences, errors, tokens, coordination, top agents and tools), Recent activity timeline, Health card, System metadata" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/02-agent-editor.png"><img src="./docs/assets/admin-console/02-agent-editor.png" alt="Agent editor with tab strip, model + system prompt fields, and right-side draft sandbox" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/03-agents-list.png"><img src="./docs/assets/admin-console/03-agents-list.png" alt="Agents list with filter chips, plugin pills, and an Inferences column (registry window)" /></a></td>
  </tr>
  <tr>
    <td align="center"><sub><b>Dashboard</b><br/>Workload · Agent activity · Health · Recent audit</sub></td>
    <td align="center"><sub><b>Agent Editor</b><br/>Tabbed UI · Draft sandbox · Save</sub></td>
    <td align="center"><sub><b>Agents</b><br/>Filter chips · Plugin pills · Inferences (window)</sub></td>
  </tr>
  <tr>
    <td colspan="3"><a href="./docs/assets/admin-console/04-dark-dashboard.png"><img src="./docs/assets/admin-console/04-dark-dashboard.png" alt="Dashboard in dark mode — same content, achromatic canvas with off-white text, mono everywhere, 2px sharp corners" /></a></td>
  </tr>
  <tr>
    <td colspan="3" align="center"><sub><b>Dark mode</b> · Light/Dark/System cycle toggle in the topbar, persisted per-browser</sub></td>
  </tr>
</table>

**⌘K command palette** — jump to any page, agent, or tool from anywhere:

![Command palette](./docs/assets/admin-console/cmdk.png)

Full surface tour: [Admin Console reference](https://awakenworks.github.io/awaken/reference/admin-console) · operator manual: [Use the Admin Console](https://awakenworks.github.io/awaken/how-to/use-admin-console).

## Built-in plugins

The facade `full` feature pulls in the plugins below. Use
`default-features = false` to opt out. `awaken-ext-deferred-tools` is not
re-exported by the facade and is added as a direct dependency.

| Plugin | What it does | Feature flag |
|---|---|---|
| **Permission** | Allow/Deny/Ask rules with glob and regex matching on tool name and arguments. Deny beats Allow beats Ask; Ask suspends the run via the mailbox for HITL. | `permission` |
| **Reminder** | Injects system or conversation-level context messages when a tool call matches a configured pattern. | `reminder` |
| **Observability** | OpenTelemetry traces and metrics aligned with the GenAI Semantic Conventions; OTLP, file, and in-memory exports. | `observability` |
| **MCP** | Connects to external MCP servers and registers their tools as native Awaken tools. | `mcp` |
| **Skills** | Discovers skill packages and injects a catalog before inference so the LLM can activate skills on demand. | `skills` |
| **Generative UI** | Streams declarative UI components to frontends via A2UI, JSON Render, and OpenUI Lang integrations. | `generative-ui` |
| **Deferred Tools** | Hides large tool schemas behind a `ToolSearch` step and re-defers idle tools using a discounted Beta usage model. | direct crate: `awaken-ext-deferred-tools` |

Write your own with `ToolGateHook` (pure gate decisions) or `BeforeToolExecute` (execution-time hooks) — same trait signatures the built-ins use.

## When this fits

- You want a **Rust backend** for AI agents with compile-time guarantees.
- You need to serve **AI SDK, CopilotKit, A2A, and/or MCP** from a single backend.
- Tools need to **share state safely** during concurrent execution, and runs need **auditable history** with checkpoints and resume.
- You're comfortable registering your own tools and providers instead of relying on batteries-included defaults.

## When it doesn't

- You need **built-in file/shell/web tools** out of the box — consider OpenAI Agents SDK, Dify, or CrewAI.
- You want a **visual workflow builder** — consider Dify or LangGraph Studio.
- You want **Python** and rapid prototyping — consider LangGraph, AG2, or PydanticAI.
- You need an **LLM-managed memory** subsystem where the agent decides what to remember — consider Letta.

## Architecture

Three core layers sit under the facade, with stores and extensions branching off:

```text
awaken                   Facade crate with feature flags
├─ awaken-contract       Shared contracts: specs, tools, events, transport, state model
├─ awaken-runtime        Resolver, phase engine, loop runner, runtime control
├─ awaken-server         HTTP routes, SSE replay, mailbox dispatch, protocol adapters
├─ awaken-stores         Thread + run + config + mailbox + profile stores (memory / file / PostgreSQL / SQLite / NATS)
├─ awaken-tool-pattern   Glob/regex matching used by extensions
└─ awaken-ext-*          Optional plugins (permission, reminder, observability, mcp, skills, generative-ui, deferred-tools)
```

`awaken-runtime` resolves an `AgentSpec` into a `ResolvedExecution`, drives the 9-phase loop, and manages cancellation + HITL decisions. `awaken-server` wraps that runtime in HTTP routes and the four protocol adapters.

## Examples and learning paths

| Example | What it shows |
|---|---|
| [`live_test`](./crates/awaken/examples/live_test.rs) | Basic LLM integration |
| [`multi_turn`](./crates/awaken/examples/multi_turn.rs) | Multi-turn with persistent threads |
| [`tool_call_live`](./crates/awaken/examples/tool_call_live.rs) | Tool calling with calculator |
| [`ai-sdk-starter`](./examples/ai-sdk-starter/) | React + AI SDK v6 full-stack |
| [`copilotkit-starter`](./examples/copilotkit-starter/) | Next.js + CopilotKit full-stack |
| [`openui-chat`](./examples/openui-chat/) | OpenUI Lang chat frontend |
| [`admin-console`](./apps/admin-console/) | Config API management UI |

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken --example multi_turn

pnpm install && pnpm --filter awaken-ai-sdk-starter dev

# Terminal 1: starter backend for admin console
AWAKEN_STORAGE_DIR=./target/admin-sessions cargo run -p ai-sdk-starter-agent

# Terminal 2: admin console
pnpm install
pnpm --filter awaken-admin-console dev
```

| Goal | Start with | Then |
|---|---|---|
| Build your first agent | [Get Started](https://awakenworks.github.io/awaken/get-started) | [Build Agents](https://awakenworks.github.io/awaken/build-agents) |
| See a full-stack app | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| Manage runtime config | [Admin Console](./apps/admin-console/) | [Configure Agent Behavior](https://awakenworks.github.io/awaken/how-to/configure-agent-behavior) |
| Explore the API | [Reference docs](https://awakenworks.github.io/awaken/reference/overview) | `cargo doc --workspace --no-deps --open` |
| Understand the runtime | [Architecture](https://awakenworks.github.io/awaken/explanation/architecture) | [Run Lifecycle and Phases](https://awakenworks.github.io/awaken/explanation/run-lifecycle-and-phases) |
| Migrate from tirea | [Migration guide](https://awakenworks.github.io/awaken/appendix/migration-from-tirea) | |

## Contributing

Setup in [CONTRIBUTING.md](./CONTRIBUTING.md) and [DEVELOPMENT.md](./DEVELOPMENT.md). [Good first issues](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) is the entry-point label. Especially welcome: additional store backends (Redis, S3, etc.), built-in file/web/shell tools, token-cost budgeting, model fallback chains. Conversation: [GitHub Discussions](https://github.com/AwakenWorks/awaken/discussions).

## Acknowledgement

The `awaken` crate name on crates.io was transferred from [@brayniac](https://github.com/brayniac), who maintained an earlier crate under the same name. Versions `0.1`–`0.3` of `awaken` on crates.io belong to that earlier project; this codebase resumes the line that previously shipped as `awaken-agent 0.2.x` and starts at `0.4.0` to skip past those versions. Thank you.

Awaken is also a ground-up rewrite of [tirea](../../tree/tirea-0.5) and is not backwards-compatible with it. The tirea 0.5 codebase remains archived on the [`tirea-0.5`](../../tree/tirea-0.5) branch.

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE).
