# AG-UI Protocol

## Endpoints

- `POST /v1/ag-ui/agents/:agent_id/runs`
- `GET /v1/ag-ui/threads/:id/messages`

## Request Model (`RunAgentInput`)

Required:

- `threadId`
- `runId`

Core optional fields:

- `messages`
- `tools`
- `context`
- `state`
- `parentRunId`
- `parentThreadId`
- `model`
- `systemPrompt`
- `config`
- `forwardedProps`

Minimal request:

```json
{
  "threadId": "thread-1",
  "runId": "run-1",
  "messages": [{ "role": "user", "content": "Plan my weekend" }],
  "tools": []
}
```

## Runtime Mapping

- `RunAgentInput` is converted to internal `RunRequest`.
- `model` / `systemPrompt` override resolved agent defaults for this run.
- `config` can override tool execution mode and selected chat options.
- `forwardedProps` and `config` are preserved in run scope (`agui_forwarded_props`, `agui_config`).

### Frontend tools

When `tools[].execute = "frontend"`:

- backend registers runtime stub descriptors
- execution is suspended and returned as pending call
- client decisions are consumed as tool result (`resume`) or denial (`cancel`)

### Decision-only forwarding

If request has no new user input but contains interaction decisions, server first attempts to forward them to active run key (`ag_ui + agent + thread + run`).

Success response (`202`):

```json
{
  "status": "decision_forwarded",
  "threadId": "thread-1",
  "runId": "run-1"
}
```

## Response Transport

- `content-type: text/event-stream`
- `data:` frames contain AG-UI protocol events
- lifecycle includes events such as `RUN_STARTED`, `RUN_FINISHED`

Tool progress example:

```json
{
  "type": "ACTIVITY_SNAPSHOT",
  "messageId": "tool_call:call_123",
  "activityType": "tool-call-progress",
  "content": {
    "type": "tool-call-progress",
    "schema": "tool-call-progress.v1",
    "node_id": "tool_call:call_123",
    "parent_node_id": "tool_call:call_parent_1",
    "parent_call_id": "call_parent_1",
    "status": "running",
    "progress": 0.4,
    "message": "searching..."
  },
  "replace": true
}
```

## Validation and Errors

- empty `threadId` -> `400`
- empty `runId` -> `400`
- unknown `agent_id` -> `404`

Error shape:

```json
{ "error": "bad request: threadId cannot be empty" }
```
