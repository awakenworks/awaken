# OpenUI Chat Example — Design Spec

## Goal

Create `examples/openui-chat/` — a standalone Vite + React Router example showcasing the full OpenUI Chat SDK connected to the awaken agent server via AG-UI protocol.

## Layouts

Route-based navigation between all three OpenUI Chat layouts + Artifacts:

| Route | Layout | Context |
|-------|--------|---------|
| `/fullscreen` (default) | `FullScreen` | Full-page ChatGPT-style interface |
| `/copilot` | `Copilot` | Sidebar assistant alongside simulated main app content |
| `/bottom-tray` | `BottomTray` | Floating widget over page content |

All layouts share the same agent connection and GenUI rendering configuration.

## Protocol Integration

### Frontend → Backend

OpenUI Chat SDK connects to awaken's AG-UI endpoints natively:

- **Stream protocol**: `agUIAdapter()` from `@openuidev/react-headless` (built-in)
- **Chat endpoint**: `POST /v1/ag-ui/agents/{agentId}/runs`
- **No custom adapter needed** — AG-UI is a first-class protocol in both systems

### Thread Management

OpenUI's thread API uses callback functions mapped to awaken endpoints:

| Callback | awaken Endpoint |
|----------|----------------|
| `fetchThreadList()` | `GET /v1/threads/summaries` |
| `loadThread(id)` | `GET /v1/ag-ui/threads/:id/messages` |
| `deleteThread(id)` | `DELETE /v1/threads/:id` |
| `updateThread(t)` | `PATCH /v1/threads/:id/metadata` |
| `createThread(msg)` | Implicit (first message to agent creates thread) |

### GenUI Rendering

- `componentLibrary={openuiChatLibrary}` passed to layout components
- awaken's OpenUI agent has the system prompt pre-configured
- OpenUI SDK handles rendering OpenUI Lang output from tool calls internally

## Tech Stack

- **Vite** + React 19 + TypeScript
- **React Router v7** for layout switching
- **@openuidev/react-ui** — layout components (FullScreen, Copilot, BottomTray)
- **@openuidev/react-headless** — `agUIAdapter()`, thread hooks
- **@openuidev/react-lang** — `openuiChatLibrary` for GenUI
- **lucide-react** — icons (required by OpenUI UI)
- **TailwindCSS** — consistent with other examples

## Project Structure

```
examples/openui-chat/
├── src/
│   ├── main.tsx              # Entry point
│   ├── App.tsx               # Router + nav wrapper
│   ├── pages/
│   │   ├── fullscreen.tsx    # FullScreen layout page
│   │   ├── copilot.tsx       # Copilot layout + mock main app
│   │   └── bottom-tray.tsx   # BottomTray layout + page content
│   ├── lib/
│   │   ├── config.ts         # VITE_BACKEND_URL, agent ID
│   │   └── thread-api.ts     # Thread callback functions (~50 lines)
│   └── components/
│       └── nav.tsx           # Layout switcher navigation
├── package.json
├── vite.config.ts
├── tsconfig.json
├── tailwind.config.ts
├── postcss.config.js
└── index.html
```

## Configuration

Environment variables (via `.env` or Vite env):

- `VITE_BACKEND_URL` — awaken agent server URL (default: `http://127.0.0.1:38080`)
- `VITE_AGENT_ID` — target agent ID for chat

## Dev Script

Consistent with `ai-sdk-starter`, uses `concurrently` to run UI dev server + agent server:

```json
{
  "dev": "vite",
  "dev:full": "concurrently \"vite\" \"cargo run -p awaken-server\""
}
```

## Non-Goals

- No custom stream adapter (AG-UI is natively supported)
- No backend changes
- No custom message rendering (use OpenUI defaults + GenUI)
- No auth/login flow
