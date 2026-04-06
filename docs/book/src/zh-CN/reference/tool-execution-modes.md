# 工具执行模式

`ToolExecutionMode` 决定 LLM 在同一轮推理里返回多个 tool call 时，运行时如何执行这些调用。

## ToolExecutionMode

```rust,ignore
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    #[default]
    Sequential,
    ParallelBatchApproval,
    ParallelStreaming,
}
```

默认值是 `Sequential`。

## 模式

### Sequential

按顺序逐个执行 tool call。

- 每个调用之间都会刷新状态快照。
- 遇到挂起会在第一个挂起点停止后续调用。
- 适合 tool 之间有数据依赖或顺序很重要的场景。

### ParallelBatchApproval

并发执行所有 tool call，所有调用看到的是同一份冻结快照。

- 对挂起决策采用批量回放。
- 会做并行补丁冲突检查。
- 不会因为单个失败或挂起而提前终止当前 batch；已经启动的调用会继续执行到结束。

### ParallelStreaming

同样并发执行全部调用，但挂起决策一到就立刻回放，不等待其他挂起调用。

- 适合独立工具很多、希望尽快恢复执行的场景。
- 不会因为单个失败或挂起而提前终止当前 batch；已经启动的调用会继续执行到结束。

## 对比

| 行为 | Sequential | ParallelBatchApproval | ParallelStreaming |
|---|---|---|---|
| 执行顺序 | 串行 | 全并发 | 全并发 |
| 状态可见性 | 每次调用前刷新 | 冻结快照 | 冻结快照 |
| 遇挂起是否停止 | 是 | 否，当前 batch 内不会停止 | 否，当前 batch 内不会停止 |
| 遇失败是否停止 | 否 | 否 | 否 |
| decision 回放 | 不适用 | 批量 | 即时 |
| 冲突检查 | 否 | 是 | 是 |

并行模式表达的是 step 级并发，不是 run 级的独立活动状态。只要当前 batch 里
还有活跃工作，run 就保持 `Running`。当 batch 收敛后，如果只剩 suspended
call 等待外部 decision，run 才进入 `Waiting`。当前没有单独的
`Running+Waiting` 状态。

## Executor trait

```rust,ignore
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

并行模式下，`DecisionReplayPolicy` 决定挂起 tool call 的恢复决策何时回放：

```rust,ignore
pub enum DecisionReplayPolicy {
    Immediate,
    BatchAllSuspended,
}
```

## 关键文件

- `crates/awaken-contract/src/contract/executor.rs`
- `crates/awaken-runtime/src/execution/executor.rs`

## 相关

- [Tool Trait](./tool-trait.md)
- [HITL 与 Mailbox](../explanation/hitl-and-mailbox.md)
- [事件](./events.md)
