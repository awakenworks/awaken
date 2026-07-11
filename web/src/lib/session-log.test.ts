import { describe, expect, it } from "vitest";
import type { SessionEvent } from "./api/types";
import {
  isRunning,
  mergeEvents,
  pairToolResults,
  pendingConfirmIds,
  textOf,
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
