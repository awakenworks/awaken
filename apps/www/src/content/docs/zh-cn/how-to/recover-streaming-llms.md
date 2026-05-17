---
title: "流式 LLM 错误恢复"
description: "当流式推理过程中出现的瞬时 provider 故障不应该浮现为 run 错误时，看这里。 runtime 会重试在流式开始之前就失败的整个请求；本页讨论更难的情况——模型已 经开始产 token 之后才出现的失败。"
---

当流式推理过程中出现的瞬时 provider 故障不应该浮现为 run 错误时，看这里。
runtime 会重试在流式开始**之前**就失败的整个请求；本页讨论更难的情况——模型已
经开始产 token 之后才出现的失败。

## runtime 替你处理的部分

流式推理通过 `InferenceExecutionError::StreamInterrupted` 与 `InterruptCause`
检测四种 mid-stream 中断原因：

- `ConnectionReset`：响应头之后 TCP/HTTP/2 连接断开
- `IdleStall`：空闲窗口内没有收到新字节
- `GoAway`：响应中途收到 HTTP/2 GOAWAY
- `Provider5xxMidStream(u16)`：流式开始后 provider 返回 5xx

任意一种触发后，loop runner 会调用 `InterruptSnapshot::plan()` 选择四种恢复方
案之一。代码里命名为 `R1..R4`：

| 方案 | 触发条件 | runtime 行为 |
|---|---|---|
| **R1 — `ContinueText`** | 只有累积文本，没有 in-flight tool call | 把已累积文本作为 assistant 前缀 + 继续提示重试；模型从断点继续生成 |
| **R2 — `SynthesizeToolUse`** | 至少一个 tool call 的参数 JSON 已完整到达 | 合成 `StopReason::ToolUse` 终态，让 loop runner 执行已完成的 tool；未完成的 tool 作为提示在下一轮 user message 里告诉模型 |
| **R3 — `TruncateBeforeTool`** | 既有文本又有一个未闭合的 tool call | 截到文本前缀，发出 `AgentEvent::ToolCallCancel` 让消费者丢弃 partial 参数 delta，然后继续 |
| **R4 — `WholeRestart`** | 啥都救不回来（既无文本也无完整 tool） | 重启整个 assistant 轮次；发出 `AgentEvent::StreamReset` 让消费者丢弃这一轮已发出的所有 delta |

`Retry-After` 会被尊重：当 provider 在 `429` 或 `529` 返回 `Retry-After` 时，
`InferenceExecutionError::RateLimited` 与 `Overloaded` 携带解析得到的
`Duration`，retry 子系统至少等待该时长后再重试。

## 客户端看到的事件

恢复后的轮次中，SSE 消费者照常收到 `TextDelta` 与 `ToolCallDelta`。两个新事
件告诉消费者要丢什么：

- `ToolCallCancel { id, name, reason }`：丢掉这个 tool call 已缓冲的 partial
  delta。
- `StreamReset { reason }`：丢掉当前 assistant 轮次的**全部** delta，新的
  delta 接着到来。

这两种事件只是告知，不会进入持久化的 thread 日志；通过 `MessagesSnapshot` 重
新渲染的客户端不需要特殊处理。

## 跨进程续接

单进程的重试循环已经足以应对同一台服务器活到中断之后的场景。要做跨进程恢复
——也就是从**前一个进程**的断点继续——就需要 `StreamCheckpointStore` 契约。

```rust
use awaken::contract::stream_checkpoint::{
    InMemoryStreamCheckpointStore, StreamCheckpoint, StreamCheckpointStore,
};

#[async_trait::async_trait]
trait StreamCheckpointStore: Send + Sync {
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), _>;
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, _>;
    async fn delete(&self, run_id: &str) -> Result<(), _>;
}
```

stream 运行过程中，loop runner 会按 `run_id` 周期性地把累积的 `partial_text`、
`completed_tool_calls` 以及未闭合的 `in_flight_tool` 写入 store。当新的进程接
管这个 run 时，`execute_streaming` 起始处会读到 checkpoint，并把它转换成与
进程内重试路径相同的 R1 前缀注入。

checkpoint **不是**完整的对话日志——已提交的消息仍由 `ThreadRunStore` 拥有。
checkpoint 只保留 in-flight delta 累积态，因此契约只有三个方法（`put`、`get`、
`delete`）。

### 把 store 接到 Agent 上

store 存在 `ResolvedAgent::stream_checkpoint_store`，通过 builder 方法填充：

```rust
use awaken::contract::stream_checkpoint::{
    InMemoryStreamCheckpointStore, StreamCheckpointStore,
};
use std::sync::Arc;

let store: Arc<dyn StreamCheckpointStore> =
    Arc::new(InMemoryStreamCheckpointStore::new());

let resolved = resolved.with_stream_checkpoint_store(store);
```

默认 resolver 管线把这个字段置为 `None`。要让每次解析都带上 store，可以包装
你的 `AgentResolver`，在它返回的 `ResolvedAgent` 上调一次
`with_stream_checkpoint_store(store.clone())` 再交给 runtime。
`AgentRuntimeBuilder` 暂时还没有直接的快捷方法；如有需要可以在
[GitHub issues](https://github.com/AwakenWorks/awaken/issues) 跟进。

仓库里自带的 `InMemoryStreamCheckpointStore` 适合测试和单进程使用。要做跨进
程恢复，请基于共享后端（NATS JetStream KV、Redis、文件系统路径等）实现该
trait。每次 `put` 都应该按 `run_id` 幂等地 upsert checkpoint；`delete` 在轮次
提交完成后调用。

## 不在范围内

- 不会重试永久性错误。`ContextOverflow`、`InvalidRequest`、`Unauthorized`、
  `ModelNotFound`、`ContentFiltered` 会短路重试子系统直接抛回调用方。
- 不会修复"格式错但没截断"的 tool call JSON。这是另一回事；恢复 snapshot 只
  在参数能解析为 JSON 时才算它"完成"。
- 不会自动把恢复内容回写到持久化消息日志。恢复后轮次重新发出的 delta 会产生
  与一次新 run 相同的最终 assistant 消息，checkpoint 在该消息提交后才被清理。

## 相关

- [错误](/reference/errors/)：完整的 `InferenceExecutionError` 分类与访问
  器。
- [事件](/reference/events/)：`ToolCallCancel` / `StreamReset` 语义。
- [优化上下文窗口](/optimize-context-window/)：另一个独立的截断恢复路径，
  适用于模型自己以 `MaxTokens` 停止的场景。
