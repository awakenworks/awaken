# AI SDK v6 Protocol

## Endpoints

- `POST /v1/ai-sdk/agents/:agent_id/runs`
- `GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`
- `GET /v1/ai-sdk/threads/:id/messages`

## Request Model (`POST /runs`)

AI SDK v6 HTTP uses `id/messages`.

- `id` (required): thread id
- `messages` (required): UI messages array
- `runId` (optional): run id used for decision forwarding correlation
- `parentThreadId` (optional)
- `trigger` (optional): `submit-message` or `regenerate-message`
- `messageId` (required when `trigger=regenerate-message`)

Example (normal submit):

```json
{
  "id": "thread-1",
  "runId": "run-1",
  "messages": [
    { "id": "u1", "role": "user", "content": "Summarize latest messages" }
  ]
}
```

Example (regenerate):

```json
{
  "id": "thread-1",
  "trigger": "regenerate-message",
  "messageId": "m_assistant_1"
}
```

Legacy shape (`sessionId` + `input`) is rejected for HTTP v6 UI transport.

## Decision Forwarding

Decision-only submissions are extracted from assistant message parts (for example `tool-approval-response`).

If there is no user input and decisions are present, server first tries to forward decisions to an active run keyed by:

- protocol: `ai_sdk`
- `agent_id`
- thread id (`id`)
- `runId`

Success response:

```json
{
  "status": "decision_forwarded",
  "threadId": "thread-1"
}
```

If no active run is found, request falls back to normal run execution path.

## Response Transport

Headers:

- `content-type: text/event-stream`
- `x-vercel-ai-ui-message-stream: v1`
- `x-tirea-ai-sdk-version: v6`

Stream semantics:

- SSE frames contain AI SDK UI stream events (`start`, `text-*`, `finish`, `data-*`, ...)
- server appends `data: [DONE]` trailer

Tool progress is emitted as `data-activity-snapshot` (`activityType = tool-call-progress`).

## Resume Stream

`GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`

- `204 No Content` if no active fanout stream exists for `agent_id:chat_id`
- `200` SSE stream when active

## History Endpoint

`GET /v1/ai-sdk/threads/:id/messages`

Returns AI SDK-encoded message history for thread hydration/replay.

## Validation and Errors

- empty `id` -> `400` (`id cannot be empty`)
- no user input and no suspension decisions and not regenerate -> `400`
- regenerate without `messageId` -> `400`
- regenerate with empty `messageId` -> `400`
- unknown `agent_id` -> `404`

Error body:

```json
{ "error": "bad request: request must include user input or suspension decisions" }
```
