# Config

## AgentSpec

The serializable agent definition. Can be loaded from JSON/YAML or constructed
programmatically via builder methods.

```rust,ignore
pub struct AgentSpec {
    pub id: String,
    pub model_id: String,                            // model registry id
    pub system_prompt: String,
    pub max_rounds: usize,                          // default: 16
    pub max_continuation_retries: usize,            // default: 2
    pub context_policy: Option<ContextWindowPolicy>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub plugin_ids: Vec<String>,
    pub active_hook_filter: HashSet<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub excluded_tools: Option<Vec<String>>,
    pub endpoint: Option<RemoteEndpoint>,
    pub delegates: Vec<String>,
    pub sections: HashMap<String, Value>,
    pub registry: Option<String>,
}
```

**Crate path:** `awaken::registry_spec::AgentSpec` (re-exported at `awaken::AgentSpec`)

### Builder methods

```rust,ignore
AgentSpec::new(id) -> Self
    .with_model_id(model_id) -> Self
    .with_system_prompt(prompt) -> Self
    .with_max_rounds(n) -> Self
    .with_reasoning_effort(effort) -> Self
    .with_hook_filter(plugin_id) -> Self
    .with_config::<K>(config) -> Result<Self, StateError>
    .with_delegate(agent_id) -> Self
    .with_endpoint(endpoint) -> Self
    .with_section(key, value: Value) -> Self
```

### Typed config access

```rust,ignore
/// Read a typed plugin config section. Returns default if missing.
fn config<K: PluginConfigKey>(&self) -> Result<K::Config, StateError>

/// Set a typed plugin config section.
fn set_config<K: PluginConfigKey>(&mut self, config: K::Config) -> Result<(), StateError>
```

### Runtime-managed plugin config

`AgentSpec.sections` is the source of truth for plugin configuration whether
the spec is built in Rust, loaded from JSON/YAML, or saved through the runtime
config API. Plugins declare the same typed section with `PluginConfigKey`, expose
its JSON Schema from `Plugin::config_schemas()`, and read it during resolution or
phase hooks with `agent_spec.config::<K>()`.

This is the intended control plane for agent optimization features. Model and
provider selection, base prompts, reminder rules, generated-UI prompt guidance,
permissions, context-window behavior, retry policy, and deferred-tool policy are
data that can be validated and changed at runtime.

| Tuning surface | Implemented config surface |
|---|---|
| Base prompt | `AgentSpec.system_prompt` in the `agents` config namespace |
| Model selection | `AgentSpec.model_id`, resolved through `/v1/config/models` |
| Provider endpoint and OpenAI-compatible routing | `/v1/config/providers` (`adapter`, `base_url`, auth, timeout) |
| Context budget and prompt caching | `AgentSpec.context_policy` |
| Reasoning effort | `AgentSpec.reasoning_effort` |
| Retry and fallback models | `AgentSpec.sections["retry"]` |
| System reminders and prompt context injection | `AgentSpec.sections["reminder"]`, read through `ReminderConfigKey` |
| Generative UI prompt guidance | `AgentSpec.sections["generative-ui"]`, read through `A2uiPromptConfigKey` |
| Permission policy | `AgentSpec.sections["permission"]` |
| Deferred tool loading | `AgentSpec.sections["deferred_tools"]` |

Prompt semantic hooks are not a built-in plugin yet. When added, they should use
the same path: declare a typed config key, expose schema, render in the admin
console, and read the resolved config from hooks.

When `awaken-server` is constructed with a `ConfigStore` and config runtime
manager, `/v1/capabilities` returns `plugins[].config_schemas`. The admin
console renders those schemas on the agent editor page and saves values back
under `AgentSpec.sections[schema.key]`. A saved section takes effect on new runs
after the config write validates and publishes a new registry snapshot. If the
plugin is not listed in `plugin_ids`, the section remains stored but the plugin
is not loaded, so its hooks, tools, and request transforms do not run.

Current configurable plugin sections exposed by the starter runtime:

| Plugin ID | Section key | Admin editor |
|---|---|---|
| `permission` | `permission` | Dedicated permission rules editor |
| `reminder` | `reminder` | Dedicated reminder rules editor |
| `generative-ui` | `generative-ui` | Dedicated A2UI prompt/catalog editor |
| `ext-deferred-tools` | `deferred_tools` | Generic JSON Schema form |

## ContextWindowPolicy

Controls context window management and auto-compaction.

