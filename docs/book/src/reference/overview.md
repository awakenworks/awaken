# Overview

The `awaken` crate is the public facade for the Awaken agent framework. It
re-exports types from the internal `awaken-contract` and `awaken-runtime` crates
so that downstream code only needs a single dependency.

## Module re-exports

| Facade path | Source crate | Contents |
|---|---|---|
| `awaken::contract` | `awaken-contract` | Tool trait, events, messages, suspension, lifecycle |
| `awaken::model` | `awaken-contract` | Phase, EffectSpec, ScheduledActionSpec, JsonValue |
| `awaken::registry_spec` | `awaken-contract` | AgentSpec, ModelBindingSpec, ProviderSpec, McpServerSpec, PluginConfigKey |
| `awaken::state` | `awaken-contract` + `awaken-runtime` | StateKey, StateMap, Snapshot, StateStore, MutationBatch |
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
| `awaken::registry` | `awaken-runtime` | AgentResolver, ResolvedAgent |
| `awaken::runtime` | `awaken-runtime` | AgentRuntime |
| `awaken::stores` | `awaken-stores` | File and Postgres store implementations |

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

**From `awaken-contract`:**
`AgentSpec`, `EffectSpec`, `FailedScheduledActions`, `JsonValue`, `KeyScope`,
`MergeStrategy`, `PendingScheduledActions`, `PersistedState`, `Phase`,
`PluginConfigKey`, `ScheduledActionSpec`, `Snapshot`, `StateError`, `StateKey`,
`StateKeyOptions`, `StateMap`, `TypedEffect`, `UnknownKeyPolicy`

**From `awaken-runtime`:**
`AgentResolver`, `AgentRuntime`, `AgentRuntimeBuilder`, `BuildError`,
`CancellationToken`, `CommitEvent`, `CommitHook`, `DEFAULT_MAX_PHASE_ROUNDS`,
`ExecutionEnv`, `MutationBatch`, `PhaseContext`, `PhaseHook`, `PhaseRuntime`,
`Plugin`, `PluginDescriptor`, `PluginRegistrar`, `ResolvedAgent`, `RunRequest`,
`RuntimeError`, `StateCommand`, `StateStore`, `TypedEffectHandler`,
`TypedScheduledActionHandler`

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

- [Introduction](../introduction.md)
- [Scheduled Actions](scheduled-actions.md)
- [Effects](effects.md)
