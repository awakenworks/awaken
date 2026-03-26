# Introduction

**Awaken** is a modular AI agent runtime framework built in Rust. It provides phase-based execution with snapshot isolation and deterministic replay, a typed state engine with key scoping (thread/run) and merge strategies (exclusive/commutative), a plugin lifecycle system for extensibility, and a multi-protocol server supporting AI SDK v6, AG-UI, A2A, and ACP.

## Crate Overview

| Crate | Description |
|-------|-------------|
| `awaken-contract` | Core contracts: types, traits, state model, agent specs |
| `awaken-runtime` | Execution engine: phase loop, plugin system, agent loop, builder |
| `awaken-server` | HTTP/SSE gateway with protocol adapters |
| `awaken-stores` | Storage backends: memory, file, postgres |
| `awaken-tool-pattern` | Glob/regex tool matching for permission and reminder rules |
| `awaken-ext-permission` | Permission plugin with allow/deny/ask policies |
| `awaken-ext-observability` | OpenTelemetry-based LLM and tool call tracing |
| `awaken-ext-mcp` | Model Context Protocol client integration |
| `awaken-ext-skills` | Skill package discovery and activation |
| `awaken-ext-reminder` | Declarative reminder rules triggered after tool execution |
| `awaken-ext-generative-ui` | Declarative UI components (A2UI protocol) |
| `awaken` | Facade crate that re-exports core modules |

## Architecture

```text
┌─────────────────────────────────────────────────────┐
│  Application Layer                                    │
│  - Register tools, define agents, call run_stream    │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  AgentRuntime                                        │
│  - Resolve agents, execute phases, emit events       │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Thread + State Engine                               │
│  - Thread history, snapshot isolation, state keys    │
└─────────────────────────────────────────────────────┘
```

## Core Principle

All state access follows snapshot isolation. Phase hooks see an immutable snapshot; mutations are collected in a MutationBatch and applied atomically after convergence.

## What's in This Book

- **Tutorials** — Learn by building a first agent and first tool
- **How-to** — Task-focused implementation guides for integration and operations
- **Reference** — API, protocol, config, and schema lookup pages
- **Explanation** — Architecture and design rationale

## Recommended Reading Path

If you are new to the repository, use this order:

1. Read [First Agent](./tutorials/first-agent.md) to see the smallest runnable flow.
2. Read [First Tool](./tutorials/first-tool.md) to understand state reads and writes.
3. Read [Tool Trait](./reference/tool-trait.md) before writing production tools.
4. Use [Build an Agent](./how-to/build-an-agent.md) and [Add a Tool](./how-to/add-a-tool.md) as implementation checklists.
5. Return to [Architecture](./explanation/architecture.md) and [Run Lifecycle and Phases](./explanation/run-lifecycle-and-phases.md) when you need the full execution model.

## Repository Map

These paths matter most when you move from docs into code:

| Path | Purpose |
|------|---------|
| `crates/awaken-contract/` | Core contracts: tools, events, state interfaces |
| `crates/awaken-runtime/` | Agent runtime: execution engine, plugins, builder |
| `crates/awaken-server/` | HTTP/SSE server surfaces |
| `crates/awaken-stores/` | Storage backends |
| `crates/awaken/examples/` | Small runtime examples |
| `examples/src/` | Full-stack server examples |
| `docs/book/src/` | This documentation source |
