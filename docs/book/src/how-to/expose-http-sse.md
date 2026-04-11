# Expose HTTP with SSE

Use this when you need to serve agents over HTTP with Server-Sent Events streaming, supporting multiple protocol adapters (AI SDK, AG-UI, A2A).

## Prerequisites

- `awaken` crate with the `server` feature enabled
- `tokio` with `rt-multi-thread` and `signal` features
- A built `AgentRuntime`

## Steps

1. Add the dependency.

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "...", features = ["server"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

2. Build the runtime.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, AgentSpec};
use awaken::stores::InMemoryStore;

let store = Arc::new(InMemoryStore::new());

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(
        AgentSpec::new("assistant")
            .with_model_id("claude-sonnet")
            .with_system_prompt("You are a helpful assistant."),
    )
    .with_tool("search", Arc::new(SearchTool))
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .with_thread_run_store(store.clone())
    .build()?;

let runtime = Arc::new(runtime);
```

3. Create the application state.

```rust,ignore
use awaken::server::app::{AppState, ServerConfig};
use awaken::server::mailbox::{Mailbox, MailboxConfig};
use awaken::stores::InMemoryMailboxStore;

let mailbox_store = Arc::new(InMemoryMailboxStore::new());
let mailbox = Arc::new(Mailbox::new(
    runtime.clone(),
    mailbox_store,
    "default-consumer".to_string(),
    MailboxConfig::default(),
));

let state = AppState::new(
    runtime.clone(),
    mailbox,
    store,
    runtime.resolver_arc(),
    ServerConfig::default(),
);
```

`ServerConfig::default()` binds to `0.0.0.0:3000` with an SSE buffer size of 64.

4. Build the router.

```rust,ignore
use awaken::server::routes::build_router;

let app = build_router().with_state(state);
```

`build_router` registers all route groups:

- `/health` -- health check
- `/v1/threads` -- thread CRUD and messaging
- `/v1/runs` and `/v1/threads/:id/runs` -- run APIs
- `/v1/config/*` and `/v1/capabilities` -- config and capabilities APIs
- Protocol adapters: AI SDK v6, AG-UI, A2A, MCP

5. Start the server.

```rust,ignore
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
axum::serve(listener, app).await?;
```

6. Configure SSE buffer size.

```rust,ignore
let config = ServerConfig {
    address: "0.0.0.0:8080".into(),
    sse_buffer_size: 128,
    ..ServerConfig::default()
};
```

Increase `sse_buffer_size` if clients consume events slowly and you observe dropped messages.

## Verify

```bash
curl http://localhost:3000/health
```

Should return `200 OK`. Then create a thread and run:

```bash
curl -X POST http://localhost:3000/v1/threads \
  -H "Content-Type: application/json" \
  -d '{}'
```

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| Address already in use | Port 3000 is occupied | Change the bind address in `ServerConfig` or `TcpListener::bind` |
| SSE stream closes immediately | Client does not support `text/event-stream` | Use `curl` with `--no-buffer` or an SSE-compatible client |
| Missing protocol routes | Feature flags not enabled | Ensure `server` feature is enabled on the `awaken` crate |

## Related Example

`crates/awaken-server/tests/run_api.rs` -- integration tests demonstrating thread creation, run execution, and SSE streaming.

## Key Files

- `crates/awaken-server/src/app.rs` -- `AppState`, `ServerConfig`
- `crates/awaken-server/src/routes.rs` -- `build_router` and route definitions
- `crates/awaken-server/src/http_sse.rs` -- SSE response helpers
- `crates/awaken-server/src/mailbox.rs` -- `Mailbox` run queue

## Related

- [Build an Agent](./build-an-agent.md)
- [Use File Store](./use-file-store.md)
- [Use Postgres Store](./use-postgres-store.md)
