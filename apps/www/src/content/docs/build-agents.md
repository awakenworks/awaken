---
title: "Develop Agents"
description: "Implement executable agent capability in Rust: runtime setup, tools, plugins, state, and controlled sub-agent calls."
---

This path is for the developer side of Awaken: implement the executable
capability that a runtime can safely run. Keep code focused on tools, plugins,
state, providers, stores, and explicit execution boundaries. Move behavior that
operators should change later into managed config, then use
[Tune & Operate](/awaken/operate/) for the browser and REST workflows.

## Purpose

Build Agents explains **why a capability belongs in code** before it becomes
operator-tunable config. This keeps expensive or security-sensitive choices in
reviewed Rust while still giving operators a clear path to tune prompts, tools,
permissions, and governance later.

## Design choices to make up front

| Need | Put it here | Why this is better |
|---|---|---|
| Long-running work that should not block the current turn | Background task or background agent | The run can wait, resume, or receive inbox events without hiding work inside an untracked thread. |
| Specialist work that should return a bounded result | Delegate or sub-agent tool | The parent receives a normal tool result and can decide whether to continue, retry, or summarize. |
| A different agent should take over the same conversation | Agent handoff | The active agent changes at a safe step boundary while thread history and state remain continuous. |
| Agents need to talk while they remain independent | `send_message` / mailbox-backed communication | Live child messages and durable cross-thread messages use explicit receipts instead of ad-hoc shared memory. |
| A child agent needs state from its parent | Typed `StateKey` seed/export policy | State contracts are visible, persistent keys are intentional, and failed transfers surface as errors. |
| Threads, runs, config, or profiles need durability | File/Postgres/NATS stores and a commit coordinator | Storage boundaries are wired during development so later tuning has reliable config, mailbox, and history data. |
| A plugin needs to inject model context | `PhaseContext` + `StateCommand` + `AddContextMessage` | Hooks read snapshots and return commands; the runtime owns throttling, ordering, injection, and commits. |

## Development surfaces to keep visible

When documenting or implementing a code-owned capability, point readers to the
executable examples or tests that pin the surface:

| Capability | Development surface | Code reference |
|---|---|---|
| Runtime assembly | `AgentRuntimeBuilder`, providers, models, tools, commit coordinator | `crates/awaken-doctest/examples/http_app_builder.rs`, `crates/awaken-runtime/src/builder.rs` |
| Custom providers | `LlmExecutor`, `ProviderExecutorFactory`, `ModelPoolSpec` | `crates/awaken/tests/readme_quickstart.rs`, `crates/awaken-server/tests/config_api.rs` |
| Plugin context injection | `PhaseHook`, `PhaseContext`, `StateCommand`, `AddContextMessage`, tool filters | `crates/awaken-doctest/examples/plugin_registrar.rs`, `crates/awaken-runtime/src/agent/state/loop_actions.rs` |
| Background work | `BackgroundTaskManager`, `BackgroundTaskPlugin`, `SendMessageTool`, `CancelTaskTool` | `crates/awaken-runtime/tests/background_task_lifecycle.rs`, `crates/awaken-runtime/src/extensions/background/` |
| Sub-agent as a tool | `run_child_agent`, `ChildAgentParams`, `BackendRunResult.state` export | `crates/awaken-runtime/tests/child_agent_seed.rs`, `crates/awaken-runtime/src/child_agent/mod.rs` |
| Store boundaries | `ThreadRunStore`, `ConfigStore`, `ProfileStore`, `MailboxStore`, `VersionedRegistryStore` | `crates/awaken-doctest/examples/thread_store_trait.rs`, `crates/awaken-stores/tests/` |
| MCP integration | `McpToolRegistryManager`, custom transport, sampling handler | `crates/awaken-ext-mcp/tests/mcp_tests.rs`, `crates/awaken-ext-mcp/src/transport.rs` |
| Observability and eval | `MetricsSink`, `TraceStore`, `RuntimeReplayer`, `JudgeConfig` | `crates/awaken-ext-observability/tests/`, `crates/awaken-eval/tests/eval_integration.rs` |

## Recommended order

1. [Build an Agent](/awaken/how-to/build-an-agent/) to define the runtime, model registry, and agent spec.
2. [Add a Tool](/awaken/how-to/add-a-tool/) and [Add a Plugin](/awaken/how-to/add-a-plugin/) to extend behavior safely.
3. [State & Storage](/awaken/state-and-storage/), [State Management](/awaken/explanation/state-management/), and the store guides wire runtime state, config, profile, and mailbox storage boundaries.
4. [Multi-Agent Patterns](/awaken/explanation/multi-agent-patterns/) to choose delegation, background agents, handoff, or messaging.
5. [HITL and Mailbox](/awaken/explanation/hitl-and-mailbox/) when you need mailbox routing, waiting runs, HITL, or distributed dispatch.
6. [Use Agent Handoff](/awaken/how-to/use-agent-handoff/) when one agent should take over the current thread.
7. [Invoke a Sub-Agent from a Tool](/awaken/how-to/invoke-sub-agent-from-tool/) when custom tool code needs a controlled child run and explicit state passing.
8. [Use Generative UI](/awaken/how-to/use-generative-ui/) when an agent should stream UI documents alongside text.
9. [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) marks the boundary between code-owned capability and operator-owned tuning.

## Keep nearby

- [Tool Trait](/awaken/reference/tool-trait/) for exact tool contracts.
- [Tool and Plugin Boundary](/awaken/explanation/tool-and-plugin-boundary/) for extension design decisions.
- [Architecture](/awaken/explanation/architecture/) when you need the full runtime model.
