# Integrate CopilotKit (AG-UI)

Use this when you have a CopilotKit React frontend and need to connect it to an awaken agent server via the AG-UI protocol.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `server` enabled on the `awaken` crate
- Node.js project with `@copilotkit/react-core` and `@copilotkit/react-ui` installed

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

    let agent_spec = AgentSpec::new("copilotkit-agent")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt(
            "You are a CopilotKit-powered assistant. \
             Update shared state and suggest actions when appropriate.",
        )
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
        format!("copilotkit:{}", std::process::id()),
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

The server automatically registers AG-UI routes at:

- `POST /v1/ag-ui/run` -- create a new run and stream AG-UI events
- `POST /v1/ag-ui/threads/:thread_id/runs` -- start a thread-scoped run
- `POST /v1/ag-ui/agents/:agent_id/runs` -- start an agent-scoped run
- `POST /v1/ag-ui/threads/:thread_id/interrupt` -- interrupt a running thread
- `GET /v1/ag-ui/threads/:id/messages` -- retrieve thread messages

2. Connect the CopilotKit frontend.

   Install CopilotKit packages:

```bash
npm install @copilotkit/react-core @copilotkit/react-ui
```

Wrap your app with the CopilotKit provider:

```tsx
import { CopilotKit } from "@copilotkit/react-core";
import { CopilotChat } from "@copilotkit/react-ui";
import "@copilotkit/react-ui/styles.css";

export default function App() {
  return (
    <CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui">
      <CopilotChat
        labels={{ title: "Agent", initial: "How can I help?" }}
      />
    </CopilotKit>
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
2. Send a message through the CopilotChat widget.
3. Confirm streaming text appears in the chat UI.
4. Check the backend logs for `RunStart` and `RunFinish` events.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| CORS error in browser | No CORS middleware | Add `tower-http` CORS layer to the axum router |
| CopilotKit shows "connection failed" | Wrong `runtimeUrl` | Confirm it points to `http://localhost:3000/v1/ag-ui` |
| Events arrive but UI does not update | AG-UI event format mismatch | Ensure CopilotKit version is compatible with AG-UI protocol |
| 404 on `/v1/ag-ui/run` | Missing `server` feature | Enable `features = ["server"]` in `Cargo.toml` |

## Related Example

- `examples/copilotkit-starter/agent/src/main.rs`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-server/src/protocols/ag_ui/http.rs` | AG-UI route handlers |
| `crates/awaken-server/src/protocols/ag_ui/encoder.rs` | AG-UI SSE event encoder |
| `crates/awaken-server/src/routes.rs` | Unified router builder |
| `crates/awaken-server/src/app.rs` | `AppState` and `ServerConfig` |
| `examples/copilotkit-starter/agent/src/main.rs` | Backend entry for the CopilotKit starter |

## Related

- [Expose HTTP SSE](./expose-http-sse.md)
- [AG-UI Protocol Reference](../reference/protocols/ag-ui.md)
- [Integrate AI SDK Frontend](./integrate-ai-sdk-frontend.md)
