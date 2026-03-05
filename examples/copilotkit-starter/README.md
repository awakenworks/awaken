# with-tirea

Official-style CopilotKit starter for integrating with `tirea-agentos-server` over AG-UI.

## 10-Minute Quickstart

## 1) Prerequisites

### Mode A: Local crates.io backend (recommended default)

- Node.js 20+
- npm
- Rust toolchain (`cargo`)
- `DEEPSEEK_API_KEY` (when using default `deepseek-chat`)

### Mode B: Remote existing tirea backend

- Node.js 20+
- npm
- Reachable `tirea-agentos-server` endpoints:
  - `POST /v1/ag-ui/agents/:agent_id/runs`
  - `GET /v1/threads`
  - `GET /v1/threads/:thread_id`
  - `GET /v1/ag-ui/threads/:thread_id/messages`

## 2) First-time setup (recommended: one command for frontend + backend)

This section is for **Mode A (local crates.io backend)**.

Install frontend deps and create env file:

```bash
cd examples/copilotkit-starter
npm install
cp .env.example .env.local
export DEEPSEEK_API_KEY=<your_key>
npm run setup:agent
```

Start both services:

```bash
# Runs frontend + backend together
npm run dev
```

Default full-stack backend runtime:

- local backend entry: `agent/src/main.rs`
- editable tools: `agent/src/tools.rs`
- default agent id: `default`
- default model: `deepseek-chat`

Open:

- Base starter: `http://localhost:3000`
- Persisted threads variant: `http://localhost:3000/persisted-threads`
- Canvas variant: `http://localhost:3000/canvas`

## 3) Alternative: frontend only (use existing backend)

This section is for **Mode B (remote existing backend)**.

Edit `.env.local` and run frontend only:

```env
BACKEND_URL=http://localhost:38080
AGENT_ID=default
```

```bash
npm run dev:ui
```

If you need to run backend manually in this mode:

```bash
npm run dev:agent
```

## 4) `dev` / `dev:agent` optional environment variables

- `FRONTEND_PORT`: default `3000`
- `BACKEND_ADDR`: default `127.0.0.1:38080`
- `BACKEND_URL`: default `http://<BACKEND_ADDR>`
- `AGENT_ID`: default `default`
- `AGENT_MODEL`: model id for local starter agent (default `deepseek-chat`)
- `AGENT_MAX_ROUNDS`: max tool/model rounds (default `5`)
- `AGENT_SYSTEM_PROMPT`: system prompt override
- `AGENTOS_STORAGE_DIR`: backend thread/state storage path (default `./sessions`)

Example:

1. Full-stack custom ports:
   ```bash
   FRONTEND_PORT=3001 BACKEND_ADDR=127.0.0.1:39080 npm run dev
   ```
2. Model override:
   ```bash
   DEEPSEEK_API_KEY=<your_key> AGENT_MODEL=deepseek-chat npm run dev:agent
   ```
3. Remote backend mode:
   ```bash
   BACKEND_URL=http://your-remote-backend:38080 AGENT_ID=default npm run dev:ui
   ```

## 5) Available scripts

- `npm run setup:agent`: check Rust toolchain and prefetch backend crates.
- `npm run dev`: start frontend + backend together.
- `npm run dev:ui`: start frontend only.
- `npm run dev:agent`: start backend only.
- `npm run dev:all`: alias of `npm run dev`.
- `npm run build`: build Next.js app.
- `npm run smoke`: connectivity smoke checks.
- `npm run e2e`: Playwright tests.
## 6) Verify all 4 capabilities

In the base starter (`/`), open Copilot chat and try:

1. Shared State (`useCoAgent`)
   Prompt: `Add a todo: verify AG-UI event ordering.`
   Expected: todo list and state JSON update.
2. Frontend Actions (`useCopilotAction`)
   Prompt: `Set theme color to #16a34a, then add todo: ship starter.`
   Expected: background changes and action result reflected in UI.
3. Generative UI (`render` / `useRenderToolCall`)
   Prompt: `Call showReleaseChecklist with items: run tests, tag release.`
   Expected: a checklist card is rendered in Copilot chat.
   Optional backend tool prompt: `What's the weather in San Francisco?`
   Expected: weather card/default tool card renders from starter backend tool execution.
4. HITL (`renderAndWaitForResponse`)
   Prompt: `Ask me to approve clearing todos before executing.`
   Expected: approval card appears; Approve/Deny updates flow result.

## 7) Verify persisted threads flow

Open `/persisted-threads` and verify:

1. Left sidebar shows threads loaded from backend route (`GET /api/threads`).
2. Each chat request carries explicit `threadId` from selected thread.
3. Runtime hydrates state/message history from backend per thread before run.
4. Create/switch threads and confirm active thread id changes.

