# Introduction

**Awaken** is a modular AI agent runtime framework built in Rust. It provides phase-based execution with snapshot isolation and deterministic replay, a typed state engine with key scoping (`thread` / `run`) and merge strategies (`exclusive` / `commutative`), a plugin lifecycle system for extensibility, and a multi-protocol server surface supporting AI SDK v6, AG-UI, A2A, and MCP over HTTP and stdio, plus ACP over stdio.

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
| `awaken-ext-deferred-tools` | Deferred tool loading with probabilistic promotion |
| `awaken` | Facade crate that re-exports core modules |

## Architecture

```text
Application code
  registers tools / models / providers / plugins / agent specs
        |
        v
AgentRuntime
  resolves AgentSpec -> ResolvedAgent
  builds ExecutionEnv from plugins
  runs the phase loop and exposes cancel / decision control
        |
        v
Server + storage surfaces
  HTTP routes, SSE replay, mailbox, protocol adapters, thread/run persistence
```

## Core Principle

All state access follows snapshot isolation. Phase hooks see an immutable snapshot; mutations are collected in a MutationBatch and applied atomically after convergence.

## What's in This Book

- **Get Started** — build a working mental model with the smallest runnable flows
- **Build Agents** — add tools, plugins, MCP, skills, reminders, handoff, and UI capabilities
- **Serve & Integrate** — expose HTTP endpoints and wire AI SDK or CopilotKit frontends
- **State & Storage** — choose persistence, context shaping, and state lookup patterns
- **Operate** — harden runtime behavior with observability, permissions, progress reporting, and tests
- **Reference** — API, protocol, config, and schema lookup pages
- **Architecture** — runtime layering, phase execution, and design tradeoffs

## Recommended Reading Path

If you are new to the repository, use this order:

1. Start with [Get Started](./get-started.md) and complete [First Agent](./tutorials/first-agent.md).
2. Move to [Build Agents](./build-agents.md) when you are ready to add tools and plugins.
3. Use [Serve & Integrate](./serve-and-integrate.md) when the runtime needs to talk to HTTP clients or frontends.
4. Use [State & Storage](./state-and-storage.md) and [Operate](./operate.md) as you move from demos to production behavior.
5. Keep [Reference Overview](./reference/overview.md) and [Architecture](./explanation/architecture.md) open when you need exact contracts or runtime internals.

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
