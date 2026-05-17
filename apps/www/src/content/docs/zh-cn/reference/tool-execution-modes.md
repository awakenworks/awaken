---
title: "工具执行模式"
description: "Awaken 把可序列化的 contract enum 和真正执行工具的 runtime executor 分开处理。ToolExecutionMode 是契约层 enum；loop 实际通过 ResolvedAgent.tool_executor 里的 ToolExecutor 实现执行工具。"
---

Awaken 把可序列化的 contract enum 和真正执行工具的 runtime executor 分开处理。`ToolExecutionMode` 是契约层 enum；loop 实际通过 `ResolvedAgent.tool_executor` 里的 `ToolExecutor` 实现执行工具。

当前接线状态：

- 内置 resolver 默认安装 `SequentialToolExecutor`。
- `AgentSpec` 没有 `tool_execution_mode` 字段。
- 如需并行执行，使用 `ResolvedAgent::with_tool_executor(...)` 或自定义 resolver 安装 `ParallelToolExecutor`。
- `ToolExecutionMode` 可用于协议或配置表面携带执行模式意图，但 `AgentRuntimeBuilder` 不会自动应用它。

## ToolExecutionMode

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    #[default]
    Sequential,
    ParallelBatchApproval,
    ParallelStreaming,
}
```

默认值是 `Sequential`。

## Executors

### SequentialToolExecutor

按顺序逐个执行 tool call。

- 每个调用之间都会刷新状态快照。
- 遇到挂起会在第一个挂起点停止后续调用。
- 适合 tool 之间有数据依赖或顺序很重要的场景。

### ParallelToolExecutor::batch_approval()

并发执行所有 tool call，所有调用看到的是同一份冻结快照。

- 暴露 `DecisionReplayPolicy::BatchAllSuspended`，供需要协调挂起回放的调用方使用。
- 暴露 `requires_conflict_check() == true`。默认 loop 会按结果顺序提交 tool 返回的命令；自定义并行集成如果要合并并行 state batch，应使用 parallel merge helpers。
- 不会因单个失败或挂起而提前停止其它调用。

### ParallelToolExecutor::streaming()

同样并发执行全部调用，但挂起决策一到就立刻回放，不等待其他挂起调用。

- 暴露 `DecisionReplayPolicy::Immediate`，供默认顺序 wait loop 之外的并行 HITL 协调用方使用。
- 暴露 `requires_conflict_check() == true`。
- 适合独立工具很多、希望尽快恢复执行的场景。

## 对比

| 行为 | Sequential | ParallelBatchApproval | ParallelStreaming |
|---|---|---|---|
| 执行顺序 | 串行 | 全并发 | 全并发 |
| 状态可见性 | 每次调用前刷新 | 冻结快照 | 冻结快照 |
| 遇挂起是否停止 | 是 | 否 | 否 |
| 遇失败是否停止 | 否 | 否 | 否 |
| 暴露的 decision policy | 不适用 | 批量 | 即时 |
| 自定义并行 merge 需要冲突检查 | 否 | 是 | 是 |

## Executor trait

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        tools: &HashMap<String, Arc<dyn Tool>>,
        calls: &[ToolCall],
        base_ctx: &ToolCallContext,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError>;

    fn name(&self) -> &'static str;

    fn requires_incremental_state(&self) -> bool { false }
}
```

## DecisionReplayPolicy

`DecisionReplayPolicy` 描述并行 HITL 协调用方应何时回放挂起 tool call 的恢复决策。默认 loop 会在 step 挂起后等待 decision、写入 resume state，并通过标准工具流水线回放。

```rust
pub enum DecisionReplayPolicy {
    Immediate,
    BatchAllSuspended,
}
```

## 关键文件

- `crates/awaken-contract/src/contract/executor.rs`
- `crates/awaken-runtime/src/execution/executor.rs`

## 相关

- [Tool Trait](/tool-trait/)
- [HITL 与 Mailbox](/explanation/hitl-and-mailbox/)
- [事件](/events/)
