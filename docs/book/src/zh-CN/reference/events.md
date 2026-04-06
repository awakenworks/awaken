# 事件

agent loop 在执行过程中会持续发出 `AgentEvent`。这些事件既会被协议编码器消费，也会通过 SSE 发给客户端。

## AgentEvent

所有变体在 JSON 中都使用 `event_type` 作为标签字段（`#[serde(tag = "event_type", rename_all = "snake_case")]`）。

```rust,ignore
pub enum AgentEvent {
    RunStart {
        thread_id: String,
        run_id: String,
        parent_run_id: Option<String>,    // 为 None 时省略
    },

    RunFinish {
        thread_id: String,
        run_id: String,
        result: Option<Value>,            // 为 None 时省略
        termination: TerminationReason,
    },

    TextDelta { delta: String },

    ReasoningDelta { delta: String },

    ToolCallStart { id: String, name: String },

    ToolCallDelta { id: String, args_delta: String },

    ToolCallReady {
        id: String,
        name: String,
        arguments: Value,
    },

    ToolCallDone {
        id: String,
        message_id: String,
        result: ToolResult,
        outcome: ToolCallOutcome,
    },

    ToolCallStreamDelta {
        id: String,
        name: String,
        delta: String,
    },

    ReasoningEncryptedValue { encrypted_value: String },

    MessagesSnapshot { messages: Vec<Value> },

    ActivitySnapshot {
        message_id: String,
        activity_type: String,
        content: Value,
        replace: Option<bool>,            // 为 None 时省略
    },

    ActivityDelta {
        message_id: String,
        activity_type: String,
        patch: Vec<Value>,
    },

    /// 已挂起 tool call 在 resume/cancel 后产出的最终结果。
    ToolCallResumed { target_id: String, result: Value },

    StepStart { message_id: String },

    StepEnd,

    InferenceComplete {
        model: String,
        usage: Option<TokenUsage>,        // 为 None 时省略
        duration_ms: u64,
    },

    StateSnapshot { snapshot: Value },

    StateDelta { delta: Vec<Value> },

    Error {
        message: String,
        code: Option<String>,             // 为 None 时省略
    },
}
```

**Crate 路径：** `awaken::contract::event::AgentEvent`

### Helper

```rust,ignore
impl AgentEvent {
    /// 从 RunFinish 的 result 中提取响应文本。
    pub fn extract_response(result: &Option<Value>) -> String
}
```

## SSE 传输格式

事件通过 HTTP Server-Sent Events 传输。每帧使用 SSE 的 `id` 字段作为单调递增的序列号，支持断点续连：

```
id: {seq}
data: {json}

```

其中 `{json}` 是 `AgentEvent` 的 JSON 序列化结果。序列号从 0 开始，run 内每发出一个事件递增 1。

当流空闲时，服务器还会定期发送注释行（`: heartbeat`），以防止客户端或代理因长时间推理暂停而超时。客户端应忽略这些注释行，它们不携带任何事件数据。

该功能由 `awaken-server::http_sse` 中的 `format_sse_data_with_id` 实现。

## Run 输入

向 run 推送消息时，POST 到 `/v1/runs/{run_id}/inputs`，请求体反序列化为 `PushRunInputsPayload`：

```rust,ignore
struct PushRunInputsPayload {
    messages: Vec<RunMessage>,
}

struct RunMessage {
    role: String,
    content: String,
}
```

至少需要一条消息。成功时服务器返回 `202 Accepted`。

如果协议层要表达 resume target，传入的标识既可以是底层 tool-call ID，也可
以是当前活跃的 `suspension_id`；运行时会把这两种 target 解析到同一个挂起
中的 tool call。

## RunOutput

run 返回的事件流类型别名：

```rust,ignore
pub type RunOutput = futures::stream::BoxStream<'static, AgentEvent>;
```

## TerminationReason

run 终止的原因。使用 `#[serde(tag = "type", content = "value", rename_all = "snake_case")]` 序列化。

- 无负载的单元变体序列化为 `{ "type": "natural_end" }`，不会输出 `"value"` 键。
- 带负载的元组变体序列化为 `{ "type": "stopped", "value": { ... } }`。

```rust,ignore
pub enum TerminationReason {
    NaturalEnd,
    BehaviorRequested,
    Stopped(StoppedReason),
    Cancelled,
    Blocked(String),
    Suspended,
    Error(String),
}
```

## StoppedReason

`TerminationReason::Stopped` 携带的负载。

```rust,ignore
pub struct StoppedReason {
    pub code: String,
    pub detail: Option<String>,    // 为 None 时省略
}
```

## ToolCallOutcome

```rust,ignore
pub enum ToolCallOutcome {
    Succeeded,
    Failed,
    Suspended,
}
```

## 挂起 / 恢复事件语义

- `ToolCallDone { outcome: Suspended, result.suspension = Some(...) }` 表示新
  的挂起已经建立。`SuspendTicket` 同时携带外部 suspension payload 和 pending
  tool-call 投影。
- `ToolCallResumed { target_id, result }` 表示一个之前挂起的 call 在
  resume/cancel 之后已经产出最终结果。无论是批准后 replay 完成，还是显式
  cancel/deny，都会走这个事件。
- `target_id` 是底层运行时的 tool-call ID，用来把恢复后的结果关联回原始挂起。

## ToolResult

工具执行结果，在 `ToolCallDone` 中携带。定义于 `awaken::contract::tool::ToolResult`。

```rust,ignore
pub struct ToolResult {
    pub tool_name: String,
    pub status: ToolStatus,        // "success" | "pending" | "error"
    pub data: Value,
    pub message: Option<String>,
    pub suspension: Option<Box<SuspendTicket>>,   // 为 None 时省略
    pub metadata: HashMap<String, Value>,          // 为空时省略
}
```

## TokenUsage

```rust,ignore
pub struct TokenUsage {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub cache_creation_tokens: Option<i32>,
    pub thinking_tokens: Option<i32>,
}
```

所有字段为 `None` 时均从 JSON 中省略。`TokenUsage::default()` 产生所有字段均为 `None` 的值。

## 相关

- [Run 生命周期与 Phases](../explanation/run-lifecycle-and-phases.md)