```rust,no_run
# #[derive(Default)] pub enum ContextCompactionMode { #[default] KeepRecentRawSuffix, CompactToSafeFrontier }
pub struct ContextWindowPolicy {
    pub max_context_tokens: usize,          // default: 200_000
    pub max_output_tokens: usize,           // default: 16_384
    pub min_recent_messages: usize,         // default: 10
    pub enable_prompt_cache: bool,          // default: true
    pub autocompact_threshold: Option<usize>,  // default: None
    pub compaction_mode: ContextCompactionMode, // default: KeepRecentRawSuffix
    pub compaction_raw_suffix_messages: usize,  // default: 2
}
```

### ContextCompactionMode

```rust,no_run
pub enum ContextCompactionMode {
    KeepRecentRawSuffix,       // Keep N recent messages raw, compact the rest
    CompactToSafeFrontier,     // Compact everything up to safe frontier
}
```

## InferenceOverride

Per-inference parameter override. All fields are `Option`; `None` means "use
agent-level default". Multiple plugins can emit overrides; fields merge with
last-wins semantics.

`upstream_model` and `fallback_upstream_models` are upstream model names for the already resolved
provider. They do not re-resolve `AgentSpec.model_id` and do not switch providers.
See [Provider and Model Configuration](./provider-model-config.md).

```rust,no_run
# pub enum ReasoningEffort { None, Low, Medium, High, Max, Budget(u32) }
pub struct InferenceOverride {
    pub upstream_model: Option<String>,      // upstream model name
    pub fallback_upstream_models: Option<Vec<String>>, // upstream model names
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub reasoning_effort: Option<ReasoningEffort>,
}
```

### Methods

```rust,ignore
fn is_empty(&self) -> bool
fn merge(&mut self, other: InferenceOverride)
```

### ReasoningEffort

```rust,no_run
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Max,
    Budget(u32),
}
```

## PluginConfigKey trait

Binds a string key to a typed configuration struct at compile time.

```rust,ignore
pub trait PluginConfigKey: 'static + Send + Sync {
    const KEY: &'static str;
    type Config: Default + Clone + Serialize + DeserializeOwned
        + schemars::JsonSchema + Send + Sync + 'static;
}
```

Implementations register typed sections in `AgentSpec::sections`. Plugins read
their configuration via `agent_spec.config::<MyConfigKey>()`.

## RemoteEndpoint

Configuration for agents running on external backends. Today Awaken ships the
`"a2a"` backend; backend-specific settings live under `options`.

```rust,ignore
pub struct RemoteEndpoint {
    pub backend: String,
    pub base_url: String,
    pub auth: Option<RemoteAuth>,
    pub target: Option<String>,
    pub timeout_ms: u64,               // default: 300_000
    pub options: BTreeMap<String, Value>,
}

pub struct RemoteAuth {
    pub r#type: String,
    // backend-specific auth fields, e.g. { "token": "..." } for bearer
}
```

## ServerConfig

HTTP server configuration. Used when the `server` feature is enabled.

```rust,ignore
pub struct ServerConfig {
    pub address: String,                   // default: "0.0.0.0:3000"
    pub sse_buffer_size: usize,            // default: 64
    pub replay_buffer_capacity: usize,     // default: 1024
    pub shutdown: ShutdownConfig,
    pub max_concurrent_requests: usize,    // default: 100
    pub a2a_extended_card_bearer_token: Option<String>,
}

pub struct ShutdownConfig {
    pub timeout_secs: u64,                 // default: 30
}
```

**Crate path:** `awaken_server::app::ServerConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `address` | `String` | `"0.0.0.0:3000"` | Socket address the server binds to |
| `sse_buffer_size` | `usize` | `64` | Maximum SSE channel buffer size per connection |
| `replay_buffer_capacity` | `usize` | `1024` | Maximum SSE frames buffered per run for reconnection replay |
| `max_concurrent_requests` | `usize` | `100` | Maximum in-flight requests; excess requests receive 503 |
| `a2a_extended_card_bearer_token` | `Option<String>` | `None` | Enables authenticated `GET /v1/a2a/extendedAgentCard` when set |
| `shutdown.timeout_secs` | `u64` | `30` | Seconds to wait for in-flight requests to drain before force-exiting |

## MailboxConfig

Configuration for the persistent run queue (mailbox). Controls lease timing,
sweep/GC intervals, and retry behavior for failed jobs.

```rust,ignore
pub struct MailboxConfig {
    pub lease_ms: u64,                          // default: 30_000
    pub suspended_lease_ms: u64,                // default: 600_000
    pub lease_renewal_interval: Duration,       // default: 10s
    pub sweep_interval: Duration,               // default: 30s
    pub gc_interval: Duration,                  // default: 60s
    pub gc_ttl: Duration,                       // default: 24h
    pub default_max_attempts: u32,              // default: 5
    pub default_retry_delay_ms: u64,            // default: 250
    pub max_retry_delay_ms: u64,                // default: 30_000
}
```

**Crate path:** `awaken_server::mailbox::MailboxConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `lease_ms` | `u64` | `30_000` | Lease duration in milliseconds for active runs |
| `suspended_lease_ms` | `u64` | `600_000` | Lease duration in milliseconds for suspended runs awaiting human input |
| `lease_renewal_interval` | `Duration` | `10s` | How often the worker renews its lease on a running job |
| `sweep_interval` | `Duration` | `30s` | How often to scan for expired leases and reclaim orphaned jobs |
| `gc_interval` | `Duration` | `60s` | How often to run garbage collection for terminal (completed/failed) jobs |
| `gc_ttl` | `Duration` | `24h` | How long terminal jobs are retained before purging |
| `default_max_attempts` | `u32` | `5` | Maximum delivery attempts before a job is dead-lettered |
| `default_retry_delay_ms` | `u64` | `250` | Base retry delay in milliseconds between attempts |
| `max_retry_delay_ms` | `u64` | `30_000` | Maximum retry delay in milliseconds for exponential backoff |

