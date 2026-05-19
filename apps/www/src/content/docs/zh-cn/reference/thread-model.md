---
title: "线程模型"
description: "Thread 表示持久化会话。Thread 本身保存 thread 元信息和少量 run 投影； 消息、run 历史和 dispatch 尝试通过存储 trait 单独管理。"
---

Thread 表示持久化会话。`Thread` 本身保存 thread 元信息和少量 run 投影；
消息、run 历史和 dispatch 尝试通过存储 trait 单独管理。

持久化模型是：

```text
Thread 1 -> * MessageRecord
Thread 1 -> * RunRecord
RunRecord 1 -> * RunDispatch

RunRecord 通过 range 或显式 id 读取 MessageRecord。
RunRecord 通过 checkpoint 产出 assistant/tool 消息。
```

## Thread

```rust
pub struct Thread {
    pub id: String,
    pub resource_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub metadata: ThreadMetadata,
    pub active_run_id: Option<String>,
    pub open_run_id: Option<String>,
    pub latest_run_id: Option<String>,
}
```

`active_run_id` 指向正在 worker 上执行的 run。`open_run_id` 指向当前未完成、
可以继续恢复的用户意图。`latest_run_id` 指向最近一次 run。过期 dispatch
的 supersede epoch 属于 `RunDispatch`/mailbox 平面，不属于 thread 真相。

`parent_thread_id` 在赋值时会规范化：去除前后空白、空字符串反序列化为 `None`，
`resource_id` 同样处理。Thread hierarchy 与 run 生命周期联动：当一个 sub-agent
run 启动时，`RunRequestSnapshot.parent_thread_id` 携带父 thread；checkpoint
投影会在子 thread 第一次被物化时填充 `Thread.parent_thread_id`。

### 构造函数

```rust
fn new() -> Self
fn with_id(id: impl Into<String>) -> Self
```

### Builder 方法

```rust
fn with_title(self, title: impl Into<String>) -> Self
fn with_resource_id(self, resource_id: impl Into<String>) -> Self
fn with_parent_thread_id(self, parent_thread_id: impl Into<String>) -> Self
```

## ThreadMetadata

```rust
pub struct ThreadMetadata {
    pub created_at: Option<u64>,
    pub updated_at: Option<u64>,
    pub title: Option<String>,
    pub custom: HashMap<String, Value>,
}
```

## 存储

消息不直接嵌在 `Thread` 里，而是通过 `ThreadStore` 读写：

```rust
#[async_trait]
pub trait ThreadStore: Send + Sync {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;
    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError>;
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError>;
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;
    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError>;
    async fn list_child_threads(&self, parent_thread_id: &str) -> Result<Vec<Thread>, StorageError>;
    async fn validate_thread_hierarchy(
        &self,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> Result<(), StorageError>;
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;
    async fn load_message_records(&self, thread_id: &str) -> Result<Option<Vec<MessageRecord>>, StorageError>;
    async fn save_messages(&self, thread_id: &str, messages: &[Message]) -> Result<(), StorageError>;
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn update_thread_metadata(&self, id: &str, metadata: ThreadMetadata) -> Result<(), StorageError>;
}
```

`ThreadStore` 的默认辅助方法直接覆盖了 lineage 过滤、父线程存在性/环检测，
以及子线程删除策略，后端不需要重复实现这套逻辑。

```rust
pub enum ChildThreadDeleteStrategy {
    /// 当存在直接子 thread 时，拒绝删除。
    Reject,
    /// 保留子 thread，并清空它们的 `parent_thread_id`。默认值。
    Detach,
    /// 递归删除所有后代 thread，再删除目标 thread。
    Cascade,
}
```

默认的 `delete_thread_with_strategy` 实现会发出多次低级写操作，**不是**原子操
作。生产级的并发后端应该用事务或栅栏化的实现覆盖该方法；file、PostgreSQL 与
NATS-buffered 后端已经提供了原生覆盖。

默认的 `list_threads_query` 会按固定步长扫 `list_threads` 后在内存里做过滤；
file、PostgreSQL 与 NATS-buffered 后端各自提供了原生下推。
`ThreadQuery::encode_cursor` 返回的游标在 decode 时会校验原始 query 的形状，
因此分页序列不会漂移到不同的过滤条件。

