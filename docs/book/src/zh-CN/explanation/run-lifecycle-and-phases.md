# Run 生命周期与 Phases

本页描述 run 和 tool call 的状态机、9 个 phase、终止条件、checkpoint 触发点，以及挂起 / 恢复如何桥接 run 层与 tool-call 层。

## RunStatus

```text
Running --+--> Waiting --+--> Running (resume)
          |              |
          +--> Done      +--> Done
```

```rust,ignore
pub enum RunStatus {
    Running,
    Waiting,
    Done,
}
```

## ToolCallStatus

```text
New --> Running --+--> Succeeded
                  +--> Failed
                  +--> Cancelled
                  +--> Suspended --> Resuming --+--> Running
                                                +--> Suspended
                                                +--> Succeeded/Failed/Cancelled
```

```rust,ignore
pub enum ToolCallStatus {
    New,
    Running,
    Suspended,
    Resuming,
    Succeeded,
    Failed,
    Cancelled,
}
```

## Phase Enum

Awaken 的执行顺序由 9 个 phase 固定下来：

```rust,ignore
pub enum Phase {
    RunStart,
    StepStart,
    BeforeInference,
    AfterInference,
    ToolGate,
    BeforeToolExecute,
    AfterToolExecute,
    StepEnd,
    RunEnd,
}
```

- `RunStart`：run 级初始化
- `StepStart`：每轮推理开始
- `BeforeInference`：最后修改推理请求
- `AfterInference`：观察 LLM 返回，修改工具列表或请求终止
- `ToolGate`：纯判定阶段，用于 allow / block / suspend / set-result，可在前序 tool 提交状态后重判
- `BeforeToolExecute`：只对真正要执行的 tool 运行一次，用于执行前副作用
- `AfterToolExecute`：消费工具结果并触发副作用
- `StepEnd`：checkpoint 和 stop policy
- `RunEnd`：清理与最终持久化

## TerminationReason

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

只有 `Suspended` 会映射到 `RunStatus::Waiting`；其他都映射为 `Done`。

## Stop Conditions

可通过配置声明 stop 条件，例如：

- `MaxRounds`
- `Timeout`
- `TokenBudget`
- `ConsecutiveErrors`
- `StopOnTool`
- `ContentMatch`
- `LoopDetection`

这些条件在 `StepEnd` 评估。

## Checkpoint Triggers

`StepEnd` 会把以下内容写入 checkpoint：

- thread messages
- run 生命周期状态
- 持久化状态键
- 挂起的 tool call 状态

## 从 ToolCall 状态推导 RunStatus

run 的状态本质上是所有 tool call 状态的聚合投影：

```rust,ignore
fn derive_run_status(calls: &HashMap<String, ToolCallState>) -> RunStatus {
    let mut has_suspended = false;
    for state in calls.values() {
        match state.status {
            ToolCallStatus::Running | ToolCallStatus::Resuming => {
                return RunStatus::Running;
            }
            ToolCallStatus::Suspended => {
                has_suspended = true;
            }
            _ => {}
        }
    }
    if has_suspended { RunStatus::Waiting } else { RunStatus::Done }
}
```

### 并行 tool call 时间线

```text
Time  tool_A(需审批)  tool_B(需审批)  tool_C(正常)   → Run Status
────────────────────────────────────────────────────────────────
t0    Created        Created        Created        Running
t1    Suspended      Created        Running        Running
t2    Suspended      Suspended      Running        Running
t3    Suspended      Suspended      Succeeded      Waiting
t4    Resuming       Suspended      Succeeded      Running
t5    Succeeded      Suspended      Succeeded      Waiting
t6    Succeeded      Resuming       Succeeded      Running
t7    Succeeded      Succeeded      Succeeded      Done
```

## 挂起如何桥接 run 层与 tool-call 层

### 当前执行模型

当前 step runner 基本分三段：