## LlmRetryPolicy

Policy for retrying failed LLM inference calls with exponential backoff and
optional model fallback. Can be set per-agent via the `"retry"` section in
`AgentSpec`.

Retry is applied during agent resolution. A missing `"retry"` section uses
`LlmRetryPolicy::default()`. Set `max_retries` to `0` and keep
`fallback_upstream_models` empty to disable retry wrapping. Providers are not wrapped
with a separate hidden retry policy during provider construction. For streaming
inference, retry and fallback only apply while opening the stream.

```rust,ignore
pub struct LlmRetryPolicy {
    pub max_retries: u32,              // default: 2
    pub fallback_upstream_models: Vec<String>,  // default: []
    pub backoff_base_ms: u64,          // default: 500
}
```

**Crate path:** `awaken_runtime::engine::retry::LlmRetryPolicy`

| Field | Type | Default | Description |
|---|---|---|---|
| `max_retries` | `u32` | `2` | Maximum retry attempts after the initial call (0 = no retry) |
| `fallback_upstream_models` | `Vec<String>` | `[]` | Model names to try in order after the primary model exhausts retries |
| `backoff_base_ms` | `u64` | `500` | Base delay in milliseconds for exponential backoff; actual delay = min(base * 2^attempt, 8000ms). Set to 0 to disable backoff |

### AgentSpec integration

Register via the `RetryConfigKey` plugin config key (`"retry"` section):

```rust,ignore
use awaken_runtime::engine::retry::RetryConfigKey;

let spec = AgentSpec::new("my-agent")
    .with_config::<RetryConfigKey>(LlmRetryPolicy {
        max_retries: 3,
        fallback_upstream_models: vec!["claude-sonnet-4-20250514".into()],
        backoff_base_ms: 1000,
    })?;
```

## CircuitBreakerConfig

Per-model circuit breaker configuration. Prevents cascading failures by
short-circuiting requests to models that have experienced repeated consecutive
failures. After a cooldown the circuit transitions to half-open, allowing
limited probe requests before fully closing on success.

```rust,ignore
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,    // default: 5
    pub cooldown: Duration,        // default: 30s
    pub half_open_max: u32,        // default: 1
}
```

**Crate path:** `awaken_runtime::engine::circuit_breaker::CircuitBreakerConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `failure_threshold` | `u32` | `5` | Consecutive failures before the circuit opens and rejects requests |
| `cooldown` | `Duration` | `30s` | How long the circuit stays open before transitioning to half-open |
| `half_open_max` | `u32` | `1` | Maximum probe requests allowed in the half-open state before the circuit reopens on failure or closes on success |

## Feature flags and their effects

| Flag | Runtime behavior |
|---|---|
| `permission` | Registers the permission plugin; tools can be gated with HITL approval |
| `observability` | Registers the observability plugin; emits traces and metrics |
| `mcp` | Enables MCP tool bridge; tools from MCP servers are auto-registered |
| `skills` | Enables the skills subsystem for reusable agent capabilities |
| `server` | Builds the HTTP server with SSE streaming and protocol adapters |
| `generative-ui` | Enables generative UI component streaming to frontends |

## Custom plugin configuration

Plugins declare typed configuration sections using the `PluginConfigKey` trait,
which binds a string key to a Rust struct at compile time:

```rust,ignore
pub trait PluginConfigKey: 'static + Send + Sync {
    const KEY: &'static str;               // section name in AgentSpec.sections
    type Config: Default + Clone + Serialize + DeserializeOwned
        + schemars::JsonSchema + Send + Sync + 'static;
}
```

### Declaring schemas for validation

Plugins override `config_schemas()` to return JSON Schemas generated from
their config structs. The resolve pipeline (Stage 2) validates every
`AgentSpec.sections` entry against these schemas before any hook runs.

```rust,ignore
fn config_schemas(&self) -> Vec<ConfigSchema> {
    vec![ConfigSchema {
        key: RateLimitConfigKey::KEY,
        json_schema: schemars::schema_for!(RateLimitConfig),
    }]
}
```

### Reading config at runtime

Plugins read their typed config via `agent_spec.config::<K>()`. If the section
is absent, the `Default` impl is returned.

```rust,ignore
let cfg = ctx.agent_spec().config::<RateLimitConfigKey>()?;
```

### Worked example

```rust,ignore
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use awaken::PluginConfigKey;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RateLimitConfig {
    pub max_calls_per_step: u32,   // default: 0 (unlimited)
    pub cooldown_ms: u64,          // default: 0
}

