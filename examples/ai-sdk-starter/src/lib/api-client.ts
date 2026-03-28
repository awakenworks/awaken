import type { UIMessage } from "@ai-sdk/react";

const BACKEND_URL =
  import.meta.env.VITE_BACKEND_URL ?? "http://localhost:38080";

export function chatApiUrl(agentId: string): string {
  return `${BACKEND_URL}/v1/ai-sdk/agents/${agentId}/runs`;
}

export function historyApiUrl(sessionId: string): string {
  return `${BACKEND_URL}/v1/ai-sdk/threads/${encodeURIComponent(sessionId)}/messages?limit=200`;
}

export function threadSummariesUrl(): string {
  return `${BACKEND_URL}/v1/threads/summaries`;
}

export type ThreadSummary = {
  id: string;
  title: string | null;
  updated_at: number | null;
  created_at: number | null;
  message_count: number;
};

export async function fetchHistory(sessionId: string): Promise<UIMessage[]> {
  try {
    const res = await fetch(historyApiUrl(sessionId));
    if (!res.ok) return [];
    const data = await res.json();
    if (Array.isArray(data.messages)) return data.messages;
    if (Array.isArray(data.items)) return data.items;
    return [];
  } catch {
    return [];
  }
}

export async function fetchThreadSummaries(): Promise<ThreadSummary[]> {
  try {
    const res = await fetch(threadSummariesUrl());
    if (!res.ok) return [];
    const data = await res.json();
    return Array.isArray(data) ? data : [];
  } catch {
    return [];
  }
}

export async function patchThreadTitle(
  threadId: string,
  title: string,
): Promise<void> {
  try {
    await fetch(
      `${BACKEND_URL}/v1/threads/${encodeURIComponent(threadId)}/metadata`,
      {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ title }),
      },
    );
    // Silently ignore non-OK responses — thread may not exist yet
    // (created on first run, not when frontend generates session ID)
  } catch {
    // Ignore network errors for non-critical metadata update
  }
}

export async function deleteThread(threadId: string): Promise<void> {
  await fetch(`${BACKEND_URL}/v1/threads/${encodeURIComponent(threadId)}`, {
    method: "DELETE",
  });
}
