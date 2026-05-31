# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io awaken](https://img.shields.io/crates/v/awaken.svg?label=awaken)](https://crates.io/crates/awaken) [![crates.io awaken-agent](https://img.shields.io/crates/v/awaken-agent.svg?label=awaken-agent)](https://crates.io/crates/awaken-agent) [![Changelog](https://img.shields.io/badge/changelog-current-informational)](./CHANGELOG.md) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.93-orange)

Build agent capabilities once in Rust, tune behavior live, and serve every client from the same runtime. Awaken is a production AI agent backend where tools, state, and plugins stay in code; agents, models, and prompts move through online config; and server mode adds protocols, durable orchestration, trace/eval, and the admin console. Use runtime mode when your application owns I/O; use server mode when the agent surface must be shared.

Docs: [Awaken docs](https://awakenworks.github.io/awaken) · [中文文档](https://awakenworks.github.io/awaken/zh-cn) · [Changelog](./CHANGELOG.md). MSRV: Rust 1.93. The published crate is `awaken`; `awaken-agent` is a compatibility republish from when the project shipped under that name (same import path either way).

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken demo — tool call + LLM streaming" width="800">
</p>

## Choose your programming mode

Awaken separates the **agent execution loop** from the **service control plane**. The runtime owns agent reasoning, tool selection, typed phases, state commits, and direct run APIs. The server owns service orchestration: HTTP/SSE, protocol adapters, mailbox dispatch, managed config, audit/restore, and the admin-console workflow.

| Mode | Start with | You own | Awaken provides |
|---|---|---|---|
| **Runtime development** | `awaken` / `awaken-runtime` | HTTP/UI/job scheduling, auth, config storage, concrete tools/providers/stores | Direct run APIs, streaming events, 9-phase loop, typed tools/state, cancellation and HITL primitives |
| **Server development** | `awaken-server` + `awaken-stores` | Deployment, tenant/auth policy, registered tools/providers, store selection | HTTP resources, SSE replay, AI SDK/AG-UI/A2A/MCP/ACP adapters, mailbox orchestration, `/v1/config/*`, registry snapshots, admin console |

Start with runtime mode when you are building a Rust application or test harness and want direct control over I/O. Use server mode when multiple clients, operators, or background workers need the same agent surface with durable runs and online configuration.

Runtime mode means in-process library use inside a standard Rust program. It is not a `no_std` or Tokio-free embedded-device target: `awaken-runtime` currently depends on Tokio for timers, timeouts, async coordination, and HTTP/provider execution.

Current IO/runtime boundary:

| Component | Tokio / IO profile |
|---|---|
| `awaken-runtime` | Requires Tokio. The phase loop is in-process, but the crate includes `genai` / `reqwest` provider paths and Tokio-based timeout/retry/background-task machinery. |
| `awaken-runtime-contract` / `awaken-server-contract` | Contract/type surfaces only; useful for API boundaries, but still target `std` Rust crates, not `no_std` embedded targets. |
| Permission, Reminder, Deferred Tools, Generative UI | Mostly in-process policy/state/event logic, but they depend on the runtime contract/runtime stack and therefore inherit the Tokio/std assumption. |
| MCP and Skills | IO-capable: MCP uses network/stdio/process transports; Skills can read skill packages from disk, spawn configured commands, and optionally register MCP tools. |
| Observability | In-memory recording is local; OTLP/file/metrics exporters introduce external IO. |
| Stores and Server | Explicit IO layers: memory/file/PostgreSQL/SQLite/NATS stores, HTTP routes, SSE, mailbox workers, and protocol replay. |

## Why Awaken is different

- **One agent backend, many clients.** AI SDK v6, AG-UI / CopilotKit, A2A, MCP, and ACP are adapters over the same runtime event stream and run model instead of separate agent implementations per protocol.
- **Managed config is the control plane.** Providers, `ModelSpec` entries, model pools, agents, tools, plugin sections, and MCP servers can be validated and published as registry snapshots while the server stays up.
- **Provider and model operations are first-class.** `ModelSpec` carries addressing, capability bounds, modalities, knowledge cutoff, and pricing; model pools add failover; provider discovery can fill safe capability fields without trusting arbitrary custom adapters by default.
- **Streaming is treated as production I/O.** Mid-stream interruptions and idle stalls trigger typed recovery plans, honor `Retry-After`, and can use `StreamCheckpointStore` for recovery across process restarts. ([details](https://awakenworks.github.io/awaken/how-to/recover-streaming-llms))
- **State and tool execution are typed and replayable.** Typed `StateKey`s with merge strategies, generated JSON Schema for `TypedTool`, pure `ToolGate` interception, and atomic phase commits make concurrent tools auditable instead of hidden side effects.
- **Operational boundaries are explicit.** Parent-child threads, HITL mailbox suspension, cancellation, audit log restore, redacted secrets, and admin config validation are part of the runtime/server contract.

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

The high-leverage tuning surface includes system prompts, tool description overrides, system reminders, ToolSearch/deferred-tool policy, skill catalog and activation metadata, plugin sections, and explicit sub-agent delegates. These are behavior/config changes, not arbitrary code execution: ToolSearch is implemented by `awaken-ext-deferred-tools`; skills are catalog-injected and activated through the `skill` tool; sub-agents are explicit `AgentSpec.delegates` exposed as delegate tools. A separate SkillSearch or AgentSearch tool is not currently shipped.

When the server is wired with audit and versioned-registry stores, config writes are traceable through record revisions and audit restore, published runtime registry snapshots are immutable, and durable runs carry a `resolution_id` so resume and replay can reselect the same published graph. Manual "pin this arbitrary config version as production" is a server/versioned-registry concern, not a generic runtime API.

The runtime drives 9 typed phases per round, including a pure `ToolGate` before tool execution. State mutations are batched and committed atomically.

## Quickstart: runtime mode

Prerequisites: Rust 1.93+ and an OpenAI-compatible API key.

```toml
[dependencies]
awaken = { git = "https://github.com/AwakenWorks/awaken" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

These snippets follow the current main-branch API. Use the
[0.5 to 0.6 migration guide](https://awakenworks.github.io/awaken/how-to/migrate-to-0-6/)
when upgrading from the published `0.5` line.

```bash
export OPENAI_API_KEY=<your-key>
```

`src/main.rs` (run with `cargo run`):

```rust,no_run
use awaken::engine::GenaiExecutor;
use awaken::prelude::*;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

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

## Server mode: serve over any protocol

Put the runtime behind server transports and the same agent serves React, Next.js,
A2A peers, MCP clients, and ACP hosts — no agent-code changes. Server mode adds
the service layer around the runtime:

- HTTP resources for threads, runs, config, capabilities, and health.
- Streaming and replay over SSE plus protocol adapters for AI SDK v6, AG-UI,
  A2A, MCP, and ACP.
- Durable mailbox dispatch for resumable, cancellable, interruptible, and
  HITL-blocked runs.
- Managed config APIs and admin-console workflows for validating, previewing,
  publishing, restoring, and auditing agent/model/provider/plugin config.
- Optional server modules for canonical events, trace persistence, eval
  datasets/runs, system discovery, runtime stats, and run summaries.

Three pieces sit between the runtime and the wire:

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

#### Protocol adapters

| Protocol | Route / transport | Typical client |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/chat` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/run` | CopilotKit `<CopilotKit>` |
| A2A | `POST /v1/a2a/message:send` | Other agents |
| MCP | `POST /v1/mcp` | JSON-RPC 2.0 clients |
| ACP | stdio via `serve_stdio` | Agent Client Protocol hosts |

The optional admin console reads `/v1/capabilities` and writes through `/v1/config/*` to manage agents, models, providers, MCP servers, and plugin config sections. It also includes a server-managed Admin Assistant on `/v1/admin/assistant/runs`: the assistant can read platform capabilities, create/publish AgentSpecs, draft without publishing, and validate drafts with locked admin-only tools that never appear in the normal tool registry. It unlocks automatically when the first provider-backed model is configured. Saved config changes publish a new registry snapshot that takes effect on the next `/v1/runs` request. OpenAI-compatible providers (including BigModel) use the `openai` adapter with their own `base_url`; non-secret extras go in `ProviderSpec.adapter_options`.

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

Wire a `ConfigStore` into `ServerState` and the SPA in [`apps/admin-console`](./apps/admin-console/) becomes a browser control plane over the same config API (reads `VITE_BACKEND_URL` for the server base URL). Operators can validate drafts, tune prompts/tool descriptions/reminders/deferred-tool policy/skills/delegates, publish registry snapshots, test providers, inspect runtime health, preview agent changes before saving, and restore prior config versions from the audit log. The dashboard emphasizes live operational signals — awaiting HITL decisions, running/queued workload, provider/MCP health, rolling-window inference/error/token stats, and recent audit activity.

The screenshots below are static documentation captures made with sample API data. A running admin console reads its values from the configured backend APIs.

<table>
  <tr>
    <td width="33%"><a href="./docs/assets/admin-console/01-dashboard.png"><img src="./docs/assets/admin-console/01-dashboard.png" alt="Dashboard — Live workload, Agent activity, Recent activity timeline, Health card, System metadata" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/02-agent-editor.png"><img src="./docs/assets/admin-console/02-agent-editor.png" alt="Agent editor with model and system prompt fields plus draft preview" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/03-agents-list.png"><img src="./docs/assets/admin-console/03-agents-list.png" alt="Agents list with filters, plugin metadata, and inference statistics" /></a></td>
  </tr>
  <tr>
    <td align="center"><sub><b>Dashboard</b><br/>Workload · Health · Recent audit</sub></td>
    <td align="center"><sub><b>Agent Editor</b><br/>Validate · Preview · Save</sub></td>
    <td align="center"><sub><b>Agents</b><br/>Filters · Plugins · Runtime stats</sub></td>
  </tr>
</table>

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
- You need to serve **AI SDK, CopilotKit, A2A, MCP, and/or ACP** from a single backend.
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
├─ awaken-runtime-contract Runtime contracts: specs, tools, events, state, commit coordinator
├─ awaken-server-contract  Server/store contracts: queries, scoped stores, mailbox/outbox, staged commits
├─ awaken-runtime        Resolver, phase engine, loop runner, runtime control
├─ awaken-server         HTTP routes, SSE replay, mailbox dispatch, protocol adapters
├─ awaken-stores         Thread + run + config + mailbox + profile stores (memory / file / PostgreSQL / SQLite / NATS)
├─ awaken-tool-pattern   Glob/regex matching used by extensions
└─ awaken-ext-*          Optional plugins (permission, reminder, observability, mcp, skills, generative-ui, deferred-tools)
```

`awaken-server` is the service orchestration and control-plane layer: HTTP, SSE replay, mailbox background runs, protocol adapters, managed config APIs, and the admin-console workflow. It calls `awaken-runtime`, the in-process execution core that resolves an `AgentSpec` into a local `ResolvedAgent` or backend-backed execution plan, drives the 9-phase loop, and manages cancellation + HITL decisions.

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
# Optional: seed sample agents/tools for demos
AWAKEN_SEED_PROFILE=demo AWAKEN_STORAGE_DIR=./target/admin-sessions cargo run -p ai-sdk-starter-agent

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

## Contributing

Setup in [CONTRIBUTING.md](./CONTRIBUTING.md) and [DEVELOPMENT.md](./DEVELOPMENT.md). [Good first issues](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) is the entry-point label. Especially welcome: additional store backends (Redis, S3, etc.), built-in file/web/shell tools, token-cost budgeting, model fallback chains. Conversation: [GitHub Discussions](https://github.com/AwakenWorks/awaken/discussions).

## Acknowledgement

The `awaken` crate name on crates.io was transferred from [@brayniac](https://github.com/brayniac), who maintained an earlier crate under the same name. Versions `0.1`–`0.3` of `awaken` on crates.io belong to that earlier project; this codebase resumes the line that previously shipped as `awaken-agent 0.2.x` and starts at `0.4.0` to skip past those versions. Thank you.


## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE).
