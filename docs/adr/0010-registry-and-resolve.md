# ADR-0010: Registry, AgentSpec, and Runtime Resolution

- **Status**: Accepted
- **Date**: 2026-03-22
- **Depends on**: ADR-0001, ADR-0009

## Context

The current `AgentConfig` holds `Arc<dyn Tool>`, `Arc<dyn LlmExecutor>`, and `Arc<dyn ToolExecutor>` directly. This makes agent definitions non-serializable, non-persistable, and tightly coupled to concrete implementations. Handoff requires passing live instances; config files cannot describe agents; agents cannot be created from stored definitions.

Reference: uncarve's `AgentDefinition` + `RegistrySet` + `resolve()` pattern — all components referenced by ID string, resolved at runtime from registries.

## Decisions

### D1: AgentSpec — serializable, ID-only agent definition

```rust
#[derive(Serialize, Deserialize)]
pub struct AgentSpec {
    pub id: String,
    pub description: Option<String>,           // Human-readable catalog/delegate text
    pub backend: AgentBackendSpec,             // Execution backend: awaken, a2a, ...
    pub model_id: String,                      // ModelRegistry ID
    pub system_prompt: String,
    pub max_rounds: usize,
    pub tool_execution_mode: ToolExecutionMode,
    pub allowed_tools: Option<Vec<String>>,     // ToolRegistry IDs (None = all)
    pub excluded_tools: Option<Vec<String>>,
    pub plugin_ids: Vec<String>,               // PluginSource IDs
    pub permission_rules: Vec<PermissionRuleSpec>,
    pub stop_condition_specs: Vec<StopConditionSpec>,
    // Plugin-specific sections (opaque JSON per plugin)
    pub sections: HashMap<String, Value>,
}

pub struct AgentBackendSpec {
    pub kind: String,                          // "awaken" for in-process execution
    pub version: u32,                          // Backend config schema version
    pub config: Value,                         // Backend-specific config object
}
```

No `Arc<dyn T>`, no trait objects. Pure data. Can be saved to JSON, loaded from config files, transmitted over network.

`backend` is the canonical execution selector. Local in-process agents use
`kind = "awaken"` and keep the existing `model_id` / `system_prompt` fields
for compatibility with the model/provider resolution path. Remote A2A agents
use `kind = "a2a"` and carry endpoint-shaped backend config, so they do not
require a local model or prompt. The legacy top-level `endpoint` field is still
accepted and normalized into `backend` to preserve existing A2A discovery and
operator tooling.

### D2: Five registries, one RegistrySet

| Registry | Key | Value | Purpose |
|----------|-----|-------|---------|
| `ToolRegistry` | tool_id | `Arc<dyn Tool>` | Available tools |
| `ModelRegistry` | model_id | `ModelSpec` | Provider id, upstream model name, capability, pricing |
| `ProviderRegistry` | provider_id | `Arc<dyn LlmExecutor>` | LLM API clients |
| `AgentRegistry` | agent_id | `AgentSpec` | Agent definitions |
| `PluginSource` | plugin_id | `Arc<dyn Plugin>` | All extensions: hooks, permissions, MCP, skills |

```rust
pub struct RegistrySet {
    pub agents: Arc<dyn AgentRegistry>,
    pub tools: Arc<dyn ToolRegistry>,
    pub models: Arc<dyn ModelRegistry>,
    pub providers: Arc<dyn ProviderRegistry>,
    pub plugins: Arc<dyn PluginSource>,
}
```

Each registry is a trait with named lookup methods. Serializable specs are
returned as owned values so resolution can cross async and task boundaries
without borrowing registry internals:

- `ModelRegistry::get_model(&str) -> Option<ModelSpec>`
- `AgentSpecRegistry::get_agent(&str) -> Option<AgentSpec>`
- `ToolRegistry::get_tool(&str) -> Option<Arc<dyn Tool>>`
- `ProviderRegistry::get_provider(&str) -> Option<Arc<dyn LlmExecutor>>`
- `PluginSource::get_plugin(&str) -> Option<Arc<dyn Plugin>>`

List methods use matching plural names such as `model_ids()` and
`agent_ids()`. Default implementations are map-backed registries.

**No separate BehaviorRegistry or ExtensionRegistry.** Behaviors, extensions, MCP bridges, skill runtimes, permission checkers — all are `Plugin`. A Plugin that contributes tools registers them in `ToolRegistry` during build. A Plugin that contributes hooks does so via its `register()` method. `PluginSource` is the lookup source for pluggable functionality.

### D3: ModelSpec and provider resolution

