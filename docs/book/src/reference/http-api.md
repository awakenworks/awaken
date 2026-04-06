# HTTP API

The `awaken-server` crate (feature flag `server`) exposes an HTTP API via Axum.
Most responses are JSON. Streaming endpoints use Server-Sent Events (SSE).

This page mirrors the current route tree in `crates/awaken-server/src/routes.rs`
and `crates/awaken-server/src/config_routes.rs`.

## Health and metrics

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Readiness probe. Checks store connectivity and returns `200` or `503` |
| `GET` | `/health/live` | Liveness probe. Always returns `200 OK` |
| `GET` | `/metrics` | Prometheus scrape endpoint |

## Threads

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/threads` | List thread IDs |
| `POST` | `/v1/threads` | Create a thread. Body: `{ "title": "..." }` |
| `GET` | `/v1/threads/summaries` | List thread summaries |
| `GET` | `/v1/threads/:id` | Get a thread by ID |
| `PATCH` | `/v1/threads/:id` | Update thread metadata |
| `DELETE` | `/v1/threads/:id` | Delete a thread |
| `POST` | `/v1/threads/:id/cancel` | Cancel a specific queued or running job addressed by this thread ID. Returns `cancel_requested`. |
| `POST` | `/v1/threads/:id/decision` | Submit a HITL decision for a waiting run on this thread |
| `POST` | `/v1/threads/:id/interrupt` | Interrupt the thread: bumps the thread generation, supersedes all pending queued jobs, and cancels the active run. Returns `interrupt_requested` with `superseded_jobs` count. Unlike `/cancel`, this performs a clean-slate interrupt via `mailbox.interrupt()`. |
| `PATCH` | `/v1/threads/:id/metadata` | Alias for thread metadata updates |
| `GET` | `/v1/threads/:id/messages` | List thread messages |
| `POST` | `/v1/threads/:id/messages` | Submit messages as a background run on this thread |
| `POST` | `/v1/threads/:id/mailbox` | Push a message payload to the thread mailbox |
| `GET` | `/v1/threads/:id/mailbox` | List mailbox jobs for the thread |
| `GET` | `/v1/threads/:id/runs` | List runs for the thread |
| `GET` | `/v1/threads/:id/runs/latest` | Get the latest run for the thread |

## Runs

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/runs` | List runs |
| `POST` | `/v1/runs` | Start a run and stream events over SSE |
| `GET` | `/v1/runs/:id` | Get a run record |
| `POST` | `/v1/runs/:id/inputs` | Submit follow-up input messages as a background run on the same thread |
| `POST` | `/v1/runs/:id/cancel` | Cancel a run by run ID |
| `POST` | `/v1/runs/:id/decision` | Submit a HITL decision by run ID |

Decision endpoints resolve the submitted target identifier against either the
underlying tool-call ID or the active external `suspension_id`.

## Config and capabilities

These endpoints are exposed by `config_routes()`. They are available when
`AppState` is constructed with a config store. Without a config store, the
mutation and read routes return `400` with `config management API not enabled`.

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/capabilities` | List registered agents, tools, plugins, models, providers, and config namespaces |
| `GET` | `/v1/config/:namespace` | List entries in a config namespace |
| `POST` | `/v1/config/:namespace` | Create an entry; the body must contain `"id"` |
| `GET` | `/v1/config/:namespace/:id` | Get one config entry |
| `PUT` | `/v1/config/:namespace/:id` | Replace a config entry |
| `DELETE` | `/v1/config/:namespace/:id` | Delete a config entry |
| `GET` | `/v1/config/:namespace/$schema` | Return the JSON Schema for a namespace |
| `GET` | `/v1/agents` | Convenience alias for `/v1/config/agents` |
| `GET` | `/v1/agents/:id` | Convenience alias for `/v1/config/agents/:id` |

Current built-in namespaces:

- `agents`
- `models`
- `providers`
- `mcp-servers`

## AI SDK v6 routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/ai-sdk/chat` | Start a chat run and stream protocol-encoded events |
| `POST` | `/v1/ai-sdk/threads/:thread_id/runs` | Start a thread-scoped AI SDK run |
| `POST` | `/v1/ai-sdk/agents/:agent_id/runs` | Start an agent-scoped AI SDK run |
| `GET` | `/v1/ai-sdk/chat/:thread_id/stream` | Resume an SSE stream by thread ID |
| `GET` | `/v1/ai-sdk/threads/:thread_id/stream` | Alias for stream resume by thread ID |
| `GET` | `/v1/ai-sdk/threads/:thread_id/messages` | List thread messages |
| `POST` | `/v1/ai-sdk/threads/:thread_id/cancel` | Cancel the active or queued run on a thread |
| `POST` | `/v1/ai-sdk/threads/:thread_id/interrupt` | Interrupt a thread (bump generation, supersede pending jobs, cancel active run) |

## AG-UI routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/ag-ui/run` | Start an AG-UI run and stream AG-UI events |
| `POST` | `/v1/ag-ui/threads/:thread_id/runs` | Start a thread-scoped AG-UI run |
| `POST` | `/v1/ag-ui/agents/:agent_id/runs` | Start an agent-scoped AG-UI run |
| `POST` | `/v1/ag-ui/threads/:thread_id/interrupt` | Interrupt a thread |
| `GET` | `/v1/ag-ui/threads/:id/messages` | List thread messages |

## A2A routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/a2a/tasks/send` | Send a task |
| `GET` | `/v1/a2a/tasks/:task_id` | Get task status |
| `POST` | `/v1/a2a/tasks/:task_id/cancel` | Cancel a task |
| `GET` | `/v1/a2a/.well-known/agent` | Get the default agent card |
| `GET` | `/v1/a2a/agents` | List available agents |
| `GET` | `/v1/a2a/agents/:agent_id/agent-card` | Get a specific agent card |
| `POST` | `/v1/a2a/agents/:agent_id/message:send` | Send a message to a specific agent |
| `GET`/`POST` | `/v1/a2a/agents/:agent_id/tasks/:task_action` | Task actions scoped to an agent |

## MCP HTTP routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/mcp` | MCP JSON-RPC request/response endpoint |
| `GET` | `/v1/mcp` | Reserved for MCP server-initiated SSE; currently returns `405` |

## Common query parameters

- `offset` — number of items to skip
- `limit` — maximum items to return, clamped to `1..=200`
- `status` — run filter: `running`, `waiting`, or `done`
- `visibility` — message filter: omit for external-only, set to `all` to include internal messages

## Error format

Most route groups return:

```json
{ "error": "human-readable message" }
```

MCP routes return JSON-RPC error objects instead of the generic shape above.

## Related

- [Expose HTTP with SSE](../how-to/expose-http-sse.md)
- [Config](./config.md)
