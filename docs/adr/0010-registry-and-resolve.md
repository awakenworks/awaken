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
    pub model: String,                         // ModelRegistry ID
    pub system_prompt: String,
    pub max_rounds: usize,
    pub tool_execution_mode: ToolExecutionMode,
    pub allowed_tools: Option<Vec<String>>,     // ToolRegistry IDs (None = all)
    pub excluded_tools: Option<Vec<String>>,
    pub plugin_ids: Vec<String>,               // PluginRegistry IDs
    pub permission_rules: Vec<PermissionRuleSpec>,
    pub stop_condition_specs: Vec<StopConditionSpec>,
    // Plugin-specific sections (opaque JSON per plugin)
    pub sections: HashMap<String, Value>,
}
```

No `Arc<dyn T>`, no trait objects. Pure data. Can be saved to JSON, loaded from config files, transmitted over network.

### D2: Five registries, one RegistrySet

| Registry | Key | Value | Purpose |
|----------|-----|-------|---------|
| `ToolRegistry` | tool_id | `Arc<dyn Tool>` | Available tools |
| `ModelRegistry` | model_id | `ModelEntry` | Model name + provider + default options |
| `ProviderRegistry` | provider_id | `Arc<dyn LlmExecutor>` | LLM API clients |
| `AgentRegistry` | agent_id | `AgentSpec` | Agent definitions |
| `PluginRegistry` | plugin_id | `Arc<dyn Plugin>` | All extensions: hooks, permissions, MCP, skills |

```rust
pub struct RegistrySet {
    pub agents: Arc<dyn AgentRegistry>,
    pub tools: Arc<dyn ToolRegistry>,
    pub models: Arc<dyn ModelRegistry>,
    pub providers: Arc<dyn ProviderRegistry>,
    pub plugins: Arc<dyn PluginRegistry>,
}
```

Each registry is a trait with `get(id) -> Option<&T>` and `ids() -> Vec<String>`. Default implementation: `MapXxxRegistry` backed by `HashMap`.

**No separate BehaviorRegistry or ExtensionRegistry.** Behaviors, extensions, MCP bridges, skill runtimes, permission checkers — all are `Plugin`. A Plugin that contributes tools registers them in `ToolRegistry` during build. A Plugin that contributes hooks does so via its `register()` method. The `PluginRegistry` is the single source of all pluggable functionality.

### D3: ModelEntry and provider resolution

```rust
pub struct ModelEntry {
    pub provider: String,           // ProviderRegistry ID
    pub model_name: String,         // Actual model name for API call
    pub default_options: ChatOptions,
}
```

Resolution: `model_id → ModelEntry → provider_id → Arc<dyn LlmExecutor>`.

### D4: resolve(agent_id) → ResolvedRun

```rust
pub fn resolve(
    registries: &RegistrySet,
    agent_id: &str,
) -> Result<ResolvedRun, ResolveError> {
    let spec = registries.agents.get(agent_id)?;
    let model = registries.models.get(&spec.model)?;
    let executor = registries.providers.get(&model.provider)?;

    // Resolve tools: snapshot + allow/exclude filter
    let tools = resolve_tools(registries, spec)?;

    // Resolve plugins: lookup by ID
    let plugins = resolve_plugins(registries, spec)?;

    Ok(ResolvedRun {
        spec: spec.clone(),
        executor: Arc::clone(executor),
        model_name: model.model_name.clone(),
        chat_options: model.default_options.clone(),
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
    pub model_name: String,
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
    let all_ids = registries.tools.ids();

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
            if let Some(tool) = registries.tools.get(id) {
                tools.insert(id.to_string(), Arc::clone(tool));
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
        registries.plugins.get(id)
            .cloned()
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
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub agents: Vec<AgentSpec>,
}

#[derive(Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,                   // "anthropic", "openai", "ollama"
    pub endpoint: Option<String>,
    pub auth: Option<AuthConfig>,
}

#[derive(Serialize, Deserialize)]
pub struct AuthConfig {
    pub env: Option<String>,            // Environment variable name
    pub token: Option<String>,          // Literal token (dev only)
}

#[derive(Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,               // ProviderConfig key
    pub model: String,                  // Actual model name
    #[serde(default)]
    pub options: ChatOptions,
}
```

Parse flow: `JSON/TOML → AgentSystemConfig → build registries → resolve agents`.

Plugins are not in the config file — they are registered programmatically (they hold trait object implementations). The config file covers data-only definitions (agents, models, providers).

### D8: ~~run_agent_loop accepts ResolvedRun~~ (superseded by ADR-0011 D6)

The loop runner accepts `&dyn AgentResolver` and resolves dynamically at startup and step boundaries. `ResolvedRun` is an internal type. The production entry point is `AgentRuntime::run(RunRequest)`.

### D9: ~~Handoff via orchestration layer~~ (superseded by ADR-0011 D6)

Handoff is resolved inside the loop at step boundaries via `ActiveAgentKey` check + `AgentResolver::resolve()`. No external orchestration needed.

## Consequences

### Replaces
- `AgentConfig` (struct with Arc<dyn T>) → `AgentSpec` (serializable) + `ResolvedRun` (runtime)
- `AgentProfile.active_plugins` → `AgentSpec.plugin_ids` (resolved at build time)
- Ad-hoc tool HashMap on agent → `ToolRegistry` + allow/exclude filtering
- Separate Behavior/Extension registries → unified `PluginRegistry`

### Implemented
- Registry traits (5): implemented
- `MapXxxRegistry` implementations (5): implemented
- `RegistrySet`: implemented
- `AgentSpec`: implemented
- `ModelEntry`: implemented (without ChatOptions)
- `resolve()` → `ResolvedRun` (internal): implemented
- `RegistrySet` implements `AgentResolver`: implemented
- `AgentRuntime::run(RunRequest)`: implemented

### To implement
- `AgentSystemConfig` (JSON config format)
- `ChatOptions` on `ModelEntry`

### Deferred
- `CompositeXxxRegistry` (merge multiple sources)
- Remote agent support (A2A protocol)
- ~~Plugin contrib during build (MCP tools, skill tools)~~ → Implemented via `register_tool()`
- Config file hot-reload
- Registry snapshot consistency
