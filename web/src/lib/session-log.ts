// Pure projections over a session's committed event log. These are the reducers
// the transcript reads — extracted from session-detail so the same engine backs
// the Sandbox, the Admin Assistant, the model Test modal, and the trace view.
// Kept free of React/DOM so they are unit-testable in isolation.

import type { ContentBlock, SessionEvent } from "./api/types";

export type SessionStatusPresentation = "running" | "idle" | "preparing" | "terminated" | "unknown";

/** Closed presentation of the aggregate-owned Session status. Unknown wire
 * values fail closed instead of being presented as sendable/idle. */
export function sessionStatusPresentation(status?: string): SessionStatusPresentation {
  switch (status) {
    case "running":
      return "running";
    case "idle":
      return "idle";
    case "rescheduling":
      return "preparing";
    case "terminated":
      return "terminated";
    default:
      return "unknown";
  }
}

/** The SSE event family the host names by `type` (a committed-replay stream). */
export const SSE_EVENT_NAMES = [
  "agent.message",
  "agent.tool_use",
  "agent.tool_result",
  "agent.custom_tool_use",
  "session.status_running",
  "session.status_idle",
  "session.error",
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

export interface SessionRuntimeProjection {
  phase: "unknown" | "running" | "idle" | "error";
  pendingConfirmIds: Set<string>;
  latestError?: SessionEvent;
}

/**
 * Fold the committed stream into its one current Runtime projection. Status,
 * Chat, Trace, send admission, and Stop controls must all consume this reducer;
 * historical `requires_action` frames are facts, not permanent pending state.
 */
export function projectSessionRuntime(log: SessionEvent[]): SessionRuntimeProjection {
  let phase: SessionRuntimeProjection["phase"] = "unknown";
  let ids = new Set<string>();
  let latestError: SessionEvent | undefined;
  for (const ev of log) {
    if (ev.type === "session.status_running") {
      phase = "running";
      ids = new Set();
    } else if (ev.type === "session.status_idle" && "stop_reason" in ev) {
      phase = "idle";
      const sr = ev.stop_reason as { type: string; event_ids?: string[] };
      ids = new Set(sr.type === "requires_action" ? sr.event_ids ?? [] : []);
    } else if (ev.type === "session.error") {
      phase = "error";
      ids = new Set();
      latestError = ev;
    } else if (ev.type === "agent.tool_result" && "tool_use_id" in ev) {
      ids.delete(String(ev.tool_use_id));
    } else if (ev.type === "user.tool_confirmation" && "tool_use_id" in ev) {
      ids.delete(String(ev.tool_use_id));
    } else if (ev.type === "user.custom_tool_result" && "custom_tool_use_id" in ev) {
      ids.delete(String(ev.custom_tool_use_id));
    }
  }
  return { phase, pendingConfirmIds: ids, latestError };
}

/** The currently unresolved tool ids from the canonical Runtime projection. */
export function pendingConfirmIds(log: SessionEvent[]): Set<string> {
  return projectSessionRuntime(log).pendingConfirmIds;
}

/** Whether the latest lifecycle frame says the run is still working. */
export function isRunning(log: SessionEvent[]): boolean {
  return projectSessionRuntime(log).phase === "running";
}

/** Send admission from the current event projection, falling back to the
 * Session row only while no lifecycle event has arrived. */
export function canSendToSession(
  runtime: SessionRuntimeProjection,
  sessionStatus?: string,
): boolean {
  const phase = runtime.phase === "unknown"
    ? sessionStatusPresentation(sessionStatus)
    : runtime.phase;
  return phase != null
    && phase !== "unknown"
    && phase !== "running"
    && phase !== "preparing"
    && phase !== "terminated"
    && runtime.pendingConfirmIds.size === 0;
}

/** A concise, actionable explanation for a committed run failure. */
export function sessionErrorText(event: SessionEvent): string {
  if (event.type !== "session.error") return "";
  const raw = "error" in event && event.error && typeof event.error === "object"
    ? (event.error as { type?: unknown; message?: unknown })
    : {};
  const message = typeof raw.message === "string"
    ? raw.message
    : "message" in event && typeof event.message === "string"
      ? event.message
      : "The run failed without an error message.";
  if (/usage limit|quota/i.test(message)) {
    return "Model-provider quota is exhausted. Check billing/quota or switch the credential or model, then retry.";
  }
  if (/401|403|unauthorized|forbidden/i.test(message)) {
    return "The model provider rejected this request. Check the credential, endpoint access, and account quota, then retry.";
  }
  return message.length > 600 ? `${message.slice(0, 597)}…` : message;
}

// ---- trace projection (the same log read as spans) ----

export type SpanKind = "inference" | "tool" | "tool_result" | "status" | "outcome" | "other";

export interface TraceSpan {
  id: string;
  kind: SpanKind;
  /** A short one-line label (tool name, stop reason, "agent message"). */
  label: string;
  /** ms since the previous committed event, when both carry `processed_at`. */
  durationMs?: number;
  /** The event's payload (input / result / content), for a JSON drill-down. */
  detail?: unknown;
  /** Whether this span is an error (a failed tool result). */
  error?: boolean;
}

function spanKind(type: string): SpanKind {
  switch (type) {
    case "agent.message":
      return "inference";
    case "agent.tool_use":
    case "agent.custom_tool_use":
      return "tool";
    case "agent.tool_result":
      return "tool_result";
    case "session.status_running":
    case "session.status_idle":
    case "session.error":
      return "status";
    case "span.outcome_evaluation_start":
    case "span.outcome_evaluation_end":
      return "outcome";
    default:
      return "other";
  }
}

/** ms between two ISO timestamps, or undefined if either is missing/unparseable. */
export function spanDurationMs(prev?: string | null, cur?: string | null): number | undefined {
  if (!prev || !cur) return undefined;
  const a = Date.parse(prev);
  const b = Date.parse(cur);
  if (Number.isNaN(a) || Number.isNaN(b)) return undefined;
  const d = b - a;
  return d >= 0 ? d : undefined;
}

/** Project the event log into ordered trace spans with inter-event durations. */
export function traceSpans(log: SessionEvent[]): TraceSpan[] {
  return log.map((ev, i) => {
    const kind = spanKind(ev.type);
    let label = ev.type;
    if ((kind === "tool" || kind === "tool_result") && "name" in ev && typeof ev.name === "string") {
      label = ev.name;
    } else if (kind === "inference") {
      label = "agent message";
    } else if (kind === "status" && "stop_reason" in ev) {
      const sr = ev.stop_reason as { type?: string };
      label = sr?.type ?? ev.type;
    } else if (ev.type === "session.error") {
      label = "run failed";
    }
    const detail =
      "input" in ev ? ev.input : "content" in ev ? ev.content : "stop_reason" in ev ? ev.stop_reason : ev.type === "session.error" ? ev.error : undefined;
    return {
      id: ev.id,
      kind,
      label,
      durationMs: spanDurationMs(log[i - 1]?.processed_at, ev.processed_at),
      detail,
      error: ev.type === "session.error" || (kind === "tool_result" && "is_error" in ev && ev.is_error === true),
    };
  });
}
