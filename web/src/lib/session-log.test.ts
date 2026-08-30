import { describe, expect, it } from "vitest";
import type { ContentBlock, SessionEvent } from "./api/types";
import {
  pairToolResults,
  sessionErrorText,
  spanDurationMs,
  textOf,
  toolDiagnostics,
  traceSpans,
} from "./session-log";

const ev = (value: Record<string, unknown>) => value as unknown as SessionEvent;

describe("Session presentation projections", () => {
  it("flattens the closed official content-block union", () => {
    // Cause/effect rules: C1 text block -> E1 append exact text; C2 any
    // non-text official block -> E2 render its discriminator without inventing
    // content; C3 absent content -> E3 empty string.
    const content: ContentBlock[] = [{ type: "text", text: "hi" }, { type: "redacted" }];
    expect(textOf(content)).toBe("hi[redacted]");
    expect(textOf(undefined)).toBe("");
  });

  it("pairs server tool results by their exact tool-use id", () => {
    // Cause/effect rule: C1 agent.tool_result names one tool_use_id -> E1 only
    // that tool card receives the result; unrelated ids remain absent.
    const results = pairToolResults([
      ev({ id: "u1", type: "agent.tool_use", name: "get_weather" }),
      ev({ id: "r1", type: "agent.tool_result", tool_use_id: "u1" }),
    ]);
    expect(results.get("u1")?.id).toBe("r1");
    expect(results.has("u2")).toBe(false);
  });

  it("computes only nonnegative durations between committed anchors", () => {
    // Decision rules: D1 two valid ordered timestamps -> elapsed ms; D2 absent,
    // invalid, or reversed timestamp -> undefined rather than fabricated time.
    expect(spanDurationMs("2020-01-01T00:00:00.000Z", "2020-01-01T00:00:01.500Z")).toBe(1500);
    expect(spanDurationMs(null, "2020-01-01T00:00:00Z")).toBeUndefined();
    expect(spanDurationMs("nope", "2020-01-01T00:00:00Z")).toBeUndefined();
    expect(spanDurationMs("2020-01-01T00:00:02Z", "2020-01-01T00:00:01Z")).toBeUndefined();
  });

  it("projects tool, message, and complete lifecycle status spans", () => {
    // Cause/effect graph: C1 tool/message/status families -> E1 stable trace
    // kind; C2 failed result/error -> E2 error marker; C3 processed anchors ->
    // E3 duration. The status family includes recovery and both terminal facts.
    const spans = traceSpans([
      ev({ id: "u1", type: "agent.tool_use", name: "get_weather", input: { q: "SF" }, processed_at: "2020-01-01T00:00:00.000Z" }),
      ev({ id: "r1", type: "agent.tool_result", tool_use_id: "u1", is_error: true, processed_at: "2020-01-01T00:00:00.250Z" }),
      ev({ id: "m1", type: "agent.message", content: [{ type: "text", text: "hi" }], processed_at: "2020-01-01T00:00:00.500Z" }),
      ev({ id: "retry", type: "session.status_rescheduled", processed_at: "2020-01-01T00:00:00.750Z" }),
      ev({ id: "terminated", type: "session.status_terminated", processed_at: "2020-01-01T00:00:01.000Z" }),
      ev({ id: "deleted", type: "session.deleted", processed_at: "2020-01-01T00:00:01.250Z" }),
    ]);
    expect(spans.map(({ kind }) => kind)).toEqual(["tool", "tool_result", "inference", "status", "status", "status"]);
    expect(spans[0]?.label).toBe("get_weather");
    expect(spans[0]?.detail).toEqual({ q: "SF" });
    expect(spans[1]?.error).toBe(true);
    expect(spans[1]?.durationMs).toBe(250);
  });

  it("aggregates tool outcomes only from exact committed result pairs", () => {
    // Cause graph: C1 repeated tool calls, C2 success/failure/pending result,
    // C3 valid/invalid anchors. Effects: E1 exact counts, E2 failures stay
    // distinct from pending, E3 median uses only valid completed durations.
    const rows = toolDiagnostics([
      ev({ id: "a1", type: "agent.tool_use", name: "search", processed_at: "2020-01-01T00:00:00.000Z" }),
      ev({ id: "a2", type: "agent.tool_use", name: "search", processed_at: "2020-01-01T00:00:01.000Z" }),
      ev({ id: "a3", type: "agent.tool_use", name: "search", processed_at: "bad" }),
      ev({ id: "a4", type: "agent.custom_tool_use", name: "publish", processed_at: "2020-01-01T00:00:03.000Z" }),
      ev({ id: "r1", type: "agent.tool_result", tool_use_id: "a1", processed_at: "2020-01-01T00:00:00.100Z" }),
      ev({ id: "r2", type: "agent.tool_result", tool_use_id: "a2", is_error: true, processed_at: "2020-01-01T00:00:01.300Z" }),
      ev({ id: "r3", type: "agent.tool_result", tool_use_id: "a3", processed_at: "2020-01-01T00:00:02.000Z" }),
      ev({ id: "orphan", type: "agent.tool_result", tool_use_id: "missing", is_error: true }),
    ]);
    expect(rows).toEqual([
      { name: "publish", calls: 1, completed: 0, failures: 0, medianDurationMs: undefined },
      { name: "search", calls: 3, completed: 3, failures: 1, medianDurationMs: 200 },
    ]);
  });

  it("turns provider quota failures into an actionable message", () => {
    // Cause/effect rules: C1 quota/auth classifier in the official error message
    // -> E1 actionable operator guidance; C2 ordinary error -> E2 preserve it.
    expect(sessionErrorText(ev({
      id: "e1",
      type: "session.error",
      error: { type: "model_request_failed_error", message: "403: usage limit reached for this quota", retry_status: { type: "exhausted" } },
    }))).toMatch(/quota is exhausted.*switch the credential or model/i);
    expect(sessionErrorText(ev({
      id: "e2",
      type: "session.error",
      error: { type: "unknown_error", message: "worker disconnected", retry_status: { type: "exhausted" } },
    }))).toBe("worker disconnected");
  });
});
