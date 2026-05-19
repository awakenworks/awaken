---
title: "Scheduled Actions"
description: "Scheduled actions are the primary mechanism for plugins and tools to request side-effects during a phase execution cycle. Any hook, tool, or external module can schedule an action via…"
---

Scheduled actions are the primary mechanism for plugins and tools to request
side-effects during a phase execution cycle.  Any hook, tool, or external module
can schedule an action via `StateCommand::schedule_action::<A>(payload)`.  The
runtime dispatches the action to its registered handler during the EXECUTE stage
of the target phase.

## How it works

```text
Hook / Tool                    Runtime
    |                            |
    |-- StateCommand ----------->|  (contains scheduled_actions)
    |   schedule_action::<A>(p)  |
    |                            |-- commit state updates
    |                            |-- dispatch to handler(A, p)
    |                            |      |
    |                            |      |-- handler returns StateCommand
    |                            |      |   (may schedule more actions)
    |                            |<-----'
    |                            |-- commit handler results
```

### Scheduling from a hook

```rust
use awaken_runtime::agent::state::ExcludeTool;

async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<ExcludeTool>("dangerous_tool".into())?;
    Ok(cmd)
}
```

### Scheduling from a tool

```rust
use awaken_runtime::agent::state::AddContextMessage;
use awaken_contract::contract::context_message::ContextMessage;

async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<AddContextMessage>(
        ContextMessage::system("my_tool.hint", "Remember to check the docs."),
    )?;
    Ok(ToolOutput::with_command(
        ToolResult::success("my_tool", json!({"ok": true})),
        cmd,
    ))
}
```

---

## Core Actions (awaken-runtime)

These are always available.  Registered by the internal `LoopActionHandlersPlugin`.

### AddContextMessage

| | |
|---|---|
| **Key** | `runtime.add_context_message` |
| **Phase** | `BeforeInference` |
| **Payload** | `ContextMessage` |
| **Import** | `awaken_runtime::agent::state::AddContextMessage` |

Injects a context message into the LLM conversation for the current step.
Messages can be persistent (survive across steps), ephemeral (one-shot), or
throttled (cooldown-based).

**Used by:** skills plugin (skill catalog), reminder plugin (rule-based hints),
deferred-tools plugin (deferred tool list), custom hooks.

```rust
cmd.schedule_action::<AddContextMessage>(
    ContextMessage::system_persistent("my_plugin.info", "Always verify inputs."),
)?;
```

### SetInferenceOverride

| | |
|---|---|
| **Key** | `runtime.set_inference_override` |
| **Phase** | `BeforeInference` |
| **Payload** | `InferenceOverride` |
| **Import** | `awaken_runtime::agent::state::SetInferenceOverride` |

Overrides inference parameters (model, temperature, max_tokens, top_p) for the
current step only.  Multiple overrides are merged; last-writer-wins per field.

```rust
cmd.schedule_action::<SetInferenceOverride>(InferenceOverride {
    temperature: Some(0.0),  // force deterministic
    ..Default::default()
})?;
```

### ExcludeTool

| | |
|---|---|
| **Key** | `runtime.exclude_tool` |
| **Phase** | `BeforeInference` |
| **Payload** | `String` (tool ID) |
| **Import** | `awaken_runtime::agent::state::ExcludeTool` |

Removes a tool from the set offered to the LLM for the current step.  Multiple
exclusions are additive.

**Used by:** permission plugin (unconditionally denied tools), deferred-tools
plugin (deferred tools replaced by ToolSearch).

```rust
cmd.schedule_action::<ExcludeTool>("rm".into())?;
```

### IncludeOnlyTools

| | |
|---|---|
| **Key** | `runtime.include_only_tools` |
| **Phase** | `BeforeInference` |
| **Payload** | `Vec<String>` (tool IDs) |
| **Import** | `awaken_runtime::agent::state::IncludeOnlyTools` |

Restricts the tool set to only the listed IDs.  Multiple `IncludeOnlyTools`
actions are unioned.  Combined with `ExcludeTool`, exclusions are applied after
the include-only filter.

```rust
cmd.schedule_action::<IncludeOnlyTools>(vec!["search".into(), "calculator".into()])?;
```

> Tool interception is **not** modeled as a scheduled action anymore.
> Implement `ToolGateHook` and register it with
> `PluginRegistrar::register_tool_gate_hook()` when you need to block, suspend,
> or short-circuit tool calls before execution.

---

## Deferred-Tools Actions (awaken-ext-deferred-tools)

Available when the deferred-tools plugin is active.

### DeferToolAction

| | |
|---|---|
| **Key** | `deferred_tools.defer` |
| **Phase** | `BeforeInference` |
| **Payload** | `Vec<String>` (tool IDs) |
| **Import** | `awaken_ext_deferred_tools::state::DeferToolAction` |

