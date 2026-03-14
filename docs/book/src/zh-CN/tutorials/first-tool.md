> 本文档为中文翻译版本，英文原版请参阅 [First Tool](../../tutorials/first-tool.md)

# 第一个工具

## 目标

实现一个读取并更新类型化状态的工具。

> **State 是可选的。** 很多工具（API 调用、搜索、Shell 命令等）不需要状态 —— 只需实现 `execute` 并返回 `ToolResult`。

## 前置条件

- 先完成[第一个 Agent](./first-agent.md)。
- 复用[第一个 Agent](./first-agent.md)中的运行时依赖。
- 确保依赖中包含 `State` derive 宏：

```toml
[dependencies]
async-trait = "0.1"
serde_json = "1"
serde = { version = "1", features = ["derive"] }
tirea = "0.5.0-alpha.1"
tirea-state-derive = "0.5.0-alpha.1"
```

## 1. 定义带 Action 的类型化状态

Tirea 的状态变更基于 Action 模式：定义一个 action 枚举和 reducer，运行时通过 `ToolExecutionEffect` 应用变更。直接通过 `ctx.state::<T>().set_*()` 写入状态会被运行时拒绝。

`#[tirea(action = "...")]` 属性关联 action 类型并生成 `StateSpec`。状态作用域默认为 `thread`（跨 run 持久化）；可设置 `#[tirea(scope = "run")]` 表示 per-run 状态，或 `#[tirea(scope = "tool_call")]` 表示单次调用的临时数据。

```rust,ignore
use serde::{Deserialize, Serialize};
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "CounterAction")]
struct Counter {
    value: i64,
    label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CounterAction {
    Increment(i64),
}

impl Counter {
    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::Increment(amount) => self.value += amount,
        }
    }
}
```

## 2. 实现工具

重写 `execute_effect`，通过 `ToolExecutionEffect` 以类型化 action 的形式返回状态变更。

```rust,ignore
use async_trait::async_trait;
use serde_json::{json, Value};
use tirea::contracts::{AnyStateAction, ToolCallContext};
use tirea::contracts::runtime::tool_call::ToolExecutionEffect;
use tirea::prelude::*;

struct IncrementCounter;

#[async_trait]
impl Tool for IncrementCounter {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("increment_counter", "Increment Counter", "Increment counter state")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "amount": { "type": "integer", "default": 1 }
                }
            }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext<'_>) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx).await?.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let amount = args["amount"].as_i64().unwrap_or(1);
        let current = ctx.snapshot_of::<Counter>()
            .map(|c| c.value)
            .unwrap_or(0);

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "increment_counter",
            json!({ "before": current, "after": current + amount }),
        ))
        .with_action(AnyStateAction::new::<Counter>(
            CounterAction::Increment(amount),
        )))
    }
}
```

## 3. 注册工具

```rust,ignore
use tirea::composition::{tool_map, AgentOsBuilder};

let os = AgentOsBuilder::new()
    .with_tools(tool_map([IncrementCounter]))
    .build()?;
```

## 4. 验证行为

发送一个触发 `increment_counter` 的请求，然后验证：

- 事件流中包含 `increment_counter` 的 `ToolCallDone`
- 线程状态 `counter.value` 按预期数量增加
- 线程补丁历史中至少追加了一个新补丁

## 5. 读取状态

使用 `snapshot_of` 将当前状态读取为普通 Rust 值：

```rust,ignore
let snap = ctx.snapshot_of::<Counter>().unwrap_or_default();
println!("current value = {}", snap.value);
```

> **说明：** `ctx.state::<T>("path")` 和 `ctx.snapshot_at::<T>("path")` 用于同一状态类型在不同路径复用的高级场景。大多数工具使用 `snapshot_of` 即可 —— 它会自动使用状态类型上声明的路径。

## 6. `TypedTool`

对于参数结构固定的工具，参阅 [`TypedTool`](../../reference/typed-tool.md) —— 它从 Rust 结构体自动生成 JSON Schema 并处理反序列化。

## 常见错误

- 使用 `ctx.state::<T>().set_*()` 写入：运行时会拒绝直接状态写入。请改用 `ToolExecutionEffect` + `AnyStateAction`。
- 缺少 derive 宏导入：确保存在 `use tirea_state_derive::State;`。
- 数值解析回退掩盖 bug：如需严格输入校验，请对 `amount` 进行验证。
- 过早使用原始 `Value` 解析：如果参数可以清晰地映射到一个结构体，请切换到 [`TypedTool`](../../reference/typed-tool.md)。
- 在工具方法内对状态读取使用 `?`：`snapshot_of` 返回 `TireaResult<T>`，但工具方法返回 `Result<_, ToolError>`，两者之间没有 `From` 转换；对派生了 `Default` 的类型请使用 `unwrap_or_default()`。
- `TypedTool::Args` 上忘记 `#[derive(JsonSchema)]`：缺少它会导致编译失败。

## 下一步

- [构建 Agent](../../how-to/build-an-agent.md)
- [添加工具](../../how-to/add-a-tool.md)
- [Typed Tool 参考](../../reference/typed-tool.md)
- [State Ops 参考](../../reference/state-ops.md)