pub struct RateLimitConfigKey;

impl PluginConfigKey for RateLimitConfigKey {
    const KEY: &'static str = "rate_limit";
    type Config = RateLimitConfig;
}

// In plugin register():
fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_phase_hook("rate_limit", Phase::BeforeToolExecute, RateLimitHook)?;
    Ok(())
}

fn config_schemas(&self) -> Vec<ConfigSchema> {
    vec![ConfigSchema {
        key: RateLimitConfigKey::KEY,
        json_schema: schemars::schema_for!(RateLimitConfig),
    }]
}

// In a hook:
let cfg = ctx.agent_spec().config::<RateLimitConfigKey>()?;
if cfg.max_calls_per_step > 0 { /* enforce limit */ }
```

### Validation behavior

- **Section present but invalid:** resolve fails with a schema validation error.
- **Section present but unclaimed:** a warning is logged (possible typo or
  removed plugin).
- **Section absent:** allowed; the plugin receives `Config::default()`.

## DeferredToolsConfig

`awaken-ext-deferred-tools` is registered with plugin ID `ext-deferred-tools`.
Its agent config section key is `deferred_tools`, bound by
`DeferredToolsConfigKey`. The crate is not included in the `awaken` facade
`full` feature; add `awaken-ext-deferred-tools` directly and register
`DeferredToolsPlugin` with seed tool descriptors.

```json
{
  "enabled": null,
  "default_mode": "deferred",
  "beta_overhead": 1136.0,
  "rules": [
    { "tool": "get_weather", "mode": "eager" },
    { "tool": "debug_*", "mode": "deferred" }
  ],
  "agent_priors": {
    "get_weather": 0.03
  },
  "disc_beta": {
    "omega": 0.95,
    "n0": 5.0,
    "defer_after": 5,
    "thresh_mult": 0.5,
    "gamma": 2000.0
  }
}
```

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | `bool \| null` | `null` | `true` always enables, `false` disables, `null`/missing auto-enables when estimated schema savings exceed `beta_overhead` |
| `rules` | `DeferralRule[]` | `[]` | Ordered exact/glob tool rules; first match wins |
| `default_mode` | `"eager" \| "deferred"` | `"deferred"` | Mode for tools that match no rule |
| `beta_overhead` | `number` | `1136.0` | Estimated per-turn overhead of `ToolSearch` plus the deferred-tool list |
| `agent_priors` | `object` | `{}` | Optional per-tool prior usage frequencies in `0..1`; missing tools use `0.01` |
| `disc_beta.omega` | `number` | `0.95` | Per-turn discount factor; effective memory is approximately `1/(1-omega)` turns |
| `disc_beta.n0` | `number` | `5.0` | Prior strength in equivalent observations |
| `disc_beta.defer_after` | `integer` | `5` | Minimum idle turns before a promoted tool can be re-deferred |
| `disc_beta.thresh_mult` | `number` | `0.5` | Multiplier applied to the breakeven frequency threshold |
| `disc_beta.gamma` | `number` | `2000.0` | Estimated token cost of one `ToolSearch` call |

See [Use Deferred Tools](../how-to/use-deferred-tools.md) for the activation
heuristic, `ToolSearch` behavior, and the full DiscBeta probability model.

## ConfigStore

`ConfigStore` is the async persistence contract behind the server-side `/v1/config/*` APIs. Use it when configuration must be created, listed, or updated at runtime instead of being baked into `AgentSpec`.

```rust,ignore
#[async_trait]
pub trait ConfigStore: Send + Sync {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError>;
    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError>;
    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError>;
    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError>;
}
```

Related types:

- `ConfigChangeNotifier` / `ConfigChangeSubscriber` — optional native change notifications
- `AppState::with_config_store(...)` — enables runtime config routes in `awaken-server`

## Related

- [Build an Agent](../how-to/build-an-agent.md)
