# Use Agent Handoff

Use this when you need to dynamically switch an agent's behavior (system prompt, model, or tool set) within a running agent loop without terminating the run or spawning a new thread.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- Familiarity with `Plugin`, `StateKey`, and `AgentRuntimeBuilder`

## Overview

Handoff performs dynamic same-thread agent variant switching. Instead of ending the current run and starting a new agent, handoff applies an `AgentOverlay` that overrides parts of the base agent configuration. The switch is instant -- no run termination or re-resolution occurs.

Key types:

- `HandoffPlugin` -- the plugin that manages overlay registration and lifecycle hooks.
- `AgentOverlay` -- per-variant overrides for system prompt, model, and tool filters.
- `HandoffState` -- tracks the active variant and any pending handoff request.
- `HandoffAction` -- reducer actions: `Request`, `Activate`, `Clear`.

## Steps

1. Define agent variant overlays.

Each overlay specifies which parts of the base agent configuration to override. Fields left as `None` inherit the base agent's values.

```rust,ignore
use awaken::AgentOverlay;

let researcher = AgentOverlay {
    system_prompt: Some("You are a research specialist. Find and cite sources.".into()),
    model: Some("anthropic/claude-sonnet".into()),
    allowed_tools: Some(vec!["web_search".into(), "read_document".into()]),
    excluded_tools: None,
};

let writer = AgentOverlay {
    system_prompt: Some("You are a technical writer. Produce clear documentation.".into()),
    model: None, // inherits base model
    allowed_tools: None, // all tools available
    excluded_tools: Some(vec!["web_search".into()]),
};
```

2. Build a `HandoffPlugin` with variant overlays.

```rust,ignore
use std::collections::HashMap;
use std::sync::Arc;
use awaken::HandoffPlugin;

let mut overlays = HashMap::new();
overlays.insert("researcher".to_string(), researcher);
overlays.insert("writer".to_string(), writer);

let handoff = HandoffPlugin::new(overlays);
```

3. Register the plugin on the runtime builder.

```rust,ignore
use awaken::AgentRuntimeBuilder;

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("agent_handoff", Arc::new(handoff))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

The plugin ID must be `"agent_handoff"` (exported as `HANDOFF_PLUGIN_ID`). The plugin registers hooks on `Phase::RunStart` and `Phase::StepEnd` to synchronize handoff state.

4. Request a handoff from within a tool or hook.

Use the action helpers to create `HandoffAction` mutations and dispatch them through a `StateCommand`:

```rust,ignore
use awaken::{request_handoff, activate_handoff, clear_handoff};
use awaken::state::StateCommand;
use awaken::extensions::handoff::ActiveAgentKey;

// Request a switch to the "researcher" variant (pending until next phase boundary)
let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(request_handoff("researcher"));

// Directly activate a variant (skips the request step)
let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(activate_handoff("writer"));

// Clear handoff state and return to the base agent
let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(clear_handoff());
```

5. Look up overlays from plugin state.

The plugin exposes a method to retrieve the overlay for any registered variant:

```rust,ignore
let overlay = handoff.overlay("researcher");
// Returns Option<&AgentOverlay>
```

The effective agent ID is determined by `HandoffPlugin::effective_agent`, which returns the requested variant if one is pending, otherwise the currently active variant:

```rust,ignore
let state: &HandoffState = /* from context */;
let agent_id = HandoffPlugin::effective_agent(state);
// Returns Option<&String> -- None means the base agent is active
```

## How It Works

`HandoffState` has two fields:

- `active_agent: Option<String>` -- the currently active variant (`None` = base agent).
- `requested_agent: Option<String>` -- a pending handoff request, consumed at the next phase boundary.

The internal `HandoffSyncHook` runs at `RunStart` and `StepEnd`. When it detects a `requested_agent`, it promotes the request to `active_agent` and clears the request. This two-phase approach ensures the switch happens at a safe boundary in the agent loop.

## Handoff vs Delegation

| | Handoff | Delegation |
|---|---|---|
| Thread | Same thread, same run | Spawns a sub-agent on a separate thread |
| State | Shared -- overlays modify the current agent in-place | Isolated -- delegate has its own state |
| Use case | Switching personas or tool sets mid-conversation | Offloading a self-contained subtask |
| Overhead | Zero -- no run restart | Higher -- new run lifecycle |

Use handoff when you want the agent to change behavior while retaining conversational context. Use delegation when the subtask is independent and the delegate should not see or modify the parent's state.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| Overlay not applied | Variant name in `request_handoff` does not match a key in the overlays map | Ensure the string matches exactly |
| `StateError::KeyAlreadyRegistered` | Another plugin registers the `ActiveAgentKey` | Only one `HandoffPlugin` should be registered per runtime |
| Hook not firing | Plugin not in the agent's active hook filter | Add `"agent_handoff"` to `active_hook_filter` or leave the filter empty |

## Key Files

- `crates/awaken-runtime/src/extensions/handoff/mod.rs` -- module root and public exports
- `crates/awaken-runtime/src/extensions/handoff/plugin.rs` -- `HandoffPlugin` implementation
- `crates/awaken-runtime/src/extensions/handoff/types.rs` -- `AgentOverlay` struct
- `crates/awaken-runtime/src/extensions/handoff/state.rs` -- `HandoffState` and `ActiveAgentKey`
- `crates/awaken-runtime/src/extensions/handoff/action.rs` -- `HandoffAction` and helper functions

## Related

- [Add a Plugin](./add-a-plugin.md)
- [Build an Agent](./build-an-agent.md)
