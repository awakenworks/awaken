---
title: "Config"
---

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
    pub allowed_tools: Option<Vec<String>>,           // literal tool ids
    pub excluded_tools: Option<Vec<String>>,          // literal tool ids
    pub allowed_tool_patterns: Option<Vec<String>>,   // glob patterns
    pub excluded_tool_patterns: Option<Vec<String>>,  // glob patterns
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
| Retry policy | `AgentSpec.sections["retry"]` |
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

The admin surface also exposes read-only preflight endpoints for integrations:

| Endpoint | Purpose |
|---|---|
| `GET /v1/config/providers/:id/removal-preview` | Returns the provider's referencing `model_ids`, affected `agent_ids`, and whether strict or cascade removal policies are allowed |
| `GET /v1/config/diagnostics` | Returns registry diagnostics in a stable serializable shape with `code`, `severity`, `resource`, optional `depends_on`, and `message` |

Current configurable plugin sections exposed by the starter runtime:

| Plugin ID | Section key | Admin editor |
|---|---|---|
| `permission` | `permission` | Dedicated permission rules editor |
| `reminder` | `reminder` | Dedicated reminder rules editor |
| `generative-ui` | `generative-ui` | Dedicated A2UI prompt/catalog editor |
| `ext-deferred-tools` | `deferred_tools` | Generic JSON Schema form |

## Tool catalog

Each agent's tool catalog is composed of four fields. Literals and patterns
are independent; combine them freely.

```yaml
allowed_tools:          [Bash, Read]    # literal tool ids
allowed_tool_patterns:  ["mcp:*"]       # glob patterns
excluded_tools:         []              # literal tool ids
excluded_tool_patterns: []              # glob patterns
```

The runtime computes:

```text
allow_set    = allowed_tools ∪ {id | ∃p ∈ allowed_tool_patterns. matches(p, id)}
exclude_set  = excluded_tools ∪ {id | ∃p ∈ excluded_tool_patterns. matches(p, id)}
final_set    = allow_set − exclude_set
```

Deny always wins: a tool in `excluded_*` is dropped even if it appears in
`allowed_*`.

### Pattern grammar

