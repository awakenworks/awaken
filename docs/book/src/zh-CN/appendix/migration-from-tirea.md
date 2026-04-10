# 从 Tirea 迁移

Awaken 不是对 tirea 的增量升级，而是一次从头重写。本页帮助你把 tirea 0.5 的核心概念映射到 Awaken。

## Crate 映射

| tirea 0.5 | awaken | 说明 |
|-----------|--------|------|
| `tirea-state` | `awaken-contract` | 状态引擎并入 contract crate |
| `tirea-state-derive` | — | 移除；直接实现 `StateKey` |
| `tirea-contract` | `awaken-contract` | 契约统一到一个 crate |
| `tirea-agentos` | `awaken-runtime` | 运行时改名 |
| `tirea-store-adapters` | `awaken-stores` | 存储适配层改名 |
| `tirea-agentos-server` | `awaken-server` | server 改名，协议已并入 |
| `tirea-protocol-ai-sdk-v6` | `awaken-server::protocols::ai_sdk_v6` | 合并到 server |
| `tirea-protocol-ag-ui` | `awaken-server::protocols::ag_ui` | 合并到 server |
| `tirea-protocol-acp` | `awaken-server::protocols::acp` | 合并到 server |
| `tirea-extension-permission` | `awaken-ext-permission` | 改名 |
| `tirea-extension-observability` | `awaken-ext-observability` | 改名 |
| `tirea-extension-skills` | `awaken-ext-skills` | 改名 |
| `tirea-extension-mcp` | `awaken-ext-mcp` | 改名 |
| `tirea-extension-handoff` | `awaken-runtime::extensions::handoff` | 并入 runtime |
| `tirea-extension-a2ui` | `awaken-ext-generative-ui` | 改名 |
| `tirea` | `awaken` | 门面 crate 改名 |

## 状态模型

tirea 使用 derive-macro 状态系统；Awaken 改为 trait-based：

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

| tirea 概念 | Awaken 对应 |
|---------------|-------------|
| `StateSlot` derive | `StateKey` trait |
| `ScopeDomain::Run` | `KeyScope::Run` |
| `ScopeDomain::Thread` | `KeyScope::Thread` |
| `ScopeDomain::Global` | 已移除，通常改为 `Thread` + `ProfileStore` |
| 可变 state 直接读写 | 不可变快照 + `StateCommand` |

## Action 系统

tirea 使用按 phase 分类的 action enum；Awaken 统一改为 `ScheduledActionSpec` + handler：

```rust,ignore
// tirea 0.5
BeforeInferenceAction::AddContextMessage(
    ContextMessage::system("key", "content")
)

// awaken
let mut cmd = StateCommand::new();
cmd.schedule_action::<AddContextMessage>(
    ContextMessage::system("key", "content"),
)?;
```

需要拦截工具调用时，不再调度 `ToolInterceptAction`；应实现 `ToolGateHook`，并返回 `ToolInterceptPayload::Block` / `Suspend` / `SetResult`。

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

## Runtime Builder

```rust,ignore
// tirea 0.5
let runtime = AgentOsBuilder::new()
    .agent(agent_spec)
    .tool("search", SearchTool)
    .extension(PermissionExtension::new(rules))
    .build()?;

// awaken
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_plugin("permissions", Arc::new(PermissionPlugin::new(rules)))
    .build()?;
```

## 被移除的概念

- `StateSlot` derive macro
- `Global` scope
- `RuntimeEffect`
- `EffectLog` / `ScheduledActionLog`
- `ConfigSlot`
- `AgentProfile`
- `ExtensionContext` 的动态激活

## 新增的概念

- `PluginRegistrar`
- 带 `StateCommand` 的 `ToolOutput`
- `ToolGateHook`
- `CircuitBreaker`
- `Mailbox`
- `EventReplayBuffer`
- `DeferredToolsPlugin`
- `ProfileStore`

## 快速检查清单

- [ ] `Cargo.toml` 里把 `tirea` 替换成 `awaken`
- [ ] `use tirea::*` 改成 `use awaken::prelude::*`
- [ ] `#[derive(StateSlot)]` 改成 `impl StateKey`
- [ ] `Extension` 改成 `Plugin`
- [ ] `TypedTool` 改成 `Tool`
- [ ] action enum 改成 `cmd.schedule_action::<ActionType>(...)`；工具拦截迁到 `ToolGateHook`
- [ ] `AgentOsBuilder` 改成 `AgentRuntimeBuilder`
- [ ] store 和 server import 切到 `awaken-stores`、`awaken-server`