```text
Stage 1 - ToolGate:
  ToolGate hooks
  可能得到 Suspend / Block / SetResult

Stage 2 - BeforeToolExecute:
  仅对允许执行的调用运行执行前 hook

Stage 3 - Execute:
  对允许执行的调用做串行或并行执行
```

如果任一调用挂起，step 会返回 `StepOutcome::Suspended`，然后：

1. checkpoint 持久化
2. 发出 `RunFinish(Suspended)`
3. 进入 `wait_for_resume_or_cancel`

### wait_for_resume_or_cancel 循环

```rust,ignore
loop {
    let decisions = decision_rx.next().await;
    emit_decision_events_and_messages(decisions);
    prepare_resume(decisions);
    detect_and_replay_resume();
    if !has_suspended_calls() {
        return WaitOutcome::Resumed;
    }
}
```

### Resume replay

恢复时会扫描 `status == Resuming` 的 tool call，并按 `ToolCallResumeMode` 回放：

| Resume Mode | Replay 参数 | 行为 |
|---|---|---|
| `ReplayToolCall` | 原始参数 | 完整重跑 |
| `UseDecisionAsToolResult` | decision 结果 | `ToolGateHook` 在回放时返回 `ToolInterceptPayload::SetResult` |
| `PassDecisionToTool` | decision 结果 | 作为新参数传给 tool |

### 局限：执行中到达的 decision

在当前串行模型里，decision 即使更早到达，也要等当前 Phase 2 工具执行结束后才会被消费，因此恢复存在额外延迟。

## 并发执行模型（未来方向）

理想模型会让“等待 decision”和“执行允许的工具”并发进行，使某个工具一旦得到决策就能立刻恢复。

### 架构

```text
Phase 1 - Intercept

Phase 2 - Concurrent execution:
  execute(tool_C)
  execute(tool_D)
  wait_decision(tool_A) -> replay(tool_A)
  wait_decision(tool_B) -> replay(tool_B)
  barrier: 所有 task 进入终态
```

### 按调用分发 decision

共享 `decision_rx` 需要先 demux 到每个 call 自己的等待通道。

### 状态转移时机

并发模型下，状态会随着事件实时前进，而不是整批推进。

## 协议适配器：SSE 重连

长生命周期 run 可能跨多个前端 SSE 连接，尤其是 AI SDK v6 这类“一次 HTTP 请求对应一次 SSE 流”的协议。

### 问题

```text
Turn 1:
  HTTP POST -> SSE 1 -> tool suspend -> stream 关闭
  但 run 还活着，正在 wait_for_resume_or_cancel

Turn 2:
  新 HTTP POST 带 decision 到来
  如果事件仍然发往旧 channel，就会丢失
```

### 解决方案：ReconnectableEventSink

用一个可替换底层 sender 的 event sink 包装原始 channel，新连接到来时先 `reconnect()` 再投递 decision。

### 重连流程

```text
Turn 1:
  submit() -> 创建 event_tx1 / event_rx1
  run suspend -> SSE 1 结束

Turn 2:
  新请求创建 event_tx2 / event_rx2
  sink.reconnect(event_tx2)
  send_decision
  后续事件都发往 SSE 2
```

### 协议层差异

| 协议 | 挂起信号 | 恢复机制 |
|---|---|---|
| AI SDK v6 | `finish(finishReason: "tool-calls")` | 新 HTTP POST -> reconnect -> send_decision |
| AG-UI | `RUN_FINISHED(outcome: "interrupt")` | 新 HTTP POST -> reconnect -> send_decision |
| CopilotKit | `renderAndWaitForResponse` UI | 同一 SSE 或新请求恢复 |

## 另见

- [HITL 与 Mailbox](./hitl-and-mailbox.md)
- [工具执行模式](../reference/tool-execution-modes.md)
- [状态与快照模型](./state-and-snapshot-model.md)
- [架构](./architecture.md)
- [取消](../reference/cancellation.md)
