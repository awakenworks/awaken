# A2A Protocol

The Agent-to-Agent (A2A) adapter implements the [A2A protocol](https://a2a-protocol.org/latest/specification/) for remote agent discovery, task delegation, and inter-agent communication.

**Feature gate**: `server`

## Endpoints

| Route | Method | Description |
|-------|--------|-------------|
| `/.well-known/agent-card.json` | GET | Discovery endpoint for the public/default agent card. |
| `/v1/a2a/message:send` | POST | Send a message to the public/default A2A agent. Returns a task wrapper. |
| `/v1/a2a/message:stream` | POST | Streaming send over SSE. |
| `/v1/a2a/tasks` | GET | List A2A tasks. |
| `/v1/a2a/tasks/:task_id` | GET | Poll task status by ID. |
| `/v1/a2a/tasks/:task_id:cancel` | POST | Cancel a running task. |
| `/v1/a2a/tasks/:task_id:subscribe` | POST | Subscribe to task updates over SSE. |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | POST | Create a push notification config. |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | GET | List push notification configs. |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | GET / DELETE | Read or delete a push notification config. |
| `/v1/a2a/extendedAgentCard` | GET | Extended agent card. Returns `501` unless `capabilities.extendedAgentCard=true`. |

Tenant-scoped variants mirror the same interface under `/v1/a2a/:tenant/...`, for example `/v1/a2a/research/message:send` and `/v1/a2a/research/tasks/:task_id`.

## Agent Card

The discovery endpoint returns an `AgentCard` describing the exposed interface and capabilities:

```json
{
  "name": "My Agent",
  "description": "A helpful assistant",
  "supportedInterfaces": [
    {
      "url": "https://example.com/v1/a2a",
      "protocolBinding": "HTTP+JSON",
      "protocolVersion": "1.0"
    }
  ],
  "version": "1.0.0",
  "capabilities": {
    "streaming": true,
    "pushNotifications": true,
    "stateTransitionHistory": false,
    "extendedAgentCard": false
  },
  "defaultInputModes": ["text/plain"],
  "defaultOutputModes": ["text/plain"],
  "skills": [
    {
      "id": "general",
      "name": "General Q&A",
      "description": "Answer general questions",
      "tags": ["qa"],
      "inputModes": ["text/plain"],
      "outputModes": ["text/plain"]
    }
  ]
}
```

Agent cards are derived from registered `AgentSpec` entries. The top-level legacy `url`/`id` fields are not emitted.

## Message Send

```json
{
  "message": {
    "taskId": "optional-client-provided-id",
    "contextId": "optional-client-provided-id",
    "messageId": "msg-123",
    "role": "ROLE_USER",
    "parts": [{ "text": "Summarize this document" }]
  },
  "configuration": {
    "returnImmediately": true
  }
}
```

The server maps A2A tasks to Awaken thread/mailbox execution. The response uses the v1 task wrapper shape:

```json
{
  "task": {
    "id": "optional-client-provided-id",
    "contextId": "optional-client-provided-id",
    "status": {
      "state": "TASK_STATE_SUBMITTED"
    }
  }
}
```

If `returnImmediately` is omitted or `false`, the adapter waits for a terminal/interrupted task state before responding.

## Task Status

`GET /v1/a2a/tasks/:task_id` returns a `Task` resource:

```json
{
  "id": "abc-123",
  "contextId": "abc-123",
  "status": {
    "state": "TASK_STATE_COMPLETED",
    "message": {
      "messageId": "msg-response",
      "role": "ROLE_AGENT",
      "parts": [{ "text": "..." }]
    }
  },
  "history": []
}
```

Task states follow the v1 enum names such as `TASK_STATE_SUBMITTED`, `TASK_STATE_WORKING`, `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, and `TASK_STATE_CANCELED`.

## Optional capability defaults

Awaken currently enables these A2A capabilities by default:

- `streaming = true`
- `pushNotifications = true`

`extendedAgentCard` remains opt-in and is enabled only when `ServerConfig.a2a_extended_card_bearer_token` is configured. When disabled, the extended card endpoints return spec-shaped unsupported errors.

## Remote Agent Delegation

Awaken agents can delegate to remote A2A agents via `AgentTool::remote()`. The `A2aBackend` sends a `message:send` request to the remote endpoint, reads the returned `task.id`, then polls `/tasks/:task_id` for completion. From the LLM's perspective, this is a regular tool call — the A2A transport is transparent.

Configuration for remote agents is declared in `AgentSpec`. `RemoteEndpoint` is generic, and A2A uses `backend: "a2a"`:

```json
{
  "id": "remote-researcher",
  "endpoint": {
    "backend": "a2a",
    "base_url": "https://remote-agent.example.com/v1/a2a",
    "auth": { "type": "bearer", "token": "..." },
    "target": "researcher",
    "timeout_ms": 300000,
    "options": {
      "poll_interval_ms": 1000
    }
  }
}
```

Agents with an `endpoint` field are resolved as remote backend agents. Today the built-in delegate backend is A2A. Agents without `endpoint` run locally.

## Related

- [Multi-Agent Patterns](../../explanation/multi-agent-patterns.md) — delegation and handoff design
- [A2A Specification](https://a2a-protocol.org/latest/specification/) — official protocol reference
