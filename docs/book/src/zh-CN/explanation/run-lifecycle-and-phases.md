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

### Tool call 状态时间线

当 LLM 一次返回多个 tool call（例如 `[tool_A, tool_B, tool_C]`）时，每个 call 都有自己的状态。挂起调用可以等待外部 decision，允许执行的调用则继续通过当前配置的 executor 运行。

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

step runner 中的工具执行分三段：

```text
Stage 1 - ToolGate（串行，逐 call）:
  对每个 call:
    ToolGate hooks -> 允许 / 阻断 / 挂起 / 设置结果
    Suspend?  -> 标记 Suspended，继续检查后续 call
    Block?    -> 标记 Failed，立即返回
    SetResult -> 写入提供的结果，继续
    None      -> 加入 allowed_calls

Stage 2 - BeforeToolExecute（仅 allowed_calls）:
  对将要执行的调用运行执行前 hook

Stage 3 - Execute（仅 allowed_calls）:
  Sequential mode: 逐个执行，遇到首次挂起就停止
  Parallel mode:   批量执行，收集所有结果
```

如果任一调用挂起，step 会返回 `StepOutcome::Suspended`。orchestrator 随后：

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

关键属性：

- 循环支持部分恢复：如果只收到 tool_A 的 decision，而 tool_B 仍挂起，tool_A 会先回放，循环继续等待 tool_B。
- decision 可以批量到达，也可以逐个到达。
- 返回 `WaitOutcome::Resumed` 后，orchestrator 会回到 step loop，进入下一轮 LLM 推理。

### Resume replay

恢复时会扫描 `status == Resuming` 的 tool call，并按 `ToolCallResumeMode` 回放：

| Resume Mode | Replay 参数 | 行为 |
|---|---|---|
| `ReplayToolCall` | 原始参数 | 完整重跑 |
| `UseDecisionAsToolResult` | decision 结果 | `ToolGateHook` 在回放时返回 `ToolInterceptPayload::SetResult` |
| `PassDecisionToTool` | decision 结果 | 作为新参数传给 tool |

已完成调用（`Succeeded`、`Failed`、`Cancelled`）会被跳过。

### 工具执行期间到达的 decision

内置 resolver 默认安装 `SequentialToolExecutor`，因此允许执行的 tool call 会逐个运行。若 decision 在某个工具仍在执行时到达，它会留在 decision channel 中，直到 step 进入 `wait_for_resume_or_cancel` 后才被消费。

如果通过自定义 resolver 或 `ResolvedAgent::with_tool_executor(...)` 安装 `ParallelToolExecutor`，allowed batch 可以并发执行。即便如此，内置 resume loop 仍会在 step 挂起后消费 decision，准备 resume state，并通过标准的 `ToolGate` -> `BeforeToolExecute` -> tool -> `AfterToolExecute` 流水线回放挂起调用。契约层的 `DecisionReplayPolicy` 用于描述自定义并行 HITL 集成里的协同行为；它不是 `AgentSpec` 字段。

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
