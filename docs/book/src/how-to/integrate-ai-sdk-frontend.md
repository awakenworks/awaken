# Integrate AI SDK Frontend

Use this when your web app uses `@ai-sdk/react` and backend is `tirea-agentos-server`.

## Prerequisites

- Backend is reachable (default `http://localhost:8080`).
- AI SDK routes are enabled:
  - `POST /v1/ai-sdk/agents/:agent_id/runs`
  - `GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`
  - `GET /v1/ai-sdk/threads/:id/messages`

## Steps

1. Install frontend deps.

```bash
npm install @ai-sdk/react ai
```

2. Create `app/api/chat/route.ts` as pass-through proxy.

```ts,ignore
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";
const AGENT_ID = process.env.AGENT_ID ?? "default";

export async function POST(req: Request) {
  const incoming = await req.json();
  const agentId = incoming.agentId ?? AGENT_ID;

  const sessionId =
    req.headers.get("x-session-id") ??
    incoming.id ??
    `ai-sdk-${crypto.randomUUID()}`;

  const upstream = await fetch(
    `${BACKEND_URL}/v1/ai-sdk/agents/${agentId}/runs`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...incoming, id: sessionId }),
    },
  );

  if (!upstream.ok) {
    return new Response(await upstream.text(), { status: upstream.status });
  }
  if (!upstream.body) {
    return new Response("upstream body missing", { status: 502 });
  }

  return new Response(upstream.body, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
      "x-vercel-ai-ui-message-stream": "v1",
      "x-session-id": sessionId,
    },
  });
}
```

3. Build transport with explicit `id/messages` payload.

```tsx,ignore
"use client";

import { useMemo } from "react";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

export default function ChatPage() {
  const sessionId = "ai-sdk-demo-session";

  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: "/api/chat",
        headers: { "x-session-id": sessionId },
        prepareSendMessagesRequest: ({ messages, trigger, messageId }) => {
          const lastAssistantIndex = (() => {
            for (let i = messages.length - 1; i >= 0; i -= 1) {
              if (messages[i]?.role === "assistant") return i;
            }
            return -1;
          })();

          const newUserMessages = messages
            .slice(lastAssistantIndex + 1)
            .filter((m) => m.role === "user");

          return {
            body: {
              id: sessionId,
              runId: crypto.randomUUID(),
              messages: trigger === "regenerate-message" ? [] : newUserMessages,
              ...(trigger ? { trigger } : {}),
              ...(messageId ? { messageId } : {}),
            },
          };
        },
      }),
    [sessionId],
  );

  const { messages, sendMessage, status } = useChat({ transport });
  // render + sendMessage({ text })
}
```

4. Optional: load history from `GET /v1/ai-sdk/threads/:id/messages` before first render.

## Verify

- `/api/chat` responds with `text/event-stream`.
- Response includes `x-vercel-ai-ui-message-stream: v1`.
- Normal chat streams tokens.
- Regenerate flow (`trigger=regenerate-message`) works with `messageId`.

## Common Errors

- Sending legacy `sessionId/input` payload to backend HTTP v6 route.
- Missing `id` (thread id) in forwarded payload.
- Missing stream headers, causing client parser failures.

## Related

- [AI SDK v6 Protocol](../reference/protocols/ai-sdk-v6.md)
- [HTTP API](../reference/http-api.md)
- `examples/ai-sdk-starter/src/lib/transport.ts`
