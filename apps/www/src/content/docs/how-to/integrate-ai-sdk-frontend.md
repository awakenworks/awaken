---
title: "Integrate AI SDK Frontend"
description: "Use this when you have a Vercel AI SDK (v6) React frontend and need to connect it to an awaken agent server."
---

Use this when you have a Vercel AI SDK (v6) React frontend and need to connect it to an awaken agent server.

## Prerequisites

- A working awaken agent runtime (see [First Agent](/awaken/tutorials/first-agent/))
- Feature `server` enabled on the `awaken` crate
- Node.js project with `@ai-sdk/react` installed

```toml
[dependencies]
awaken = { git = "https://github.com/AwakenWorks/awaken", features = ["server"] }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
tracing-subscriber = "0.3"
```

## Steps

1. Build the backend server.

```rust
use std::sync::Arc;

use awaken::engine::GenaiExecutor;
use awaken::contract::storage::ThreadRunStore;
use awaken::registry_spec::ModelSpec;
use awaken::registry_spec::AgentSpec;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use awaken::AgentRuntimeBuilder;
use awaken::server::app::{ServerState, ServerConfig};
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
        .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
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
        store.clone() as Arc<dyn ThreadRunStore>,
        format!("ai-sdk:{}", std::process::id()),
        MailboxConfig::default(),
    ));

    let state = ServerState::new(
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
- `POST /v1/ai-sdk/agents/:agent_id/runs` -- create a run pinned to one saved agent
- `GET /v1/ai-sdk/chat/:thread_id/stream` -- resume an existing stream by thread ID
- `GET /v1/ai-sdk/threads/:thread_id/stream` -- alias for thread-based resume
- `GET /v1/ai-sdk/threads/:thread_id/replay` -- replay durable protocol frames when a `ProtocolReplayLog` is wired
- `GET /v1/ai-sdk/threads/:id/messages` -- retrieve thread messages

Live stream resume uses numeric `Last-Event-ID` positions from the in-memory
SSE buffer. Durable replay uses opaque protocol replay cursors from the replay
endpoint; keep them separate in frontend resume code.

2. Connect the React frontend.

   Install the AI SDK React package:

```bash
npm install ai @ai-sdk/react
```

Use the `useChat` hook pointed at your awaken server. AI SDK v6 returns
`{ messages, sendMessage, status, ... }` and reads requests from a transport,
so the awaken endpoint goes inside `DefaultChatTransport`:
No custom frontend protocol adapter is required for normal chat. Awaken emits
standard AI SDK stream parts; `data-*` parts carry optional platform metadata
such as run status and traces, and can be ignored unless your UI wants to show
those details.

```tsx
import { useState } from "react";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

export default function Chat() {
  const { messages, sendMessage } = useChat({
    id: "thread-1",
    transport: new DefaultChatTransport({
      api: "http://localhost:3000/v1/ai-sdk/chat",
    }),
  });
  const [input, setInput] = useState("");

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>
          <strong>{m.role}:</strong>
          {m.parts.map((part, idx) =>
            part.type === "text" ? <span key={idx}>{part.text}</span> : null,
          )}
        </div>
      ))}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (!input.trim()) return;
          sendMessage({ text: input });
          setInput("");
        }}
      >
        <input value={input} onChange={(event) => setInput(event.target.value)} />
        <button type="submit">Send</button>
      </form>
    </div>
  );
}
```

When the Admin Console sandbox passes for a saved agent, copy the agent-scoped
route from the **Frontend integration** card:

```tsx
transport: new DefaultChatTransport({
  api: "http://localhost:3000/v1/ai-sdk/agents/support-agent/runs",
})
```

Multimodal turns use the standard AI SDK `file` part shape. Configure the model
with matching input modalities, then call `sendMessage({ text, files })` with a
`FileList` or `FileUIPart[]`; Awaken converts image/audio/video/PDF/text parts
into runtime `ContentBlock`s before inference.

For the full pattern with custom transport headers, automatic resubmission, and
typed tool parts, see the working example in
[`examples/ai-sdk-starter/src/hooks/use-chat-session.ts`](../../../../examples/ai-sdk-starter/src/hooks/use-chat-session.ts).

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
| `crates/awaken-server/src/app.rs` | `ServerState` and `ServerConfig` |
| `examples/ai-sdk-starter/agent/src/main.rs` | Backend entry for the AI SDK starter |

## Related

- [Expose HTTP SSE](/awaken/how-to/expose-http-sse/)
- [AI SDK v6 Protocol Reference](/awaken/reference/protocols/ai-sdk-v6/)
- [Integrate CopilotKit (AG-UI)](/awaken/how-to/integrate-copilotkit-ag-ui/)
