# A2A Protocol

The Agent-to-Agent (A2A) adapter implements Google's [A2A protocol](https://google.github.io/A2A/) for remote agent discovery, task delegation, and inter-agent communication.

**Feature gate**: `a2a`

## Endpoints

| Route | Method | Description |
|-------|--------|-------------|
| `/v1/a2a/tasks/send` | POST | Submit a task to an agent. Returns a task ID and initial status. |
| `/v1/a2a/tasks/:task_id` | GET | Poll task status by ID. |
| `/v1/a2a/tasks/:task_id/cancel` | POST | Cancel a running task. |
| `/v1/a2a/.well-known/agent` | GET | Discovery endpoint. Returns the default agent card. |
| `/v1/a2a/agents` | GET | List all registered agents. |
| `/v1/a2a/agents/:agent_id/agent-card` | GET | Agent card for a specific agent. |
| `/v1/a2a/agents/:agent_id/message:send` | POST | Send a message to a specific agent. |
| `/v1/a2a/agents/:agent_id/tasks/:action` | GET/POST | Agent-scoped task operations. |

## Agent Card

The discovery endpoint returns an `AgentCard` describing the agent's capabilities:

```json
{
  "id": "assistant",
  "name": "My Agent",
  "description": "A helpful assistant",
  "url": "https://example.com/v1/a2a",
  "version": "1.0.0",
  "capabilities": {
    "streaming": false,
    "push_notifications": false,
    "state_transition_history": false
  },
  "skills": [
    {
      "id": "general",
      "name": "General Q&A",
      "description": "Answer general questions",
      "tags": ["qa"]
    }
  ]
}
```

Agent cards are derived from registered `AgentSpec` entries. Each agent with an `id` in the spec registry produces a discoverable card.

## Task Send

```json
{
  "taskId": "optional-client-provided-id",
  "agentId": "optional-agent-id",
  "message": {
    "role": "user",
    "parts": [{ "type": "text", "text": "Summarize this document" }]
  }
}
```

The task is submitted as a background `MailboxJob`. The response returns immediately with a `taskId` and `"submitted"` status. Clients poll `/v1/a2a/tasks/:task_id` for completion.

## Task Status

```json
{
  "taskId": "abc-123",
  "status": {
    "state": "completed",
    "message": { "role": "agent", "parts": [{ "type": "text", "text": "..." }] }
  }
}
```

Task states follow the A2A lifecycle: `submitted`, `working`, `completed`, `failed`, `cancelled`.

## Remote Agent Delegation

Awaken agents can delegate to remote A2A agents via `AgentTool::remote()`. The `A2aBackend` sends a `tasks/send` request to the remote endpoint and polls for completion. From the LLM's perspective, this is a regular tool call -- the A2A transport is transparent.

Configuration for remote agents is declared in `AgentSpec`:

```json
{
  "id": "remote-researcher",
  "endpoint": {
    "base_url": "https://remote-agent.example.com",
    "bearer_token": "...",
    "poll_interval_ms": 1000,
    "timeout_ms": 300000
  }
}
```

Agents with an `endpoint` field are resolved as remote A2A agents. Agents without it run locally.

## Related

- [Multi-Agent Patterns](../../explanation/multi-agent-patterns.md) -- delegation and handoff design
- [Agent Card contract](https://google.github.io/A2A/#/documentation?id=agent-card) -- A2A specification
