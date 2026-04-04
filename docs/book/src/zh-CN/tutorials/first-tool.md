# 第一个 Tool

## 目标

实现一个在执行时从 `ToolCallContext` 读取类型化状态的工具。

> **状态是可选的。** 许多工具（API 调用、搜索、Shell 命令）不需要状态——只需实现 `execute` 并返回 `ToolResult` 即可。

## 前置条件

- 先完成 [第一个 Agent](./first-agent.md)。
- 复用 [第一个 Agent](./first-agent.md) 中的运行时依赖。

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

## 1. 定义 `StateKey`

`StateKey` 描述状态映射中的一个命名槽位，声明值类型、更新策略和生命周期范围。

```rust,no_run
# use awaken::{StateKey, KeyScope, MergeStrategy};
#
/// 记录问候工具被调用的次数。
struct GreetCount;

impl StateKey for GreetCount {
    const KEY: &'static str = "greet_count";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = u32;
    type Update = u32;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}
# fn main() {}
```

关键选项说明：

- `KeyScope::Run` — 状态在每次运行开始时重置。使用 `KeyScope::Thread` 可跨运行持久化。
- `MergeStrategy::Commutative` — 并发更新安全。当只有一个写入方时使用 `Exclusive`。
- `apply` 定义 `Update` 如何修改当前 `Value`，此处为递增。

## 2. 实现 Tool

该工具通过 `ctx.state::<GreetCount>()` 读取当前计数并返回个性化问候。

```rust,no_run
# use awaken::{StateKey, KeyScope, MergeStrategy};
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{json, Value};
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};

struct GreetTool;

#[async_trait]
impl Tool for GreetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "Greet", "Greet a user by name")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name to greet" }
                },
                "required": ["name"]
            }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        args["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("name is required".into()))?;
        Ok(())
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let name = args["name"].as_str().unwrap_or("world");

        // 读取状态——如果该键尚未设置则返回 None。
        let count = ctx.state::<GreetCount>().copied().unwrap_or(0);

        Ok(ToolResult::success("greet", json!({
            "greeting": format!("Hello, {}!", name),
            "times_greeted": count,
        })).into())
    }
}
# fn main() {}
```

## 3. 注册 Tool

```rust,no_run
# use std::sync::Arc;
# use async_trait::async_trait;
# use serde_json::{json, Value};
# use awaken::{StateKey, KeyScope, MergeStrategy};
# use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
# struct GreetTool;
# #[async_trait]
# impl Tool for GreetTool {
#     fn descriptor(&self) -> ToolDescriptor {
#         ToolDescriptor::new("greet", "Greet", "Greet a user by name")
#     }
#     async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
#         Ok(ToolResult::success("greet", json!({})).into())
#     }
# }
use awaken::registry_spec::AgentSpec;
use awaken::AgentRuntimeBuilder;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let agent_spec = AgentSpec::new("assistant")
    .with_model("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant. Use the greet tool when asked.")
    .with_max_rounds(5);

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("greet", Arc::new(GreetTool))
    .build()?;
# Ok(())
# }
```

## 4. 运行

```rust,no_run
# use std::sync::Arc;
# use async_trait::async_trait;
# use serde_json::{json, Value};
# use awaken::{StateKey, KeyScope, MergeStrategy};
# use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
# use awaken::registry_spec::AgentSpec;
# use awaken::AgentRuntimeBuilder;
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
# struct GreetTool;
# #[async_trait]
# impl Tool for GreetTool {
#     fn descriptor(&self) -> ToolDescriptor {
#         ToolDescriptor::new("greet", "Greet", "Greet a user by name")
#     }
#     async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
#         Ok(ToolResult::success("greet", json!({})).into())
#     }
# }
use awaken::contract::message::Message;
use awaken::contract::event_sink::VecEventSink;
use awaken::RunRequest;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let runtime = AgentRuntimeBuilder::new()
#     .with_agent_spec(AgentSpec::new("assistant").with_model("gpt-4o-mini"))
#     .with_tool("greet", Arc::new(GreetTool))
#     .build()?;
let request = RunRequest::new(
    "thread-1",
    vec![Message::user("Greet Alice")],
)
.with_agent_id("assistant");

let sink = Arc::new(VecEventSink::new());
runtime.run(request, sink.clone()).await?;
# Ok(())
# }
```

## 5. 验证

检查收集到的事件中是否包含 `name == "greet"` 的 `ToolCallDone` 事件：

```rust,ignore
use awaken::contract::event::AgentEvent;

let events = sink.take();
let tool_done = events.iter().any(|e| matches!(
    e,
    AgentEvent::ToolCallDone { id: _, message_id: _, result: _, outcome: _ }
));
println!("tool_call_done_seen: {}", tool_done);
```

预期结果：

- `tool_call_done_seen: true`
- `ToolCallDone` 中的 `result` 包含 `greeting` 和 `times_greeted` 字段。

## 你创建了什么

一个工具，它：

1. 通过 `descriptor()` 声明参数的 JSON Schema。
2. 通过 `validate_args()` 在执行前验证参数。
3. 通过 `ctx.state::<K>()` 从快照读取类型化状态。
4. 通过 `ToolResult::success()` 返回结构化 JSON。

`StateKey` trait 提供了类型安全的、有范围的状态管理，无需手动操作原始 JSON。

## 下一步阅读

- 了解完整工具生命周期：[Tool Trait](../../reference/tool-trait.md)
- 添加跨运行管理状态的插件：[添加插件](../how-to/add-a-plugin.md)
- 学习状态范围规则：[State Keys](../../reference/state-keys.md)

## 常见错误

- `ctx.state::<K>()` 返回 `None`：该状态键在本次运行中尚未写入。对数值类型使用 `.unwrap_or_default()` 或 `.copied().unwrap_or(0)`。
- `StateError::KeyEncode` / `StateError::KeyDecode`：`Value` 类型无法通过 JSON 往返序列化。确保正确派生了 `Serialize` 和 `Deserialize`。
- `ToolError::InvalidArguments` 未被触发：运行时在 `execute` 之前调用 `validate_args`。如果跳过验证，错误输入将到达 `execute` 并可能在 `.unwrap()` 处崩溃。
- 范围不匹配：`KeyScope::Run` 状态在运行之间被清除。如果需要持久化，使用 `KeyScope::Thread`。
