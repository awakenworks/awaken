---
title: "Overview"
description: "The awaken crate is the public facade for the Awaken agent framework. It re-exports runtime contracts, server contracts, runtime APIs, stores, and extensions so downstream code can start with one dependency."
---

The `awaken` crate is the public facade for the Awaken agent framework. It
re-exports runtime contracts, server contracts, runtime APIs, stores, and
extensions so downstream code can start with one dependency.

## Module re-exports

| Facade path | Source crate | Contents |
|---|---|---|
| `awaken::contract` | `awaken-runtime-contract` | Runtime-facing tools, events, messages, suspension, lifecycle, commit coordinator |
| `awaken::server_contract` | `awaken-server-contract` | Server/store-facing storage queries, scoped stores, staged commits |
| `awaken::model` | `awaken-runtime-contract` | Phase, EffectSpec, ScheduledActionSpec, JsonValue |
| `awaken::registry_spec` | `awaken-runtime-contract` | AgentSpec, ModelSpec, ProviderSpec, McpServerSpec, PluginConfigKey |
| `awaken::state` | `awaken-runtime-contract` + `awaken-runtime` | StateKey, StateMap, Snapshot, StateStore, MutationBatch |
| `awaken::agent` | `awaken-runtime` | Agent configuration and state |
| `awaken::builder` | `awaken-runtime` | AgentRuntimeBuilder, BuildError |
| `awaken::context` | `awaken-runtime` | PhaseContext |
| `awaken::engine` | `awaken-runtime` | LLM engine abstraction |
| `awaken::execution` | `awaken-runtime` | ExecutionEnv |
| `awaken::extensions` | `awaken-runtime` | Built-in extension infrastructure |
| `awaken::loop_runner` | `awaken-runtime` | Agent loop runner |
| `awaken::phase` | `awaken-runtime` | PhaseRuntime, PhaseHook |
| `awaken::plugins` | `awaken-runtime` | Plugin, PluginDescriptor, PluginRegistrar |
| `awaken::policies` | `awaken-runtime` | Context window and retry policies |
| `awaken::registry` | `awaken-runtime` | AgentResolver, ResolvedAgent, ResolvedBackendAgent |
| `awaken::runtime` | `awaken-runtime` | AgentRuntime |
| `awaken::stores` | `awaken-stores` | Memory, file, PostgreSQL, and SQLite-backed store implementations |

## Feature-gated modules

| Facade path | Feature flag | Source crate |
|---|---|---|
| `awaken::ext_permission` | `permission` | `awaken-ext-permission` |
| `awaken::ext_observability` | `observability` | `awaken-ext-observability` |
| `awaken::ext_mcp` | `mcp` | `awaken-ext-mcp` |
| `awaken::ext_skills` | `skills` | `awaken-ext-skills` |
| `awaken::ext_generative_ui` | `generative-ui` | `awaken-ext-generative-ui` |
| `awaken::ext_reminder` | `reminder` | `awaken-ext-reminder` |
| `awaken::server` | `server` | `awaken-server` |

## Root-level re-exports

The following types are re-exported at the crate root for convenience:

**From `awaken-runtime-contract`:**
`AgentSpec`, `EffectSpec`, `FailedScheduledActions`, `JsonValue`, `KeyScope`,
`MergeStrategy`, `PendingScheduledActions`, `PersistedState`, `Phase`,
`PluginConfigKey`, `ScheduledActionSpec`, `Snapshot`, `StateError`, `StateKey`,
`StateKeyOptions`, `StateMap`, `TypedEffect`, `UnknownKeyPolicy`

**From `awaken-runtime`:**
`AgentResolver`, `AgentRuntime`, `AgentRuntimeBuilder`, `BuildError`,
`CancellationToken`, `CommitEvent`, `CommitHook`, `DEFAULT_MAX_PHASE_ROUNDS`,
`ExecutionEnv`, `MutationBatch`, `PhaseContext`, `PhaseHook`, `PhaseRuntime`,
`Plugin`, `PluginDescriptor`, `PluginRegistrar`, `ResolvedAgent`, `RunActivation`,
`RuntimeError`, `StateCommand`, `StateStore`, `ToolGateHook`,
`TypedEffectHandler`, `TypedScheduledActionHandler`

## Feature flags

| Flag | Default | Description |
|---|---|---|
| `permission` | yes | Tool-level permission gating (HITL) |
| `observability` | yes | Tracing and metrics integration |
| `mcp` | yes | MCP (Model Context Protocol) tool bridge |
| `skills` | yes | Skills subsystem for reusable agent capabilities |
| `reminder` | yes | Reminder extension for injecting context messages |
| `server` | yes | HTTP server with SSE streaming and protocol adapters |
| `generative-ui` | yes | Generative UI component streaming |
| `full` | yes | Enables all of the above |

Workspace extension crates can exist outside the facade feature set. The current
one is `awaken-ext-deferred-tools`; add it as a direct dependency when you need
deferred tool loading.

## Related

- [Introduction](/awaken/introduction/)
- [Scheduled Actions](/awaken/reference/scheduled-actions/)
- [Effects](/awaken/reference/effects/)
