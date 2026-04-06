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

- `Running -> Waiting`: the current step has no runnable work left and at least
  one tool call is suspended.
- `Waiting -> Running`: decisions arrive, the run resumes.
- `Running -> Done` or `Waiting -> Done`: terminal transition on completion, cancellation, or error.
- `Done -> *`: not allowed. Terminal state.

`RunStatus` is intentionally coarse. It does not encode a separate
`Running+Waiting` state. If some tool calls are suspended while other calls in
the same batch are still running, the run remains `Running` until the batch
reaches quiescence.

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

- `Suspended` maps to `RunStatus::Waiting` once the current step has quiesced
  and no runnable work remains.
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

## Current RunStatus Projection

Conceptually, a run is waiting only when there are suspended tool calls and no
remaining runnable work. Tool-call state is what determines that quiescence,
but the current runtime persists `RunStatus` only at the run level and only at
step boundaries:

```rust,ignore
fn derive_run_status(calls: &HashMap<String, ToolCallState>) -> RunStatus {
    let mut has_suspended = false;
    for state in calls.values() {
        match state.status {
            // Any Running or Resuming call → Run is still executing
            ToolCallStatus::Running | ToolCallStatus::Resuming => {
                return RunStatus::Running;
            }
            ToolCallStatus::Suspended => {
                has_suspended = true;
            }
            // Succeeded / Failed / Cancelled are terminal — keep checking
            _ => {}
        }
    }
    if has_suspended {
        RunStatus::Waiting   // No executing calls, but some await decisions
    } else {
        RunStatus::Done      // All calls in terminal state → step complete
    }
}
```

Decision table for the current coarse run state:

| Any `Running`/`Resuming`? | Any `Suspended`? | Run Status | Meaning |
|---|---|---|---|
| Yes | — | **Running** | Tools are actively executing |
| No | Yes | **Waiting** | No runnable work remains; awaiting external decisions |
| No | No | **Done** | All calls terminal → proceed to next step |

This is a quiescence rule, not a separate persisted `Running+Waiting` run
status. During a parallel batch, some tool calls may already be suspended while
others are still running; the run remains `Running` until that batch finishes.

### Parallel tool call timeline

When an LLM returns multiple tool calls (e.g. `[tool_A, tool_B, tool_C]`), their
states evolve independently:

```text
Time  tool_A(approval-req)  tool_B(approval-req)  tool_C(normal)   → Run Status
────────────────────────────────────────────────────────────────
t0    Created        Created        Created        Running     Step starts
t1    Suspended      Created        Running        Running     tool_A intercepted
t2    Suspended      Suspended      Running        Running     tool_B intercepted, tool_C executing
t3    Suspended      Suspended      Succeeded      Waiting     batch quiesced, only suspended calls remain
t4    Resuming       Suspended      Succeeded      Running     tool_A decision arrives
t5    Succeeded      Suspended      Succeeded      Waiting     tool_A replay done
t6    Succeeded      Resuming       Succeeded      Running     tool_B decision arrives
t7    Succeeded      Succeeded      Succeeded      Done        All terminal → next step
```

The key point is that the run does not publish a separate `Running+Waiting`
state. It flips to `Waiting` only after the current batch has finished all
active work and unresolved suspended calls are the only remaining blockers.

## Suspension Bridges Run and Tool-Call Layers

### Persistent suspend context

Each suspended tool call persists more than just a status flag. The active
`ToolCallState` stores:

- `resume_mode` -- how the decision should be projected back into execution
- `suspension_id` -- the current external suspension key
- `suspension_reason` -- the current suspension action / reason string
- `resume_input` -- the most recent external decision payload applied to the call

These fields are replaced each time the same tool call suspends again. This is
why a single tool call can suspend, resume, and suspend again with a fresh
external key each time.

### Current execution model (step-quiesced)

Tool execution is split into two serial phases inside
`execute_tools_with_interception`:

```text
Phase 1 — Intercept (serial, per-call):
  for each call:
    BeforeToolExecute hooks → check for intercept actions
    Suspend?  → mark Suspended, set suspended=true, continue
    Block?    → mark Failed, return immediately
    SetResult → mark with provided result, continue
    None      → add to allowed_calls

Phase 2 — Execute (allowed_calls only):
  Sequential mode: one by one, break on first suspension
  Parallel mode:   batch execute, collect all results
```

After both phases, if `suspended == true`, the step returns
`StepOutcome::Suspended`. The orchestrator then:

