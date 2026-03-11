# Introduction

**Tirea** is an immutable state-driven agent framework built in Rust. It combines typed JSON state management with an agent loop, providing full traceability of state changes, replay capability, and component isolation.

## Crate Overview

| Crate | Description |
|-------|-------------|
| `tirea-state` | Core library: typed state, JSON patches, apply, conflict detection |
| `tirea-state-derive` | Proc-macro for `#[derive(State)]` |
| `tirea-contract` | Shared contracts: thread/events/tools/plugins/runtime/storage/protocol |
| `tirea-agentos` | Agent runtime: inference engine, tool execution, orchestration, plugin composition |
| `tirea-extension-*` | Plugins: permission, reminder, observability, skills, MCP |
| `tirea-protocol-ag-ui` | AG-UI protocol adapters |
| `tirea-protocol-ai-sdk-v6` | Vercel AI SDK v6 protocol adapters |
| `tirea-store-adapters` | Storage adapters: memory/file/postgres/nats-buffered |
| `tirea-agentos-server` | HTTP/SSE/NATS gateway server |
| `tirea` | Umbrella crate that re-exports core modules |

## Architecture

```text
┌─────────────────────────────────────────────────────┐
│  Application Layer                                    │
│  - Register tools, define agents, call run_stream    │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  AgentOs                                             │
│  - Prepare run, execute phases, emit events          │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Thread + State Engine                               │
│  - Thread history, RunContext delta, apply_patch     │
└─────────────────────────────────────────────────────┘
```

## Core Principle

All state transitions follow a deterministic, pure-function model:

```text
State' = apply_patch(State, Patch)
```

- Same `(State, Patch)` always produces the same `State'`
- `apply_patch` never mutates its input
- Full history enables replay to any point in time

## What's in This Book

- **Tutorials** — Learn by building a first agent and first tool
- **How-to** — Task-focused implementation guides for integration and operations
- **Reference** — API, protocol, config, and schema lookup pages
- **Explanation** — Architecture and design rationale

## Recommended Reading Path

If you are new to the repository, use this order:

1. Read [First Agent](./tutorials/first-agent.md) to see the smallest runnable flow.
2. Read [First Tool](./tutorials/first-tool.md) to understand state reads and writes.
3. Read [Typed Tool Reference](./reference/typed-tool.md) before writing production tools.
4. Use [Build an Agent](./how-to/build-an-agent.md) and [Add a Tool](./how-to/add-a-tool.md) as implementation checklists.
5. Return to [Architecture](./explanation/architecture.md) and [Run Lifecycle and Phases](./explanation/run-lifecycle-and-phases.md) when you need the full execution model.

## Repository Map

These paths matter most when you move from docs into code:

| Path | Purpose |
|------|---------|
| `crates/tirea-contract/` | Core runtime contracts: tools, events, state/runtime interfaces |
| `crates/tirea-agentos/` | Agent runtime: inference engine, tool execution, orchestration, extensions |
| `crates/tirea-agentos-server/` | HTTP/SSE/NATS server surfaces |
| `crates/tirea-state/` | Immutable state patch/apply/conflict engine |
| `examples/src/` | Small backend examples for tools, agents, and state |
| `examples/ai-sdk-starter/` | Shortest browser-facing end-to-end example |
| `examples/copilotkit-starter/` | Richer end-to-end UI example with approvals and persistence |
| `docs/book/src/` | This documentation source |

For the full Rust API documentation, see the [API Reference](./reference/api.md).
