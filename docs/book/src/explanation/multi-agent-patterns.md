# Multi-Agent Patterns

Awaken supports multiple patterns for composing agents. This page describes delegation, remote agents, sub-agent execution, and handoff.

## Agent Delegation via AgentSpec.delegates

An agent can declare sub-agents it is allowed to delegate to:

```json
{
  "id": "orchestrator",
  "model": "gpt-4o",
  "system_prompt": "You coordinate tasks across specialized agents.",
  "delegates": ["researcher", "writer", "reviewer"]
}
```

Each ID in `delegates` must be a registered agent in the `AgentSpecRegistry`. During resolution, the runtime creates an `AgentTool` for each delegate. From the LLM's perspective, each sub-agent appears as a regular tool named `agent_run_{delegate_id}`.

When the LLM calls a delegate tool, the `AgentTool` dispatches to the appropriate backend:

- **Local agents** (no `endpoint` field) use `LocalBackend`, which resolves and executes the sub-agent inline within the same runtime.
- **Remote agents** (with `endpoint` field) use `A2aBackend`, which sends an A2A `message:send` request and polls the resulting task for completion.

## Remote Agents via A2A

Remote agents are declared with an `endpoint` in `AgentSpec`:

```json
{
  "id": "remote-analyst",
  "model": "unused-for-remote",
  "system_prompt": "",
  "endpoint": {
    "base_url": "https://analyst.example.com",
    "bearer_token": "token-abc",
    "poll_interval_ms": 1000,
    "timeout_ms": 300000
  }
}
```

The `A2aBackend` handles the A2A protocol lifecycle:

1. Sends a `message:send` request with the user message.
2. Receives a task wrapper, extracts `task.id`, and polls `/tasks/:task_id` at the configured interval.
3. Returns the completed response as a `DelegateRunResult`.
4. The result is formatted as a `ToolResult` and returned to the parent agent's LLM context.

If the remote agent times out or fails, the `DelegateRunStatus` reflects the failure and the parent agent receives an error tool result.

## Sub-Agent Patterns

### Sequential Delegation

The orchestrator calls sub-agents one at a time, using each result to decide the next step:

```text
Orchestrator -> researcher (tool call) -> result
             -> writer (tool call, using researcher output) -> result
             -> reviewer (tool call, using writer output) -> result
```

Each delegation is a tool call within the orchestrator's step loop. The orchestrator sees tool results and decides whether to delegate further or respond directly.

### Parallel Delegation

When the LLM emits multiple tool calls in a single inference response, sub-agent delegations can execute concurrently. The runtime processes tool calls in parallel by default -- if the LLM calls both `agent_run_researcher` and `agent_run_writer` in the same response, both execute simultaneously.

### Nested Delegation

Sub-agents can themselves have `delegates`, creating hierarchies:

```text
orchestrator
  -> team_lead (delegates: [dev_a, dev_b])
       -> dev_a
       -> dev_b
```

Each level resolves independently through the `AgentResolver`. There is no hard depth limit, but each level adds latency and token cost.

## Agent Handoff

Handoff transfers control from one agent to another mid-run without stopping the loop. The mechanism:

1. A plugin (or the handoff extension) writes a new agent ID to the `ActiveAgentKey` state key.
2. At the next step boundary, the loop runner detects the changed key.
3. The loop re-resolves the agent from the `AgentResolver` -- new config, new model, new tools, new system prompt.
4. Execution continues in the same run with the new agent's configuration.

Handoff is a re-resolve, not a loop restart. Thread history is preserved. The new agent sees all prior messages and can continue the conversation seamlessly.

### Handoff vs Delegation

| Aspect | Delegation | Handoff |
|--------|-----------|---------|
| Control flow | Parent calls sub-agent as tool, gets result back | Control transfers entirely to new agent |
| Thread continuity | Sub-agent may use a separate thread context | Same thread, same message history |
| Return path | Result flows back to parent LLM | No return -- new agent owns the run |
| Use case | Task decomposition, specialized subtasks | Role switching, escalation, routing |

## AgentBackend Trait

Both local and remote delegation use the `AgentBackend` trait:

```rust,ignore
pub trait AgentBackend: Send + Sync {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError>;
}
```

`DelegateRunResult` carries the agent ID, terminal status, optional response text, and step count. `DelegateRunStatus` variants: `Completed`, `Failed(String)`, `Cancelled`, `Timeout`.

This trait is the extension point for custom delegation backends beyond local and A2A.

## See Also

- [A2A Protocol Reference](../reference/protocols/a2a.md) -- wire protocol details
- [Architecture](./architecture.md) -- runtime and resolver layers
- [Tool and Plugin Boundary](./tool-and-plugin-boundary.md) -- where delegation tools fit