Anchored full match. `*` matches any sequence of characters (including `/`,
`:`, `_`). `\` escapes the next character — `\*` is a literal `*`, `\\` is a
literal `\`. No `?`, no character classes, no `{…}`, no `!` negation.

### "Allow all" shorthand

The universal pattern is just `*`:

```yaml
allowed_tool_patterns: ["*"]
```

### Default behavior (backward compatibility)

If an agent spec specifies **neither** `allowed_tools` nor
`allowed_tool_patterns`, the runtime injects `allowed_tool_patterns: ["*"]`
during deserialization. This preserves the historical "absent catalog =
allow all" default. Any explicit value (including empty lists) suppresses
the injection — `allowed_tools: []` with no `allowed_tool_patterns` means
"no tools allowed".

### Validation

| Condition                                | Effect                          |
|------------------------------------------|----------------------------------|
| `*` in `allowed_tools` / `excluded_tools`| Warning at load; entry treated as a literal (matches nothing useful). |
| Invalid pattern in `*_tool_patterns`     | **Error** at load; spec is rejected. |
| Pattern matches no registered tool       | Warning at resolve time.        |
| Catalog entry shaped like `name(args)`   | Warning at resolve time; belongs in `sections["permission"]`. |
| Permission rule names tool removed by catalog | Warning at resolve time.   |

### Migrating from the old single-field shape

The old `allowed_tools: ["mcp:*"]` (literal entry containing `*`) silently
matched nothing on prior releases. The new runtime emits a warning at load
and continues to treat it as a literal. To actually use it as a glob, move
the entry to `allowed_tool_patterns`. The admin console writes the new
shape automatically.

## AgentSpecPatch

`AgentSpecPatch` is the field-level override type for built-in agent
customization. Every field is optional: missing fields inherit from the base
`AgentSpec`, and present fields override the corresponding base value through
`merge_agent_spec(base, patch)`. For optional `AgentSpec` fields, JSON `null`
clears the base value.

Patchable fields are `model_id`, `system_prompt`, `max_rounds`,
`max_continuation_retries`, `context_policy`, `plugin_ids`,
`active_hook_filter`, `sections`, `allowed_tools`, `allowed_tool_patterns`,
`excluded_tools`, `excluded_tool_patterns`, `delegates`,
`reasoning_effort`, and `endpoint`.

`sections` uses a shallow per-key merge. A JSON `null` value in the patch
removes that section key from the effective spec. Optional fields such as
`endpoint`, `allowed_tools`, `allowed_tool_patterns`, `excluded_tools`,
`excluded_tool_patterns`, `context_policy`, and `reasoning_effort` are
tri-state: missing means inherit, `null` means clear, and a value means
override. Other list and scalar fields replace the base value when
present.

Note on catalog patches: PATCH-level `null` does **not** re-fire the
"absent catalog = allow all" shim — that shim only runs on initial
deserialize of a full `AgentSpec`. If a PATCH clears both `allowed_tools`
and `allowed_tool_patterns` to `null`, the merged spec has no allow rules
and the matcher denies every tool. To restore the "allow all" default
through a PATCH, set `allowed_tool_patterns: ["*"]` explicitly.

Unknown patch fields are rejected. Use `validate_agent_spec_patch(value)` when a
caller needs to apply Awaken's canonical parsing and unknown-field policy before
storing a patch.

## ConfigRecord Helpers

`ConfigRecord<T>` wraps a stored spec with provenance, visibility, timestamps,
revision, and optional `user_overrides`. The decoder accepts both the envelope
shape and legacy bare specs; `to_value()` always writes the envelope shape.

| Helper | Purpose |
|---|---|
| `validate_agent_spec(value)` | Decode an `AgentSpec` and reject unknown fields |
| `validate_agent_spec_patch(value)` | Decode an `AgentSpecPatch` and reject unknown fields |
| `validate_provider_spec(value)` | Decode a `ProviderSpec`, reject unknown write-surface fields, and reject empty `id` / `adapter` |
| `validate_model_spec(value)` | Decode a `ModelSpec`, reject unknown fields, and reject empty `id` / `provider_id` / `upstream_model` |
| `decode_config_record<T>(value)` | Decode a `ConfigRecord<T>`, accepting legacy bare specs, without checking `user_overrides` |
| `validate_config_record<T>(value)` | Decode a `ConfigRecord<T>` and validate `meta.user_overrides` against `T`'s patch type |
| `effective_config_record(record)` | Apply `meta.user_overrides` to a single record |
| `effective_visible_config_records<T>(records)` | Decode records, skip hidden entries, and return effective specs |

`AgentSpec`, `AgentSpecPatch`, provider writes, and model binding writes use
`UnknownFieldPolicy::Reject`; the exported constants
`AGENT_SPEC_UNKNOWN_FIELD_POLICY`, `AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY`,
`PROVIDER_SPEC_UNKNOWN_FIELD_POLICY`, and
`MODEL_SPEC_UNKNOWN_FIELD_POLICY` make that behavior explicit for
integrations. `ProviderSpec` deserialization remains lenient for read-time
compatibility, but config write and validate surfaces use
`validate_provider_spec(value)` to reject silently ignored fields.

## ContextWindowPolicy

Controls context window management and auto-compaction.

```rust,no_run
#[derive(Default)]
pub enum ContextCompactionMode {
    #[default]
    KeepRecentRawSuffix,
    CompactToSafeFrontier,
}

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

`upstream_model` is an upstream model name for the already resolved provider.
It does not re-resolve `AgentSpec.model_id` and does not switch providers.
Use `ModelPoolSpec` when an agent needs model failover.

```rust,no_run
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Max,
    Budget(u32),
}

pub struct InferenceOverride {
    pub upstream_model: Option<String>,      // upstream model name
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

For A2A, `base_url` points at the A2A interface root, for example
`https://agent.example.com/v1/a2a`; `target` selects the remote agent when the
backend exposes more than one agent. Legacy A2A fields (`bearer_token`,
`agent_id`, `poll_interval_ms`) deserialize only when no canonical fields are
present. New config should use `auth`, `target`, and `options`.

## ServerConfig

HTTP server configuration. Used when the `server` feature is enabled.

