---
title: "Use Agent Handoff"
description: "Use this when you need to switch to another registered agent ID within the same thread and run, without terminating the run or spawning a new thread."
---

Use this when you need to switch to another registered agent ID within the same
thread and run, without terminating the run or spawning a new thread.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- Familiarity with `Plugin`, `StateKey`, and `AgentRuntimeBuilder`

## Overview

Handoff records a requested active agent in state. At the next step boundary,
the loop reads `ActiveAgentIdKey`, re-resolves that agent ID through the
`AgentResolver`, deactivates the old plugins, activates the new plugins, and
continues on the same thread history.

Register concrete `AgentSpec` values for the variants you want to switch to.
`AgentOverlay` is optional metadata stored by `HandoffPlugin` and retrievable
through `overlay()`; the built-in loop does not merge overlay fields into the
base `AgentSpec`.

Key types:

- `HandoffPlugin` -- the plugin that syncs handoff state into the active agent ID.
- `AgentOverlay` -- optional per-variant metadata for integrations that want to inspect system prompt, model, and tool filters.
- `HandoffState` -- tracks the active variant and any pending handoff request.
- `HandoffAction` -- reducer actions: `Request`, `Activate`, `Clear`.

## Steps

1. Define agent variants as registered specs.

Each handoff target is a normal `AgentSpec`. The string passed to
`request_handoff()` must match one of these agent IDs.

```rust
use awaken::registry_spec::AgentSpec;

let mut base = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.");

let mut researcher = AgentSpec::new("researcher")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a research specialist. Find and cite sources.");
researcher.allowed_tools = Some(vec!["web_search".into(), "read_document".into()]);

let writer = AgentSpec::new("writer")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a technical writer. Produce clear documentation.");
```

2. Build a `HandoffPlugin`.

```rust
use awaken::extensions::handoff::HandoffPlugin;

let handoff = HandoffPlugin::new(Default::default());
```

3. Register the plugin on the runtime builder.

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::AgentRuntimeBuilder;

base.plugin_ids.push("agent_handoff".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("agent_handoff", Arc::new(handoff))
    .with_agent_spec(base)
    .with_agent_spec(researcher)
    .with_agent_spec(writer)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .build()?;
```

The plugin ID must be `"agent_handoff"` (exported as `HANDOFF_PLUGIN_ID`) and
must be listed in `AgentSpec.plugin_ids`. The plugin registers hooks on
`Phase::RunStart` and `Phase::StepEnd` to synchronize handoff state.

4. Request a handoff from within a tool or hook.

Use the action helpers to create `HandoffAction` mutations and dispatch them through a `StateCommand`:

```rust
use awaken::extensions::handoff::{request_handoff, activate_handoff, clear_handoff, ActiveAgentKey};
use awaken::state::StateCommand;

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

5. Optionally look up overlay metadata.

If you configured overlays for your own integration, the plugin exposes a
lookup method:

```rust
let overlay = handoff.overlay("researcher");
// Returns Option<&AgentOverlay>
```

The effective agent ID is determined by `HandoffPlugin::effective_agent`, which returns the requested variant if one is pending, otherwise the currently active variant:

```rust
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
| State | Same thread state; active agent is re-resolved at a step boundary | Isolated -- delegate has its own state |
| Use case | Switching personas or tool sets mid-conversation | Offloading a self-contained subtask |
| Overhead | Zero -- no run restart | Higher -- new run lifecycle |

Use handoff when you want the agent to change behavior while retaining conversational context. Use delegation when the subtask is independent and the delegate should not see or modify the parent's state.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| Handoff resolve failed | Variant name in `request_handoff` does not match a registered agent ID | Register an `AgentSpec` with that ID |
| `StateError::KeyAlreadyRegistered` | Another plugin registers the `ActiveAgentKey` | Only one `HandoffPlugin` should be registered per runtime |
| Hook not firing | Agent hook filter excludes the plugin | Include `"agent_handoff"` in the hook filter, or leave the filter empty |

## Key Files

- `crates/awaken-runtime/src/extensions/handoff/mod.rs` -- module root and public exports
- `crates/awaken-runtime/src/extensions/handoff/plugin.rs` -- `HandoffPlugin` implementation
- `crates/awaken-runtime/src/extensions/handoff/types.rs` -- `AgentOverlay` struct
- `crates/awaken-runtime/src/extensions/handoff/state.rs` -- `HandoffState` and `ActiveAgentKey`
- `crates/awaken-runtime/src/extensions/handoff/action.rs` -- `HandoffAction` and helper functions

## Related

- [Add a Plugin](/awaken/how-to/add-a-plugin/)
- [Build an Agent](/awaken/how-to/build-an-agent/)
