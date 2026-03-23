# Config

## Server Environment Variables

From `tirea-agentos-server` CLI (`crates/tirea-agentos-server/src/main.rs`):

- `AGENTOS_HTTP_ADDR` (default `127.0.0.1:8080`)
- `AGENTOS_STORAGE_DIR` (default `./threads`)
- `AGENTOS_CONFIG` (JSON config file path)
- `AGENTOS_NATS_URL` (enables NATS gateway)
- `TENSORZERO_URL` (routes model calls through TensorZero provider)

Run records are stored under `${AGENTOS_STORAGE_DIR}/runs` when using the default file run store.

To print the canonical JSON Schema for `AGENTOS_CONFIG`:

```bash
cargo run -p tirea-agentos-server -- --print-agent-config-schema
```

## `AGENTOS_CONFIG` JSON Shape

The top-level config object has three optional sections: `providers`, `models`, and the required `agents` array. Providers and models are keyed by id; agents reference them by those ids.

```json
{
  "providers": {
    "my-proxy": {
      "endpoint": "http://127.0.0.1:10531/v1",
      "auth": { "kind": "env", "name": "OPENAI_API_KEY" },
      "adapter_kind": "openai"
    }
  },
  "models": {
    "gpt-5": {
      "provider": "my-proxy",
      "model": "gpt-5.4",
      "chat_options": {
        "temperature": 0.7,
        "max_tokens": 4096,
        "reasoning_effort": "high"
      }
    }
  },
  "agents": [
    {
      "kind": "local",
      "id": "assistant",
      "name": "Assistant",
      "description": "Primary hosted assistant",
      "model": "gpt-5",
      "system_prompt": "You are a helpful assistant.",
      "max_rounds": 10,
      "tool_execution_mode": "parallel_streaming",
      "behavior_ids": ["tool_policy", "permission"],
      "stop_condition_specs": [
        { "type": "max_rounds", "rounds": 10 }
      ]
    }
  ]
}
```

You can also register remote A2A agents:

```json
{
  "agents": [
    {
      "kind": "a2a",
      "id": "researcher",
      "name": "Researcher",
      "description": "Remote research agent",
      "endpoint": "https://example.test/v1/a2a",
      "remote_agent_id": "remote-researcher",
      "poll_interval_ms": 250,
      "auth": {
        "kind": "bearer_token",
        "token": "secret"
      }
    }
  ]
}
```

Local agent file fields:

- `id` (required)
- `name` (optional, defaults to `id`)
- `description` (optional, default empty string)
- `model` (optional, defaults to `AgentDefinition::default().model`)
- `system_prompt` (optional, default empty string)
- `max_rounds` (optional)
- `tool_execution_mode` (optional, default `parallel_streaming`)
- `behavior_ids` (optional, default `[]`)
- `stop_condition_specs` (optional, default `[]`)

Remote A2A agent file fields:

- `id` (required)
- `name` (optional, defaults to `id`)
- `description` (optional, default empty string)
- `endpoint` (required, A2A base URL)
- `remote_agent_id` (optional, defaults to `id`)
- `poll_interval_ms` (optional, clamped to the runtime minimum)
- `auth` (optional)
  - `{ "kind": "bearer_token", "token": "..." }`
  - `{ "kind": "header", "name": "X-Api-Key", "value": "..." }`

Legacy local entries without `"kind": "local"` are still accepted.
Configs with only `agents` (no `providers`/`models`) remain valid for backward compatibility.

## Providers (`ProviderConfig`)

Declare custom LLM endpoints in the `providers` map. Each key is a provider id referenced by model configs.

- `endpoint` (required) -- base URL for the provider API
- `auth` (optional) -- authentication method:
  - `{ "kind": "env", "name": "ENV_VAR_NAME" }` -- read API key from environment
  - `{ "kind": "token", "value": "sk-..." }` -- literal token value
- `adapter_kind` (optional) -- override the genai adapter used for all requests through this provider (e.g. `"openai"`, `"anthropic"`, `"bigmodel"`). Useful when a third-party endpoint speaks a compatible protocol but is not auto-detected by model name.

```json
"providers": {
  "bigmodel-coding": {
    "endpoint": "https://open.bigmodel.cn/api/coding/paas/v4/",
    "auth": { "kind": "token", "value": "my-key" },
    "adapter_kind": "openai"
  }
}
```

## Models (`ModelConfig`)

Declare model aliases in the `models` map. Each key is a model id that agents reference in their `model` field.

- `provider` (required) -- provider id from the `providers` map
- `model` (required) -- upstream model name sent to the provider
- `chat_options` (optional) -- inference parameters (see `ChatOptionsConfig` below)

## Chat Options (`ChatOptionsConfig`)

Strongly-typed universal fields plus a passthrough `extra` map for provider-specific parameters.

| Field | Type | Description |
|---|---|---|
| `temperature` | `f64` | Sampling temperature |
| `max_tokens` | `u32` | Maximum output tokens |
| `top_p` | `f64` | Nucleus sampling threshold |
| `stop_sequences` | `string[]` | Stop sequences |
| `reasoning_effort` | `string` | `"low"`, `"medium"`, or `"high"` |
| `seed` | `u64` | Deterministic sampling seed |
| `extra` | `object` | Provider-specific fields merged into `genai::ChatOptions` via JSON round-trip |

