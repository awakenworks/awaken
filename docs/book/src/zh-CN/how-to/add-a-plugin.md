# 添加 Plugin

当你需要通过 state key、phase hook、scheduled action 或 effect handler 扩展 agent 生命周期时，使用本页。

## 前置条件

- 已在 `Cargo.toml` 中添加 `awaken`
- 已了解 `Phase` 与 `StateKey`

## 步骤

1. 定义一个状态键：

```rust,ignore
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

```rust,ignore
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

```rust,ignore
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

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentSpec, AgentRuntimeBuilder};

let spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("audit");

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("audit", Arc::new(AuditPlugin))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .build()?;
```

`with_hook_filter` 会为该 agent 激活指定插件的 phase hook。

## 验证

运行 agent 后查看状态快照，确认 `audit_log` 中出现了 hook 写入的条目。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `StateError::KeyAlreadyRegistered` | 多个插件注册了同一个 key | 保证每个 `StateKey::KEY` 全局唯一 |
| `StateError::UnknownKey` | 读取了未注册的状态键 | 确保注册该 key 的插件已激活 |
| hook 没有执行 | `with_hook_filter` 中没有对应插件 ID | 把插件 ID 加到 agent spec |

## 相关示例

`crates/awaken-ext-observability/`

## 关键文件

- `crates/awaken-runtime/src/plugins/lifecycle.rs`
- `crates/awaken-runtime/src/plugins/registry.rs`
- `crates/awaken-runtime/src/hooks/phase_hook.rs`

## 相关

- [构建 Agent](./build-an-agent.md)
- [添加 Tool](./add-a-tool.md)