Moves tools from eager to deferred mode.  Deferred tools are excluded from the
LLM tool set and made available via ToolSearch instead, reducing prompt token
usage.

The handler updates `DeferralState`, setting each tool's mode to `Deferred`.

```rust
cmd.schedule_action::<DeferToolAction>(vec!["rarely_used_tool".into()])?;
```

### PromoteToolAction

| | |
|---|---|
| **Key** | `deferred_tools.promote` |
| **Phase** | `BeforeInference` |
| **Payload** | `Vec<String>` (tool IDs) |
| **Import** | `awaken_ext_deferred_tools::state::PromoteToolAction` |

Moves tools from deferred to eager mode.  Promoted tools are included in the
LLM tool set for subsequent steps.

The handler updates `DeferralState`, setting each tool's mode to `Eager`.

Typically triggered automatically when ToolSearch returns results, but can be
scheduled manually by any plugin or tool.

```rust
cmd.schedule_action::<PromoteToolAction>(vec!["needed_tool".into()])?;
```

---

## Plugin Action Usage Matrix

Which plugins **schedule** which actions:

| Plugin | AddContext | SetOverride | Exclude | IncludeOnly | Defer | Promote |
|--------|:---------:|:-----------:|:-------:|:-----------:|:-----:|:-------:|
| **permission** | | | X | | | |
| **skills** | X | | | | | | |
| **reminder** | X | | | | | | |
| **deferred-tools** | X | | X | | X | X |
| **observability** | | | | | | |
| **mcp** | | | | | | |
| **generative-ui** | | | | | | |

---

## Defining Custom Actions

Plugins can define their own actions by implementing `ScheduledActionSpec` and
registering a handler that implements `TypedScheduledActionHandler<A>` via
`PluginRegistrar::register_scheduled_action`.

### Spec

`ScheduledActionSpec` declares the action's identity, phase, and payload type.
The default `encode_payload` / `decode_payload` impls use the runtime's JSON
codec; override them only when you need custom serialization.

```rust
use awaken_contract::error::StateError;
use awaken_contract::model::{JsonValue, Phase, ScheduledActionSpec};

pub struct MyCustomAction;

impl ScheduledActionSpec for MyCustomAction {
    const KEY: &'static str = "my_plugin.custom_action";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = MyPayload;

    // Default impls of encode_payload / decode_payload come from the trait;
    // override only when custom serialization is needed.
}
```

### Handler

The handler trait the runtime dispatches to:

```rust
#[async_trait]
pub trait TypedScheduledActionHandler<A>: Send + Sync + 'static
where
    A: ScheduledActionSpec,
{
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: A::Payload,
    ) -> Result<StateCommand, StateError>;
}
```

The handler receives a `PhaseContext` (snapshot + run metadata) and returns a
`StateCommand`, which can carry state mutations, additional scheduled actions
(triggering another convergence round), and effects.

### Wiring

```rust
fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_scheduled_action::<MyCustomAction, _>(MyHandler)?;
    Ok(())
}
```

Other plugins and tools can then schedule your action:

```rust
cmd.schedule_action::<MyCustomAction>(my_payload)?;
```

---

## Convergence and cascading

Scheduled actions execute within the phase convergence loop. After each round
of action dispatch, the runtime checks whether new actions were produced. If
so, the loop repeats to process them.

### How the loop works

```text
Phase EXECUTE stage:
  round 1: dispatch queued actions -> handlers return StateCommands
           commit state, collect newly scheduled actions
  round 2: dispatch new actions    -> handlers may schedule more
           ...
  round N: no new actions          -> phase converges, loop exits
```

An action handler can schedule new actions for the **same phase**, which
causes another round. This enables cascading behaviors (e.g., a handler adds
a context message, which triggers a filter action from another plugin).

### Limits

The loop is bounded by `DEFAULT_MAX_PHASE_ROUNDS` (16). If actions are still
being produced after 16 rounds, the runtime returns a
`StateError::PhaseRunLoopExceeded` error with the phase name and round count.
This prevents infinite loops from misconfigured or recursive handlers.

### Failed actions

When an action handler returns an error, the action is not retried. Instead,
it is recorded in the `FailedScheduledActions` state key, which holds a list
of `FailedScheduledAction` entries (action key, payload, and error message).
Plugins or tests can inspect this key to detect handler failures.

```rust
let failed = store.read::<FailedScheduledActions>().unwrap_or_default();
assert!(failed.is_empty(), "expected no failed actions");
```

See [Plugin Internals](/awaken/explanation/plugin-internals/) for the full
convergence loop description and phase execution model.
