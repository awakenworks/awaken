# Migration Notes

## 0.1 → 0.2

### Module Reorganization

The `event` and `protocol` modules have been consolidated into a unified `io` module.

```rust,ignore
// Before (0.1)
use tirea_contract::event::AgentEvent;
use tirea_contract::protocol::{ProtocolInputAdapter, ProtocolOutputEncoder, ProtocolHistoryEncoder};

// After (0.2)
use tirea_contract::io::{AgentEvent, RunRequest, RuntimeInput, ToolCallDecision};
use tirea_contract::transport::{Transcoder, Identity};
```

The three protocol traits (`ProtocolInputAdapter`, `ProtocolOutputEncoder`, `ProtocolHistoryEncoder`)
are replaced by a single `Transcoder` trait. Use `Identity<T>` as a pass-through when no
transformation is needed.

### Plugin Interface

The single `on_phase(Phase, StepContext)` method is replaced by individual per-phase methods,
each with a typed context parameter.

```rust,ignore
// Before (0.1)
#[async_trait]
impl AgentPlugin for MyPlugin {
    fn id(&self) -> &str { "my_plugin" }
    async fn on_phase(&self, phase: Phase, ctx: &mut StepContext<'_, '_>) {
        match phase {
            Phase::BeforeInference => { /* ... */ }
            Phase::AfterInference => { /* ... */ }
            _ => {}
        }
    }
}

// After (0.2)
#[async_trait]
impl AgentPlugin for MyPlugin {
    fn id(&self) -> &str { "my_plugin" }
    async fn before_inference(&self, ctx: &mut BeforeInferenceContext<'_, '_>) {
        // ...
    }
    async fn after_inference(&self, ctx: &mut AfterInferenceContext<'_, '_>) {
        // ...
    }
}
```

Available phase methods: `run_start`, `step_start`, `before_inference`, `after_inference`,
`before_tool_execute`, `after_tool_execute`, `step_end`, `run_end`. All have empty default
implementations.

Other plugin API changes:

| 0.1 | 0.2 |
|-----|-----|
| `ctx.skip_inference()` | `ctx.terminate_plugin_requested()` |
| `ctx.ask_frontend_tool(...)` | Removed — use `SuspendTicket` via `BeforeToolExecuteContext` |
| `ctx.ask(...)` / `ctx.allow(...)` / `ctx.deny(...)` | Removed — use block/allow/suspend on `BeforeToolExecuteContext` |
| `StepContext` skip/termination fields | Removed — use `RunAction` / `ToolCallAction` on typed contexts |

### Event Stream Changes

Removed variants:

| Removed | Replacement |
|---------|-------------|
| `AgentEvent::InteractionRequested` | Tool-call suspension emitted via `ToolCallDone` with pending status |
| `AgentEvent::InteractionResolved` | `AgentEvent::ToolCallResumed { target_id, result }` |
| `AgentEvent::Pending` | Run lifecycle uses `TerminationReason::Suspended` |

New variants:

- `AgentEvent::ReasoningDelta { delta }` — streaming reasoning content.
- `AgentEvent::ReasoningEncryptedValue { encrypted_value }` — encrypted reasoning block.
- `AgentEvent::ToolCallResumed { target_id, result }` — resume decision applied.
- `AgentEvent::InferenceComplete` now includes `duration_ms: u64`.

### Suspension Model

The single-slot "pending interaction" model is replaced by per-call tool-call suspension.

```rust,ignore
// Before (0.1)
let pending = ctx.pending_interaction();
let frontend = ctx.pending_frontend_invocation();

// After (0.2)
let suspended: &HashMap<String, SuspendedCall> = ctx.suspended_calls();
```

Key type changes:

| 0.1 | 0.2 |
|-----|-----|
| `ToolSuspension` | `SuspendTicket` (deprecated alias provided) |
| `pending_interaction()` | `suspended_calls()` |
| `pending_frontend_invocation()` | Removed |
| Single pending slot | Per-call `SuspendedCall` map |
| Resume via outbox replay | Resume via `ToolCallDecision` on decision channel |

Each `SuspendedCall` carries: `call_id`, `tool_name`, `arguments`, `suspension` payload,
`pending` projection, and `resume_mode` (ReplayToolCall / UseDecisionAsToolResult / PassDecisionToTool).

### Type Renames

Deprecated aliases are provided for all renames. Update imports to suppress warnings:

| 0.1 | 0.2 |
|-----|-----|
| `RunLifecycleStatus` | `RunStatus` |
| `RunLifecycleState` | `RunState` |
| `RunLifecycleAction` | `RunAction` |
| `ToolCallLifecycleAction` | `ToolCallAction` |
| `ToolCallLifecycleState` | `ToolCallState` |
| `ToolCallLifecycleStatesState` | `ToolCallStatesMap` |
| `ToolSuspension` | `SuspendTicket` |

### Method Renames

Deprecated forwarding methods are provided. Update call sites to suppress warnings:

| 0.1 | 0.2 |
|-----|-----|
| `StateManager::apply(patch)` | `StateManager::commit(patch)` |
| `StateManager::apply_batch(patches)` | `StateManager::commit_batch(patches)` |

### Stop Conditions

Stop conditions moved from core config to `StopPolicyPlugin`. If you were passing stop conditions
to `BaseAgent`, register them as a plugin instead:

```rust,ignore
use tirea::runtime::StopPolicyPlugin;
use tirea::runtime::StopConditionSpec;

let stop_plugin = StopPolicyPlugin::from_specs(vec![
    StopConditionSpec::MaxRounds { rounds: 10 },
    StopConditionSpec::Timeout { seconds: 300 },
]);
// Register stop_plugin as an AgentPlugin
```

### `RunRequest` Changes

`RunRequest` now requires `initial_decisions` and optionally accepts `parent_run_id`:

```rust,ignore
// Before (0.1)
let req = RunRequest {
    agent_id: "agent_1".into(),
    thread_id: Some("thread_1".into()),
    run_id: None,
    resource_id: None,
    state: None,
    messages: vec![user_message],
};

// After (0.2)
let req = RunRequest {
    agent_id: "agent_1".into(),
    thread_id: Some("thread_1".into()),
    run_id: None,
    parent_run_id: None,  // new
    parent_thread_id: None,
    resource_id: None,
    origin: RunOrigin::default(),
    state: None,
    messages: vec![user_message],
    initial_decisions: vec![],  // new, required
};
```

### Durable State Paths

New durable state paths persisted in thread state:

| Path | Type | Content |
|------|------|---------|
| `__run` | `RunState` | Run id, status, done_reason, updated_at |
| `__tool_call_states` | `ToolCallStatesMap` | Per-call lifecycle status map |
| `__suspended_tool_calls` | `SuspendedToolCallsState` | Suspended call payloads |

### Deleted Crates

- `tirea-interaction-plugin` — functionality absorbed into core suspension model.

## Pre-0.1 → 0.1

### Terminology Updates

- `Session` -> `Thread`
- Storage traits remain `ThreadReader` / `ThreadWriter` / `ThreadStore`
- Session routes -> `/v1/threads` routes

### Runtime Surface Updates

- Prefer `AgentOs::run_stream(RunRequest)` for app-level integration.
- Use `RunContext` as run-scoped mutable workspace.
- Use protocol adapters (`AG-UI`, `AI SDK v6`) for transport-specific request/response mapping.
