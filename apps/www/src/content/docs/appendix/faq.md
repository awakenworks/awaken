---
title: "FAQ"
description: "Common questions about Awaken: when to use the runtime vs the server, choosing a protocol, providers and models, state, and operations."
---

## Which LLM providers are supported?

Any provider compatible with `genai`. This includes OpenAI, Anthropic, DeepSeek, Google Gemini, Ollama, and others. Register a provider executor under a provider ID, register a `ModelSpec { id, provider_id, upstream_model, .. }` carrying optional capability fields (context window, max output, modalities, knowledge cutoff) and pricing, and reference that stable `id` from `AgentSpec.model_id`.

## How do I add a new storage backend?

Implement the storage trait for the surface you need: `ThreadRunStore` for thread/run persistence, `ConfigStore` for runtime-managed config, `ProfileStore` for profile/shared state, and `MailboxStore` for HITL/background jobs. See `InMemoryStore`, `FileStore`, `PostgresStore`, `InMemoryMailboxStore`, and `SqliteMailboxStore` in `awaken-stores` for reference implementations.

## Can I use awaken without the server?

Yes. `AgentRuntime` is a standalone library type. Create a runtime, build a `RunActivation`, and call `runtime.run_to_completion(request)` when you only need the final result. Use `runtime.run(request, sink)` when your caller needs streaming events. The server crate (`awaken-server`) is an optional HTTP/SSE gateway layered on top.

## How do I run multiple agents?

Two approaches:

- **Delegates**: Define delegate agent IDs in the parent `AgentSpec`. The runtime exposes each delegate as an `AgentTool`; local delegates run in-process and endpoint-backed delegates run through an `ExecutionBackend`.
- **Handoff**: Use the handoff extension when one agent should take over the current thread instead of returning a delegate result.
- **A2A protocol**: Register or discover remote agents via `AgentRuntimeBuilder::with_remote_agents()` or endpoint-backed `AgentSpec` values. Remote agents are invoked over HTTP using the Agent-to-Agent protocol.

## What is the difference between Run scope and Thread scope?

- **Run scope**: State exists only for the duration of a single run. Cleared when the run ends. Use for transient data like step counters, token budgets, and per-run configuration.
- **Thread scope**: State persists across runs within the same thread. Use for conversation memory, user preferences, and accumulated context.

Scope is declared when defining a `StateKey`.

## How do I handle tool errors?

Return `ToolResult::error(tool_name, message)` from your tool's `execute` method. The runtime writes the error result back to the conversation as a tool response message and continues the inference loop. The LLM sees the error and can retry or adjust its approach. For fatal errors that should stop the run, return a `ToolError` instead.

## Can tools run in parallel?

Yes, but not through `AgentSpec`. The built-in resolver defaults to `SequentialToolExecutor`. Install `ParallelToolExecutor` with a custom resolver or `ResolvedAgent::with_tool_executor(...)` when the tools are independent and can share a frozen state snapshot safely.

## How do I debug a run that is stuck?

Check `RunStatus` in state (`__runtime.run_lifecycle` key). If `Waiting`, look at `__runtime.tool_call_states` for pending decisions. If `Running`, check if max_rounds or timeout was hit. Enable observability plugin to get per-phase tracing.

## How do I test without a real LLM?

Implement `LlmExecutor` with canned responses. See [Testing Strategy](/awaken/how-to/testing-strategy/) for patterns.

## What happens when parallel tools write to the same state key?

If you merge parallel state batches yourself, `MergeStrategy::Exclusive` conflicts when two batches write the same key, while `MergeStrategy::Commutative` allows deterministic merging for keys designed for concurrent writes. The default loop commits tool results in result order; custom parallel integrations should use the parallel merge helpers. See [State and Snapshot Model](/awaken/explanation/state-and-snapshot-model/).

## How do request transforms work?

Plugins register `InferenceRequestTransform` via the registrar. Transforms modify the inference request (system prompt, tools, parameters) before it reaches the LLM. Only active plugins' transforms apply. See [Plugin Internals](/awaken/explanation/plugin-internals/).

## Can I write a custom storage backend?

Yes. Implement `ThreadRunStore` for state persistence, `ConfigStore` for runtime config, `ProfileStore` for profile/shared state, and optionally `MailboxStore` for HITL/background jobs. File, PostgreSQL, memory, and SQLite mailbox implementations serve as references.

## How does context compaction work?

When `autocompact_threshold: Option<usize>` is set in `ContextWindowPolicy`, the `CompactionPlugin` monitors token usage. When the context exceeds that threshold, it finds a safe compaction boundary (where all tool call/result pairs are complete), summarizes older messages via LLM, and replaces them with a `<conversation-summary>` message. See [Optimize Context Window](/awaken/how-to/optimize-context-window/).

## How do I choose between AI SDK v6, AG-UI, A2A, MCP, and ACP protocols?

- **AI SDK v6**: Best for React frontends using Vercel AI SDK. Supports text streaming, tool calls, and state snapshots.
- **AG-UI**: Best for CopilotKit frontends. Supports generative UI components and agent collaboration.
- **A2A**: Best for agent-to-agent communication. Used for delegate agents and inter-service orchestration.
- **MCP HTTP**: Best when external MCP clients need to call Awaken tools over JSON-RPC with an `MCP-Session-Id` lifecycle.
- **ACP stdio**: Best when an Agent Client Protocol host launches Awaken as a local process and exchanges messages over stdin/stdout.

Choose based on the client ecosystem and wire protocol you need.
