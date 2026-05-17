---
title: "Effects"
description: "Effect 是类型化的、fire-and-forget 的副作用事件。和会在 phase 收敛循环内部执行、还能继续级联的 scheduled actions 不同，effect 在 commit 之后才分发，而且 handler 不能再返回新的 StateCommand。"
---

Effect 是类型化的、fire-and-forget 的副作用事件。和会在 phase 收敛循环内部执行、还能继续级联的 [scheduled actions](/zh-cn/reference/scheduled-actions/) 不同，effect 在 commit 之后才分发，而且 handler 不能再返回新的 `StateCommand`。

常见用途：审计日志、外部 webhook、指标上报、通知投递。

## EffectSpec trait

```rust
pub trait EffectSpec: 'static + Send + Sync {
    const KEY: &'static str;
    type Payload: Serialize + DeserializeOwned + Send + Sync + 'static;
}
```

约定 `KEY` 采用 `"<plugin>.<effect_name>"` 这类全局唯一名字。

## 发出 effect

通过 `StateCommand::emit::<E>(payload)` 发出：

```rust
use awaken::{StateCommand, StateError};

async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
    let mut cmd = StateCommand::new();
    cmd.emit::<AuditEffect>(AuditPayload {
        action: "user_login".into(),
        actor: "agent-1".into(),
    })?;
    Ok(cmd)
}
```

## TypedEffect 封装

运行时内部用 `TypedEffect` 做类型擦除：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEffect {
    pub key: String,
    pub payload: JsonValue,
}
```

- `TypedEffect::from_spec::<E>(payload)`：把 typed payload 序列化成 `TypedEffect`
- `TypedEffect::decode::<E>()`：把 JSON 反序列化回具体 payload

## 注册 effect handler

```rust
#[async_trait]
pub trait TypedEffectHandler<E>: Send + Sync + 'static
where
    E: EffectSpec,
{
    async fn handle_typed(
        &self,
        payload: E::Payload,
        snapshot: &Snapshot,
    ) -> Result<(), String>;
}
```

关键点：

- handler 收到的是 post-commit `Snapshot`
- 返回值是 `Result<(), String>`，不是 `StateError`
- handler 失败会被记录，但不会回滚已提交的状态

注册方式：

```rust
fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_effect::<AuditEffect, _>(AuditEffectHandler)?;
    Ok(())
}
```

## 分发生命周期

1. hook / action handler / tool 调用 `emit::<E>()`
2. `submit_command` 校验所有 effect key 是否都有 handler
3. 状态变更提交到 store
4. 依次调用每个 handler
5. handler 失败只记录，不影响后续 effect

```text
Hook / Tool                         Runtime
    |                                 |
    |-- StateCommand (with effects) ->|
    |                                 |-- validate all effect keys
    |                                 |-- commit state mutations
    |                                 |-- dispatch effects sequentially
    |<--------------------------------|
```

## 示例

定义 effect：

```rust
use awaken::EffectSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditPayload {
    pub action: String,
    pub actor: String,
}

pub struct AuditEffect;

impl EffectSpec for AuditEffect {
    const KEY: &'static str = "audit.record";
    type Payload = AuditPayload;
}
```

在 hook 中发出：

```rust
use async_trait::async_trait;
use awaken::{PhaseContext, PhaseHook, StateCommand, StateError};

pub struct AuditHook;

#[async_trait]
impl PhaseHook for AuditHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.emit::<AuditEffect>(AuditPayload {
            action: "phase_entered".into(),
            actor: "system".into(),
        })?;
        Ok(cmd)
    }
}
```

处理 effect：

```rust
use async_trait::async_trait;
use awaken::{Snapshot, TypedEffectHandler};

pub struct AuditEffectHandler;

#[async_trait]
impl TypedEffectHandler<AuditEffect> for AuditEffectHandler {
    async fn handle_typed(
        &self,
        payload: AuditPayload,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        tracing::info!(
            action = %payload.action,
            actor = %payload.actor,
            "audit effect dispatched"
        );
        Ok(())
    }
}
```

## Effects 与 Scheduled Actions 的区别

| | Effects | Scheduled Actions |
|---|---|---|
| 执行时机 | commit 后 | phase 收敛循环内 |
| 是否可级联 | 否 | 是 |
| 能否产出 `StateCommand` | 否 | 是 |
| 失败处理 | 记录日志，不阻塞 | 错误会上抛 |
| 状态可见性 | post-commit snapshot | pre-commit context |
| 适用场景 | 外部 I/O、日志、指标 | 内部控制流、状态变更 |

## 另见

- [插件系统内部机制](/zh-cn/explanation/plugin-internals/)
- [Scheduled Actions](/zh-cn/reference/scheduled-actions/)
