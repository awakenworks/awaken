import type { UIMessage } from "@ai-sdk/react";
import {
  parseHistoryPayload,
  parseThreadSummariesPayload,
  type ThreadSummary,
} from "./protocol";

const BACKEND_URL =
  import.meta.env.VITE_BACKEND_URL ?? "http://127.0.0.1:38080";

export function chatApiUrl(agentId: string): string {
  return `${BACKEND_URL}/v1/ai-sdk/agents/${agentId}/runs`;
}

export function historyApiUrl(sessionId: string): string {
  return `${BACKEND_URL}/v1/ai-sdk/threads/${encodeURIComponent(sessionId)}/messages?limit=200`;
}

export function threadSummariesUrl(): string {
  return `${BACKEND_URL}/v1/threads/summaries?limit=200`;
}

export async function fetchHistory(sessionId: string): Promise<UIMessage[]> {
  try {
    const res = await fetch(historyApiUrl(sessionId));
    if (!res.ok) return [];
    const data = await res.json();
    return parseHistoryPayload(data);
  } catch {
    return [];
  }
}

export async function fetchThreadSummaries(): Promise<ThreadSummary[]> {
  try {
    const res = await fetch(threadSummariesUrl());
    if (!res.ok) return [];
    const data = await res.json();
    return parseThreadSummariesPayload(data);
  } catch {
    return [];
  }
}

export async function patchThreadTitle(
  threadId: string,
  title: string,
): Promise<boolean> {
  try {
    const res = await fetch(
      `${BACKEND_URL}/v1/threads/${encodeURIComponent(threadId)}/metadata`,
      {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ title }),
      },
    );
    return res.ok;
  } catch {
    return false;
  }
}

export async function deleteThread(threadId: string): Promise<void> {
  await fetch(`${BACKEND_URL}/v1/threads/${encodeURIComponent(threadId)}`, {
    method: "DELETE",
  });
}
