# HITL 与 Mailbox

本页解释 Awaken 如何通过 tool call 挂起和 mailbox 队列来实现 human-in-the-loop（HITL）。

## SuspendTicket

当 tool call 需要外部审批或输入时，会产出一个 `SuspendTicket`：

```rust,ignore
pub struct SuspendTicket {
    pub suspension: Suspension,
    pub pending: PendingToolCall,
    pub resume_mode: ToolCallResumeMode,
}
```

其中：

- `suspension`：外部可见的动作描述、提示语、参数 schema
- `pending`：事件流里暴露给前端的待处理 tool call 投影
- `resume_mode`：decision 到来后如何恢复

`suspension.id` 和 `pending.id` 表达的是两层不同的标识：

- `suspension.id`：面向协议/前端的挂起 key，适合做 approval / interrupt id
- `pending.id`：运行时内部的底层 tool-call ID

运行时恢复时可以接受这两种 target。

之所以同时支持两种，是因为它们回答的是两类不同的问题：

- 内部运行时更自然地持有稳定的 tool-call ID
- 协议层和前端更自然地持有“当前这次挂起”的 approval / interrupt id
- 同一个 tool call 的生命周期里可以多次挂起；底层 tool-call ID 保持稳定，
  但每次挂起都会生成新的外部 `suspension.id`

## ToolCallResumeMode

```rust,ignore
pub enum ToolCallResumeMode {
    ReplayToolCall,
    UseDecisionAsToolResult,
    PassDecisionToTool,
}
```

- `ReplayToolCall`：用原始参数重跑
- `UseDecisionAsToolResult`：直接把 decision 结果当 tool 结果
- `PassDecisionToTool`：把 decision 结果作为新参数传入工具

## ResumeDecisionAction

```rust,ignore
pub enum ResumeDecisionAction {
    Resume,
    Cancel,
}
```

## ToolCallResume

恢复载荷：

```rust,ignore
pub struct ToolCallResume {
    pub decision_id: String,
    pub action: ResumeDecisionAction,
    pub result: Value,
    pub reason: Option<String>,
    pub updated_at: u64,
}
```

## Permission 插件的 Ask 模式

`awaken-ext-permission` 利用挂起来实现审批：

1. tool call 命中 `behavior: ask`
2. permission checker 生成 `SuspendTicket`
3. tool call 进入 `Suspended`
4. 如果当前 step 已经没有可继续推进的工作且仍有挂起中的 tool call，run
   才会进入 `Waiting`
5. 前端提示用户审批
6. 用户提交 `Resume` 或 `Cancel`
7. `Resume` 时按 `resume_mode` 恢复；`Cancel` 时该 tool call 标记为取消

`RunStatus` 是粗粒度状态。当前实现没有单独的 `Running+Waiting`。串行模式下，
一旦挂起通常就会很快进入 `Waiting`；并行模式下，当前 batch 里已经启动的其
他调用会先执行完，只有当 batch 收敛后仍存在尚未恢复的 suspended call，
run 才会进入 `Waiting`。

## Mailbox 架构

Mailbox 是所有 run 请求的持久化队列。无论是 streaming、background、A2A 还是内部请求，最终都会变成一个 `MailboxJob`。

### MailboxJob

```rust,ignore
pub struct MailboxJob {
    pub job_id: String,
    pub mailbox_id: String,
    pub agent_id: String,
    pub messages: Vec<Message>,
    pub origin: MailboxJobOrigin,
    pub sender_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub request_extras: Option<Value>,
    pub priority: u8,
    pub dedupe_key: Option<String>,
    pub generation: u64,
    pub status: MailboxJobStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}
```

### MailboxJobStatus

```text
Queued --claim--> Claimed --ack--> Accepted
  |                  |
  |               nack(retry) --> Queued
  |                  |
  |               nack(permanent) --> DeadLetter
  |
  |-- cancel --> Cancelled
  +-- interrupt(generation bump) --> Superseded
```

```rust,ignore
pub enum MailboxJobStatus {
    Queued,
    Claimed,
    Accepted,
    Cancelled,
    Superseded,
    DeadLetter,
}
```

### MailboxJobOrigin

```rust,ignore
pub enum MailboxJobOrigin {
    User,
    A2A,
    Internal,
}
```

### MailboxStore Trait

`MailboxStore` 负责 durable enqueue、原子 claim、ack/nack、cancel、lease 延长以及 interrupt。

实现必须保证：

- enqueue 持久化
- claim 原子化，且只能一个消费者成功
- ack/nack 校验 claim token
- interrupt 与 generation bump 原子完成

### MailboxInterrupt

```rust,ignore
pub struct MailboxInterrupt {
    pub new_generation: u64,
    pub active_job: Option<MailboxJob>,
    pub superseded_count: usize,
}
```

当更高优先级请求到来时，旧 job 会被 supersede，活动 run 需要被取消。

## 另见

- [Run 生命周期与 Phases](./run-lifecycle-and-phases.md)
- [启用工具权限 HITL](../how-to/enable-tool-permission-hitl.md)
