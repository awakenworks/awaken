# Run 生命周期与 Phases

本页描述 run 和 tool call 的状态机、8 个 phase、终止条件、checkpoint 触发点，以及挂起 / 恢复如何桥接 run 层与 tool-call 层。

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

- `Running -> Waiting`：当前 step 已经没有可继续推进的工作，且至少还有一个
  tool call 处于 `Suspended`
- `Waiting -> Running`：decision 到来后恢复执行
- `Running -> Done` / `Waiting -> Done`：正常结束、取消或错误

`RunStatus` 是粗粒度状态，当前没有单独的 `Running+Waiting`。如果同一批并行
调用里一部分已挂起、另一部分仍在执行，run 仍然保持 `Running`，直到这批工
作收敛。

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

Awaken 的执行顺序由 8 个 phase 固定下来：

```rust,ignore
pub enum Phase {
    RunStart,
    StepStart,
    BeforeInference,
    AfterInference,
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
- `BeforeToolExecute`：权限检查、拦截、挂起
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

只有在当前 step 已收敛且没有 runnable work 时，`Suspended` 才会映射到
`RunStatus::Waiting`；其他都映射为 `Done`。

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

## 当前 RunStatus 投影规则

从语义上说，只有“存在 suspended call，且已经没有 runnable work”时，run
才应该进入 waiting。tool-call 状态正是这个判断依据，但当前 runtime 只在
run 层、并且只在 step 边界持久化 `RunStatus`：

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

这是 quiescence 规则，不是一个独立持久化的 `Running+Waiting` 状态。并行
batch 执行过程中，即使部分调用已经 suspended，只要还有调用在运行，run 仍
然是 `Running`。

### 并行 tool call 时间线

```text
Time  tool_A(需审批)  tool_B(需审批)  tool_C(正常)   → Run Status
────────────────────────────────────────────────────────────────
t0    Created        Created        Created        Running
t1    Suspended      Created        Running        Running
t2    Suspended      Suspended      Running        Running
t3    Suspended      Suspended      Succeeded      Waiting     batch 收敛，仅剩 suspended call
t4    Resuming       Suspended      Succeeded      Running
t5    Succeeded      Suspended      Succeeded      Waiting
t6    Succeeded      Resuming       Succeeded      Running
t7    Succeeded      Succeeded      Succeeded      Done
```

## 挂起如何桥接 run 层与 tool-call 层

### 持久化的挂起上下文

挂起中的 tool call 不只是保存一个状态位。当前活跃的 `ToolCallState` 还会持久化：

- `resume_mode`：decision 应如何重新投影回执行链路
- `suspension_id`：当前这一次挂起的外部 key
- `suspension_reason`：当前挂起动作/原因
- `resume_input`：最近一次应用到该 call 的外部 decision 载荷

同一个 tool call 再次挂起时，这些字段会被新的挂起上下文覆盖。因此一次
tool call 可以反复 suspend -> resume -> suspend，并且每次都带新的外部 key。

### 当前执行模型（按 step 收敛）

当前 `execute_tools_with_interception` 基本分两段：

```text
Phase 1 - Intercept:
  BeforeToolExecute hooks
  可能得到 Suspend / Block / SetResult

Phase 2 - Execute:
  对允许执行的调用做串行或并行执行
```

如果当前 step 结束时仍有挂起调用，step 会返回 `StepOutcome::Suspended`，然后：

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

- 只要部分 decision 到达，waiting loop 就可以做局部恢复；未恢复的 call 会继续等待
- decision 可以批量到达，也可以逐个到达
- 如果 replay 后再次挂起，新的 `suspension_id` / `suspension_reason` 会替换旧值，waiting loop 继续等待

### Resume target 解析

恢复时，运行时会先用 target 去匹配活跃的 tool-call ID；若未命中，再匹配当前
活跃的 `suspension_id`。因此协议侧既可以发送：

- 内部 tool-call ID
- 面向外部的 suspension key

两者最终都会路由到同一个 `ToolCallState`。

之所以允许两种 target，是因为它们表达的是两层不同语义：

- `call_id`：这是哪一个 tool call
- `suspension_id`：这是这个 tool call 当前哪一次活跃挂起

运行时内部用稳定的 `call_id` 维持状态，而对外协议可以继续使用用户可见的
`suspension_id`。

### Resume replay

恢复时会扫描 `status == Resuming` 的 tool call，并按 `ToolCallResumeMode` 回放：

| Resume Mode | Replay 参数 | 行为 |
|---|---|---|
| `ReplayToolCall` | 原始参数 | 完整重跑 |
| `UseDecisionAsToolResult` | decision 结果 | 直接作为 tool result |
| `PassDecisionToTool` | decision 结果 | 作为新参数传给 tool |

恢复 replay 时，运行时也会把完整的挂起/恢复上下文重新注入 hook 和 tool：

- `PhaseContext.resume_input / suspension_id / suspension_reason`
- `ToolCallContext.resume_input / suspension_id / suspension_reason`

权限 hook、前端工具以及其它拦截器都应通过这组上下文字段判断自己当前是首次执行，
还是一次恢复后的重入执行。

### 局限：没有独立的 run 级 `Running+Waiting`

当前实现的并发仍然是 step 级的。decision 即使更早到达，也要等当前 batch
收敛并进入 waiting loop 后才会被消费；runtime 还没有把“部分 tool 正在跑，
部分 tool 正在等 decision”表达成独立的 run 级状态。

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
