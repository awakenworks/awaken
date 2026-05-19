---
title: "Tool Trait"
description: "Tool 是 Awaken 暴露能力给 LLM 的主扩展点。tool 接收 JSON 参数和只读上下文，返回 ToolOutput。"
---

`Tool` 是 Awaken 暴露能力给 LLM 的主扩展点。tool 接收 JSON 参数和只读上下文，返回 `ToolOutput`。

## Trait 定义

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError>;
}
```

## ToolDescriptor

描述 tool 的 ID、名称、说明和参数 schema。它会被注册到运行时，并在推理请求里暴露给 LLM。

```rust
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub category: Option<String>,
}
```

Builder 方法：

```rust
ToolDescriptor::new(id, name, description) -> Self
    .with_parameters(schema: Value) -> Self
    .with_category(category: impl Into<String>) -> Self
```

## ToolResult

`Tool::execute` 返回的结构化结果。

```rust
pub struct ToolResult {
    pub tool_name: String,
    pub status: ToolStatus,
    pub data: Value,
    pub message: Option<String>,
    pub suspension: Option<Box<SuspendTicket>>,
}
```

### ToolStatus

```rust
pub enum ToolStatus {
    Success,
    Pending,
    Error,
}
```

### 构造函数

| 方法 | 状态 | 用途 |
|---|---|---|
| `ToolResult::success(name, data)` | `Success` | 正常完成 |
| `ToolResult::success_with_message(name, data, msg)` | `Success` | 带补充说明的成功 |
| `ToolResult::error(name, message)` | `Error` | 可恢复失败 |
| `ToolResult::error_with_code(name, code, message)` | `Error` | 带 code 的结构化失败 |
| `ToolResult::suspended(name, message)` | `Pending` | HITL 挂起 |
| `ToolResult::suspended_with(name, message, ticket)` | `Pending` | 带 `SuspendTicket` 的挂起 |

### 判定方法

- `is_success()`
- `is_pending()`
- `is_error()`
- `to_json()`

## ToolError

`ToolError` 和 `ToolResult::error(...)` 的区别是：前者直接终止该次 tool call，后者会把失败信息回传给 LLM。

```rust
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

## ToolCallContext

tool 执行期拿到的只读上下文：

```rust
pub struct ToolCallContext {
    pub call_id: String,
    pub tool_name: String,
    pub run_identity: RunIdentity,
    pub agent_spec: Arc<AgentSpec>,
    pub snapshot: Snapshot,
    pub activity_sink: Option<Arc<dyn EventSink>>,
    /// 协同取消 token。长跑工具(MCP 调用、子 agent 执行)应周期性
    /// `is_cancelled()` 或在 `tokio::select!` 里用 `cancelled()`。
    pub cancellation_token: Option<CancellationToken>,
    /// Resume 决策输入 —— runtime 在重放挂起的 tool call 时设置
    /// (见 `ToolCallResumeMode`)。
    pub resume_input: Option<ToolCallResume>,
    /// 当前执行是恢复的某个挂起时的 suspension id。
    pub suspension_id: Option<String>,
    /// 当前执行是恢复的某个挂起时的 reason/action。
    pub suspension_reason: Option<String>,
}
```

### 方法

```rust
fn state<K: StateKey>(&self) -> Option<&K::Value>
async fn report_activity(&self, activity_type: &str, content: &str)
async fn report_activity_delta(&self, activity_type: &str, patch: Value)
async fn report_progress(
    &self,
    status: ProgressStatus,
    message: Option<&str>,
    progress: Option<f64>,
)
```

## 示例

### 最小 tool

```rust
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};
use serde_json::{Value, json};

struct Greet;

#[async_trait]
impl Tool for Greet {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "greet", "Greet a user by name")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("name required".into()))?;
        Ok(ToolResult::success("greet", json!({ "greeting": format!("Hello, {name}!") })).into())
    }
}
```

### 从上下文读取状态

```rust
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};
use awaken::state::StateKey;
use serde_json::{Value, json};

struct GetPreferences;

#[async_trait]
impl Tool for GetPreferences {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_prefs", "get_preferences", "Get user preferences")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        // let prefs = ctx.state::<UserPreferences>().cloned().unwrap_or_default();
        Ok(ToolResult::success("get_prefs", json!({})).into())
    }
}
```

## Tool 执行钩子

每次 tool call 在真正执行前后都会经过插件钩子。

### 完整生命周期

```text
LLM 选择 tool
  -> validate_args()
  -> ToolGate
     纯判定：Allow / Block / Suspend / SetResult
  -> BeforeToolExecute
     仅对放行的调用执行一次性钩子
  -> execute()
  -> AfterToolExecute
```

### ToolGate

参数校验后首先进入 `ToolGate`。插件实现 `ToolGateHook`，返回
`Option<ToolInterceptPayload>`，用来决定：

- `Block`
- `Suspend`
- `SetResult`

拦截优先级：

`Block > Suspend > SetResult`

`ToolGate` 必须保持纯判定，可在同一步内于前序工具提交新状态后被重新评估。

### BeforeToolExecute

只有在 `ToolGate` 放行后才运行。适合做真正执行前的一次性副作用，例如计数、节流状态写入或观测打点。

### AfterToolExecute

tool 完成后运行。插件可以观察 `ToolResult`、更新状态、追加事件或调度后续 action。

### ToolCallStatus 转移

```text
New -> Running -> Succeeded
                  Failed
                  Suspended -> Resuming -> Running -> ...
                  Cancelled
```

终态（`Succeeded`、`Failed`、`Cancelled`）不能再向前转移。

## 相关

- [第一个 Tool](/awaken/zh-cn/tutorials/first-tool/)
- [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/)
