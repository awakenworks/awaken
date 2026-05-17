---
title: "HITL 与 Mailbox"
description: "本页解释 Awaken 如何通过 tool call 挂起和 mailbox 队列来实现 human-in-the-loop（HITL）。"
---

本页解释 Awaken 如何通过 tool call 挂起和 mailbox 队列来实现 human-in-the-loop（HITL）。

## SuspendTicket

当 tool call 需要外部审批或输入时，会产出一个 `SuspendTicket`：

```rust
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

## ToolCallResumeMode

```rust
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

```rust
pub enum ResumeDecisionAction {
    Resume,
    Cancel,
}
```

## ToolCallResume

恢复载荷：

```rust
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
4. run 进入 `Waiting`
5. 前端提示用户审批
6. 用户提交 `Resume` 或 `Cancel`
7. `Resume` 时按 `resume_mode` 恢复；`Cancel` 时该 tool call 标记为取消

## Mailbox 架构

Mailbox 是 run 激活的持久化 dispatch 队列。无论是 streaming、background、A2A
还是内部请求，最终都会变成一个 `RunDispatch`。

`RunDispatch` 负责 delivery、lease、retry 和队列审计；run 的业务事实保存在
`RunRecord` 上。

### RunDispatch

```rust
pub struct RunDispatch {
    pub dispatch_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub priority: u8,
    pub dedupe_key: Option<String>,
    pub dispatch_epoch: u64,
    pub status: RunDispatchStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<u64>,
    pub dispatch_instance_id: Option<String>,
    pub run_status: Option<RunStatus>,
    pub termination: Option<TerminationReason>,
    pub run_response: Option<String>,
    pub run_error: Option<String>,
    pub completed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}
```

Dispatch 记录不保存 request message、agent 身份、request extras 或 transport payload。
激活重建会读取 `RunRecord.request` 和 thread message log。

### RunDispatchStatus

```text
Queued --claim--> Claimed --ack--> Acked
  |                  |
  |               nack(retry) --> Queued
  |                  |
  |               nack(permanent) --> DeadLetter
  |
  |-- cancel --> Cancelled
  +-- interrupt(dispatch epoch bump) --> Superseded
```

```rust
pub enum RunDispatchStatus {
    Queued,
    Claimed,
    Acked,
    Cancelled,
    Superseded,
    DeadLetter,
}
```

`Acked` 是 dispatch 状态，不是成功状态。判断 agent 是否成功，需要读取
`RunRecord.status`、`RunRecord.waiting` 和 `RunRecord.outcome`。

### RunDispatchResult

队列记录会保存一份紧凑的 runtime 结果投影，便于排障，但不会把队列状态当作业务状态：

```rust
pub struct RunDispatchResult {
    pub run_id: String,
    pub dispatch_instance_id: String,
    pub status: RunStatus,
    pub termination: Option<TerminationReason>,
    pub response: Option<String>,
    pub error: Option<String>,
}
```

### RunRequestOrigin

```rust
pub enum RunRequestOrigin {
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
- interrupt 与 dispatch epoch bump 原子完成

## Waiting Run 与 Run Control

挂起是同一个 run 的非终态中间状态。等待审批或输入时，`RunRecord` 会持久化
`RunWaitingState`：

```rust
pub struct RunWaitingState {
    pub reason: WaitingReason,
    pub ticket_ids: Vec<String>,
    pub tickets: Vec<RunWaitingTicket>,
    pub since_dispatch_id: Option<String>,
    pub message: Option<String>,
}
```

当 run 进入等待状态，当前 dispatch 会被 ack，thread 保留 `open_run_id`。后续审批或用户输入
会为同一个 `run_id` 创建新的 dispatch，而不是新建另一个 run。

`RunControlService` 是服务端统一控制面：

- `get_active_run` 读取 thread 的 active/open run 投影。
- `decide` 记录 tool-call decision 并恢复等待中的 run。
- `cancel_run` 终止 run。
- `interrupt_thread` 中断当前 thread 上的工作。
- `inject_user_input` 和 `inject_run_input` 追加用户输入，并可恢复同一个 open run。

Web/IDE 风格前端应通过这组 API 实现重连、审批、取消、中断和转向。

### MailboxInterrupt

```rust
pub struct MailboxInterrupt {
    pub new_dispatch_epoch: u64,
    pub active_dispatch: Option<RunDispatch>,
    pub superseded_count: usize,
}
```

当更高优先级请求到来时，旧 dispatch 会被 supersede，活动 run 需要被取消。

## 另见

- [Run 生命周期与 Phases](/run-lifecycle-and-phases/)
- [启用工具权限 HITL](/how-to/enable-tool-permission-hitl/)
- ADR-0022: Run Dispatch Data Model
