# Expose HTTP SSE

Use this when clients consume run events over HTTP streaming.

## Prerequisites

- `AgentOs` is wired with tools and agents.
- `ThreadReader` is available for query routes.
- Run service is initialized when using Run API/A2A (`init_run_service(...)`).

## Endpoints

Run streams:

- `POST /v1/ag-ui/agents/:agent_id/runs`
- `POST /v1/ai-sdk/agents/:agent_id/runs`
- `POST /v1/runs`

Run stream resume:

- `GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`

Query APIs:

- `GET /v1/threads`
- `GET /v1/threads/:id`
- `GET /v1/threads/:id/messages`
- `GET /v1/runs`
- `GET /v1/runs/:id`

## Steps

1. Build router from route groups.

```rust,ignore
use std::sync::Arc;
use tirea_agentos_server::http::{self, AppState};
use tirea_agentos_server::protocol;

let app = axum::Router::new()
    .merge(http::health_routes())
    .merge(http::thread_routes())
    .merge(http::run_routes())
    .merge(protocol::a2a::http::well_known_routes())
    .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
    .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
    .nest("/v1/a2a", protocol::a2a::http::routes())
    .with_state(AppState {
        os: Arc::new(agent_os),
        read_store,
    });
```

2. Call AI SDK v6 stream.

```bash
curl -N \
  -H 'content-type: application/json' \
  -d '{"id":"thread-1","messages":[{"role":"user","content":"hello"}],"runId":"run-1"}' \
  http://127.0.0.1:8080/v1/ai-sdk/agents/assistant/runs
```

3. Call AG-UI stream.

```bash
curl -N \
  -H 'content-type: application/json' \
  -d '{"threadId":"thread-2","runId":"run-2","messages":[{"role":"user","content":"hello"}],"tools":[]}' \
  http://127.0.0.1:8080/v1/ag-ui/agents/assistant/runs
```

4. Query persisted data.

```bash
curl 'http://127.0.0.1:8080/v1/threads/thread-1/messages?limit=20'
curl 'http://127.0.0.1:8080/v1/runs?thread_id=thread-1&limit=20'
```

## Verify

- Stream routes return `200` with `content-type: text/event-stream`.
- AI SDK stream returns `x-vercel-ai-ui-message-stream: v1` and `data: [DONE]` trailer.
- AG-UI stream includes lifecycle events (for example `RUN_STARTED` and `RUN_FINISHED`).
- `GET /v1/runs/:id` returns run projection metadata.

## Common Errors

- `400` for payload validation (`id`, `threadId`, `runId`, or message/decision rules).
- `404` for unknown `agent_id`.
- Run/A2A routes fail with internal error when run service is not initialized.

## Related

- [HTTP API](../reference/http-api.md)
- [Run API](../reference/run-api.md)
- [AG-UI Protocol](../reference/protocols/ag-ui.md)
- [AI SDK v6 Protocol](../reference/protocols/ai-sdk-v6.md)
- [A2A Protocol](../reference/protocols/a2a.md)
