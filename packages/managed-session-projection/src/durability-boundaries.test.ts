import { describe, expect, it } from "vitest";
import {
  EMPTY_LIVE_PREVIEW,
  isCommittedStreamEvent,
  mergeCommittedEvents,
  reduceLivePreview,
  type ManagedEvent,
  type ManagedStreamEvent,
} from "./index.js";

describe("Managed preview loss across the commit boundary", () => {
  it("replaces any volatile prefix with one replayable complete message", () => {
    // No preview, a received prefix, and a fully received preview all converge
    // on the same committed fact. Reconnection rebuilds from committed Events;
    // neither preview loss nor replay duplicates alter that fact's content.
    const complete = {
      id: "response",
      type: "agent.message",
      content: [{ type: "text", text: "complete committed response" }],
      processed_at: "2026-11-20T00:00:00Z",
    } as ManagedEvent;
    const start = {
      type: "event_start",
      event: { id: complete.id, type: "agent.message" },
    } as ManagedStreamEvent;
    const chunks = ["uncommitted prefix", " and volatile tail"].map((text) => ({
      type: "event_delta",
      event_id: complete.id,
      delta: { type: "content_delta", content: { type: "text", text } },
    }) as ManagedStreamEvent);

    for (const received of [0, 1, 2]) {
      let preview = reduceLivePreview(EMPTY_LIVE_PREVIEW, start);
      let history: ManagedEvent[] = [];
      for (const frame of [start, ...chunks.slice(0, received)]) {
        preview = reduceLivePreview(preview, frame);
        if (isCommittedStreamEvent(frame)) history = mergeCommittedEvents(history, [frame]);
      }
      expect(history).toEqual([]);
      history = mergeCommittedEvents(history, [complete]);
      preview = reduceLivePreview(preview, complete);
      expect(preview).toEqual(EMPTY_LIVE_PREVIEW);
      expect(reduceLivePreview(preview, chunks[1]!)).toEqual(EMPTY_LIVE_PREVIEW);
      const cold = mergeCommittedEvents<ManagedEvent>([], [complete], "authoritative-history");
      expect(history).toEqual(cold);
      expect(mergeCommittedEvents(history, cold)).toEqual([complete]);
    }
  });
});
