# Tool Execution Modes

`ToolExecutionMode` controls how the runtime executes tool calls that the LLM
requests in a single inference step.

**Crate path:** `awaken::contract::executor::ToolExecutionMode`

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

The default is `Sequential`.

## Modes

### Sequential

Executes tool calls one at a time in the order the LLM returned them.

- The state snapshot is refreshed between calls (`requires_incremental_state`
  returns `true`), so each tool sees the effects of the previous one.
- Stops at the first suspension. If tool call 2 of 4 suspends, tool calls
  3 and 4 are not executed. Failures do not stop execution.
- Simplest mode. Use when tool calls have data dependencies or when ordering
  matters.

```rust,ignore
// SequentialToolExecutor runs calls in order, stopping at first suspension.
let executor = SequentialToolExecutor;
let results = executor.execute(&tools, &calls, &ctx).await?;
```

### ParallelBatchApproval

Executes all tool calls concurrently. All tools see the same frozen state
snapshot.

- Suspension decisions are replayed using `DecisionReplayPolicy::BatchAllSuspended`:
  the runtime waits until every suspended call has a decision before replaying
  any of them.
- Enforces parallel patch conflict checks (`requires_conflict_check` returns
  `true`).
- Does not stop the current batch on suspension or failure; all already-started
  calls run to completion.
- Use when tool calls are independent and you want an all-or-nothing approval
  gate for HITL workflows.

```rust,ignore
let executor = ParallelToolExecutor::batch_approval();
// decision_replay_policy() == DecisionReplayPolicy::BatchAllSuspended
```

### ParallelStreaming

Executes all tool calls concurrently with the same frozen state snapshot.

- Suspension decisions are replayed using `DecisionReplayPolicy::Immediate`:
  each decision is replayed as soon as it arrives, without waiting for the
  others.
- Enforces parallel patch conflict checks.
- Does not stop the current batch on suspension or failure; all already-started
  calls run to completion.
- Use when tool calls are independent and you want the fastest end-to-end
  completion without batching approval.

```rust,ignore
let executor = ParallelToolExecutor::streaming();
// decision_replay_policy() == DecisionReplayPolicy::Immediate
```

## Comparison

| Behavior | Sequential | ParallelBatchApproval | ParallelStreaming |
|---|---|---|---|
| Execution order | One at a time | All concurrently | All concurrently |
| State freshness | Refreshed between calls | Frozen snapshot | Frozen snapshot |
| Stops on suspension | Yes (first suspension) | No, not within the current batch | No, not within the current batch |
| Stops on failure | No | No | No |
| Decision replay | N/A | Batch (all at once) | Immediate (one by one) |
| Conflict checks | No | Yes | Yes |

Parallel modes are step-level concurrency, not a separate run-level activity
state. The run remains `Running` while the current batch still has active work.
After the batch quiesces, if suspended calls remain and no runnable work is
left, the run transitions to `Waiting`. There is no distinct `Running+Waiting`
run status today.

## Executor trait

Both `SequentialToolExecutor` and `ParallelToolExecutor` implement the
`ToolExecutor` trait:

```rust,ignore
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

Controls when resume decisions for suspended tool calls are replayed into the
execution pipeline. Only relevant for parallel modes.

```rust,ignore
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

- [Tool Trait](./tool-trait.md)
- [HITL and Mailbox](../explanation/hitl-and-mailbox.md)
- [Events](./events.md)
