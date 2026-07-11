import { describe, expect, it } from "vitest";
import type { SessionEvent } from "./api/types";
import {
  isRunning,
  mergeEvents,
  pairToolResults,
  pendingConfirmIds,
  spanDurationMs,
  textOf,
  traceSpans,
} from "./session-log";

const ev = (e: Partial<SessionEvent> & { id: string; type: string }) => e as SessionEvent;

describe("textOf", () => {
  it("joins text blocks and tags non-text blocks", () => {
    expect(textOf([{ type: "text", text: "hi" }, { type: "image" }])).toBe("hi[image]");
  });
  it("handles undefined", () => {
    expect(textOf(undefined)).toBe("");
  });
});

describe("mergeEvents", () => {
  it("appends only unseen ids, preserving order", () => {
    const log = [ev({ id: "a", type: "agent.message" })];
    const merged = mergeEvents(log, [
      ev({ id: "a", type: "agent.message" }),
      ev({ id: "b", type: "agent.message" }),
    ]);
    expect(merged.map((e) => e.id)).toEqual(["a", "b"]);
  });
  it("returns the same reference when nothing is fresh (no needless rerender)", () => {
    const log = [ev({ id: "a", type: "agent.message" })];
    expect(mergeEvents(log, [ev({ id: "a", type: "agent.message" })])).toBe(log);
  });
});

describe("pairToolResults", () => {
  it("keys results by their tool_use id", () => {
    const results = pairToolResults([
      ev({ id: "u1", type: "agent.tool_use", name: "get_weather" }),
      ev({ id: "r1", type: "agent.tool_result", tool_use_id: "u1" }),
    ]);
    expect(results.get("u1")?.id).toBe("r1");
    expect(results.has("u2")).toBe(false);
  });
});

describe("pendingConfirmIds", () => {
  it("collects event ids from a requires_action idle stop", () => {
    const ids = pendingConfirmIds([
      ev({ id: "u1", type: "agent.tool_use", name: "delete" }),
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
    ]);
    expect([...ids]).toEqual(["u1"]);
  });
  it("is empty for an end_turn stop", () => {
    const ids = pendingConfirmIds([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "end_turn" } }),
    ]);
    expect(ids.size).toBe(0);
  });
});

describe("spanDurationMs", () => {
  it("computes the positive delta between two ISO stamps", () => {
    expect(spanDurationMs("2020-01-01T00:00:00.000Z", "2020-01-01T00:00:01.500Z")).toBe(1500);
  });
  it("is undefined when a stamp is missing or unparseable", () => {
    expect(spanDurationMs(null, "2020-01-01T00:00:00Z")).toBeUndefined();
    expect(spanDurationMs("nope", "2020-01-01T00:00:00Z")).toBeUndefined();
  });
  it("is undefined for a negative delta (out-of-order stamps)", () => {
    expect(spanDurationMs("2020-01-01T00:00:02Z", "2020-01-01T00:00:01Z")).toBeUndefined();
  });
});

describe("traceSpans", () => {
  it("labels tool spans by name and flags failed results", () => {
    const spans = traceSpans([
      ev({ id: "u1", type: "agent.tool_use", name: "get_weather", input: { q: "SF" } }),
      ev({ id: "r1", type: "agent.tool_result", tool_use_id: "u1", name: "get_weather", is_error: true }),
      ev({ id: "m1", type: "agent.message", content: [{ type: "text", text: "hi" }] }),
    ]);
    expect(spans.map((s) => s.kind)).toEqual(["tool", "tool_result", "inference"]);
    expect(spans[0].label).toBe("get_weather");
    expect(spans[0].detail).toEqual({ q: "SF" });
    expect(spans[1].error).toBe(true);
    expect(spans[2].label).toBe("agent message");
  });
  it("carries inter-event durations when processed_at is present", () => {
    const spans = traceSpans([
      ev({ id: "a", type: "agent.message", processed_at: "2020-01-01T00:00:00.000Z" }),
      ev({ id: "b", type: "agent.message", processed_at: "2020-01-01T00:00:00.250Z" }),
    ]);
    expect(spans[0].durationMs).toBeUndefined();
    expect(spans[1].durationMs).toBe(250);
  });
});

describe("isRunning", () => {
  it("is true when the last frame is a running status", () => {
    expect(isRunning([ev({ id: "s1", type: "session.status_running" })])).toBe(true);
  });
  it("is false when the last frame is idle", () => {
    expect(
      isRunning([
        ev({ id: "s1", type: "session.status_running" }),
        ev({ id: "s2", type: "session.status_idle", stop_reason: { type: "end_turn" } }),
      ]),
    ).toBe(false);
  });
  it("is false for an empty log", () => {
    expect(isRunning([])).toBe(false);
  });
});
