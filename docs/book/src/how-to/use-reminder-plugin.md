# Use Reminder Plugin

Use this when reminders should be injected into inference context from persisted state.

## Prerequisites

- `tirea-extension-reminder` is enabled.
- Agent includes reminder behavior id.

## Steps

1. Register plugin and attach behavior.

```rust,ignore
use std::sync::Arc;
use tirea::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea::extensions::reminder::ReminderPlugin;

let os = AgentOsBuilder::new()
    .with_registered_behavior(
        "reminder",
        Arc::new(ReminderPlugin::new().with_clear_after_llm_request(true)),
    )
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("deepseek-chat").with_behavior_id("reminder"),
    ))
    .build()?;
```

2. Write reminder state actions.

```rust,ignore
use tirea::extensions::reminder::add_reminder_action;

let add = add_reminder_action("Call Alice at 3pm");
// dispatch as state action in your behavior/tool pipeline
```

`ReminderState` path is `reminders` and stores deduplicated `items: Vec<String>`.

3. Choose clear strategy.

- `true` (default): reminders are cleared after each LLM call.
- `false`: reminders persist until explicit clear action.

## Verify

- On next inference, reminder text is injected as session context.
- When `clear_after_llm_request=true`, reminder list is cleared after injection.

## Common Errors

- Behavior registered but not attached to target agent.
- Assuming reminders are per-run; reminder scope is thread-level state.

## Related Example

- No dedicated starter ships with reminders enabled by default; layer this plugin onto `examples/ai-sdk-starter/README.md` or `examples/copilotkit-starter/README.md`

## Key Files

- `crates/tirea-extension-reminder/src/lib.rs`
- `crates/tirea-extension-reminder/src/actions.rs`
- `crates/tirea-extension-reminder/src/state.rs`

## Related

- [Add a Plugin](./add-a-plugin.md)
- [Run Lifecycle and Phases](../explanation/run-lifecycle-and-phases.md)
