# FAQ

## Which LLM providers are supported?

Any provider compatible with genai. This includes OpenAI, Anthropic, DeepSeek, Google Gemini, Ollama, and others. Configure the provider via model ID string in `AgentSpec` or `AgentConfig`. The `GenaiExecutor` handles provider routing based on the model prefix.

## How do I add a new storage backend?

Implement the `ThreadRunStore` trait from `awaken-contract`. The trait requires methods for loading and saving threads, runs, and checkpoints. See `InMemoryStore` and `FileStore` in `awaken-stores` for reference implementations. Pass your store to `AgentRuntime::new().with_thread_run_store(store)` or `AgentRuntimeBuilder::new().with_thread_run_store(store)`.

## Can I use awaken without the server?

Yes. `AgentRuntime` is a standalone library type. Create a runtime, build a `RunRequest`, and call `runtime.run_to_completion(request)` when you only need the final result. Use `runtime.run(request, sink)` when your caller needs streaming events. The server crate (`awaken-server`) is an optional HTTP/SSE gateway layered on top.

## How do I run multiple agents?

Two approaches:

- **Delegates**: Define delegate agent IDs in the parent `AgentSpec`. The runtime handles handoff via `ActiveAgentIdKey` at step boundaries.
- **A2A protocol**: Register remote agents via `AgentRuntimeBuilder::with_remote_agents()`. Remote agents are discovered and invoked over HTTP using the Agent-to-Agent protocol.

## What is the difference between Run scope and Thread scope?

- **Run scope**: State exists only for the duration of a single run. Cleared when the run ends. Use for transient data like step counters, token budgets, and per-run configuration.
- **Thread scope**: State persists across runs within the same thread. Use for conversation memory, user preferences, and accumulated context.

Scope is declared when defining a `StateKey`.

## How do I handle tool errors?

Return `ToolResult::error(tool_name, message)` from your tool's `execute` method. The runtime writes the error result back to the conversation as a tool response message and continues the inference loop. The LLM sees the error and can retry or adjust its approach. For fatal errors that should stop the run, return a `ToolError` instead.

## Can tools run in parallel?

Yes. Configure `ToolExecutionMode` in the agent spec. When set to parallel mode, the runtime executes independent tool calls concurrently. Results are collected and merged before proceeding to the next inference step.

## How do I debug a run that is stuck?

Check `RunStatus` in state (`__runtime.run_lifecycle` key). If `Waiting`, look at `__runtime.tool_call_states` for pending decisions. If `Running`, check if max_rounds or timeout was hit. Enable observability plugin to get per-phase tracing.

## How do I test without a real LLM?

Implement `LlmExecutor` with canned responses. See [Testing Strategy](../how-to/testing-strategy.md) for patterns.

## What happens when parallel tools write to the same state key?

If the key uses `MergeStrategy::Exclusive`, the merge fails and hooks fall back to serial execution. Use `MergeStrategy::Commutative` for keys that support concurrent writes. See [State and Snapshot Model](../explanation/state-and-snapshot-model.md).

## How do request transforms work?

Plugins register `InferenceRequestTransform` via the registrar. Transforms modify the inference request (system prompt, tools, parameters) before it reaches the LLM. Only active plugins' transforms apply. See [Plugin Internals](../explanation/plugin-internals.md).

## Can I write a custom storage backend?

Yes. Implement `ThreadRunStore` (for state persistence) and optionally `MailboxStore` (for HITL). See trait definitions in `awaken-stores`. File and PostgreSQL implementations serve as references.

## How does context compaction work?

When `autocompact_threshold: Option<usize>` is set in `ContextWindowPolicy`, the `CompactionPlugin` monitors token usage. When the context exceeds that threshold, it finds a safe compaction boundary (where all tool call/result pairs are complete), summarizes older messages via LLM, and replaces them with a `<conversation-summary>` message. See [Optimize Context Window](../how-to/optimize-context-window.md).

## How do I choose between AI SDK v6, AG-UI, and A2A protocols?

- **AI SDK v6**: Best for React frontends using Vercel AI SDK. Supports text streaming, tool calls, and state snapshots.
- **AG-UI**: Best for CopilotKit frontends. Supports generative UI components and agent collaboration.
- **A2A**: Best for agent-to-agent communication. Used for delegate agents and inter-service orchestration.

All three run over HTTP/SSE. Choose based on your frontend framework.
