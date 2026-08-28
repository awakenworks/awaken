import { describe, expect, it } from "vitest";
import type { SessionEvent } from "./api/types";
import {
  isRunning,
  pairToolResults,
  pendingConfirmIds,
  projectSessionRuntime,
  canSendToSession,
  sessionErrorText,
  sessionStatusPresentation,
  spanDurationMs,
  textOf,
  traceSpans,
} from "./session-log";

describe("sessionStatusPresentation", () => {
  it("is total, exact, and fail-closed for every aggregate status", () => {
    // Cause/effect graph: C1 wire status is running/idle/rescheduling/
    // terminated/unknown. Effects: E1 the UI says working/sendable/preparing/
    // terminal respectively; E2 an unknown value never impersonates idle.
    // Decision rules: P1 running=>running; P2 idle=>idle; P3
    // rescheduling=>preparing; P4 terminated=>terminated; P5 other=>unknown.
    expect(sessionStatusPresentation("running")).toBe("running");
    expect(sessionStatusPresentation("idle")).toBe("idle");
    expect(sessionStatusPresentation("rescheduling")).toBe("preparing");
    expect(sessionStatusPresentation("terminated")).toBe("terminated");
    expect(sessionStatusPresentation("future-status")).toBe("unknown");
    expect(sessionStatusPresentation()).toBe("unknown");
  });
});

const ev = (e: Partial<SessionEvent> & { id: string; type: string }) => e as SessionEvent;

describe("textOf", () => {
  it("joins text blocks and tags non-text blocks", () => {
    expect(textOf([{ type: "text", text: "hi" }, { type: "image" }])).toBe("hi[image]");
  });
  it("handles undefined", () => {
    expect(textOf(undefined)).toBe("");
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
  it("projects only the latest unresolved requires_action set", () => {
    // Cause/effect graph: C1 lifecycle is running/requires-action/end/error;
    // C2 an answer/result for one requested id is absent/present; C3 unrelated
    // frames follow a lifecycle frame. Effects: E1 current phase follows the
    // latest lifecycle fact; E2 pending ids are replaced, individually closed,
    // or terminally cleared; E3 non-lifecycle frames cannot fabricate idle.
    //
    // | Rule | Lifecycle/input | Effect |
    // |---|---|---|
    // | S1 | running + message | running/no pending |
    // | S2 | requires_action(u1,u2) | idle/u1,u2 |
    // | S3 | S2 + confirmation(u1) + result(u2) | idle/empty |
    // | S4 | stale requires_action + later end/error | idle-or-error/empty |
    const ids = pendingConfirmIds([
      ev({ id: "u1", type: "agent.tool_use", name: "delete" }),
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
    ]);
    expect([...ids]).toEqual(["u1"]);
  });
  it("closes answered ids and does not retain an old approval card", () => {
    const state = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1", "u2"] } }),
      ev({ id: "a1", type: "user.tool_confirmation", tool_use_id: "u1", result: "allow" }),
      ev({ id: "a2", type: "agent.tool_result", tool_use_id: "u2" }),
    ]);
    expect(state.phase).toBe("idle");
    expect([...state.pendingConfirmIds]).toEqual([]);
  });
  it("distinguishes an accepted reply from a processed reply", () => {
    // Cause/effect graph: C1 requires_action exposes u1; C2 an exact reply is
    // committed; C3 its processed_at is absent/present. Effects: E1 C2 closes
    // the approval card; E2 C2+C3(absent) remains visible as resolving; E3 only
    // C3(present) closes resolving. The committed event is the only source.
    //
    // | Rule | reply committed | processed_at | pending | resolving |
    // |---|---|---|---|---|
    // | RP1 | no  | n/a     | u1 | empty |
    // | RP2 | yes | absent  | empty | u1 |
    // | RP3 | yes | present | empty | empty |
    const resolving = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
      ev({ id: "a1", type: "user.tool_confirmation", tool_use_id: "u1", result: "allow" }),
    ]);
    expect([...resolving.pendingConfirmIds]).toEqual([]);
    expect([...resolving.resolvingConfirmIds]).toEqual(["u1"]);
    expect(canSendToSession(resolving, "idle")).toBe(true);

    const processed = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
      ev({ id: "a1", type: "user.tool_confirmation", tool_use_id: "u1", result: "allow", processed_at: "2026-08-28T00:00:00Z" }),
    ]);
    expect([...processed.resolvingConfirmIds]).toEqual([]);
  });
  it("a later terminal frame clears stale requires_action state", () => {
    const ended = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
      ev({ id: "s2", type: "session.status_idle", stop_reason: { type: "end_turn" } }),
    ]);
    expect(ended.phase).toBe("idle");
    expect(ended.pendingConfirmIds.size).toBe(0);
    const failed = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
      ev({ id: "e1", type: "session.error", error: { message: "worker failed" } }),
    ]);
    expect(failed.phase).toBe("error");
    expect(failed.pendingConfirmIds.size).toBe(0);
    expect(failed.latestError?.id).toBe("e1");
  });
  it("derives send admission from event truth with a Session fallback", () => {
    // Cause/effect graph: C1 event phase is known/unknown; C2 Session fallback
    // is running/rescheduling/idle; C3 pending tools are empty/nonempty.
    // Effect E1 permits send only at a known non-running boundary with no
    // pending tool. Decision rules: A1 unknown+running=>deny; A2
    // unknown+idle+empty=>allow; A3 idle+pending=>deny; A4 idle+empty=>allow.
    const unknown = projectSessionRuntime([]);
    expect(canSendToSession(unknown, "running")).toBe(false);
    expect(canSendToSession(unknown, "idle")).toBe(true);
    expect(canSendToSession(unknown, "terminated")).toBe(false);
    expect(canSendToSession(unknown, "future-status")).toBe(false);
    const pending = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["u1"] } }),
    ]);
    expect(canSendToSession(pending, "idle")).toBe(false);
    const idle = projectSessionRuntime([
      ev({ id: "s1", type: "session.status_idle", stop_reason: { type: "end_turn" } }),
    ]);
    expect(canSendToSession(idle, "running")).toBe(true);
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
  it("is true when the latest lifecycle frame is running", () => {
    expect(isRunning([
      ev({ id: "s1", type: "session.status_running" }),
      ev({ id: "m1", type: "agent.message" }),
    ])).toBe(true);
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

describe("sessionErrorText", () => {
  it("turns provider quota failures into an actionable message", () => {
    expect(sessionErrorText(ev({
      id: "e1",
      type: "session.error",
      error: { type: "model_request_failed_error", message: "403: usage limit reached for this quota" },
    }))).toMatch(/quota is exhausted.*switch the credential or model/i);
  });

  it("preserves an ordinary runtime failure", () => {
    expect(sessionErrorText(ev({ id: "e1", type: "session.error", error: { message: "worker disconnected" } })))
      .toBe("worker disconnected");
  });
});
