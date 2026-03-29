# HTTP API

The `awaken-server` crate (feature flag `server`) exposes an HTTP API via Axum.
All responses are JSON unless otherwise noted. Streaming endpoints use
Server-Sent Events (SSE).

## Health

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Returns `200 OK` |

## Threads

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/threads` | List thread IDs. Query: `?offset=0&limit=50` |
| `POST` | `/v1/threads` | Create a thread. Body: `{ "title": "..." }` |
| `GET` | `/v1/threads/summaries` | List thread summaries (id, title, updated_at) |
| `GET` | `/v1/threads/:id` | Get a thread by ID |
| `DELETE` | `/v1/threads/:id` | Delete a thread and its messages |
| `PATCH` | `/v1/threads/:id` | Update thread metadata. Body: `{ "title": "...", "custom": {} }` |
| `POST` | `/v1/threads/:id/interrupt` | Interrupt thread: bumps generation, supersedes queued jobs, cancels active run |
| `POST` | `/v1/threads/:id/cancel` | Cancel active run on thread |
| `POST` | `/v1/threads/:id/decision` | Submit HITL decision. Body: `{ "toolCallId": "...", "action": "resume"\|"cancel", "payload": {} }` |
| `PATCH` | `/v1/threads/:id/metadata` | Update thread metadata (alias) |
| `GET` | `/v1/threads/:id/messages` | List messages. Query: `?offset=0&limit=50&visibility=all` |
| `POST` | `/v1/threads/:id/messages` | Submit messages to run. Body: `{ "agent_id": "...", "messages": [...] }` |
| `GET` | `/v1/threads/:id/runs` | List runs for thread. Query: `?offset=0&limit=50&status=running` |
| `GET` | `/v1/threads/:id/runs/latest` | Get latest run for thread |

## Runs

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/runs` | List runs. Query: `?offset=0&limit=50&status=running` |
| `POST` | `/v1/runs` | Start a run (SSE). Body: `{ "agentId": "...", "threadId": "...", "messages": [...] }` |
| `GET` | `/v1/runs/:id` | Get run record |

## Mailbox

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/threads/:id/mailbox` | Push a message to the thread mailbox |
| `GET` | `/v1/threads/:id/mailbox` | Peek at mailbox jobs |

## Protocol routes

### AI SDK v6

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/ai-sdk/chat` | Start an AI SDK chat (SSE) |
| `GET` | `/v1/ai-sdk/streams/:run_id` | Resume an SSE stream |
| `GET` | `/v1/ai-sdk/runs/:run_id/stream` | Resume an SSE stream (alias) |
| `GET` | `/v1/ai-sdk/threads/:id/messages` | List thread messages |

### AG-UI

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/ag-ui/run` | Start an AG-UI run (SSE) |
| `GET` | `/v1/ag-ui/threads/:id/messages` | List thread messages |

### A2A

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/a2a/tasks/send` | Send a task |
| `GET` | `/v1/a2a/tasks/:task_id` | Get task status |
| `POST` | `/v1/a2a/tasks/:task_id/cancel` | Cancel a task |
| `GET` | `/v1/a2a/.well-known/agent` | Get default agent card |
| `GET` | `/v1/a2a/agents` | List agents |
| `GET` | `/v1/a2a/agents/:agent_id/agent-card` | Get agent card by ID |
| `POST` | `/v1/a2a/agents/:agent_id/message:send` | Send message to agent |
| `GET`/`POST` | `/v1/a2a/agents/:agent_id/tasks/:task_action` | Task actions per agent |

## Common query parameters

- `offset` -- Number of items to skip (default `0`)
- `limit` -- Maximum items to return (default `50`, max `200`)
- `status` -- Filter by run status: `running`, `waiting`, `done`
- `visibility` -- Message visibility filter: omit for external-only, `all` for everything

## Error format

All errors return a JSON body:

```json
{ "error": "human-readable message" }
```

Status codes: `400` (bad request), `404` (not found), `500` (internal error).

## Related

- [Expose HTTP with SSE](../how-to/expose-http-sse.md)
