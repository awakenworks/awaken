---
title: "Tool Execution Modes"
description: "Awaken separates the serializable contract enum from the runtime executor. ToolExecutionMode is the contract-level enum. The loop actually executes tools through ResolvedAgent.tool_executor, a…"
---

Awaken separates the serializable contract enum from the runtime executor.
`ToolExecutionMode` is the contract-level enum. The loop actually executes
tools through `ResolvedAgent.tool_executor`, a `ToolExecutor` implementation.

**Crate path:** `awaken::contract::executor::ToolExecutionMode`

Current wiring:

- The built-in resolver installs `SequentialToolExecutor` by default.
- `AgentSpec` does not contain a `tool_execution_mode` field.
- Use `ResolvedAgent::with_tool_executor(...)` or a custom resolver to install
  `ParallelToolExecutor`.
- The contract enum is available for protocol or config surfaces that want to
  carry an execution-mode intent, but it is not automatically applied by
  `AgentRuntimeBuilder`.

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

The default is `Sequential`.

## Executors

### SequentialToolExecutor

Executes tool calls one at a time in the order the LLM returned them.

- The state snapshot is refreshed between calls (`requires_incremental_state`
  returns `true`), so each tool sees the effects of the previous one.
- Stops at the first suspension. If tool call 2 of 4 suspends, tool calls
  3 and 4 are not executed. Failures do not stop execution.
- Simplest mode. Use when tool calls have data dependencies or when ordering
  matters.

```rust
// SequentialToolExecutor runs calls in order, stopping at first suspension.
let executor = SequentialToolExecutor;
let results = executor.execute(&tools, &calls, &ctx).await?;
```

### ParallelToolExecutor::batch_approval()

Executes all tool calls concurrently. All tools see the same frozen state
snapshot.

- Exposes `DecisionReplayPolicy::BatchAllSuspended` for callers that need to
  coordinate suspended-call replay.
- Exposes `requires_conflict_check() == true`. The default loop commits returned
  tool results in result order; custom executor integrations that merge parallel
  state batches should use the parallel merge helpers.
- Does not stop on suspension or failure; all calls run to completion.
- Use when tool calls are independent and you want an all-or-nothing approval
  gate for HITL workflows.

```rust
let executor = ParallelToolExecutor::batch_approval();
// decision_replay_policy() == DecisionReplayPolicy::BatchAllSuspended
```

### ParallelToolExecutor::streaming()

Executes all tool calls concurrently with the same frozen state snapshot.

- Exposes `DecisionReplayPolicy::Immediate` for callers that coordinate replay
  outside the default sequential wait loop.
- Exposes `requires_conflict_check() == true`.
- Does not stop on suspension or failure; all calls run to completion.
- Use when tool calls are independent and you want the fastest end-to-end
  completion without batching approval.

```rust
let executor = ParallelToolExecutor::streaming();
// decision_replay_policy() == DecisionReplayPolicy::Immediate
```

## Comparison

| Behavior | Sequential | ParallelBatchApproval | ParallelStreaming |
|---|---|---|---|
| Execution order | One at a time | All concurrently | All concurrently |
| State freshness | Refreshed between calls | Frozen snapshot | Frozen snapshot |
| Stops on suspension | Yes (first suspension) | No | No |
| Stops on failure | No | No | No |
| Exposed decision policy | N/A | Batch (all at once) | Immediate (one by one) |
| Requires conflict checks for custom parallel merges | No | Yes | Yes |

## Executor trait

Both `SequentialToolExecutor` and `ParallelToolExecutor` implement the
`ToolExecutor` trait:

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute tool calls and return results.
    async fn execute(
        &self,
        tools: &HashMap<String, Arc<dyn Tool>>,
        calls: &[ToolCall],
        base_ctx: &ToolCallContext,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError>;

    /// Strategy name for logging.
    fn name(&self) -> &'static str;

    /// Whether the executor needs state refreshed between individual tool calls.
    fn requires_incremental_state(&self) -> bool { false }
}
```

The `name()` values are `"sequential"`, `"parallel_batch_approval"`, and
`"parallel_streaming"`.

## DecisionReplayPolicy

Describes when resume decisions for suspended tool calls should be replayed by
a caller that coordinates parallel HITL. The default loop waits for decisions
after a step suspends, prepares resume state, and replays through the standard
tool pipeline.

```rust
pub enum DecisionReplayPolicy {
    /// Replay each resolved suspended call as soon as its decision arrives.
    Immediate,
    /// Replay only when all currently suspended calls have decisions.
    BatchAllSuspended,
}
```

## Key Files

- `crates/awaken-contract/src/contract/executor.rs` -- `ToolExecutionMode` enum
- `crates/awaken-runtime/src/execution/executor.rs` -- `SequentialToolExecutor`, `ParallelToolExecutor`, `ToolExecutor` trait

## Related

- [Tool Trait](/tool-trait/)
- [HITL and Mailbox](/explanation/hitl-and-mailbox/)
- [Events](/events/)
