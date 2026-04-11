# Integrate AI SDK Frontend

Use this when you have a Vercel AI SDK (v6) React frontend and need to connect it to an awaken agent server.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `server` enabled on the `awaken` crate
- Node.js project with `@ai-sdk/react` installed

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["server"] }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
tracing-subscriber = "0.3"
```

## Steps

1. Build the backend server.

```rust,ignore
use std::sync::Arc;

use awaken::engine::GenaiExecutor;
use awaken::contract::storage::ThreadRunStore;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use awaken::AgentRuntimeBuilder;
use awaken::server::app::{AppState, ServerConfig};
use awaken::server::mailbox::{Mailbox, MailboxConfig};
use awaken::server::routes::build_router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let agent_spec = AgentSpec::new("my-agent")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant.")
        .with_max_rounds(10);

    let runtime = AgentRuntimeBuilder::new()
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model_binding(
            "gpt-4o-mini",
            ModelBinding {
                provider_id: "openai".into(),
                upstream_model: "gpt-4o-mini".into(),
            },
        )
        .with_agent_spec(agent_spec)
        .build()
        .expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let store = Arc::new(InMemoryStore::new());
    let resolver = runtime.resolver_arc();

    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store as Arc<dyn awaken::contract::MailboxStore>,
        format!("ai-sdk:{}", std::process::id()),
        MailboxConfig::default(),
    ));

    let state = AppState::new(
        runtime,
        mailbox,
        store as Arc<dyn ThreadRunStore>,
        resolver,
        ServerConfig {
            address: "127.0.0.1:3000".into(),
            ..Default::default()
        },
    );

    let app = build_router().with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server crashed");
}
```

The server automatically registers AI SDK v6 routes at:

- `POST /v1/ai-sdk/chat` -- create a new run and stream events
- `GET /v1/ai-sdk/chat/:thread_id/stream` -- resume an existing stream by thread ID
- `GET /v1/ai-sdk/threads/:thread_id/stream` -- alias for thread-based resume
- `GET /v1/ai-sdk/threads/:id/messages` -- retrieve thread messages

2. Connect the React frontend.

   Install the AI SDK React package:

```bash
npm install ai @ai-sdk/react
```

Use the `useChat` hook pointed at your awaken server:

```tsx
import { useChat } from "@ai-sdk/react";

export default function Chat() {
  const { messages, input, handleInputChange, handleSubmit } = useChat({
    api: "http://localhost:3000/v1/ai-sdk/chat",
    id: "thread-1",
  });

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>
          <strong>{m.role}:</strong> {m.content}
        </div>
      ))}
      <form onSubmit={handleSubmit}>
        <input value={input} onChange={handleInputChange} />
        <button type="submit">Send</button>
      </form>
    </div>
  );
}
```

3. Run both sides.

```bash
# Terminal 1: backend
cargo run

# Terminal 2: frontend
npm run dev
```

## Verify

1. Open the frontend in a browser.
2. Send a message.
3. Confirm that streaming text appears incrementally.
4. Check the backend logs for `RunStart` and `RunFinish` events.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| CORS error in browser | No CORS middleware | Add `tower-http` CORS layer to the axum router |
| `useChat` receives no events | Wrong endpoint URL | Confirm the `api` prop points to `/v1/ai-sdk/chat` |
| `stream closed unexpectedly` | SSE buffer overflow | Increase `sse_buffer_size` in `ServerConfig` |
| 404 on `/v1/ai-sdk/chat` | Missing `server` feature | Enable `features = ["server"]` in `Cargo.toml` |

## Related Example

- `examples/ai-sdk-starter/agent/src/main.rs`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-server/src/protocols/ai_sdk_v6/http.rs` | AI SDK v6 route handlers |
| `crates/awaken-server/src/protocols/ai_sdk_v6/encoder.rs` | AI SDK v6 SSE event encoder |
| `crates/awaken-server/src/routes.rs` | Unified router builder |
| `crates/awaken-server/src/app.rs` | `AppState` and `ServerConfig` |
| `examples/ai-sdk-starter/agent/src/main.rs` | Backend entry for the AI SDK starter |

## Related

- [Expose HTTP SSE](./expose-http-sse.md)
- [AI SDK v6 Protocol Reference](../reference/protocols/ai-sdk-v6.md)
- [Integrate CopilotKit (AG-UI)](./integrate-copilotkit-ag-ui.md)
