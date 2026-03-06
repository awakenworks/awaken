# Use Sub-Agent Delegation

Use this when one agent orchestrates other agents through built-in delegation tools.

## What is auto-wired

By default, `AgentOs::resolve(...)` wires these tools and behaviors:

- tools: `agent_run`, `agent_stop`, `agent_output`
- behaviors: `agent_tools`, `agent_recovery`

No manual plugin registration is required for baseline delegation.

## Steps

1. Define worker agents and orchestrator.

```rust,ignore
let os = AgentOs::builder()
    .with_agent(
        "writer",
        AgentDefinition::new("deepseek-chat")
            .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
    )
    .with_agent(
        "reviewer",
        AgentDefinition::new("deepseek-chat")
            .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
    )
    .with_agent(
        "orchestrator",
        AgentDefinition::new("deepseek-chat")
            .with_allowed_agents(vec!["writer".to_string(), "reviewer".to_string()]),
    )
    .build()?;
```

2. In orchestrator prompt/tool flow, call delegation tools.

- start or resume: `agent_run`
- stop background run tree: `agent_stop`
- fetch output snapshot: `agent_output`

3. Choose foreground/background execution per `agent_run` call.

- `background=false`: parent waits and receives child progress
- `background=true`: child runs asynchronously and can be resumed/stopped later

## Verify

- Orchestrator can call `agent_run` for allowed child agents.
- Child run status transitions are visible (`running`, `completed`, `failed`, `stopped`).
- `agent_output` returns child-thread outputs for the requested `run_id`.

## Common Errors

- Target agent filtered by `allowed_agents` / `excluded_agents`.
- Worker agents accidentally retain delegation tools and recurse unexpectedly.
- Background runs left running without `agent_stop`/resume policy.

## Related

- [Sub-Agent Delegation](../explanation/sub-agent-delegation.md)
- [Capability Matrix](../reference/capability-matrix.md)
- `crates/tirea-agentos/tests/real_multi_subagent_deepseek.rs`
