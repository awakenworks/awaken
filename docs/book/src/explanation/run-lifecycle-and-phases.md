# Run Lifecycle and Phases

This page describes the state machines that govern run execution and tool call processing, the phase enum, termination conditions, and checkpoint triggers.

## RunStatus

A run's coarse lifecycle is captured by `RunStatus`:

```text
Running --+--> Waiting --+--> Running (resume)
          |              |
          +--> Done      +--> Done
```

```rust,ignore
pub enum RunStatus {
    Running,  // Actively executing (default)
    Waiting,  // Paused, waiting for external decisions
    Done,     // Terminal -- cannot transition further
}
```

- `Running -> Waiting`: a tool call suspends, the run pauses for external input.
- `Waiting -> Running`: decisions arrive, the run resumes.
- `Running -> Done` or `Waiting -> Done`: terminal transition on completion, cancellation, or error.
- `Done -> *`: not allowed. Terminal state.

## ToolCallStatus

Each tool call in a run has its own lifecycle:

```text
New --> Running --+--> Succeeded (terminal)
                  +--> Failed (terminal)
                  +--> Cancelled (terminal)
                  +--> Suspended --> Resuming --+--> Running
                                                +--> Suspended (re-suspend)
                                                +--> Succeeded/Failed/Cancelled
```

```rust,ignore
pub enum ToolCallStatus {
    New,        // Created, not yet executing
    Running,    // Currently executing
    Suspended,  // Waiting for external decision
    Resuming,   // Decision received, about to re-execute
    Succeeded,  // Completed successfully (terminal)
    Failed,     // Completed with error (terminal)
    Cancelled,  // Cancelled externally (terminal)
}
```

Key transitions:

- `Suspended` can only move to `Resuming` or `Cancelled` -- it cannot jump directly to `Running` or a success/failure state.
- `Resuming` has wide transitions: it can re-enter `Running`, re-suspend, or reach any terminal state.
- Terminal states (`Succeeded`, `Failed`, `Cancelled`) cannot transition to any non-self state.

## Phase Enum

The `Phase` enum defines the eight execution phases in order:

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

**RunStart** -- fires once at the beginning of a run. Plugins initialize run-scoped state.

**StepStart** -- fires at the beginning of each inference round. Step counter increments.

**BeforeInference** -- last chance to modify the inference request (system prompt, tools, parameters). Plugins can skip inference by setting a behavior flag.

**AfterInference** -- fires after the LLM response arrives. Plugins can inspect the response, modify tool call lists, or request termination.

**BeforeToolExecute** -- fires before each tool call batch. Permission checks, interception, and suspension happen here.

**AfterToolExecute** -- fires after tool results are available. Plugins can inspect results and trigger side effects.

**StepEnd** -- fires at the end of each inference round. Checkpoint persistence happens here. Stop conditions (max rounds, token budget, loop detection) are evaluated.

**RunEnd** -- fires once when the run terminates, regardless of reason. Cleanup and final state persistence.

## TerminationReason

When a run ends, the `TerminationReason` records why:

```rust,ignore
pub enum TerminationReason {
    NaturalEnd,           // LLM returned no tool calls
    BehaviorRequested,    // A plugin requested inference skip
    Stopped(StoppedReason), // A stop condition fired (code + optional detail)
    Cancelled,            // External cancellation signal
    Blocked(String),      // Permission checker blocked the run
    Suspended,            // Waiting for external tool-call resolution
    Error(String),        // Error path
}
```

`TerminationReason::to_run_status()` maps each variant to the appropriate `RunStatus`:

- `Suspended` maps to `RunStatus::Waiting` (the run can resume).
- All other variants map to `RunStatus::Done`.

## Stop Conditions

Declarative stop conditions are configured per agent via `StopConditionSpec`:

| Variant | Trigger |
|---------|---------|
| `MaxRounds { rounds }` | Step count exceeds limit |
| `Timeout { seconds }` | Wall-clock time exceeds limit |
| `TokenBudget { max_total }` | Cumulative token usage exceeds budget |
| `ConsecutiveErrors { max }` | Sequential tool errors exceed threshold |
| `StopOnTool { tool_name }` | A specific tool is called |
| `ContentMatch { pattern }` | LLM output matches a regex pattern |
| `LoopDetection { window }` | Repeated identical tool calls within a sliding window |

Stop conditions are evaluated at `StepEnd`. When one fires, the run terminates with `TerminationReason::Stopped`.

## Checkpoint Triggers

State is persisted at `StepEnd` after each inference round. The checkpoint includes:

- Thread messages (append-only)
- Run lifecycle state (`RunStatus`, step count, termination reason)
- Persistent state keys (those registered with `persistent: true`)
- Tool call states for suspended calls

Checkpoints enable resume from the last completed step after a crash or intentional suspension.

## Suspension Bridges Run and Tool-Call Layers

When a tool call suspends:

1. The tool returns `ToolResult` with `ToolStatus::Pending`.
2. `AfterToolExecute` hooks fire with the pending result.
3. The loop runner transitions the tool call to `ToolCallStatus::Suspended`.
4. If all tool calls in the batch are resolved or suspended, the step ends.
5. If any tool call remains suspended, the run transitions to `RunStatus::Waiting` with `TerminationReason::Suspended`.
6. The run persists its checkpoint and yields control.

On resume:

1. External decisions arrive as `ToolCallResume` values.
2. The runtime applies decisions via `prepare_resume`, transitioning tool calls to `Resuming`.
3. The loop detects `Resuming` tool calls and replays them according to their `ToolCallResumeMode`.
4. The run transitions back to `RunStatus::Running` and re-enters the step loop.

## See Also

- [HITL and Mailbox](./hitl-and-mailbox.md) -- suspension, resume, and decision handling
- [State and Snapshot Model](./state-and-snapshot-model.md) -- how state is read and written during phases
- [Architecture](./architecture.md) -- three-layer overview
