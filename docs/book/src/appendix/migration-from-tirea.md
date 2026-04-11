# Migrating from Tirea

Awaken is a ground-up rewrite of tirea, not an incremental upgrade. This guide
maps tirea 0.5 concepts to their awaken equivalents for developers porting
existing agents.

The tirea 0.5 source is archived on the
[`tirea-0.5`](https://github.com/AwakenWorks/awaken/tree/tirea-0.5) branch.

## Crate Mapping

| tirea 0.5 | awaken | Notes |
|-----------|--------|-------|
| `tirea-state` | `awaken-contract` | State engine merged into contract crate |
| `tirea-state-derive` | — | Removed; use `StateKey` trait directly |
| `tirea-contract` | `awaken-contract` | Merged types + traits into one crate |
| `tirea-agentos` | `awaken-runtime` | Renamed; same role (execution engine) |
| `tirea-store-adapters` | `awaken-stores` | Renamed |
| `tirea-agentos-server` | `awaken-server` | Renamed; protocols now inline |
| `tirea-protocol-ai-sdk-v6` | `awaken-server::protocols::ai_sdk_v6` | Merged into server crate |
| `tirea-protocol-ag-ui` | `awaken-server::protocols::ag_ui` | Merged into server crate |
| `tirea-protocol-acp` | `awaken-server::protocols::acp` | Merged into server crate |
| `tirea-extension-permission` | `awaken-ext-permission` | Renamed |
| `tirea-extension-observability` | `awaken-ext-observability` | Renamed |
| `tirea-extension-skills` | `awaken-ext-skills` | Renamed |
| `tirea-extension-mcp` | `awaken-ext-mcp` | Renamed |
| `tirea-extension-handoff` | `awaken-runtime::extensions::handoff` | Merged into runtime |
| `tirea-extension-a2ui` | `awaken-ext-generative-ui` | Renamed |
| `tirea` (facade) | `awaken` | Renamed |

**Key structural change:** tirea had 16 crates; awaken has 13. Protocol crates
were merged into `awaken-server`, the state derive macro was removed, and
`tirea-state` + `tirea-contract` were unified into `awaken-contract`.

## State Model

tirea used a derive-macro-based state system. Awaken uses a trait-based approach.

```rust,ignore
// tirea 0.5
#[derive(StateSlot)]
#[state(key = "my_plugin.counter", scope = "run")]
struct Counter(u32);

// awaken
use awaken::prelude::*;

pub struct Counter;

impl StateKey for Counter {
    const KEY: &'static str = "my_plugin.counter";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    type Value = u32;
    type Update = u32;

    fn apply(value: &mut u32, update: u32) {
        *value = update;
    }
}
```

| tirea concept | awaken equivalent |
|---------------|-------------------|
| `StateSlot` derive | `StateKey` trait impl |
| `ScopeDomain::Run` | `KeyScope::Run` (default) |
| `ScopeDomain::Thread` | `KeyScope::Thread` |
| `ScopeDomain::Global` | Removed (use `KeyScope::Thread` + `ProfileStore`) |
| `state.get::<T>()` | `snapshot.get::<T>()` (via `ctx.state::<T>()` in hooks) |
| `state.set(value)` | `cmd.update::<T>(value)` (via `StateCommand`) |
| Mutable state access | Immutable snapshots + `StateCommand` returns |

**Key change:** State is never mutated directly. Hooks read from immutable
snapshots and return `StateCommand` with updates. The runtime applies all
updates atomically after the gather phase.

## Action System

tirea used typed action enums per phase. Awaken uses `ScheduledActionSpec` with
handlers.

```rust,ignore
// tirea 0.5
BeforeInferenceAction::AddContextMessage(
    ContextMessage::system("key", "content")
)

// awaken
use awaken::prelude::*;

let mut cmd = StateCommand::new();
cmd.schedule_action::<AddContextMessage>(
    ContextMessage::system("key", "content"),
)?;
```

| tirea action | awaken action | Notes |
|-------------|---------------|-------|
| `BeforeInferenceAction::AddContextMessage` | `AddContextMessage` | Same semantics |
| `BeforeInferenceAction::SetInferenceOverride` | `SetInferenceOverride` | Same semantics |
| `BeforeInferenceAction::ExcludeTool` | `ExcludeTool` | Same semantics |
| `BeforeToolExecuteAction::Block` | `ToolGateHook` returning `ToolInterceptPayload::Block` | Interception moved to `ToolGate` |
| `BeforeToolExecuteAction::Suspend` | `ToolGateHook` returning `ToolInterceptPayload::Suspend` | Interception moved to `ToolGate` |
| `AfterToolExecuteAction::AddMessage` | `AddContextMessage` | Generalized |

See [Scheduled Actions](../reference/scheduled-actions.md) for the full list.

## Plugin Trait

```rust,ignore
// tirea 0.5
impl Extension for MyPlugin {
    fn name(&self) -> &str { "my_plugin" }
    fn register(&self, ctx: &mut ExtensionContext) -> Result<()> {
        ctx.add_hook(Phase::BeforeInference, MyHook);
        Ok(())
    }
}

// awaken
impl Plugin for MyPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "my_plugin" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_phase_hook("my_plugin", Phase::BeforeInference, MyHook)?;
        r.register_key::<MyState>(StateKeyOptions::default())?;
        Ok(())
    }
}
```

| tirea | awaken | Notes |
|-------|--------|-------|
| `Extension` trait | `Plugin` trait | Renamed |
| `ExtensionContext` | `PluginRegistrar` | More structured; explicit key/tool/hook registration |
| `ctx.add_hook()` | `r.register_phase_hook()` | Requires plugin_id parameter |
| — | `r.register_key::<T>()` | New: state keys must be declared at registration |
| — | `r.register_tool()` | New: plugin-scoped tools |
| — | `r.register_scheduled_action::<A, _>(handler)` | New: custom action handlers |

## Tool Trait

```rust,ignore
// tirea 0.5
#[async_trait]
impl TypedTool for MyTool {
    type Args = MyArgs;
    async fn call(&self, args: Self::Args, ctx: ToolContext) -> ToolOutput {
        ToolOutput::success(json!({"result": 42}))
    }
}

// awaken
#[async_trait]
impl Tool for MyTool {
    fn descriptor(&self) -> ToolDescriptor { ... }
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("my_tool", json!({"result": 42})).into())
    }
}
```

| tirea | awaken | Notes |
|-------|--------|-------|
| `TypedTool` with `type Args` | `Tool` with `Value` args | No generic Args; validate in `execute()` |
| `ToolOutput` (direct return) | `ToolOutput` (result + optional `StateCommand`) | New: tools can schedule actions |
| `ToolContext` | `ToolCallContext` | Renamed; adds `state()`, `profile_access()` |

## Runtime Builder

```rust,ignore
// tirea 0.5
let runtime = AgentOsBuilder::new()
    .agent(agent_spec)
    .tool("search", SearchTool)
    .extension(PermissionExtension::new(rules))
    .build()?;

// awaken
let rules_config = PermissionRulesConfig::from_file("permissions.yaml")?;
let mut agent_spec = agent_spec;
agent_spec.plugin_ids.push("permission".into());
agent_spec.set_config::<PermissionConfigKey>(rules_config)?;

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_plugin("permission", Arc::new(PermissionPlugin))
    .build()?;
```

| tirea | awaken | Notes |
|-------|--------|-------|
| `AgentOsBuilder` | `AgentRuntimeBuilder` | Renamed |
| `.agent(spec)` | `.with_agent_spec(spec)` | Consistent `with_` prefix |
| `.tool(id, impl)` | `.with_tool(id, Arc<dyn Tool>)` | Explicit Arc wrapping |
| `.extension(ext)` | `.with_plugin(id, Arc<dyn Plugin>)` | Renamed; requires ID |
| `.build()` | `.build()` | Same |

## Concepts Removed

The following tirea concepts have no awaken equivalent:

| tirea concept | Reason removed |
|---------------|----------------|
| `StateSlot` derive macro | Trait impl is simpler and doesn't require proc-macro |
| `Global` scope | Thread scope + `ProfileStore` covers this |
| `RuntimeEffect` | Replaced by `StateCommand` effects |
| `EffectLog` / `ScheduledActionLog` | Replaced by tracing |
| `ConfigSlot` | Replaced by typed `AgentSpec` sections for static config, plus `ConfigStore` for runtime-managed config |
| `AgentProfile` | Merged into `AgentSpec` |
| `ExtensionContext` live activation | Replaced by static `plugin_ids` on `AgentSpec` |

## Concepts Added

| awaken concept | Description |
|----------------|-------------|
| `PluginRegistrar` | Structured registration (keys, tools, hooks, actions) |
| `ToolOutput` with `StateCommand` | Tools can schedule actions as side-effects |
| `ToolGateHook` | Pure interception hook for Block / Suspend / SetResult decisions |
| `CircuitBreaker` | Per-model LLM failure protection |
| `Mailbox` | Durable job queue with lease-based claim |
| `EventReplayBuffer` | SSE reconnection with frame replay |
| `DeferredToolsPlugin` | Lazy tool loading with probability model |
| `ProfileStore` | Cross-session persistent state |

## Quick Checklist

- [ ] Replace `tirea` dependency with `awaken` in `Cargo.toml`
- [ ] Replace `use tirea::*` with `use awaken::prelude::*`
- [ ] Convert `#[derive(StateSlot)]` to `impl StateKey for ...`
- [ ] Convert `Extension` impls to `Plugin` impls with `PluginRegistrar`
- [ ] Convert `TypedTool` impls to `Tool` impls
- [ ] Replace action enum variants with `cmd.schedule_action::<ActionType>(...)` or `ToolGateHook` for tool interception
- [ ] Replace `AgentOsBuilder` with `AgentRuntimeBuilder`
- [ ] Update store imports: `tirea_store_adapters` → `awaken_stores`
- [ ] Update server imports: protocol crates → `awaken_server::protocols::*`
