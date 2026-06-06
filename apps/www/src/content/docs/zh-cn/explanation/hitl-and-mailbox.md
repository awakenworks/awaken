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

## Agent 消息路由

Awaken 保留两条明确的消息路径：

| 路径 | 代码表面 | 适用场景 | 交付边界 |
|---|---|---|---|
| 实时 child inbox | `BackgroundTaskManager::spawn_agent_with_context(...)` + `SendMessageTool` 的 `relation: "child"` | 父 agent 和后台 child agent 在同一进程内，需要低延迟通信 | 进程内 inbox；task id/name 必须解析为拥有该 thread 的 live child task |
| 持久 mailbox | `Mailbox::submit(...)`、`submit_background(...)`、HTTP `/v1/threads/:id/mailbox`、A2A `message:send`、MCP HTTP mailbox tools，或 host 为 `SendMessageTool` 的 `parent` / `agent` 提供的 `DurableMessageSink` | agent、协议入口或 worker 可能位于不同 thread、进程或副本 | 持久 `RunDispatch`；由一个 mailbox worker 通过 lease、retry 和 recovery 认领执行 |

内部后台 agent 消息留在 live inbox，避免不必要的持久队列成本。外部协议消息和跨 thread agent 消息进入 mailbox，让分布式 worker 可以安全认领并执行。`SendMessageTool` 不引入第三种 transport：`child` 走 manager inbox，`parent` 和 `agent` 需要 host 提供 durable sink，并由 host 映射到 mailbox dispatch 或其他持久 transport。

## 分布式 dispatch 保证

Mailbox 是分布式处理边界。它把请求存储（`RunRecord.request` + thread message log）和交付（`RunDispatch`）分开，因此任意 worker 在认领 dispatch 后都能重建 activation。正确的 store 必须提供 durable enqueue、单赢家原子 claim、claim-token 校验、lease extension、lease recovery、interrupt epoch bump，以及队列/结果投影更新。NATS mailbox 使用 JetStream/KV 做多副本 ownership 和 wakeup；SQLite mailbox 是单节点持久；in-memory mailbox 只在进程内有效。

## Pending message steering

当 mailbox 通过 `Mailbox::new_with_pending_thread_run_store(...)` 构建时，用户消息会先以 `PendingMessageRecord` 形式暂存在同一个拥有 thread message 与 run record 的后端里。pending record 表示「已投递但尚未写入 committed history」。runtime 在明确边界上 freeze pending record，把被选中的消息追加进历史，并在同一个后端事务里更新 `RunRecord.input`。

`DeliveryMode` 决定 pending message 何时、如何被消费：

| 字段 | 代码行为 |
|---|---|
| `boundary` | `Interrupt`、`NextStep`、`OnNaturalEnd`、`ResumeInput` 或 `NewRun`。除 `ResumeInput` 必须精确匹配外，较早边界可以通过 `DeliveryBoundary::eligible_at` 逐级落到较晚边界。 |
| `granularity` | `Batch` 消费所有 eligible records；`One` 在第一个 eligible record 后停止。 |
| `barrier` | 阻止跳过该 pending record 处理后续记录；foreground interrupt preflight 会在取消 active run 前返回 `DeliveryBlockedByBarrier`。 |
| `target_run_id` | 限定 active-run delivery 只能被指定 run 消费。`submit_live_then_queue(..., expected_run_id)` 也用它避免 steer 到 stale run。 |
| `fallback_to_new_run` | 允许 active-run pending 在目标 run 先结束时落到 `NewRun`。targeted live steering 使用 `false`；普通 queued record 默认是 `true`。 |

Mailbox 为需要在 freeze 前展示 review queue 的 host 暴露了带乐观锁的 pending 编辑操作：

- `update_pending_message_checked(thread_id, pending_id, expected_revision, message)`：在 record revision 保护下编辑消息内容。
- `retract_pending_message_checked(thread_id, pending_id, expected_revision)`：在消费前撤回 pending entry。
- `reorder_pending_messages_checked(thread_id, expected_queue_revision, ordered_pending_ids)`：在 queue revision 保护下调整 pending 顺序。

pending record 一旦 freeze/consume，这些编辑会失败，不会改写 committed history。freeze retry 会使用 pending selection conflict 和 message-version check，因此并发 edit、reorder 或 retract 不会在 `RunRecord.input` 里留下 phantom trigger id。

### 选择处理方式

HTTP `POST /v1/threads/:id/messages` 与 `POST /v1/runs/:id/inputs` 会映射到 `RunControlService` 的 input modes：

