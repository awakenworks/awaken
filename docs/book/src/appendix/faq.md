# FAQ

## Which LLM providers are supported?

Any provider compatible with genai. This includes OpenAI, Anthropic, DeepSeek, Google Gemini, Ollama, and others. Configure the provider via model ID string in `AgentSpec` or `AgentConfig`. The `GenaiExecutor` handles provider routing based on the model prefix.

## How do I add a new storage backend?

Implement the `ThreadRunStore` trait from `awaken-contract`. The trait requires methods for loading and saving threads, runs, and checkpoints. See `InMemoryStore` and `FileStore` in `awaken-stores` for reference implementations. Pass your store to `AgentRuntime::new().with_thread_run_store(store)` or `AgentRuntimeBuilder::new().with_thread_run_store(store)`.

## Can I use awaken without the server?

Yes. `AgentRuntime` is a standalone library type. Create a runtime, build a `RunRequest`, and call `runtime.run(request, sink)` directly. The server crate (`awaken-server`) is an optional HTTP/SSE gateway layered on top.

## How do I run multiple agents?

Two approaches:

- **Delegates**: Define delegate agent IDs in the parent `AgentSpec`. The runtime handles handoff via `ActiveAgentIdKey` at step boundaries.
- **A2A protocol**: Register remote agents via `AgentRuntimeBuilder::with_remote_agents()`. Remote agents are discovered and invoked over HTTP using the Agent-to-Agent protocol.

## What is the difference between Run scope and Thread scope?

- **Run scope**: State exists only for the duration of a single run. Cleared when the run ends. Use for transient data like step counters, token budgets, and per-run configuration.
- **Thread scope**: State persists across runs within the same thread. Use for conversation memory, user preferences, and accumulated context.

Scope is declared when defining a `StateKey`.

## How do I handle tool errors?

Return `ToolResult::Error` from your tool's `execute` method. The runtime writes the error result back to the conversation as a tool response message and continues the inference loop. The LLM sees the error and can retry or adjust its approach. For fatal errors that should stop the run, return a `RuntimeError` instead.

## Can tools run in parallel?

Yes. Configure `ToolExecutionMode` in the agent spec. When set to parallel mode, the runtime executes independent tool calls concurrently. Results are collected and merged before proceeding to the next inference step.
