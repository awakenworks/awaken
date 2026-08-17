import { describe, expect, it } from "vitest";
import {
  EMPTY_LIVE_PREVIEW,
  isCommittedStreamEvent,
  mergeCommittedEvents,
  reduceLivePreview,
  type ManagedEvent,
  type ManagedStreamEvent,
} from "./index.js";

const event = (value: Record<string, unknown>): ManagedEvent => value as unknown as ManagedEvent;
const stream = (value: Record<string, unknown>): ManagedStreamEvent => value as unknown as ManagedStreamEvent;

describe("Managed Session projection decision table", () => {
  /**
   * Cause-effect rules:
   * R1: C1 current event and C2 incoming event have different IDs -> E1 retain both in first-seen order.
   * R2: C1 and C2 share one ID -> E2 retain one row and replace its material with the incoming committed event.
   * Constraint: event identity is the only deduplication key; payload equality and arrival channel are irrelevant.
   */
  it("merges SSE/history overlap by committed event identity", () => {
    const first = event({ id: "message", type: "agent.message", content: [] });
    const replay = event({
      id: "message",
      type: "agent.message",
      content: [{ type: "text", text: "complete" }],
    });
    const status = event({ id: "idle", type: "session.status_idle" });

    expect(mergeCommittedEvents([first], [replay, status])).toEqual([replay, status]);
  });

  /**
   * Cause-effect rules:
   * R3: C3 message start -> E3 open one empty message preview.
   * R4: C4 matching/foreign text delta -> E4 append/ignore.
   * R5: C5 matching committed event or terminal fact -> E5 clear the volatile preview.
   * R6: C6 thinking start -> E6 expose thinking without inventing text.
   * Constraint: no preview value enters the committed collection.
   */
  it("overlays only the active best-effort message and clears it at commit", () => {
    const opened = reduceLivePreview(EMPTY_LIVE_PREVIEW, stream({
      type: "event_start",
      event: { id: "message", type: "agent.message" },
    }));
    const foreign = reduceLivePreview(opened, stream({
      type: "event_delta",
      event_id: "other",
      delta: { content: { type: "text", text: "ignore" } },
    }));
    const appended = reduceLivePreview(foreign, stream({
      type: "event_delta",
      event_id: "message",
      delta: { content: { type: "text", text: "hello" } },
    }));

    expect(appended).toEqual({ eventId: "message", text: "hello", thinking: false });
    expect(reduceLivePreview(appended, stream({
      id: "message",
      type: "agent.message",
      content: [{ type: "text", text: "hello" }],
    }))).toEqual(EMPTY_LIVE_PREVIEW);
    expect(reduceLivePreview(EMPTY_LIVE_PREVIEW, stream({
      type: "event_start",
      event: { id: "thinking", type: "agent.thinking" },
    }))).toEqual({ text: "", thinking: true });
  });

  /**
   * Coverage rationale: the wire union has two volatile event families and committed events carry IDs.
   * The guard must reject both volatile families and admit only an ID-bearing committed fact.
   */
  it("separates volatile stream envelopes from committed facts", () => {
    expect(isCommittedStreamEvent(stream({ type: "event_start", event: {} }))).toBe(false);
    expect(isCommittedStreamEvent(stream({ type: "event_delta", event_id: "message", delta: {} }))).toBe(false);
    expect(isCommittedStreamEvent(stream({ id: "idle", type: "session.status_idle" }))).toBe(true);
  });
});
