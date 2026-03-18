# Use Agent Handoff

Agent handoff switches the active agent identity on the same thread — changing the system prompt, model, and available tools without spawning a child agent or losing message history.

## When to use handoff vs delegation

| Use case | Mechanism |
|---|---|
| Independent subtask on a separate thread | `agent_run` (delegation) |
| Switch active persona on the same thread | `agent_handoff` (handoff) |
| Restrict tools temporarily (e.g., plan mode) | `agent_handoff` to a restricted agent |
| Route to a specialist with shared context | `agent_handoff` |

## Setup

Register 2+ agents. The runtime auto-wires `agent_handoff` and `HandoffPlugin` when multiple agents exist.

```rust,ignore
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};

let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are a helpful assistant."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "researcher",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are a research specialist. Search the web and summarize findings.")
            .with_allowed_tools(vec![
                "WebSearch".into(), "WebFetch".into(), "Read".into(), "agent_handoff".into(),
            ]),
    ))
    .build()?;
```

The LLM sees `agent_handoff` in its tool set and can call it:

```json
{ "agent_id": "researcher" }
```

## How it works

1. LLM calls `agent_handoff("researcher")` → writes `HandoffAction::Request` to thread state
2. Next `before_inference` phase: `HandoffPlugin` reads the request, applies the overlay:
   - `OverrideModel` if the target has a different model
   - `AddSystemContext` with the target's system prompt
   - `IncludeOnlyTools` / `ExcludeTool` from `allowed_tools` / `excluded_tools`
3. The LLM now operates under the target agent's configuration
4. To return: call `agent_handoff("assistant")` or any other registered agent

## Tool restrictions

`allowed_tools` on an agent definition acts as a **hard whitelist**:

- The LLM only sees tools in the list (soft enforcement via `IncludeOnlyTools`)
- If the LLM calls a tool not in the list, `HandoffPlugin` blocks it in `before_tool_execute` (hard enforcement)
- `agent_handoff` is always allowed regardless of the whitelist

## Inheriting the base model

Set `model` to an empty string to inherit the base agent's model. This is useful for mode switching where only the prompt and tools change:

```rust,ignore
AgentDefinition::new("")  // inherits model from the base agent
    .with_system_prompt("Read-only mode. Only explore, do not modify.")
    .with_allowed_tools(vec!["Read".into(), "Glob".into(), "Grep".into(), "agent_handoff".into()])
```

## Example: Plan mode

A planner agent restricts tools to read-only exploration:

```rust,ignore
let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "coder",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are a coding assistant."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "planner",
        AgentDefinition::new("")
            .with_system_prompt(
                "Plan mode is active. Explore the codebase using read-only tools only. \
                 Do NOT edit files. When your plan is ready, call agent_handoff(\"coder\") \
                 to return and start implementation.",
            )
            .with_allowed_tools(vec![
                "Read".into(), "Glob".into(), "Grep".into(), "LSP".into(),
                "WebSearch".into(), "WebFetch".into(), "agent_handoff".into(),
            ]),
    ))
    .build()?;
```

The LLM enters plan mode with `agent_handoff("planner")` and exits with `agent_handoff("coder")`.

## Example: Multi-persona routing

Route between specialists based on the conversation topic:

```rust,ignore
let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "router",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt(
                "You are a customer service router. Based on the user's question:\n\
                 - Billing questions → agent_handoff(\"billing\")\n\
                 - Technical issues → agent_handoff(\"tech\")\n\
                 - General questions → answer directly",
            ),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "billing",
        AgentDefinition::new("claude-haiku")
            .with_system_prompt("You are a billing specialist. When done, call agent_handoff(\"router\")."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "tech",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are a tech support specialist. When done, call agent_handoff(\"router\")."),
    ))
    .build()?;
```

Each specialist can use a different model — the handoff overlay applies the model switch automatically.

## Handoff state

Handoff state is thread-scoped and persists across runs:

- `active_agent`: the currently active agent variant (None = base agent)
- `requested_agent`: a pending handoff request, consumed on next inference

To clear all handoff state programmatically (return to base agent):

```rust,ignore
use tirea_extension_handoff::clear_handoff_action;

// In a tool's execute_effect:
effect.with_action(AfterToolExecuteAction::State(clear_handoff_action()))
```

## See also

- [Multi-Agent Design Patterns](../explanation/multi-agent-design-patterns.md#swarm--peer-handoff) for the Swarm / Peer Handoff pattern
- [Use Sub-Agent Delegation](./use-sub-agent-delegation.md) for hierarchical delegation with `agent_run`