## 8) Verify canvas flow

Open `/canvas` and verify:

1. Canvas uses v2 chat + state hooks (`CopilotChat` + `useAgent`).
2. Local board edits update shared `todos` state (pending/done, edit, emoji, delete).
3. Prompt `Call pieChart with title 'Sprint Mix' and items: dev 5, test 3, docs 2.`
   - Expected: pie chart card renders in chat.
4. Prompt `Call barChart with title 'Weekly Load' and items: mon 2, tue 4, wed 3.`
   - Expected: bar chart card renders in chat.
5. Prompt `Schedule a meeting with reason 'Review canvas UX' and duration 45.`
   - Expected: HITL picker appears; Approve/Deny sends structured response.

## Troubleshooting

Mode selection tips:

1. If you don't need to modify tirea internals, use Mode A (local crates.io backend) first.
2. If your team already hosts a backend, use Mode B with `BACKEND_URL`.

1. `404`/`500` from `/api/copilotkit`
   - Verify `BACKEND_URL` and `AGENT_ID` in `.env.local`.
   - Verify backend route exists: `POST /v1/ag-ui/agents/:agent_id/runs`.
2. `fetch failed` / `ECONNREFUSED 127.0.0.1:38080`
   - Backend is not running on configured port.
   - If using full-stack mode: run `npm run dev`.
   - If using frontend-only mode: start backend manually with:
     - `npm run dev:agent`
3. `DEEPSEEK_API_KEY is required for model 'deepseek-chat'`
   - Export DeepSeek key before `npm run dev`:
     - `export DEEPSEEK_API_KEY=<your_key>`
   - Or switch model for local debug:
     - `AGENT_MODEL=gpt-4o-mini npm run dev:agent`
4. `/api/threads` returns empty list
   - Verify backend thread APIs are reachable:
     - `GET /v1/threads`
     - `GET /v1/threads/:thread_id`
     - `GET /v1/ag-ui/threads/:thread_id/messages`
5. Chat opens but agent does not act
   - Ensure `AGENT_ID` matches the actual backend agent id.
6. CORS or cross-origin issues
   - Keep frontend runtime route as same-origin (`/api/copilotkit`).
   - Ensure backend accepts requests from frontend host if called directly.
7. Wrong runtime URL
   - `app/page.tsx`, `app/persisted-threads/page.tsx`, and `app/canvas/page.tsx`
     should keep `runtimeUrl="/api/copilotkit"`.

## What this starter demonstrates

- AG-UI transport via `@ag-ui/client` `HttpAgent` with tirea snapshot hydration
- CopilotKit runtime proxy route (`/api/copilotkit`)
- Optional thread list route (`/api/threads`)
- Shared state (`useCoAgent`)
- Frontend actions (`useCopilotAction`)
- Generative UI tool rendering (`useRenderToolCall`)
- Default backend tool rendering (`useDefaultTool`)
- Human-in-the-loop action (`renderAndWaitForResponse`)
- Persisted thread switching with explicit `threadId`
- Canvas v2 flow (`CopilotChat`, `useAgent`, multi-tool rendering, structured HITL)

AG-UI activity example emitted by backend for tool progress:

```json
{
  "type": "ACTIVITY_SNAPSHOT",
  "messageId": "tool_call:call_123",
  "activityType": "tool-call-progress",
  "content": {
    "type": "tool-call-progress",
    "schema": "tool-call-progress.v1",
    "node_id": "tool_call:call_123",
    "parent_call_id": "call_parent_1",
    "parent_node_id": "tool_call:call_parent_1",
    "status": "running",
    "progress": 0.4
  }
}
```

## Key files

- `lib/copilotkit-app.ts`: CopilotKit runtime + persisted thread-capable AG-UI agent
- `lib/persisted-http-agent.ts`: runtime hydration logic (backend history as source of truth)
- `lib/tirea-backend.ts`: backend thread APIs and message/state snapshot loading
- `app/api/copilotkit/route.ts`: runtime endpoint
- `app/api/threads/route.ts`: thread list proxy route
- `agent/src/main.rs`: local starter backend runtime entry
- `agent/src/tools.rs`: editable backend sample tools
- `agent/src/state.rs`: backend persisted state model
- `app/page.tsx`: base starter (4 CopilotKit capabilities)
- `components/default-tool-ui.tsx`: catch-all backend tool renderer
- `components/weather.tsx`: weather card renderer for `get_weather`
- `app/persisted-threads/page.tsx`: persisted threads variant
- `app/canvas/page.tsx`: canvas variant