```rust
pub struct ModelSpec {
    pub id: String,
    pub provider_id: String,        // ProviderRegistry ID
    pub upstream_model: String,     // Actual model name for API call
    // Intrinsic capabilities (all optional).
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub modalities: Modalities,
    pub knowledge_cutoff: Option<String>,
    // Per-million-token pricing in USD (all optional).
    pub input_token_price_per_million_usd: Option<f64>,
    pub output_token_price_per_million_usd: Option<f64>,
}

pub struct ProviderSpec {
    pub id: String,
    pub adapter: String,                       // genai adapter kind
    pub api_key: Option<RedactedString>,       // redacted in Debug, zeroized on drop
    pub base_url: Option<String>,
    pub timeout_secs: u64,
    pub adapter_options: BTreeMap<String, Value>,  // non-secret adapter knobs
}
```

Local Awaken resolution: `model_id → ModelSpec → provider_id →
Arc<dyn LlmExecutor>`. Non-Awaken backends skip local model/provider binding
and resolve through the registered agent-backend factory for their `kind`.

`ModelRegistry::get_model(&str) -> Option<ModelSpec>` returns an owned
`ModelSpec`. Earlier revisions of this ADR split addressing (`provider_id`,
`upstream_model`) into a separate runtime mirror type; that mirror is removed.
Addressing, capability, and pricing now travel together through the same struct
from config persistence to resolution.

**Duplicate-id rejection.** A `RegistrySpec` carrying two `ModelSpec`
entries with the same `id` is rejected by `validate_registry_spec` (in
`awaken-contract`) and again by registry lifecycle (`apply` /
`rebuild_agent_model_provider_registries` in `awaken-runtime`). The
second check catches programmatic builder paths that bypass the spec
validator.

**Capability-aware context policy.** At resolve time, the agent's
`ContextWindowPolicy` is folded against the resolved `ModelSpec` via
`effective_policy(agent.context_policy, model)`. When `context_window`
and/or `max_output_tokens` are set, the policy is clamped so
`max_output_tokens ≤ max_context_tokens ≤ model.context_window`;
`autocompact_threshold` is further clamped to the usable input budget
(`max_context_tokens − max_output_tokens`) and dropped to `None` if no
usable input budget remains. When the capability fields are `None` the
policy passes through unchanged.

**Spec invariants** (enforced at the type level rather than by convention):

- **Secrets are typed.** `api_key` is wrapped in `RedactedString` so `Debug` /
  `Display` cannot leak the value. The plaintext is reachable only via
  `expose_secret()`; `preview()` returns a head-4/tail-4 mask suitable for
  operator-facing logs. The wire format remains a plain JSON string.
- **`adapter_options` is non-secret.** Adapter-specific knobs (custom HTTP
  headers, organization IDs, API versions) live here and may be inspected
  freely. Credentials must use `api_key`. A future cross-adapter
  authentication abstraction (`AuthStrategy`) is deferred to a 0.x version
  bump; until then secrets that do not fit `api_key` are out of scope.
- **Empty strings deserialize as `None`.** `api_key` and `base_url` accept
  `""` from JSON and treat it as unset, removing the need for callers to
  strip empty values.
- **Forward-compatible options.** Unknown keys in `adapter_options` are
  ignored at build time so older binaries do not reject newer specs.

**Amendment: explicit environment credentials.** Bearer providers without
`api_key` are rejected by default. `adapter_options.allow_env_credentials =
true` may explicitly delegate bearer lookup to the host environment. The option
is non-secret and must be a boolean.

### D4: resolve(agent_id) → ResolvedRun

```rust
pub fn resolve(
    registries: &RegistrySet,
    agent_id: &str,
) -> Result<ResolvedRun, ResolveError> {
    let spec = registries.agents.get_agent(agent_id)?;
    let model = registries.models.get_model(&spec.model_id)?;
    let executor = registries.providers.get_provider(&model.provider_id)?;

    // Resolve tools: snapshot + allow/exclude filter
    let tools = resolve_tools(registries, spec)?;

    // Resolve plugins: lookup by ID
    let plugins = resolve_plugins(registries, spec)?;

    Ok(ResolvedRun {
        spec: spec.clone(),
        executor: Arc::clone(executor),
        upstream_model: model.upstream_model.clone(),
        tools,
        plugins,
    })
}
```

**ResolvedRun** — not serializable, holds live references:

```rust
pub struct ResolvedRun {
    pub spec: AgentSpec,
    pub executor: Arc<dyn LlmExecutor>,
    pub upstream_model: String,
    pub chat_options: ChatOptions,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub plugins: Vec<Arc<dyn Plugin>>,
}
```

### D5: Tool resolution with allow/exclude filtering

```rust
fn resolve_tools(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<HashMap<String, Arc<dyn Tool>>, ResolveError> {
    let all_ids = registries.tools.tool_ids();

    let included: HashSet<&str> = match &spec.allowed_tools {
        Some(allow) => allow.iter().map(|s| s.as_str()).collect(),
        None => all_ids.iter().map(|s| s.as_str()).collect(),
    };

    let excluded: HashSet<&str> = spec.excluded_tools
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let mut tools = HashMap::new();
    for id in included {
        if !excluded.contains(id) {
            if let Some(tool) = registries.tools.get_tool(id) {
                tools.insert(id.to_string(), tool);
            }
        }
    }
    Ok(tools)
}
```

