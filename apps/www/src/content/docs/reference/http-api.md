---
title: "HTTP API"
description: "The awaken-server crate (feature flag server) exposes an HTTP API via Axum. Most responses are JSON. Streaming endpoints use Server-Sent Events (SSE)."
---

The `awaken-server` crate (feature flag `server`) exposes an HTTP API via Axum.
Most responses are JSON. Streaming endpoints use Server-Sent Events (SSE).

This page mirrors the current route tree in `crates/awaken-server/src/routes.rs`
and `crates/awaken-server/src/config_routes.rs`.

## Health and metrics

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Readiness probe. Checks store connectivity and returns `200` or `503` |
| `GET` | `/health/live` | Liveness probe. Always returns `200 OK` |
| `GET` | `/v1/system/info` | Server identity for the admin console: `{version, uptime_seconds, config_store_enabled, audit_log_enabled, runtime_stats_enabled}` |
| `GET` | `/metrics` | Prometheus scrape endpoint |

`GET /v1/system/info` is the admin-console "System" card source. It does not
reveal concrete store backends — embedders that want to expose those should
add a separate route on top of their own `AppState`.

## Threads

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/threads` | List thread IDs with paging and lineage filters; returns `{ items, offset, limit, total, has_more, next_cursor }` |
| `POST` | `/v1/threads` | Create a thread. Body: `{ "title"?: string, "resource_id"?: string, "parent_thread_id"?: string }` |
| `GET` | `/v1/threads/summaries` | List thread summaries (id, `resource_id`, `parent_thread_id`, title, `updated_at`, `agent_id`) with the same paging and lineage filters as `/v1/threads` |
| `GET` | `/v1/threads/:id` | Get a thread by ID |
| `PATCH` | `/v1/threads/:id` | Update thread metadata |
| `DELETE` | `/v1/threads/:id` | Delete a thread; accepts `?child_strategy=detach\|reject\|cascade` (default `detach`) to control how direct and transitive child threads are handled |
| `POST` | `/v1/threads/:id/cancel` | Cancel a specific queued or running dispatch addressed by this thread ID. Returns `cancel_requested`. |
| `POST` | `/v1/threads/:id/decision` | Submit a HITL decision for a waiting run on this thread |
| `POST` | `/v1/threads/:id/interrupt` | Interrupt the thread: bumps the thread dispatch epoch, supersedes all pending queued dispatches, and cancels the active run. Returns `interrupt_requested` with `superseded_dispatches` count. Unlike `/cancel`, this performs a clean-slate interrupt via `mailbox.interrupt()`. |
| `PATCH` | `/v1/threads/:id/metadata` | Alias for thread metadata updates |
| `GET` | `/v1/threads/:id/messages` | List thread messages with cursor pagination, sequence range, ordering, and visibility/run filters |
| `POST` | `/v1/threads/:id/messages` | Submit messages as a background run on this thread |
| `POST` | `/v1/threads/:id/mailbox` | Push a message payload to the thread mailbox |
| `GET` | `/v1/threads/:id/mailbox` | List mailbox dispatches for the thread |
| `GET` | `/v1/threads/:id/runs` | List runs for the thread |
| `GET` | `/v1/threads/:id/runs/active` | Get the active run for the thread, if any |
| `GET` | `/v1/threads/:id/runs/latest` | Get the latest run for the thread |

`POST /v1/threads/:id/messages` and `POST /v1/runs/:id/inputs` accept an
optional `mode` field. `queue` appends a durable mailbox dispatch,
`live_then_queue` first tries to deliver the messages to the active run and
queues only when live delivery is unavailable, `steer` is an alias for
`live_then_queue`, `interrupt_then_queue` cancels the active run before
queueing, and `resume_open_run` continues a resumable waiting run.

## Runs

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/runs` | List runs |
| `POST` | `/v1/runs` | Start a run and stream events over SSE |
| `GET` | `/v1/runs/:id` | Get a run record |
| `POST` | `/v1/runs/:id/inputs` | Submit follow-up input messages as a background run on the same thread |
| `POST` | `/v1/runs/:id/cancel` | Cancel a run by run ID |
| `POST` | `/v1/runs/:id/decision` | Submit a HITL decision by run ID |

## Agent runtime stats

