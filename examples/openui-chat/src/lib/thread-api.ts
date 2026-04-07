import type { Thread, Message, UserMessage } from "@openuidev/react-headless";
import { BACKEND_URL } from "./config";

export async function fetchThreadList(): Promise<{
  threads: Thread[];
  nextCursor?: undefined;
}> {
  try {
    const res = await fetch(`${BACKEND_URL}/v1/threads/summaries?limit=200`);
    if (!res.ok) return { threads: [] };
    const data = await res.json();
    const items: unknown[] = Array.isArray(data?.items) ? data.items : [];
    const threads: Thread[] = items
      .filter(
        (item): item is Record<string, unknown> =>
          typeof item === "object" && item !== null,
      )
      .filter((item) => typeof item.id === "string")
      .map((item) => ({
        id: item.id as string,
        title: typeof item.title === "string" ? item.title : "",
        createdAt:
          typeof item.created_at === "number"
            ? item.created_at
            : typeof item.updated_at === "number"
              ? item.updated_at
              : Date.now(),
      }));
    return { threads };
  } catch {
    return { threads: [] };
  }
}

/**
 * Flatten multimodal content arrays to plain strings.
 *
 * The awaken AG-UI endpoint stores content as `[{ type: "text", text }]`
 * arrays, but OpenUI SDK expects `content` to be a plain string.
 */
function flattenContent(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter(
        (p): p is { text: string } =>
          typeof p === "object" && p !== null && typeof p.text === "string",
      )
      .map((p) => p.text)
      .join("");
  }
  return "";
}

export async function loadThread(threadId: string): Promise<Message[]> {
  try {
    const res = await fetch(
      `${BACKEND_URL}/v1/ag-ui/threads/${encodeURIComponent(threadId)}/messages?limit=200`,
    );
    if (!res.ok) return [];
    const data = await res.json();
    const raw: unknown[] = Array.isArray(data?.messages) ? data.messages : [];
    return raw
      .filter(
        (m): m is Record<string, unknown> =>
          typeof m === "object" && m !== null,
      )
      .map((m) => ({
        ...m,
        content: flattenContent(m.content),
      })) as Message[];
  } catch {
    return [];
  }
}

export async function createThread(_firstMessage: UserMessage): Promise<Thread> {
  const res = await fetch(`${BACKEND_URL}/v1/threads`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({}),
  });
  const data = await res.json();
  return {
    id: data.id,
    title: data.metadata?.title ?? "",
    createdAt: data.metadata?.created_at ?? Date.now(),
  };
}

export async function deleteThread(id: string): Promise<void> {
  await fetch(`${BACKEND_URL}/v1/threads/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

export async function updateThread(thread: Thread): Promise<Thread> {
  await fetch(
    `${BACKEND_URL}/v1/threads/${encodeURIComponent(thread.id)}/metadata`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title: thread.title }),
    },
  );
  return thread;
}
