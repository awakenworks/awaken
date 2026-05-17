---
title: "事件"
description: "agent loop 在执行过程中会持续发出 AgentEvent。这些事件既会被协议编码器消费，也会通过 SSE 发给客户端。"
---

agent loop 在执行过程中会持续发出 `AgentEvent`。这些事件既会被协议编码器消费，也会通过 SSE 发给客户端。

## AgentEvent

所有变体在 JSON 中都使用 `event_type` 作为标签字段（`#[serde(tag = "event_type", rename_all = "snake_case")]`）。

```rust
pub enum AgentEvent {
    RunStart {
        thread_id: String,
        run_id: String,
        parent_run_id: Option<String>,    // 为 None 时省略
        identity: Option<RunIdentity>,    // 为 None 时省略
    },

    RunFinish {
        thread_id: String,
        run_id: String,
        identity: Option<RunIdentity>,    // 为 None 时省略
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

    ToolCallResumed { target_id: String, result: Value },

    /// 已经开始流式产生的 tool call，在参数 JSON 闭合前被中途取消（mid-stream
    /// 中断恢复路径触发）。消费者应该把对应 `id` 已缓冲的 partial delta 丢掉。
    ToolCallCancel {
        id: String,
        name: String,
        reason: String,                   // 例如 "connection reset"、"idle stall"
    },

    /// 当前 assistant 轮次因 mid-stream 中断而无法通过 continuation 恢复，
    /// 整个轮次重启。消费者应该把这一轮已发出的所有 delta 丢掉。
    StreamReset { reason: String },

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

### 流式恢复语义

`ToolCallCancel` 和 `StreamReset` 是 mid-stream 恢复期间发出的"丢弃信号"。
消费者把对应 tool call（或整轮）的 partial buffer 丢掉、继续读流即可，恢复后
的 delta 会通过常规的 `TextDelta` / `ToolCallDelta` 通道到来。四种恢复方案与
`StreamCheckpointStore` 接入见
[流式 LLM 错误恢复](/zh-cn/how-to/recover-streaming-llms/)。

### Helper

```rust
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

```rust
struct PushRunInputsPayload {
    messages: Vec<RunMessage>,
}

struct RunMessage {
    role: String,
    content: String,
}
```

至少需要一条消息。成功时服务器返回 `202 Accepted`。

## RunOutput

run 返回的事件流类型别名：

```rust
pub type RunOutput = futures::stream::BoxStream<'static, AgentEvent>;
```

## TerminationReason

run 终止的原因。使用 `#[serde(tag = "type", content = "value", rename_all = "snake_case")]` 序列化。

- 无负载的单元变体序列化为 `{ "type": "natural_end" }`，不会输出 `"value"` 键。
- 带负载的元组变体序列化为 `{ "type": "stopped", "value": { ... } }`。

```rust
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

```rust
pub struct StoppedReason {
    pub code: String,
    pub detail: Option<String>,    // 为 None 时省略
}
```

## ToolCallOutcome

```rust
pub enum ToolCallOutcome {
    Succeeded,
    Failed,
    Suspended,
}
```

## ToolResult

工具执行结果，在 `ToolCallDone` 中携带。定义于 `awaken::contract::tool::ToolResult`。

```rust
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

```rust
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

- [Run 生命周期与 Phases](/zh-cn/explanation/run-lifecycle-and-phases/)