These return rolling-window snapshots from the
`RuntimeStatsRegistry` published by the observability plugin. Both routes
return `503 {"error":"runtime_stats registry not configured"}` when the
embedder has not wired one — the admin console treats this as a feature
flag and shows a friendly notice.

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/agents/:id/runtime-stats?window=` | Per-agent snapshot. `window` is optional (`1h`, `24h`, `7d`, `<n>s`); unset returns the registry's full retained window |
| `GET` | `/v1/agents/runtime-stats` | One snapshot per known agent: `{ "agents": AgentRuntimeSnapshot[] }` |

`AgentRuntimeSnapshot` shape (Rust source: `awaken_ext_observability::AgentRuntimeSnapshot`):

```jsonc
{
  "agent_id": "research",
  "window_seconds": 86400,
  "bucket_window_seconds": 3600,
  "bucket_count": 24,
  "inference_count": 12,
  "error_count": 0,
  "input_tokens": 4180,
  "output_tokens": 980,
  "avg_inference_duration_ms": 480.5,
  "min_inference_duration_ms": 110,
  "max_inference_duration_ms": 1820,
  "p50_inference_duration_ms": 410,
  "p95_inference_duration_ms": 1410,
  "p99_inference_duration_ms": 1810,
  "inference_duration_histogram": [
    { "upper_bound_ms": 100, "count": 0 },
    { "upper_bound_ms": 250, "count": 1 }
    /* ... */
  ],
  "suspensions": 0,
  "handoffs": 0,
  "delegations": 0,
  "tool_calls_by_tool": [
    {
      "tool": "search",
      "call_count": 7,
      "failure_count": 0,
      "total_duration_ms": 2840,
      "avg_duration_ms": 405.7,
      "min_duration_ms": 110,
      "max_duration_ms": 920,
      "p50_duration_ms": 380,
      "p95_duration_ms": 880,
      "p99_duration_ms": 920
    }
  ]
}
```

`inference_duration_histogram` is a *value distribution* (latency in ms),
not a time series. Use the `window` query parameter for coarse time
filtering.

## Config and capabilities

These endpoints are exposed by `config_routes()`. Read and schema routes require
`AppState` to be constructed with a config store. Mutation routes additionally
require a config runtime manager so writes can validate and publish a new
registry snapshot. Without the required config wiring, the routes return `400`
with `config management API not enabled`.

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/capabilities` | List registered agents, tools, plugins, models, providers, and config namespaces |
| `GET` | `/v1/config/:namespace` | List entries in a config namespace |
| `POST` | `/v1/config/:namespace` | Create an entry; the body must contain `"id"` |
| `POST` | `/v1/config/:namespace/validate?id=` | Dry-run validate. Runs the same `prepare_body` + schema check as `create`/`update` but does **not** persist or apply. Returns `{"ok":true,"normalized":{...}}` on success, the same `400`/`409` errors as a real save on failure. The optional `?id=` query lets callers validate an update without going through `:id` in the path. |
| `GET` | `/v1/config/:namespace/:id` | Get one config entry |
| `PUT` | `/v1/config/:namespace/:id` | Replace a config entry |
| `DELETE` | `/v1/config/:namespace/:id` | Delete a config entry. `?force=true` bypasses the dependency check (and audits the override). Returns `409` with `{"error":"...","used_by":[...]}` when other records depend on this one |
| `POST` | `/v1/config/:namespace/:id/restore` | Restore a previous version. Body: `{"version": "<event-id>"}` — the audit-event id of the version to roll back to. Emits a fresh audit event of type `restore` with `restored_from = <event-id>` |
| `GET` | `/v1/config/:namespace/$schema` | Return the JSON Schema for a namespace |
| `GET` | `/v1/config/:namespace/meta` | List metadata (created_at / updated_at / version / actor) for every entry without returning the full bodies |
| `GET` | `/v1/config/:namespace/:id/meta` | Single-entry metadata variant of the above |
| `GET` | `/v1/config/diagnostics` | Registry-wide validation report — surfaces dangling model/provider refs and other cross-entity inconsistencies that per-entity validate would miss |
| `PATCH` | `/v1/config/agents/:id/overrides` | Patch a single field of an agent spec (system prompt, max rounds, etc.) without sending the whole body. Audited as `update` with `overrides` payload |
| `DELETE` | `/v1/config/agents/:id/overrides` | Drop all agent overrides; reverts to the base spec |
| `DELETE` | `/v1/config/agents/:id/overrides/:field` | Drop one overridden field |
| `PATCH` | `/v1/config/tools/:id/overrides` | Patch a built-in tool's `description`. Tools themselves are not deletable; only the description override is mutable |
| `DELETE` | `/v1/config/tools/:id/overrides` | Drop the tool description override |
| `DELETE` | `/v1/config/tools/:id/overrides/:field` | Drop one overridden field of a tool |
| `GET` | `/v1/agents/:id/permission-preview` | Resolve an agent's effective tool permissions (built-in + plugin + MCP, after include/exclude). Used by the editor's Tools tab to show "what the LLM will actually see" |
| `GET` | `/v1/agents` | Convenience alias for `/v1/config/agents` |
| `GET` | `/v1/agents/:id` | Convenience alias for `/v1/config/agents/:id` |
| `POST` | `/v1/providers/:id/test` | Probe an existing provider. Returns `{"ok": bool, "latency_ms": number, "error"?: string}`. The admin console wires this both into the editor and as a per-row "Test" button on the providers list |
| `GET` | `/v1/mcp-servers/:id/status` | See [MCP server status](#mcp-server-status) below |
| `POST` | `/v1/mcp-servers/:id/restart` | Reconnect a managed MCP server. `202` on success; emits an audit `restart` event |
| `GET` | `/v1/audit-log?…` | Query admin audit events. Returns `{"items": AuditEvent[], "next_cursor": string?}`. `503 {"error":"audit log is not configured"}` when audit logging is off. See [Admin audit log](#admin-audit-log) |

`GET /v1/capabilities` includes each registered plugin's `config_schemas`.
The admin console uses this field to render agent-level plugin config forms and
save values into `AgentSpec.sections`. After a successful config mutation, the
runtime manager publishes a new registry snapshot, so later `/v1/runs` requests
use the updated agents, models, providers, MCP servers, and plugin sections.

Current built-in namespaces:

- `agents`
- `models`
- `model-pools`
- `providers`
- `mcp-servers`
- `skills`

### MCP server status

```jsonc
{
  "connected": true,
  "last_error": null,                  // string when last health attempt failed
  "tools": [
    { "name": "search", "description": "Search the web." }
  ],
  "consecutive_failures": 0,           // streak since last success
  "last_attempt_at": 1777708820,       // unix seconds, null until first probe
  "last_success_at": 1777708820,       // unix seconds, null until first success
  "reconnecting": false,
  "permanently_failed": false,         // true once the manager has given up
  "session_generation": 2,             // HTTP session reset/reinitialize generation
  "transport_reconnect_count": 0,      // successful runtime re-creations
  "last_init_at": 1777708820           // unix seconds, null before initialize
}
```

`consecutive_failures` + `last_success_at` are surfaced from the existing
`McpRefreshHealth` budget. There is no separate "errors in last 24h"
counter — the health budget is the source of truth.

The raw HTTP `MCP-Session-Id` is intentionally not exposed by this endpoint.
`transport_reconnect_count` counts runtime tear-down/recreate cycles; HTTP
404 session reset churn is visible through `session_generation` and
`last_init_at`.

### Admin audit log

`AuditEvent`:

```jsonc
{
  "id": "01HXJK...",                   // ULID
  "ts": "2026-05-02T07:58:14.900Z",    // RFC 3339
  "actor": "<sha256-prefix>",          // SHA-256 of bearer token, optionally
                                       // suffixed with the X-Awaken-Actor label
  "action": "create" | "update" | "delete" | "restart" | "publish" | "restore",
  "resource": "agents/research",       // "<namespace>/<id>"
  "before": { /* spec snapshot */ },
  "after":  { /* spec snapshot */ },
  "ip": "127.0.0.1",
  "request_id": null,
  "restored_from": null                // event id this restore is rolling back to
}
```

Filters: `?resource=`, `?action=`, `?actor=`, `?since=`, `?until=`,
`?limit=` (clamped to `[1, 1000]`), `?cursor=` for pagination.

## AI SDK v6 routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/ai-sdk/chat` | Start a chat run and stream protocol-encoded events |
| `POST` | `/v1/ai-sdk/agent-previews/runs` | Run a draft `AgentSpec` without saving it; used by the admin console preview |
| `POST` | `/v1/ai-sdk/threads/:thread_id/runs` | Start a thread-scoped AI SDK run |
| `POST` | `/v1/ai-sdk/agents/:agent_id/runs` | Start an agent-scoped AI SDK run |
| `GET` | `/v1/ai-sdk/chat/:thread_id/stream` | Resume an SSE stream by thread ID |
| `GET` | `/v1/ai-sdk/threads/:thread_id/stream` | Alias for stream resume by thread ID |
| `GET` | `/v1/ai-sdk/threads/:thread_id/messages` | List thread messages |
| `POST` | `/v1/ai-sdk/threads/:thread_id/cancel` | Cancel the active or queued run on a thread |
| `POST` | `/v1/ai-sdk/threads/:thread_id/interrupt` | Interrupt a thread (bump dispatch epoch, supersede pending dispatches, cancel active run) |

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
| `GET` | `/.well-known/agent-card.json` | Get the public/default agent card |
| `POST` | `/v1/a2a/message:send` | Send a message to the public/default A2A agent |
| `POST` | `/v1/a2a/message:stream` | Streaming send over SSE |
| `GET` | `/v1/a2a/tasks` | List A2A tasks |
| `GET` | `/v1/a2a/tasks/:task_id` | Get task status |
| `POST` | `/v1/a2a/tasks/:task_id:cancel` | Cancel a task |
| `POST` | `/v1/a2a/tasks/:task_id:subscribe` | Subscribe to task updates over SSE |
| `POST` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | Create a push notification config |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | List push notification configs |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | Get a push notification config |
| `DELETE` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | Delete a push notification config |
| `GET` | `/v1/a2a/extendedAgentCard` | Get the extended agent card; returns `501` unless enabled |
| `POST` | `/v1/a2a/:tenant/message:send` | Send a message to a tenant-scoped agent |
| `POST` | `/v1/a2a/:tenant/message:stream` | Tenant-scoped streaming send |
| `GET` | `/v1/a2a/:tenant/tasks` | List tasks for a tenant-scoped agent |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id` | Get tenant-scoped task status |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:cancel` | Cancel a tenant-scoped task |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:subscribe` | Subscribe to tenant-scoped task updates |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | Create a tenant-scoped push notification config |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | List tenant-scoped push notification configs |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | Get a tenant-scoped push notification config |
| `DELETE` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | Delete a tenant-scoped push notification config |
| `GET` | `/v1/a2a/:tenant/extendedAgentCard` | Get the tenant-scoped extended agent card |

## MCP HTTP routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/mcp` | MCP JSON-RPC request/response endpoint. `initialize` creates a session and returns `MCP-Session-Id`; later requests, notifications, and responses require that header. |
| `GET` | `/v1/mcp` | Reserved for MCP server-initiated SSE; currently returns `405` |
| `DELETE` | `/v1/mcp` | Terminate a known MCP HTTP session identified by `MCP-Session-Id`; returns `204` or `404` |

`initialize` requests must not include `MCP-Session-Id`. `tools/call` may stream
responses. All MCP HTTP routes validate `Origin` when present.

## Common query parameters

Pagination:

- `offset` — number of items to skip
- `limit` — maximum items to return, clamped to `1..=200` (default `50`)
- `cursor` — opaque pagination cursor; when provided it takes precedence over
  `offset`. Cursors are bound to the original query shape and rejected if
  any filter changes between requests
- `next_cursor` / `prev_cursor` — returned in the response body when more
  pages exist

Thread list filters (`/v1/threads`, `/v1/threads/summaries`):

- `resource_id` (alias `resourceId`) — filter by external resource grouping
- `parent_thread_id` (alias `parentThreadId`) — restrict to direct children of
  this parent thread
- `root` — when `true`, restrict to root threads with no parent. Cannot be
  combined with `parent_thread_id`

Message list filters (`/v1/threads/:id/messages` and the protocol-specific
aliases):

- `after`, `before` — sequence-number window
- `order` — `asc` (default) or `desc`
- `visibility` — `external` (default), `internal`, or `all`
- `run_id` (alias `runId`) — restrict to messages produced by this run

Run list filters:

- `status` — `running`, `waiting`, or `done`

## Error format

Most route groups return:

```json
{ "error": "human-readable message" }
```

MCP routes return JSON-RPC error objects instead of the generic shape above.

## Related

- [Expose HTTP with SSE](/awaken/how-to/expose-http-sse/)
- [Config](/awaken/reference/config/)
