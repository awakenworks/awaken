# AI SDK Starter

Vite + React Router frontend using Vercel AI SDK v6 (`@ai-sdk/react`) with a Rust `tirea-agentos-server` backend.

## Architecture

```
Browser (useChat) -> Rust backend (axum + CORS) -> LLM
                 -> HTTP SSE (AI SDK v6 stream)
```

The frontend calls backend directly (no Node proxy).

## Demo Pages

- `/` canvas + shared state panel
- `/basic` tool calls + approval dialogs
- `/threads` backend-persisted thread history

## Quick Start

```bash
cd examples/ai-sdk-starter
npm install
DEEPSEEK_API_KEY=<key> npm run dev
```

Separate terminals:

```bash
# terminal 1
DEEPSEEK_API_KEY=<key> cargo run -p ai-sdk-starter-agent

# terminal 2
cd examples/ai-sdk-starter
npm run dev:ui
```

Open `http://localhost:3001`.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `VITE_BACKEND_URL` | `http://localhost:38080` | Backend base URL (frontend) |
| `AGENTOS_HTTP_ADDR` | `127.0.0.1:38080` | Backend listen address |
| `AGENT_ID` | `default` | Agent id used by frontend transport |
| `AGENT_MODEL` | `deepseek-chat` | Model id |
| `AGENT_MAX_ROUNDS` | `8` | Max loop rounds |
| `MCP_SERVER_CMD` | unset | Optional MCP stdio server command |

## Backend Endpoint Surface

This starter backend mounts full route groups, not only AI SDK:

- Health: `GET /health`
- Threads: `GET /v1/threads`, `GET /v1/threads/:id`, `GET /v1/threads/:id/messages`
- Run API: `GET/POST /v1/runs`, `GET /v1/runs/:id`, `POST /v1/runs/:id/inputs`, `POST /v1/runs/:id/cancel`
- AI SDK v6: `POST /v1/ai-sdk/agents/:agent_id/runs`, `GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`, `GET /v1/ai-sdk/threads/:id/messages`
- AG-UI: `POST /v1/ag-ui/agents/:agent_id/runs`, `GET /v1/ag-ui/threads/:id/messages`
- A2A: `GET /.well-known/agent-card.json`, `/v1/a2a/...`

AI SDK HTTP payload uses `id/messages` (not legacy `sessionId/input`).

## Verify

1. Open `http://localhost:3001` and send a prompt.
2. Confirm streamed response appears in chat.
3. Navigate to `/threads` and reload prior thread history.
4. Trigger an approval tool flow and confirm resume after approval.
5. (Optional) run `npm run dev:mcp` and verify MCP tool cards render.
