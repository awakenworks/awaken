# Config

## AgentSpec

The serializable agent definition. Can be loaded from JSON/YAML or constructed
programmatically via builder methods.

```rust,ignore
pub struct AgentSpec {
    pub id: String,
    pub model: String,
    pub system_prompt: String,
    pub max_rounds: usize,                          // default: 16
    pub max_continuation_retries: usize,            // default: 2
    pub context_policy: Option<ContextWindowPolicy>,
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
    .with_model(model) -> Self
    .with_system_prompt(prompt) -> Self
    .with_max_rounds(n) -> Self
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

## ContextWindowPolicy

Controls context window management and auto-compaction.

```rust,ignore
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

```rust,ignore
pub enum ContextCompactionMode {
    KeepRecentRawSuffix,       // Keep N recent messages raw, compact the rest
    CompactToSafeFrontier,     // Compact everything up to safe frontier
}
```

## InferenceOverride

Per-inference parameter override. All fields are `Option`; `None` means "use
agent-level default". Multiple plugins can emit overrides; fields merge with
last-wins semantics.

```rust,ignore
pub struct InferenceOverride {
    pub model: Option<String>,
    pub fallback_models: Option<Vec<String>>,
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

```rust,ignore
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

Configuration for agents running on external A2A servers.

```rust,ignore
pub struct RemoteEndpoint {
    pub base_url: String,
    pub bearer_token: Option<String>,
    pub poll_interval_ms: u64,    // default: 2000
    pub timeout_ms: u64,          // default: 300_000
}
```

## Feature flags and their effects

| Flag | Runtime behavior |
|---|---|
| `permission` | Registers the permission plugin; tools can be gated with HITL approval |
| `observability` | Registers the observability plugin; emits traces and metrics |
| `mcp` | Enables MCP tool bridge; tools from MCP servers are auto-registered |
| `skills` | Enables the skills subsystem for reusable agent capabilities |
| `server` | Builds the HTTP server with SSE streaming and protocol adapters |
| `generative-ui` | Enables generative UI component streaming to frontends |

## Related

- [Build an Agent](../how-to/build-an-agent.md)