`Message` 是协议载荷；`MessageRecord` 是 thread 消息日志的持久化投影：

```rust
pub struct MessageRecord {
    pub message_id: String,
    pub thread_id: String,
    pub seq: u64,
    pub message: Message,
    pub produced_by_run_id: Option<String>,
    pub step_index: Option<u32>,
    pub tool_call_id: Option<String>,
    pub created_at: Option<u64>,
}
```

默认的 `load_message_records` 实现基于 `load_messages` 生成记录，按追加顺序分配从 1 开始的
`seq`，并从每条 `Message` 的 metadata 投影出生产者信息。

用户和系统消息通常没有 `produced_by_run_id`。Assistant、tool 和内部消息应该通过
`Message.metadata.run_id` 记录生产它们的 run。子 agent 结果可以基于子 run 的输出消息
范围读取，结果是该范围内最后一条非 tool 的 assistant 消息。

## ThreadRunStore

`ThreadRunStore` 在 `ThreadStore + RunStore` 基础上增加了原子 checkpoint：

```rust
#[async_trait]
pub trait ThreadRunStore: ThreadStore + RunStore + Send + Sync {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;
}
```

## RunStore

```rust
#[async_trait]
pub trait RunStore: Send + Sync {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError>;
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError>;
}
```

## RunRecord

```rust
pub struct RunRecord {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    pub parent_run_id: Option<String>,
    pub request: Option<RunRequestSnapshot>,
    pub input: Option<RunMessageInput>,
    pub output: Option<RunMessageOutput>,
    pub status: RunStatus,
    pub termination_reason: Option<TerminationReason>,
    pub final_output: Option<String>,
    pub error_payload: Option<Value>,
    pub dispatch_id: Option<String>,
    pub session_id: Option<String>,
    pub transport_request_id: Option<String>,
    pub waiting: Option<RunWaitingState>,
    pub outcome: Option<RunOutcome>,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub updated_at: u64,
    pub steps: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub state: Option<PersistedState>,
}
```

`RunRecord` 是一次用户意图的事实来源。它保存 request 元信息、生命周期状态、
waiting 状态、输出/错误结果和 trace id，但不拥有消息正文。

### RunRequestSnapshot

`RunRequestSnapshot` 保存创建或恢复 run 的请求：

```rust
pub struct RunRequestSnapshot {
    pub origin: RunRequestOrigin,
    pub sender_id: Option<String>,
    pub input_message_ids: Vec<String>,
    pub input_message_count: u64,
    pub request_extras: Option<Value>,
    pub decisions: Vec<RunResumeDecision>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub parent_thread_id: Option<String>,
    pub transport_request_id: Option<String>,
}
```

`input_message_ids` 和 `input_message_count` 指向 thread 拥有的消息记录；
request snapshot 不拥有消息正文。

### RunMessageInput 和 RunMessageOutput

`RunMessageInput` 描述 run 读取的 thread 消息范围或显式消息选择；
`RunMessageOutput` 描述 run 产出的消息。两者都引用 thread 拥有的消息记录：

```rust
pub struct RunMessageInput {
    pub thread_id: String,
    pub range: Option<MessageSeqRange>,
    pub trigger_message_ids: Vec<String>,
    pub selected_message_ids: Vec<String>,
    pub context_policy: Option<String>,
    pub compacted_snapshot_id: Option<String>,
}
```

## RunDispatch

`RunDispatch` 负责 delivery、lease、retry 和队列审计。它不是 agent 成功失败的事实来源。

```text
Queued -> Claimed -> Acked | Cancelled | Superseded | DeadLetter
```

`Acked` 只表示这个 dispatch 已消费，不会再重试。判断 agent 是否成功，需要读取
`RunRecord.status`、`RunRecord.waiting` 和 `RunRecord.outcome`。

## 相关

- [使用文件存储](/awaken/zh-cn/how-to/use-file-store/)
- [使用 Postgres 存储](/awaken/zh-cn/how-to/use-postgres-store/)
- ADR-0022: Run Dispatch Data Model
