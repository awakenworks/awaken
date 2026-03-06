# A2A Protocol

A2A routes provide gateway discovery plus task-style run submission and control.

## Endpoints

Discovery:

- `GET /.well-known/agent-card.json`
- `GET /v1/a2a/agents`
- `GET /v1/a2a/agents/:agent_id/agent-card`

Task APIs:

- `POST /v1/a2a/agents/:agent_id/message:send`
- `GET /v1/a2a/agents/:agent_id/tasks/:task_id`
- `POST /v1/a2a/agents/:agent_id/tasks/:task_id:cancel`

## Discovery Semantics

`/.well-known/agent-card.json`:

- Returns a gateway card with `taskManagement`, `streaming`, and `agentDiscovery` capability flags.
- Adds HTTP caching headers:
  - `cache-control: public, max-age=30, must-revalidate`
  - `etag: W/"a2a-agents-..."`
- Supports `if-none-match` (`*` and CSV values).

Single-agent deployment:

- Well-known card `url` points directly to `/v1/a2a/agents/<id>/message:send`.

Multi-agent deployment:

- Well-known card `url` points to `/v1/a2a/agents`.

## `message:send` Request

Accepted payload fields:

- `contextId` / `context_id` (optional)
- `taskId` / `task_id` (optional)
- `input` (optional string)
- `message` (optional object: `{ role?, content }`)
- `decisions` (optional `ToolCallDecision[]`)

Example (new task):

```json
{
  "input": "hello from a2a"
}
```

Example (continue existing task):

```json
{
  "taskId": "run-1",
  "input": "continue"
}
```

Example (decision-only forward):

```json
{
  "taskId": "run-1",
  "decisions": [
    {
      "target_id": "fc_perm_1",
      "decision_id": "d1",
      "action": "resume",
      "result": { "approved": true },
      "updated_at": 1760000000000
    }
  ]
}
```

Response for submission (`202`):

```json
{
  "contextId": "thread-1",
  "taskId": "run-1",
  "status": "submitted"
}
```

Response for decision forwarding (`202`):

```json
{
  "contextId": "thread-1",
  "taskId": "run-1",
  "status": "decision_forwarded"
}
```

## Task Query

`GET /v1/a2a/agents/:agent_id/tasks/:task_id` response:

```json
{
  "taskId": "run-1",
  "contextId": "thread-1",
  "status": "completed",
  "origin": "a2a",
  "terminationCode": null,
  "terminationDetail": null,
  "createdAt": 1760000000000,
  "updatedAt": 1760000005000
}
```

## Task Cancel

`POST /v1/a2a/agents/:agent_id/tasks/:task_id:cancel`

Response (`202`):

```json
{
  "taskId": "run-1",
  "status": "cancel_requested"
}
```

## Validation and Errors

- Action must be exactly `message:send`.
- Decision-only requests require `taskId`.
- Empty payloads are rejected unless at least one of `input/message/decisions/contextId/taskId` exists.
- Cancel path must end with `:cancel` and must be `POST`.
- `404` for unknown agent or missing task/run.
- `400` when task exists but is not active (`task is not active`).

Error body shape:

```json
{ "error": "bad request: unsupported A2A action; expected 'message:send'" }
```
