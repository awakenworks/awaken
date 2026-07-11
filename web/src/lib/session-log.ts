// Pure projections over a session's committed event log. These are the reducers
// the transcript reads — extracted from session-detail so the same engine backs
// the Sandbox, the Admin Assistant, the model Test modal, and the trace view.
// Kept free of React/DOM so they are unit-testable in isolation.

import type { ContentBlock, SessionEvent } from "./api/types";

/** The SSE event family the host names by `type` (a committed-replay stream). */
export const SSE_EVENT_NAMES = [
  "agent.message",
  "agent.tool_use",
  "agent.tool_result",
  "agent.custom_tool_use",
  "session.status_running",
  "session.status_idle",
  "span.outcome_evaluation_start",
  "span.outcome_evaluation_end",
] as const;

/** Flatten a content-block array to display text (non-text blocks show a tag). */
export function textOf(content: ContentBlock[] | undefined): string {
  return (content ?? [])
    .map((b) => ("text" in b && typeof b.text === "string" ? b.text : `[${b.type}]`))
    .join("");
}

/** Merge new events into the log: dedupe by id, keep arrival order. */
export function mergeEvents(log: SessionEvent[], incoming: SessionEvent[]): SessionEvent[] {
  const seen = new Set(log.map((e) => e.id));
  const fresh = incoming.filter((e) => !seen.has(e.id));
  return fresh.length ? [...log, ...fresh] : log;
}

/** Pair each tool_use with its tool_result (keyed by the tool_use id). */
export function pairToolResults(log: SessionEvent[]): Map<string, SessionEvent> {
  const results = new Map<string, SessionEvent>();
  for (const ev of log) {
    if (ev.type === "agent.tool_result" && "tool_use_id" in ev) {
      results.set(ev.tool_use_id as string, ev);
    }
  }
  return results;
}

/** The tool_use ids the run is blocked on (a `requires_action` idle stop). */
export function pendingConfirmIds(log: SessionEvent[]): Set<string> {
  const ids = new Set<string>();
  for (const ev of log) {
    if (ev.type === "session.status_idle" && "stop_reason" in ev) {
      const sr = ev.stop_reason as { type: string; event_ids?: string[] };
      if (sr.type === "requires_action") for (const id of sr.event_ids ?? []) ids.add(id);
    }
  }
  return ids;
}

/** Whether the run's last committed frame is a "still working" status. */
export function isRunning(log: SessionEvent[]): boolean {
  return log[log.length - 1]?.type === "session.status_running";
}