1. Persists checkpoint (messages, tool call states)
2. Emits `RunFinish(Suspended)` to protocol encoders
3. Enters `wait_for_resume_or_cancel` loop

### wait_for_resume_or_cancel loop

```rust,ignore
loop {
    let decisions = decision_rx.next().await;  // block until decisions arrive
    emit_decision_events_and_messages(decisions);
    prepare_resume(decisions);       // Suspended → Resuming
    detect_and_replay_resume();      // re-execute Resuming calls
    if !has_suspended_calls() {
        return WaitOutcome::Resumed; // all resolved → exit wait
    }
    // Some calls still Suspended → continue waiting
}
```

Key properties:
- The loop handles **partial resume**: if only tool_A's decision arrives but
  tool_B is still suspended, tool_A is replayed immediately and the loop
  continues waiting for tool_B.
- Decisions can arrive in batches or one at a time.
- On `WaitOutcome::Resumed`, the orchestrator re-enters the step loop for the
  next LLM inference round.
- If a replayed call suspends again, the new `suspension_id` / `suspension_reason`
  replace the previous ones and the loop continues waiting.

### Resume target resolution

Resume decisions are routed by target identifier before any replay happens.
The runtime first tries to match the incoming target against the active
tool-call ID and then against the active `suspension_id`. This lets protocols
use either:

- the internal tool-call ID
- the external suspension key shown to users

Both resolve to the same `ToolCallState` entry.

The reason both are accepted is that they answer different questions:

- `call_id`: "which tool call is this?"
- `suspension_id`: "which active suspension of that tool call is being resolved?"

The runtime keeps `call_id` as the stable internal identity and treats
`suspension_id` as the current external-facing identity for the active
suspension.

### Resume replay

`detect_and_replay_resume` scans `ToolCallStates` for calls with
`status == Resuming` and re-executes them through the standard tool pipeline.
The `arguments` field already reflects the resume mode (set by
`prepare_resume`):

| Resume Mode | Arguments on Replay | Behavior |
|---|---|---|
| `ReplayToolCall` | Original arguments | Full re-execution |
| `UseDecisionAsToolResult` | Decision result | `FrontendToolPlugin` intercepts in `BeforeToolExecute`, returns `SetResult` |
| `PassDecisionToTool` | Decision result | Tool receives decision as arguments |

Already-completed calls (`Succeeded`, `Failed`, `Cancelled`) are skipped.

The runtime also re-injects the full suspend/resume context into hooks and
tools during replay:

- `PhaseContext.resume_input / suspension_id / suspension_reason`
- `ToolCallContext.resume_input / suspension_id / suspension_reason`

This is the canonical way for permission hooks, frontend tools, and other
interceptors to tell whether they are running for the first time or as part of
a resumed suspension.

### Limitation: decisions during execution

In the current serial model, decisions that arrive while Phase 2 tools are
still executing sit in the channel buffer. They are only consumed when the step
finishes and the orchestrator enters `wait_for_resume_or_cancel`.

This means:
- tool_A's approval arrives at t2 (while tool_C is executing)
- tool_A is not replayed until t3 (after tool_C finishes)
- The delay equals the remaining execution time of Phase 2 tools

## Concurrent Execution Model (future)

The ideal model executes suspended-tool waits and allowed-tool execution in
parallel, so a decision for tool_A can trigger immediate replay even while
tool_C is still running.

### Architecture

```text
Phase 1 — Intercept (same as current)

Phase 2 — Concurrent execution:
  ┌─ task: execute(tool_C) ──────────────────────────┐
  │                                                   │
  ├─ task: execute(tool_D) ────────────┐              │
  │                                    │              │
  ├─ task: wait_decision(tool_A) → replay(tool_A) ──┐│
  │                                                  ││
  ├─ task: wait_decision(tool_B) ──────────→ replay(tool_B)
  │                                                  │
  └─ barrier: all tasks reach terminal state ────────┘
```

### Per-call decision routing

The shared `decision_rx` channel carries batches of decisions for multiple
tool calls. A dispatcher task demuxes decisions to per-call notification
channels:

```rust,ignore
struct ToolCallWaiter {
    waiters: HashMap<String, oneshot::Sender<ToolCallResume>>,
}

impl ToolCallWaiter {
    async fn dispatch_loop(&mut self, decision_rx: &mut UnboundedReceiver<DecisionBatch>) {
        while let Some(batch) = decision_rx.next().await {
            for (call_id, resume) in batch {
                if let Some(tx) = self.waiters.remove(&call_id) {
                    let _ = tx.send(resume);
                }
            }
            if self.waiters.is_empty() { break; }
        }
    }
}
```

