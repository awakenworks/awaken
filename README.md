**English** | [中文](./docs/README.zh-CN.md)

# Tirea

**Build AI agents in Rust. Connect to any frontend. Scale to production.**

Define agents, tools, and state in Rust — then serve them to React, Next.js, CopilotKit, or other agents over AG-UI, AI SDK v6, A2A, and MCP from a single binary.

[![Crates.io](https://img.shields.io/crates/v/tirea.svg)](https://crates.io/crates/tirea)
[![docs.rs](https://img.shields.io/docsrs/tirea)](https://docs.rs/tirea)
[![License](https://img.shields.io/crates/l/tirea)](LICENSE-MIT)

## What you can build

Build components — tools, plugins, agents — then assemble them into an `AgentOs`. The OS is a container; agents are composed from the components you register.

```rust
// 1. Build tools — define args as a struct, schema is generated automatically
#[derive(Deserialize, JsonSchema)]
struct SearchFlightsArgs {
    from: String,
    to: String,
    date: String,
}

struct SearchFlightsTool;

#[async_trait]
impl TypedTool for SearchFlightsTool {
    type Args = SearchFlightsArgs;
    fn tool_id(&self) -> &str { "search_flights" }
    fn name(&self) -> &str { "Search Flights" }
    fn description(&self) -> &str { "Find flights between two cities." }

    async fn execute(&self, args: SearchFlightsArgs, _ctx: &ToolCallContext<'_>)
        -> Result<ToolResult, ToolError>
    {
        // ... call your flight API ...
        Ok(ToolResult::success("search_flights", json!({
            "flights": [{"airline": "UA", "price": 342, "from": args.from, "to": args.to}]
        })))
    }
}

// 2. Define agents — each agent selects which tools/skills/sub-agents it can use
let planner = AgentDefinition::with_id("planner", "deepseek-chat")
    .with_system_prompt("You are a travel planner. Use search tools to find options.")
    .with_max_rounds(8)
    .with_allowed_tools(vec!["search_flights".into(), "search_hotels".into()])
    .with_allowed_agents(vec!["researcher".into()]);

let researcher = AgentDefinition::with_id("researcher", "deepseek-chat")
    .with_system_prompt("You research destinations and provide summaries.")
    .with_max_rounds(4)
    .with_excluded_tools(vec!["delete_account".into()]);

// 3. Assemble into AgentOs — the container for all components
let os = AgentOsBuilder::new()
    .with_tools(tool_map_from_arc(vec![
        Arc::new(SearchFlightsTool),
        Arc::new(SearchHotelsTool),
    ]))
    .with_agent_spec(AgentDefinitionSpec::local(planner))
    .with_agent_spec(AgentDefinitionSpec::local(researcher))
    .with_agent_state_store(Arc::new(FileStore::new("./sessions")))
    .build()?;
```

Tools are registered globally on the OS. Each agent defines its own access policy — which tools, skills, and sub-agents it can use via `allowed_*` / `excluded_*` lists. At resolve time, the runtime filters the global tool pool down to what each agent is permitted to access.

Connect a React frontend with `useChat()`, a CopilotKit app via AG-UI, or another agent via A2A — no code changes needed.

## What makes it different

| What you get | Why it matters |
|---|---|
| **One server, four protocols** | UI protocols (AG-UI, AI SDK v6) and agent protocols (A2A, MCP) from the same binary. No separate deployments. |
| **State that survives concurrency** | Multiple agents can write to the same state simultaneously. CRDT fields (`GSet`, `ORSet`, `GCounter`) merge automatically — no locks, no conflicts. |
| **State scoped to its lifetime** | Mark state as Thread-scoped (persists forever), Run-scoped (reset each run), or ToolCall-scoped (gone after the tool finishes). No stale data leaking between runs. |
| **Compile-time plugin safety** | Plugins hook into 8 lifecycle phases. Wire a permission check to the wrong phase? Compiler catches it. |
| **Replay any conversation** | Every state change is an immutable patch. Replay them to reconstruct the exact state at any point. |
| **Rust performance** | No GC pauses. Low memory footprint. Native async concurrency. |

## Feature comparison

|  | Tirea | LangGraph | CrewAI | OpenAI Agents | Mastra | PydanticAI | Letta |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **Language** | Rust | Python | Python | Python/TS | TypeScript | Python | Python |
| **Multi-protocol server** | 4 (2 UI + 2 Agent) | ❌ | ❌ | ❌ | ❌ | AG-UI | REST |
| **Typed state** | ✅ derive macros | ◐ | ❌ | ❌ | ◐ | ◐ | ❌ |
| **Concurrent state safety** | ✅ CRDT | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **State lifecycle scoping** | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **State replay** | ✅ | ◐ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Plugin lifecycle** | 8 typed phases | ❌ | ❌ | Guardrails | ❌ | ❌ | ❌ |
| **Sub-agents** | ✅ | ✅ | ✅ | Handoffs | ◐ | ◐ | ✅ |
| **MCP support** | ✅ | Adapter | ✅ | ✅ | ✅ | ✅ | ❌ |
| **Human-in-the-loop** | ✅ | ✅ | ❌ | ✅ | ❌ | ✅ | ❌ |
| **Built-in general tools** | ❌ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |

✅ = native  ◐ = partial  ❌ = not available

## Quick start

### Prerequisites

- Rust toolchain from [`rust-toolchain.toml`](./rust-toolchain.toml)
- For frontend demos: Node.js 20+ and npm
- One model provider key (OpenAI, DeepSeek, Anthropic, etc.)

### Full-stack demo in 60 seconds

**React + AI SDK v6:**

```bash
cd examples/ai-sdk-starter && npm install
DEEPSEEK_API_KEY=<your-key> npm run dev
# Open http://localhost:3001
```

**Next.js + CopilotKit:**

```bash
cd examples/copilotkit-starter && npm install
cp .env.example .env.local
DEEPSEEK_API_KEY=<your-key> npm run setup:agent && npm run dev
# Open http://localhost:3000
```

### Server only

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package tirea-agentos-server -- --http-addr 127.0.0.1:8080
```

## How it works

```mermaid
graph LR
    subgraph "Your frontends"
        A["React app\n(AI SDK v6)"]
        B["Next.js app\n(CopilotKit / AG-UI)"]
        C["Another agent\n(A2A)"]
    end

    subgraph "tirea server (one binary)"
        GW["Protocol gateway\nUI: AG-UI · AI SDK\nAgent: A2A · MCP"]
        RT["Agent runtime\nLLM streaming · tool dispatch\nplugin lifecycle · context mgmt"]
        EXT["Extensions\npermission · skills · MCP\nreminder · observability"]
    end

    subgraph "Storage (pick one)"
        S1[(File)]
        S2[(PostgreSQL)]
    end

    A & B --> GW
    C --> GW
    GW --> RT
    RT --> EXT
    RT --> S1 & S2
```

## What you can do

### Connect to any frontend

One backend serves multiple protocols from the same binary. No code changes between them:

**UI protocols** — connect frontends to your agent:

- **AG-UI** (CopilotKit) — shared state, frontend actions, generative UI, human-in-the-loop
- **AI SDK v6** (Vercel) — `useChat()` streaming, canvas, thread history

**Agent protocols** — connect agents to each other:

- **A2A** — Google's agent-to-agent protocol; expose your agent as a peer service
- **MCP** — connect to MCP servers; external tools appear as native tools

**Endpoints** — start the server, then connect from any frontend:

```bash
cargo run --package tirea-agentos-server -- --http-addr 127.0.0.1:8080
```

| Protocol | Endpoint | Frontend |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/agents/:agent_id/runs` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/agents/:agent_id/runs` | CopilotKit `<CopilotKit>` |

**React + AI SDK v6** — minimal frontend:

```typescript
import { useChat } from "ai/react";

const { messages, input, handleSubmit } = useChat({
  api: "http://localhost:8080/v1/ai-sdk/agents/assistant/runs",
});
```

**Next.js + CopilotKit** — minimal frontend:

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:8080/v1/ag-ui/agents/assistant/runs">
  <YourApp />
</CopilotKit>
```

### Add tools

Define args as a typed struct — the JSON schema is generated automatically from `JsonSchema`, and args are deserialized for you:

```rust
#[derive(Deserialize, JsonSchema)]
struct MyToolArgs {
    query: String,
    limit: Option<u32>,
}

struct MyTool;

#[async_trait]
impl TypedTool for MyTool {
    type Args = MyToolArgs;
    fn tool_id(&self) -> &str { "my_tool" }
    fn name(&self) -> &str { "My Tool" }
    fn description(&self) -> &str { "Does something useful." }

    async fn execute(&self, args: MyToolArgs, ctx: &ToolCallContext<'_>)
        -> Result<ToolResult, ToolError>
    {
        // Read current state
        let state = ctx.snapshot_of::<MyState>().unwrap_or_default();

        // Do work
        let result = my_api_call(&args.query, args.limit).await?;

        // Return result (optionally with state updates)
        Ok(ToolResult::success("my_tool", json!(result)))
    }
}
```

### Built-in tools

Tirea ships with tools for sub-agents, background tasks, skills, UI rendering, and MCP integration. They're auto-registered when you enable the corresponding feature:

| Tool group | Tools | What they do |
|---|---|---|
| **Sub-agents** (core) | `agent_run`, `agent_stop`, `agent_output` | Launch, cancel, and read results from child agents running in parallel |
| **Background tasks** (core) | `task_status`, `task_cancel`, `task_output` | Monitor and manage long-running background operations |
| **Skills** (`skills` feature) | `skill`, `load_skill_resource`, `skill_script` | Discover, activate, and execute skill packages |
| **A2UI** (`a2ui` extension) | `render_a2ui` | Send declarative UI components to the frontend |
| **MCP** (`mcp` feature) | *dynamic* | Tools from connected MCP servers appear as native tools |

### Why plugins? Tools alone aren't enough

A tool is just a function the LLM can call. But a bare tool doesn't work in practice:

**The LLM doesn't know it exists.** The `agent_run` tool can launch sub-agents — but the LLM won't call it unless the system prompt lists which agents are available. That context injection isn't the tool's job. The `AgentToolsPlugin` handles it by injecting the agent catalog before each inference.

The same pattern applies everywhere: `SkillDiscoveryPlugin` injects the skill catalog so the LLM knows which skills to activate. `BackgroundTasksPlugin` injects task status so the LLM knows which tasks are running. `A2uiPlugin` injects the UI schema so the LLM knows how to render components.

**Cross-cutting concerns can't live in individual tools:**

| Problem | Why tools can't solve it | Plugin solution |
|---|---|---|
| Permission gating | Each tool would re-implement auth | `PermissionPlugin` — one `before_tool_execute` hook for all tools |
| Token budget | No single tool sees the full message history | `ContextPlugin` — truncates, summarizes, and caches across all messages |
| Stop conditions | No tool knows when to stop the agent loop | `StopPolicyPlugin` — evaluates max rounds, timeout, budget after each inference |
| Observability | Latency/token spans cross tool boundaries | `LLMMetryPlugin` — OpenTelemetry spans for the full LLM + tool pipeline |
| Persistent reminders | Reminders survive across turns, not tied to one tool | `ReminderPlugin` — injects reminders before each inference |
| Orphan recovery | Sub-agents can outlive their parent process | `AgentRecoveryPlugin` — detects and resumes orphaned runs on restart |

This is why every built-in tool ships with a companion plugin. The tool provides the capability; the plugin wires it into the LLM's awareness and the runtime's lifecycle.

### Require approval before dangerous actions

The built-in `PermissionPlugin` checks tool permissions via `PermissionPolicy` state (Allow/Deny/Ask per tool). Or write a custom plugin to gate any tool with full control over the suspension flow:

```rust
// In your plugin's before_tool_execute:
async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>)
    -> ActionSet<BeforeToolExecuteAction>
{
    let tool_id = ctx.tool_name().unwrap_or_default();
    let call_id = ctx.tool_call_id().unwrap_or_default();

    if tool_id == "delete_account" {
        let pending_id = format!("fc_{call_id}");
        let tool_args = ctx.tool_args().cloned().unwrap_or_default();
        let suspension = Suspension::new(&pending_id, "confirm_delete")
            .with_message("Requires admin approval");
        let pending = PendingToolCall::new(pending_id, "Confirm", tool_args);
        ActionSet::single(BeforeToolExecuteAction::Suspend(
            SuspendTicket::new(suspension, pending, ToolCallResumeMode::ReplayToolCall)
        ))
    } else {
        ActionSet::empty()
    }
}
```

The frontend receives the suspension event with the pending call. When the user approves, the runtime replays the original tool call — this time bypassing the permission check.

### Run sub-agents

Register multiple agents at build time, then the parent agent can orchestrate them via built-in tools:

```rust
let os = AgentOsBuilder::new()
    .with_agent_spec(AgentDefinitionSpec::local(planner_agent))
    .with_agent_spec(AgentDefinitionSpec::local(researcher_agent))
    .with_agent_spec(AgentDefinitionSpec::local(writer_agent))
    .build()?;
```

The runtime injects the available agent catalog into the system prompt. The parent agent launches sub-agents by ID — each runs in its own conversation thread:

- `agent_run` — launch a sub-agent by `agent_id`, or resume an existing one by `run_id`
- `agent_stop` — cancel a running sub-agent
- `agent_output` — read a sub-agent's results

Sub-agents must be pre-defined in the builder; the LLM selects from the registered catalog, not arbitrary configurations. Orphaned sub-agents are automatically recovered on parent restart.

### Manage state across conversations

State is typed and scoped to its intended lifetime:

```rust
#[derive(State)]
#[tirea(scope = "thread")]   // persists across all runs in this conversation
struct UserPreferences { /* ... */ }

#[derive(State)]
#[tirea(scope = "run")]      // reset at the start of each agent run
struct SearchProgress { /* ... */ }

#[derive(State)]
#[tirea(scope = "tool_call")] // exists only during a single tool execution
struct ToolWorkspace { /* ... */ }
```

Fields marked `#[tirea(lattice)]` use CRDT types that merge automatically when multiple agents write concurrently — no locks needed.

### Persist conversations

Swap storage backends without changing agent code:

| Backend | Use case |
|---|---|
| `FileStore` | Local development, single-server deployment |
| `PostgresStore` | Production with SQL queries and backups |
| `MemoryStore` | Tests |

### Extend with plugins

Plugins hook into 8 lifecycle phases. Use built-in plugins or write your own:

| Plugin | What it does | How to enable |
|---|---|---|
| **Context** | Token budget, message summarization, prompt caching | `ContextPlugin::for_model("claude-3-5-sonnet")` |
| **Stop Policy** | Terminate on max rounds, timeout, token budget, loop detection | `StopPolicyPlugin::new(conditions, specs)` |
| **Permission** | Allow/Deny/Ask per tool, human-in-the-loop suspension | `PermissionPlugin` + `ToolPolicyPlugin` |
| **Skills** | Discover and activate skill packages from filesystem | `skills` feature flag |
| **MCP** | Connect to MCP servers; tools appear as native tools | `mcp` feature flag |
| **Reminder** | Persistent system reminders that survive across turns | `ReminderPlugin::new()` |
| **Observability** | OpenTelemetry spans for LLM calls and tool executions | `LLMMetryPlugin::new(sink)` |
| **A2UI** | Declarative UI components sent to the frontend | `A2uiPlugin::with_catalog_id(url)` |
| **Agent Recovery** | Detect and resume orphaned sub-agent runs | Auto-wired with sub-agents |
| **Background Tasks** | Track and inject background task status | Auto-wired with task tools |

### Use any LLM provider

Powered by [genai](https://crates.io/crates/genai) — works with OpenAI, Anthropic, DeepSeek, Google, Mistral, Groq, Ollama, and more. Switch providers by changing one string:

```rust
model: "gpt-4o".into(),        // OpenAI
model: "deepseek-chat".into(), // DeepSeek
model: "claude-sonnet-4-20250514".into(), // Anthropic
```

## When to use Tirea

- You want a **Rust backend** for AI agents with compile-time safety
- You need to serve **multiple frontend protocols** from one server
- Your agents need to **share state concurrently** without coordination
- You need **auditable state history** and replay
- You're building for **production** — low memory, no GC, thousands of concurrent agents

## When NOT to use Tirea

- You need **built-in file/shell/web tools** out of the box — consider Dify, CrewAI
- You want a **visual workflow builder** — consider Dify, LangGraph Studio
- You want **Python** and rapid prototyping — consider LangGraph, PydanticAI
- You need **LLM-managed memory** (agent decides what to remember) — consider Letta

## Learning paths

| Goal | Start with | Then |
|---|---|---|
| Build your first agent | [First Agent tutorial](https://tirea-ai.github.io/tirea/tutorials/first-agent.html) | [Build an Agent guide](https://tirea-ai.github.io/tirea/how-to/build-an-agent.html) |
| See a full-stack app | [AI SDK starter](./examples/ai-sdk-starter/README.md) | [CopilotKit starter](./examples/copilotkit-starter/README.md) |
| Explore the API | [API reference](https://tirea-ai.github.io/tirea/reference/api.html) | `cargo doc --workspace --no-deps --open` |
| Contribute | [Contributing guide](./CONTRIBUTING.md) | [Capability matrix](https://tirea-ai.github.io/tirea/reference/capability-matrix.html) |

## Examples

| Example | What it shows |
|---|---|
| [ai-sdk-starter](./examples/ai-sdk-starter/) | React + AI SDK v6 — chat, canvas, shared state |
| [copilotkit-starter](./examples/copilotkit-starter/) | Next.js + CopilotKit — persisted threads, frontend actions |
| [travel-ui](./examples/travel-ui/) | Map canvas + approval-gated trip planning |
| [research-ui](./examples/research-ui/) | Resource collection + report writing with approval |

## Documentation

Full book: <https://tirea-ai.github.io/tirea/> · [API reference](https://docs.rs/tirea) · [Book source](./docs/book/src/)

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). Contributions welcome — especially:

- Built-in tool implementations (file read/write, search, shell execution)
- Tool-level concurrency safety flags
- Model fallback/degradation chains
- Token cost tracking
- Additional storage backends

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE).

[SECURITY.md](./SECURITY.md) · [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md)