The `extra` map supports any field recognized by the genai crate (e.g. `verbosity`, `service_tier`). Invalid values produce an `ExtraMerge` error at config load time. An all-default `chat_options` object is treated as absent.

```json
"chat_options": {
  "temperature": 0.5,
  "max_tokens": 2048,
  "reasoning_effort": "medium",
  "extra": { "service_tier": "Flex" }
}
```

`tool_execution_mode` values:

- `sequential`
- `parallel_batch_approval`
- `parallel_streaming`

## Tool Execution Mode Semantics

| Mode | Scheduler | Suspension handling | Use when |
|---|---|---|---|
| `sequential` | One tool call at a time | At most one call is actively executing at a time | Deterministic debugging and strict call ordering matter more than latency |
| `parallel_batch_approval` | Parallel tool execution per round | Approval/suspension outcomes are applied after the tool round commits | Multiple tools may fan out, but you still want batch-style resume behavior |
| `parallel_streaming` | Parallel tool execution per round | Stream mode can surface progress and apply resume decisions while tools are still in flight | Rich UIs need progress, activity events, and lower-latency approval loops |

`parallel_streaming` is the default because it fits the frontend-oriented AG-UI / AI SDK starter flows well.

`stop_condition_specs` (`StopConditionSpec`) values:

- `max_rounds`
- `timeout`
- `token_budget`
- `consecutive_errors`
- `stop_on_tool`
- `content_match`
- `loop_detection`

## Stop Policy Wiring and Semantics

Stop conditions are enforced by the internal `stop_policy` behavior. You do not register this behavior id manually; `AgentOsBuilder` wires it automatically when an agent resolves with stop specs or stop-condition ids.

`max_rounds` is still the top-level ergonomic field on `AgentDefinition`, but it is lowered into `StopConditionSpec::MaxRounds` during wiring. If you already provide an explicit `max_rounds` stop spec, the implicit one is not added a second time.

Built-in stop conditions:

| Spec | What it measures | Use when |
|---|---|---|
| `max_rounds` | Completed tool/inference rounds | You want a hard upper bound on loop depth |
| `timeout` | Wall-clock elapsed time | Long-running agents must terminate predictably |
| `token_budget` | Cumulative prompt + completion tokens | Spend must stay inside a token budget |
| `consecutive_errors` | Back-to-back failing tool rounds | You want to halt repeated tool failure cascades |
| `stop_on_tool` | Specific tool id emitted by the model | A tool should act as an explicit finish/escape hatch |
| `content_match` | Literal text pattern in model output | You need a simple semantic stop trigger without a dedicated tool |
| `loop_detection` | Repeated tool-call name patterns | You want to cut off obvious repetitive tool loops |

Stop-policy runtime bookkeeping lives in run-scoped state under `__kernel.stop_policy_runtime`, so it is reset on each new run.

## AgentDefinition Fields (Builder-Level)

`AgentDefinition` supports these orchestration fields:

- Identity/model: `id`, `model`, `system_prompt`
- Loop policy: `max_rounds`, `tool_execution_mode`
- Inference options: `chat_options`, `fallback_models`, `llm_retry_policy`
- Behavior/stop wiring: `behavior_ids`, `stop_condition_specs`, `stop_condition_ids`
- Visibility controls:
  - tools: `allowed_tools`, `excluded_tools`
  - skills: `allowed_skills`, `excluded_skills`
  - delegated agents: `allowed_agents`, `excluded_agents`

## Model Fallbacks and Retries

- `fallback_models`: ordered fallback model ids that are tried after the primary `model`
- `llm_retry_policy`: retry behavior for transient inference failures before the framework escalates to fallbacks or terminates

Keep these fields close to your provider registry definitions so a single source of truth controls which model ids are valid in each environment.

## Scope Filters (RunPolicy)

Runtime policy filters are stored as `RunPolicy` fields (set via `set_*_if_absent` methods during agent resolution):

- `allowed_tools` / `excluded_tools`
- `allowed_skills` / `excluded_skills`
- `allowed_agents` / `excluded_agents`

## AgentRunConfig (Layered Runtime Configuration)

`AgentRunConfig` (`tirea-contract::runtime::run::config`) is the runtime configuration object that bridges build-time static config with runtime state. It is created during agent resolution, remains mutable until execution starts, then is frozen as `Arc<AgentRunConfig>` for the duration of the run.

Core fields:

- `policy` (`RunPolicy`) -- scope filters (allowed/excluded tools, skills, agents)
- `agent_id` (`String`) -- resolved agent identifier
- `model` (`String`) -- resolved model id
- `extensions` (`Extensions`) -- TypeMap for plugin-specific typed config

The `extensions` field is a `TypeMap`-based store that lets plugins attach arbitrary typed configuration without modifying `AgentRunConfig` itself. Plugins insert config during resolve and read it during execution:

```rust,edition2021
# extern crate tirea;
use tirea::contracts::{AgentRunConfig, RunPolicy};

#[derive(Debug, Default)]
struct MyPluginConfig {
    enabled: bool,
}

let mut run_config = AgentRunConfig::new(RunPolicy::default());
run_config
    .extensions_mut()
    .insert(MyPluginConfig { enabled: true });

let cfg = run_config.extensions().get::<MyPluginConfig>();
assert_eq!(cfg.map(|cfg| cfg.enabled), Some(true));
```

This replaces the earlier pattern of passing `RunPolicy` directly to plugins and tools, providing a single extensible config surface for the entire run lifecycle.
