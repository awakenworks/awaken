---
title: "添加 Plugin"
description: "当你需要通过 state key、phase hook、scheduled action 或 effect handler 扩展 agent 生命周期时，使用本页。"
---

当你需要通过 state key、phase hook、scheduled action 或 effect handler 扩展 agent 生命周期时，使用本页。

## 目的

Plugin 用于参与 agent 生命周期的行为，而不是只处理某一次 tool call。它比把 hook 分散在多个 tool 中更好，因为 state key、phase hook 和 effect handler 会被统一注册、在 build 时校验，并在每次 run 中一致应用。

当 context 依赖 runtime state 时，也应通过 plugin 管理。hook 从 `PhaseContext` 读取输入，返回 `StateCommand`，并调度 `AddContextMessage`，不要直接改 prompt。之后由 loop 统一负责 context 节流、排序和清理。

## 前置条件

- 已在 `Cargo.toml` 中添加 `awaken`
- 已了解 `Phase` 与 `StateKey`

## 步骤

1. 定义一个状态键：

```rust
use awaken::{StateKey, KeyScope, MergeStrategy, StateError, JsonValue};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub entries: Vec<String>,
}

pub struct AuditLogKey;

impl StateKey for AuditLogKey {
    type Value = AuditLog;
    const KEY: &'static str = "audit_log";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;

    type Update = AuditLog;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }

    fn encode(value: &Self::Value) -> Result<JsonValue, StateError> {
        serde_json::to_value(value).map_err(|e| StateError::KeyEncode { key: Self::KEY.into(), message: e.to_string() })
    }

    fn decode(json: JsonValue) -> Result<Self::Value, StateError> {
        serde_json::from_value(json).map_err(|e| StateError::KeyDecode { key: Self::KEY.into(), message: e.to_string() })
    }
}
```

2. 实现一个 phase hook：

```rust
use async_trait::async_trait;
use awaken::{PhaseHook, PhaseContext, StateCommand, StateError};

pub struct AuditHook;

#[async_trait]
impl PhaseHook for AuditHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut log = ctx.state::<AuditLogKey>().cloned().unwrap_or(AuditLog {
            entries: Vec::new(),
        });
        log.entries.push(format!("Phase executed at {:?}", ctx.phase));
        let mut cmd = StateCommand::new();
        cmd.update::<AuditLogKey>(log);
        Ok(cmd)
    }
}
```

3. 实现 `Plugin` trait：

```rust
use awaken::{Plugin, PluginDescriptor, PluginRegistrar, Phase, StateError, StateKeyOptions, KeyScope};

pub struct AuditPlugin;

impl Plugin for AuditPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "audit" }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<AuditLogKey>(StateKeyOptions {
            scope: KeyScope::Run,
            ..Default::default()
        })?;

        registrar.register_phase_hook(
            "audit",
            Phase::AfterInference,
            AuditHook,
        )?;

        Ok(())
    }
}
```

4. 在 runtime 上注册插件，并在 agent 上激活它：

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentSpec, AgentRuntimeBuilder};

let mut spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("audit");
spec.plugin_ids.push("audit".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("audit", Arc::new(AuditPlugin))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"))
    .build()?;
```

`plugin_ids` 负责加载插件；`with_hook_filter` 只过滤已经加载的插件所提供的
hook、tool 和 request transform。

`with_plugin` 把插件注册进 runtime 的 plugin registry —— 与 tool、agent、model、
provider 背后是同一套注册模型。见 [智能体解析](/awaken/zh-cn/explanation/agent-resolution/)。

## 注入 context 和控制工具

当 plugin 需要改变模型看到的内容时，返回命令，不要直接改 prompt 或 store：

- 通过 `PhaseContext` 读取不可变 snapshot；
- 返回 `StateCommand`；
- 用 `AddContextMessage` 注入面向模型的上下文；
- 用 `IncludeOnlyTools` 或 `ExcludeTool` 收窄可用工具；
- 当 plugin 拥有该策略时，用 `InferenceOverrideState` 调整推理参数。

这比在 hook 中直接修改共享状态更好，因为 loop 统一负责排序、节流、冲突处理和
commit 时机。命令类型参考
`crates/awaken-runtime/src/agent/state/loop_actions.rs`。

## 验证

运行 agent 后查看状态快照，确认 `audit_log` 中出现了 hook 写入的条目。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `StateError::KeyAlreadyRegistered` | 多个插件注册了同一个 key | 保证每个 `StateKey::KEY` 全局唯一 |
| `StateError::UnknownKey` | 读取了未注册的状态键 | 确保注册该 key 的插件已激活 |
| hook 没有执行 | 插件未加载或 hook 被过滤 | 把插件 ID 加到 `plugin_ids`；使用 hook filter 时也加入 `with_hook_filter` |

## 相关示例

`crates/awaken-ext-observability/`

## 代码参考

- `crates/awaken-doctest/examples/plugin_registrar.rs` —— 最小 `Plugin` trait 实现。
- `crates/awaken-doctest/examples/state_command.rs` —— 带 mutation、effect 和 scheduled action 的 `StateCommand`。
- `crates/awaken-doctest/examples/effect_spec.rs` —— typed effect spec 形状。
- `crates/awaken-doctest/examples/scheduled_action.rs` —— scheduled action spec 与 handler 形状。
- `crates/awaken-runtime/src/agent/state/loop_actions.rs` —— context message、tool filter 与 inference override state commands。

## 关键文件

- `crates/awaken-runtime/src/plugins/lifecycle.rs`
- `crates/awaken-runtime/src/plugins/registry.rs`
- `crates/awaken-runtime/src/hooks/phase_hook.rs`

## 相关

- [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)
- [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/)