```rust,ignore
use awaken::RedactedString;

pub struct ServerConfig {
    pub address: String,                              // default: "0.0.0.0:3000"
    pub sse_buffer_size: usize,                       // default: 64
    pub replay_buffer_capacity: usize,                // default: 1024
    pub shutdown: ShutdownConfig,
    pub max_concurrent_requests: usize,               // default: 100
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
    pub mailbox_lifecycle: MailboxLifecycleMode,      // default: Auto
}

pub struct ShutdownConfig {
    pub timeout_secs: u64,                            // default: 30
}
```

**Crate path:** `awaken_server::app::ServerConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `address` | `String` | `"0.0.0.0:3000"` | Socket address the server binds to |
| `sse_buffer_size` | `usize` | `64` | Maximum SSE channel buffer size per connection |
| `replay_buffer_capacity` | `usize` | `1024` | Maximum SSE frames buffered per run for reconnection replay |
| `max_concurrent_requests` | `usize` | `100` | Maximum in-flight requests; excess requests receive 503 |
| `a2a_extended_card_bearer_token` | `Option<RedactedString>` | `None` | Enables authenticated `GET /v1/a2a/extendedAgentCard` when set. The token redacts itself in `Debug`/`Display`; call `expose_secret()` to read the value. JSON wire format remains a plain string |
| `mailbox_lifecycle` | `MailboxLifecycleMode` | `Auto` | `Auto` lets the framework start and shut down the mailbox; `Manual` hands lifecycle to the embedder |
| `shutdown.timeout_secs` | `u64` | `30` | Seconds to wait for in-flight requests to drain before force-exiting |

## AdminApiConfig

Admin/configuration API security settings. Attach this to `AppState` with
`AppState::with_admin_api_config`, or use
`AppState::with_admin_api_bearer_token` when only bearer auth is needed.

```rust,ignore
use awaken::RedactedString;