Each suspended tool call gets a `oneshot::Receiver`. When its decision arrives,
the receiver wakes the task, which runs the replay immediately — concurrently
with any still-executing allowed tools.

### State transition timing

With the concurrent model, state transitions happen as events occur rather
than in batches:

```text
t0: tool_C starts executing          → RunStatus: Running
t1: tool_A decision arrives, replay  → RunStatus: Running (tool_A Resuming)
t2: tool_A replay completes          → RunStatus: Running (tool_C still Running)
t3: tool_C completes                 → RunStatus: Waiting (tool_B still Suspended)
t4: tool_B decision arrives, replay  → RunStatus: Running (tool_B Resuming)
t5: tool_B replay completes          → RunStatus: Done (all terminal)
```

No artificial delay — each tool call progresses as fast as its external
dependency allows.

## Protocol Adapter: SSE Reconnection

A backend run may span multiple frontend SSE connections. This is especially
relevant for the AI SDK v6 protocol, where each HTTP request corresponds to
one "turn" and produces one SSE stream.

### Problem

```text
Turn 1 (user message):
  HTTP POST → SSE stream 1 → events flow → tool suspends
  → RunFinish(Suspended) → finish event → SSE stream 1 closes

  The run is still alive in wait_for_resume_or_cancel.
  But the event_tx channel from SSE stream 1 has been dropped.

Turn 2 (tool output / approval):
  HTTP POST → SSE stream 2 → decision delivered to orchestrator
  → orchestrator resumes → emits events
  → events go to... the dropped event_tx? Lost.
```

### Solution: ReconnectableEventSink

Replace the fixed `ChannelEventSink` with a reconnectable wrapper that allows
swapping the underlying channel sender:

```rust,ignore
struct ReconnectableEventSink {
    inner: Arc<tokio::sync::Mutex<mpsc::UnboundedSender<AgentEvent>>>,
}

impl ReconnectableEventSink {
    fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { inner: Arc::new(tokio::sync::Mutex::new(tx)) }
    }

    /// Replace the underlying channel. Called when a new SSE connection
    /// arrives for an existing suspended run.
    async fn reconnect(&self, new_tx: mpsc::UnboundedSender<AgentEvent>) {
        *self.inner.lock().await = new_tx;
    }
}

#[async_trait]
impl EventSink for ReconnectableEventSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.inner.lock().await.send(event);
    }
}
```

### Reconnection flow

```text
Turn 1:
  submit() → create (event_tx1, event_rx1)
           → ReconnectableEventSink(event_tx1)
           → spawn_execution (run starts)
  events → event_tx1 → event_rx1 → SSE stream 1
  tool suspends → finish(tool-calls) → SSE stream 1 closes
  event_tx1 still held by ReconnectableEventSink (sends fail silently)
  run alive in wait_for_resume_or_cancel

Turn 2:
  new HTTP request with decisions arrives
  create (event_tx2, event_rx2)
  sink.reconnect(event_tx2)          ← swap channel
  send_decision → decision channel → orchestrator resumes
  events → ReconnectableEventSink → event_tx2 → event_rx2 → SSE stream 2
  run completes → RunFinish(NaturalEnd) → SSE stream 2 closes
```

No events are lost between SSE connections because:
- During suspend, the orchestrator is blocked in `wait_for_resume_or_cancel`
  and emits no events.
- `reconnect()` completes before `send_decision()`, so the first resume event
  (`RunStart`) goes to the new channel.

### Protocol-specific behavior

| Protocol | Suspend Signal | Resume Mechanism |
|---|---|---|
| **AI SDK v6** | `finish(finishReason: "tool-calls")` | New HTTP POST → reconnect → send_decision |
| **AG-UI** | `RUN_FINISHED(outcome: "interrupt")` | New HTTP POST → reconnect → send_decision |
| **CopilotKit** | `renderAndWaitForResponse` UI | Same SSE or new request via AG-UI |

## See Also

- [HITL and Mailbox](./hitl-and-mailbox.md) -- suspension, resume, and decision handling
- [Tool Execution Modes](../reference/tool-execution-modes.md) -- Sequential vs Parallel execution
- [State and Snapshot Model](./state-and-snapshot-model.md) -- how state is read and written during phases
- [Architecture](./architecture.md) -- three-layer overview
- [Cancellation](../reference/cancellation.md) -- auto-cancellation on new message