### D6: Plugin = single extension unit

All pluggable functionality goes through `Plugin`. A plugin may contribute:
- Phase hooks (via `register()` → `register_phase_hook()`)
- Tool permission checkers (via `register()` → `register_tool_permission()`)
- State keys (via `register()` → `register_key()`)
- Scheduled action handlers / effect handlers
- **Tools** (via `register()` → `register_tool()`) — per-spec scoped, only available to agents that activate the plugin

`AgentSpec.plugin_ids` lists which plugins are active for this agent. At resolve time, plugins are looked up by ID and installed into the PhaseRuntime. This replaces `AgentProfile.active_plugins`.

```rust
fn resolve_plugins(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<Vec<Arc<dyn Plugin>>, ResolveError> {
    spec.plugin_ids.iter().map(|id| {
        registries.plugins
            .get_plugin(id)
            .ok_or(ResolveError::PluginNotFound(id.clone()))
    }).collect()
}
```

A plugin that bridges MCP servers contributes tools to `ToolRegistry` and hooks to its own `register()`. A plugin that provides skills registers tools directly via `register_tool()` in its `Plugin::register()` method. No separate Extension/Skill/MCP registry — all are Plugins that contribute to standard registries. See ADR-0013 for recommended extension organization patterns.

### D7: AgentSystemConfig — serializable config file format

```rust
#[derive(Serialize, Deserialize)]
pub struct AgentSystemConfig {
    #[serde(default)]
    pub models: Vec<ModelSpec>,
    #[serde(default)]
    pub agents: Vec<AgentSpec>,
}
```

Parse flow: `JSON/TOML → AgentSystemConfig + provider executors → build registries → resolve agents`.

Providers and plugins are registered programmatically in this low-level helper because they hold trait object implementations. The managed server config path uses serializable `ProviderSpec`, `ModelSpec`, and `AgentSpec` documents, then compiles them into the same runtime registries before resolution.

### D8: ~~run_agent_loop accepts ResolvedRun~~ (superseded by ADR-0011 D6)

The loop runner accepts `&dyn AgentResolver` and resolves dynamically at startup and step boundaries. `ResolvedRun` is an internal type. The production entry point is `AgentRuntime::run(RunActivation)`.

### D9: ~~Handoff via orchestration layer~~ (superseded by ADR-0011 D6)

Handoff is resolved inside the loop at step boundaries via `ActiveAgentKey` check + `AgentResolver::resolve()`. No external orchestration needed.

### D10: ExperimentResolver Step (extension by ADR-0031)

ADR-0031 adds a single new step to `resolve(...)` that runs after canonical
resolution but before the resolved entity is materialised:

```
canonical resolve  →  ExperimentResolver  →  finalise resolved entity
```

The step looks up an active `Ramping` experiment for the target, picks a
variant by consistent hash on the experiment's `bucket_key`, and substitutes
the variant's content into the result. Substitution is opaque: the output is
still a regular `ResolvedAgent` / `ToolDescriptor` whose `invoke()` and
inference paths are unchanged. When no experiment matches the target, the
step is a no-op and resolution proceeds as decided in D4–D6.

The resolver also stamps `experiment_id` + `variant_name` onto the resolution
context so `awaken-ext-observability` hooks can fill the corresponding
`SpanContext` fields (ADR-0030 D2). This is the only way variant assignment
reaches the trace stream; downstream code never sees the experiment record.

The resolve algorithm in D4 is unchanged. ExperimentResolver is a single
deterministic, constant-time step with no side effects beyond span
attribution.

## Consequences

### Replaces
- `AgentConfig` (struct with Arc<dyn T>) → `AgentSpec` (serializable) + `ResolvedRun` (runtime)
- `AgentProfile.active_plugins` → `AgentSpec.plugin_ids` (resolved at build time)
- Ad-hoc tool HashMap on agent → `ToolRegistry` + allow/exclude filtering
- Separate Behavior/Extension registries → unified `PluginSource`

### Implemented
- Registry traits (5): implemented
- `MapXxxRegistry` implementations (5): implemented
- `RegistrySet`: implemented
- `AgentSpec`: implemented
- `ModelSpec` (unified addressing + capability + pricing): implemented
- `resolve()` → `ResolvedRun` (internal): implemented
- `RegistrySet` implements `AgentResolver`: implemented
- `AgentRuntime::run(RunActivation)`: implemented

### Deferred
- `CompositeXxxRegistry` (merge multiple sources)
- Remote agent support (A2A protocol)
- ~~Plugin contrib during build (MCP tools, skill tools)~~ → Implemented via `register_tool()`
- Config file hot-reload
- Registry snapshot consistency