| Mode | 效果 |
|---|---|
| `queue` | 创建 durable mailbox dispatch。有 pending store 时，submit 会在准备 dispatch 时原子 append + freeze `NewRun` pending。 |
| `live_then_queue` / `steer` | 先尝试 steer active run。有 pending store 时，消息会作为 targeted `NextStep` pending 暂存，并向 active run 发送 `PendingBoundaryWake`；如果本地或远端 subscriber 没有接收 wake，会清理这次 pending append 并回退到 durable dispatch。 |
| `interrupt_then_queue` | bump dispatch epoch、supersede queued work、取消 active run，然后排队新输入。foreground interrupt preflight 会在前序 pending barrier 阻塞交付时拒绝取消。 |
| `resume_open_run` | 继续 thread 的 reusable waiting run。新的用户输入会作为指向该 run 的 `ResumeInput` pending 暂存，避免无关的 `NewRun` pending 被折进等待中的 run。 |

在 runtime boundary 上，`MailboxPendingBoundaryHandler` 让 loop 可以为 `NextStep`、`OnNaturalEnd` 或其他支持的边界继续 stage/freeze pending messages。这就是动态 steering 既可编辑、又 crash-safe，同时还能交给分布式 worker 处理最终 dispatch 的机制。

### 代码参考

仓库里已经有这些路径的可执行覆盖。接 host integration 时，优先参考这些测试：

- `crates/awaken-server/src/mailbox/pending_delivery_tests.rs` —— pending edit、reorder、retract 和 freeze。
- `crates/awaken-server/src/mailbox/tests.rs` —— 本地与远端 `submit_live_then_queue` steering。
- `crates/awaken-server/src/routes_test.rs` —— HTTP `mode: "steer"` alias 解析。

freeze 前的 pending review queue（生产 submit 会通过 mailbox submit path 暂存 pending；测试里用内部 `deliver` helper 构造同等状态）：

```rust
use awaken::contract::message::{Message, pending_queue_revision};

let pending = pending_store
    .load_pending_message_records("thread-edit-pending")
    .await?;
let queue_revision = pending_queue_revision(&pending);

mailbox
    .update_pending_message_checked(
        "thread-edit-pending",
        &pending[0].pending_id,
        Some(pending[0].revision),
        Message::user("edited").with_id(pending[0].pending_id.clone()),
    )
    .await?;

mailbox
    .reorder_pending_messages_checked(
        "thread-edit-pending",
        Some(queue_revision),
        &[pending[1].pending_id.clone(), pending[0].pending_id.clone()],
    )
    .await?;

mailbox
    .retract_pending_message_checked(
        "thread-edit-pending",
        &pending[1].pending_id,
        Some(pending[1].revision),
    )
    .await?;
```

先 steer active run；live delivery 不可用时再回退到队列：

```rust
let result = mailbox
    .submit_live_then_queue(
        RunActivation::new("thread-live-steer", vec![Message::user("live steer")])
            .with_agent_id("agent"),
        Some(active_run_id),
    )
    .await?;

assert_eq!(result.status, MailboxDispatchStatus::Running);
assert_eq!(result.run_id, active_run_id);
```

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

`MailboxStore` 定义持久化队列接口,trait 在 `crates/awaken-server-contract/src/contract/mailbox.rs`:

**Enqueue / claim / 生命周期:**

- **enqueue** —— 持久化 dispatch,分配当前 dispatch epoch,重复 `dedupe_key` 直接拒绝
- **claim** —— 为某个 mailbox 原子认领最多 N 个 `Queued` dispatch(基于 lease)
- **claim_dispatch** —— 按 ID 认领单个 dispatch(用于 inline streaming)
- **ack** —— 标记 dispatch 为 `Acked`(校验 claim token)
- **nack** —— 把 dispatch 退回 `Queued` 重试
- **dead_letter** —— 标记 dispatch 为 `DeadLetter`(永久失败)
- **cancel** —— 取消一个 `Queued` dispatch
- **extend_lease** —— 心跳延长活跃 claim
- **interrupt** —— 原子 bump dispatch epoch,supersede 旧 `Queued` dispatch,返回活跃 `Claimed` dispatch 以便取消
- **supersede_claimed** —— 新 epoch 到达时替换一个 `Claimed` dispatch

**Runtime 投影(让 operator 能看见发生了什么):**

- **record_dispatch_start** —— 把投影里的 `run_status` 置 `Running`
- **record_run_result** —— 写入紧凑的 `RunDispatchResult` 投影(独立于 `ack` —— ack 只闭合队列生命周期,不代表业务结果)

**查询:**

- **load_dispatch** —— 按 ID 读单个 dispatch
- **list_dispatches** —— 按 thread 分页列 dispatch
- **reclaim_expired_leases** —— 回收 lease 过期但未 ack 的 dispatch

实现必须保证:durable enqueue、原子 claim(且仅一个赢)、ack/nack/dead_letter 校验 claim token、interrupt 与 dispatch epoch bump 原子完成。两轨设计(队列生命周期 vs runtime 投影)让 operator 调试已消费 dispatch 时不会把 `Acked` 队列状态当成业务成功。

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

- [Run 生命周期与 Phases](/awaken/zh-cn/explanation/run-lifecycle-and-phases/)
- [启用工具权限 HITL](/awaken/zh-cn/how-to/enable-tool-permission-hitl/)
- ADR-0022: Run Dispatch Data Model