pub struct AdminApiConfig {
    pub bearer_token: Option<RedactedString>,
    pub cors_allowed_origins: Vec<String>,
    pub expose_config_routes: bool,                   // default: true
}
```

| Field | Type | Default | Description |
|---|---|---|---|
| `bearer_token` | `Option<RedactedString>` | `None` | Requires `Authorization: Bearer ...` for the admin surface when set: `/v1/capabilities`, `/v1/config/*`, `/v1/agents*`, `/v1/system/info`, `/v1/audit-log`, and runtime-stats endpoints. Redacts itself in `Debug`/`Display`; call `expose_secret()` to read the value. JSON wire format remains a plain string |
| `cors_allowed_origins` | `Vec<String>` | `["http://127.0.0.1:3002", "http://localhost:3002"]` | Browser origins allowed by the admin CORS layer |
| `expose_config_routes` | `bool` | `true` | Whether the server mounts the admin/configuration HTTP surface. Set to `false` to drop those routes entirely when configuration is driven through an external RBAC/audit pipeline |

Environment variables override the `AppState` admin settings:

| Variable | Description |
|---|---|
| `AWAKEN_ADMIN_API_BEARER_TOKEN` | Bearer token required for admin/configuration APIs |
| `AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS` | Comma-separated CORS origins for browser admin APIs |

## AuditLogConfig

Audit-log retention settings are kept separate from `AdminApiConfig` so the
admin security struct remains source-compatible with 0.4.0 struct literals.
Attach them to `AppState` with `AppState::with_audit_log_config` before calling
`AppState::with_audit_log_from_config`.

```rust,ignore
use awaken_server::app::AuditLogConfig;

pub struct AuditLogConfig {
    pub enabled: bool,              // default: true
    pub retention_days: u32,        // default: 90
    pub sweep_interval_secs: u64,   // default: 3600
}
```

### Secret handling

`RedactedString` (re-exported from the facade as `awaken::RedactedString`,
defined in `awaken_contract::secret`) is the single trust boundary for
credentials in serialized config. The wire format is a plain JSON
string, JSON Schema reports `string`, and the inner buffer is zeroized on drop.
`Debug` formats as `RedactedString(***)` and `Display` formats as `***`. Call
`expose_secret()` to obtain the plaintext when actually issuing a request, and
do not propagate the returned `&str` into log lines. Code that previously held
plain `String` tokens needs a one-line `.into()` at construction or a
`.expose_secret()` at the read site.

## ConfigRuntimeManager

`ConfigRuntimeManager` compiles candidate registry snapshots when configuration
changes and publishes them to the live runtime.

| Builder method | Default | Description |
|---|---|---|
| `with_provider_factory(factory)` | `GenaiProviderExecutorFactory` | Override how `ProviderSpec` is materialized into an `LlmExecutor` |
| `with_change_notifier(notifier)` | `None` | Subscribe to native change notifications instead of polling |
| `with_mcp_registry_factory(factory)` | `DefaultMcpRegistryFactory` | Override how MCP server specs are turned into a registry |
| `with_mcp_refresh_interval(interval)` | disabled | Periodically refresh MCP server connections |
| `with_min_apply_interval(interval)` | `Duration::ZERO` | Minimum interval between successive applies driven by the change listener. Bursts that arrive within this window coalesce into a single apply. Direct calls to `apply` / `apply_if_changed` are unaffected. Provider executors are reused across applies for specs whose hash is unchanged |

## MailboxConfig

Configuration for the persistent run queue (mailbox). Controls lease timing,
sweep/GC intervals, and retry behavior for failed dispatches.

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
| `lease_renewal_interval` | `Duration` | `10s` | How often the worker renews its lease on a running dispatch |
| `sweep_interval` | `Duration` | `30s` | How often to scan for expired leases and reclaim orphaned dispatches |
| `gc_interval` | `Duration` | `60s` | How often to run garbage collection for terminal dispatches |
| `gc_ttl` | `Duration` | `24h` | How long terminal dispatches are retained before purging |
| `default_max_attempts` | `u32` | `5` | Maximum delivery attempts before a dispatch is dead-lettered |
| `default_retry_delay_ms` | `u64` | `250` | Base retry delay in milliseconds between attempts |
| `max_retry_delay_ms` | `u64` | `30_000` | Maximum retry delay in milliseconds for exponential backoff |

## LlmRetryPolicy

Policy for retrying failed LLM inference calls with exponential backoff. Can be
set per-agent via the `"retry"` section in `AgentSpec`. Model failover is
configured by pointing `AgentSpec.model_id` at a `ModelPoolSpec`.

Retry is applied during agent resolution. A missing `"retry"` section uses
`LlmRetryPolicy::default()`. Set `max_retries` to `0` to disable retry wrapping.
Providers are not wrapped with a separate hidden retry policy during provider
construction. For streaming inference, retry only applies while opening the
stream.

```rust,ignore
pub struct LlmRetryPolicy {
    pub max_retries: u32,              // default: 2
    pub backoff_base_ms: u64,          // default: 500
}
```

**Crate path:** `awaken_runtime::engine::retry::LlmRetryPolicy`

| Field | Type | Default | Description |
|---|---|---|---|
| `max_retries` | `u32` | `2` | Maximum retry attempts after the initial call (0 = no retry) |
| `backoff_base_ms` | `u64` | `500` | Base delay in milliseconds for exponential backoff; actual delay = min(base * 2^attempt, 8000ms). Set to 0 to disable backoff |

### AgentSpec integration

Register via the `RetryConfigKey` plugin config key (`"retry"` section):

```rust,ignore
use awaken_runtime::engine::retry::RetryConfigKey;

let spec = AgentSpec::new("my-agent")
    .with_config::<RetryConfigKey>(LlmRetryPolicy {
        max_retries: 3,
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
- `ConfigRuntimeManager` — validates config writes by compiling a candidate registry snapshot before publishing it
- `ConfigService` — service layer used by `/v1/config/*`, `/v1/agents`, and `/v1/capabilities`

Built-in implementations:

- `InMemoryStore` implements `ThreadRunStore`, `ProfileStore`, and `ConfigStore`
- `FileStore` implements `ThreadRunStore`, `ProfileStore`, and `ConfigStore`
- `PostgresStore` implements `ThreadRunStore` and `ConfigStore`

## Related

- [Build an Agent](../how-to/build-an-agent.md)
- [Configure Agent Behavior](../how-to/configure-agent-behavior.md)
- [HTTP API](./http-api.md)
- [Provider and Model Configuration](./provider-model-config.md)
