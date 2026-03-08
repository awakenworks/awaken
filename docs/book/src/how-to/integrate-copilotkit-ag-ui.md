# Integrate CopilotKit (AG-UI)

Use this when your frontend uses CopilotKit and your backend is `tirea-agentos-server` AG-UI SSE.

Choose this path when you need frontend tools, shared state, canvas-style UX, or richer suspend/resume flows than a chat-only transport usually provides.

## Best Fit

AG-UI/CopilotKit is usually the right choice when:

- you want shared agent state surfaced directly in the frontend
- some tools should execute in the browser instead of the backend
- you need human-in-the-loop approvals and resumable interactions
- the UI is more than a plain chat transcript

## Prerequisites

- `tirea-agentos-server` is reachable (default `http://localhost:8080`).
- AG-UI endpoint is enabled: `POST /v1/ag-ui/agents/:agent_id/runs`.
- Next.js frontend.

## Minimum Architecture

```text
Browser (CopilotKit components)
  -> Next.js runtime route (/api/copilotkit)
  -> AG-UI HttpAgent
  -> Tirea AG-UI endpoint (/v1/ag-ui/agents/:agent_id/runs)
  -> SSE AG-UI events back to browser
```

Thread hydration:

```text
Browser / runtime
  -> GET /v1/ag-ui/threads/:id/messages
  -> backend returns AG-UI-encoded history
```

## Steps

1. Add frontend dependencies:

```bash
npm install @copilotkit/react-core @copilotkit/react-ui @copilotkit/runtime @ag-ui/client
```

2. Create `lib/copilotkit-app.ts` to connect CopilotKit runtime to AG-UI.

```ts,ignore
import {
  CopilotRuntime,
  ExperimentalEmptyAdapter,
  copilotRuntimeNextJSAppRouterEndpoint,
} from "@copilotkit/runtime";
import { HttpAgent } from "@ag-ui/client";

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";

const runtime = new CopilotRuntime({
  agents: {
    default: new HttpAgent({
      url: `${BACKEND_URL}/v1/ag-ui/agents/default/runs`,
    }) as any,
  },
});

const { handleRequest } = copilotRuntimeNextJSAppRouterEndpoint({
  runtime,
  serviceAdapter: new ExperimentalEmptyAdapter(),
  endpoint: "/api/copilotkit",
});

export { handleRequest };
```

3. Create route handler `app/api/copilotkit/route.ts`.

```ts,ignore
import { handleRequest } from "@/lib/copilotkit-app";

export const POST = handleRequest;
```

4. Wrap app with `CopilotKit` provider in `app/layout.tsx`.

```tsx,ignore
"use client";

import { CopilotKit } from "@copilotkit/react-core";
import "@copilotkit/react-ui/styles.css";

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>
        <CopilotKit runtimeUrl="/api/copilotkit" agent="default">
          {children}
        </CopilotKit>
      </body>
    </html>
  );
}
```

5. Add chat UI in `app/page.tsx` using `CopilotChat` or `CopilotSidebar`.

## Backend / Frontend Boundary

In the AG-UI path, backend and frontend can both participate in tool orchestration.

- backend remains the source of truth for thread history and durable agent state
- backend tools execute normally inside Tirea
- frontend tools are declared with `execute = "frontend"` and are suspended by the backend
- CopilotKit resolves those frontend interactions and resumes the run with a result or cancellation

This makes AG-UI the better fit for:

- browser-native tools
- UI approvals
- canvas updates
- generative UI surfaces

## Frontend Tool Flow

When AG-UI frontend tools are enabled, the flow is:

1. model selects a tool that is marked as frontend-executed
2. backend does not run that tool locally
3. backend emits a pending tool interaction via AG-UI
4. frontend renders the tool UI or approval card
5. frontend responds with `resume` or `cancel`
6. backend converts that decision into tool result semantics and continues the run

This behavior is implemented by the runtime-side pending plugin in:

- `crates/tirea-agentos-server/src/protocol/ag_ui/runtime.rs`

## Shared State Model

AG-UI works best when you treat backend thread state as authoritative and frontend state as a projection.

- backend persists thread state and messages
- frontend reads AG-UI events and history to render the current projection
- local UI state can exist, but should not replace durable agent state unless you intentionally send state overrides

## Suspend / Resume And HITL

AG-UI is the stronger path for HITL flows.

Typical pattern:

1. backend permission/plugin/tool logic suspends execution
2. AG-UI emits a pending interaction event
3. CopilotKit renders an approval or data-entry component
4. user responds
5. runtime sends the decision back to the backend
6. backend resumes the original run using the supplied decision or tool result

### Optional: parse `tool-call-progress` activity events

When reading raw AG-UI events, inspect `ACTIVITY_SNAPSHOT` /
`ACTIVITY_DELTA` with `activityType = "tool-call-progress"`.

```ts,ignore
function onAgUiEvent(event: any) {
  if (event?.type !== "ACTIVITY_SNAPSHOT") return;
  if (event.activityType !== "tool-call-progress") return;
  const node = event.content;
  console.log("tool progress", node.node_id, node.status, node.progress);
}
```

## Verify

- `POST /api/copilotkit` streams AG-UI events.
- Chat UI receives assistant output in real time.
- Tool calls and shared state updates are reflected in the UI.
- Frontend-executed tools suspend and resume correctly.
- Persisted thread history can be rehydrated from `GET /v1/ag-ui/threads/:id/messages`.

## Common Errors

- Wrong AG-UI URL (`/v1/ag-ui/agents/:agent_id/runs`) causes empty responses or 404.
- `agent` in `CopilotKit` provider does not match runtime `agents` key.
- Package version mismatch may require temporary `as any` cast for `HttpAgent`.

## Related Example

- `examples/copilotkit-starter/README.md` is the full-featured AG-UI + CopilotKit starter
- `examples/travel-ui/README.md` is the smaller travel-specific CopilotKit scenario demo
- `examples/research-ui/README.md` is the research-specific CopilotKit scenario demo

## Key Files

- `examples/copilotkit-starter/lib/copilotkit-app.ts`
- `examples/copilotkit-starter/lib/persisted-http-agent.ts`
- `examples/travel-ui/lib/copilotkit-app.ts`
- `examples/research-ui/lib/copilotkit-app.ts`
- `examples/copilotkit-starter/app/api/copilotkit/route.ts`

## Related

- [AG-UI Protocol](../reference/protocols/ag-ui.md)
- [Ecosystem Integrations](../reference/ecosystem-integrations.md)
- [Frontend Interaction and Approval Model](../explanation/frontend-interaction-and-approval-model.md)
- [HTTP API](../reference/http-api.md)
- `examples/copilotkit-starter/README.md`
- `examples/copilotkit-starter/lib/copilotkit-app.ts`
