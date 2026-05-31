---
title: "Scheduled Actions"
description: "Scheduled action 是插件、tool 和运行时在 phase 收敛循环里发起副作用的主要机制。任何 hook、tool 或内部模块都可以通过 StateCommand::schedule_action::<A>(payload) 调度一个 action，运行时会在目标 phase 的 EXECUTE 阶段把它交给对应 handler。"
---

Scheduled action 是插件、tool 和运行时在 phase 收敛循环里发起副作用的主要机制。任何 hook、tool 或内部模块都可以通过 `StateCommand::schedule_action::<A>(payload)` 调度一个 action，运行时会在目标 phase 的 EXECUTE 阶段把它交给对应 handler。

## 工作方式

```text
Hook / Tool                    Runtime
    |                            |
    |-- StateCommand ----------->|  (包含 scheduled_actions)
    |                            |-- commit state updates
    |                            |-- dispatch to handler(A, p)
    |                            |      |
    |                            |      |-- handler returns StateCommand
    |                            |<-----'
    |                            |-- commit handler results
```

### 从 hook 调度

```rust
use awaken_runtime::agent::state::ExcludeTool;

async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<ExcludeTool>("dangerous_tool".into())?;
    Ok(cmd)
}
```

### 从 tool 调度

```rust
use awaken_runtime::agent::state::AddContextMessage;
use awaken::contract::context_message::ContextMessage;

async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<AddContextMessage>(
        ContextMessage::system("my_tool.hint", "Remember to check the docs."),
    )?;
    Ok(ToolOutput::with_command(
        ToolResult::success("my_tool", json!({"ok": true})),
        cmd,
    ))
}
```

## 核心 Actions（awaken-runtime）

### AddContextMessage

| | |
|---|---|
| Key | `runtime.add_context_message` |
| Phase | `BeforeInference` |
| Payload | `ContextMessage` |

向当前步骤的推理上下文注入一条 context message。

### SetInferenceOverride

| | |
|---|---|
| Key | `runtime.set_inference_override` |
| Phase | `BeforeInference` |
| Payload | `InferenceOverride` |

覆盖当前步骤的推理参数，如 model、temperature、max_tokens 等。

### ExcludeTool

| | |
|---|---|
| Key | `runtime.exclude_tool` |
| Phase | `BeforeInference` |
| Payload | `String`（tool ID） |

把某个 tool 从当前步骤提供给 LLM 的工具集合中移除。

### IncludeOnlyTools

| | |
|---|---|
| Key | `runtime.include_only_tools` |
| Phase | `BeforeInference` |
| Payload | `Vec<String>` |

把当前步骤的工具集合限制为指定白名单。

### 工具拦截

> 工具拦截**不再**通过 scheduled action 实现。
> 需要在执行前阻断、挂起或直接返回结果时，应实现 `ToolGateHook`
> 并通过 `PluginRegistrar::register_tool_gate_hook()` 注册。

## Deferred Tools Actions（awaken-ext-deferred-tools）

### DeferToolAction

| | |
|---|---|
| Key | `deferred_tools.defer` |
| Phase | `BeforeInference` |
| Payload | `Vec<String>` |

把工具切换到 Deferred 模式，从 LLM 工具列表里移除，由 `ToolSearch` 间接暴露。

### PromoteToolAction

| | |
|---|---|
| Key | `deferred_tools.promote` |
| Phase | `BeforeInference` |
| Payload | `Vec<String>` |

把工具从 Deferred 提升回 Eager 模式。

## 插件 Action 使用矩阵

| 插件 | AddContext | SetOverride | Exclude | IncludeOnly | Defer | Promote |
|--------|:---------:|:-----------:|:-------:|:-----------:|:-----:|:-------:|
| `permission` | | | X | | | |
| `skills` | X | | | | | | |
| `reminder` | X | | | | | | |
| `deferred-tools` | X | | X | | X | X |
| `observability` | | | | | | |
| `mcp` | | | | | | |
| `generative-ui` | | | | | | |

## 定义自定义 action

插件通过实现 `ScheduledActionSpec` 定义自己的 action,并实现 `TypedScheduledActionHandler<A>` 作为 runtime 调度的 handler,经 `PluginRegistrar::register_scheduled_action` 注册。

### Spec

`ScheduledActionSpec` 声明 action 的 identity、phase、payload 类型。默认的 `encode_payload` / `decode_payload` 实现走 runtime 的 JSON 编解码,仅在需要自定义序列化时才 override。

```rust
use awaken::model::{JsonValue, Phase, ScheduledActionSpec};
use awaken::StateError;

pub struct MyCustomAction;

impl ScheduledActionSpec for MyCustomAction {
    const KEY: &'static str = "my_plugin.custom_action";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = MyPayload;

    // 默认 encode_payload / decode_payload 来自 trait;
    // 仅在需要自定义序列化时 override。
}
```

### Handler

Runtime dispatch 的 handler trait:

```rust
#[async_trait]
pub trait TypedScheduledActionHandler<A>: Send + Sync + 'static
where
    A: ScheduledActionSpec,
{
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: A::Payload,
    ) -> Result<StateCommand, StateError>;
}
```

Handler 收 `PhaseContext`(snapshot + run metadata),返回 `StateCommand` —— 可以携带状态变更、再调度 action(触发下一轮收敛)、emit effect。

### 注册

```rust
fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_scheduled_action::<MyCustomAction, _>(MyHandler)?;
    Ok(())
}
```

其它插件 / 工具就能调度你的 action:

```rust
cmd.schedule_action::<MyCustomAction>(my_payload)?;
```

## 收敛与级联

scheduled actions 在 phase 收敛循环内部执行。某个 handler 可以再为同一 phase 调度新的 action，于是运行时会继续下一轮 dispatch，直到没有新 action 产生。

### 循环工作方式

```text
Phase EXECUTE stage:
  round 1: dispatch queued actions -> handlers return StateCommands
           commit state, collect newly scheduled actions
  round 2: dispatch new actions
           ...
  round N: no new actions -> phase converges
```

### 限制

循环上限是 `DEFAULT_MAX_PHASE_ROUNDS`（当前默认 16）。如果超过上限仍不断产生 action，会返回 `StateError::PhaseRunLoopExceeded`。

### 失败 action

handler 返回错误时不会重试，失败会被写入 `FailedScheduledActions`：

```rust
let failed = store.read::<FailedScheduledActions>().unwrap_or_default();
assert!(failed.is_empty(), "expected no failed actions");
```
