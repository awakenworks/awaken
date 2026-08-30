import { describe, expect, it } from "vitest";
import { parseEventTypes, webhookHealth } from "./webhooks";

describe("webhook authoring and delivery presentation", () => {
  it("keeps all-events empty while trimming and deduplicating exact filters", () => {
    expect(parseEventTypes("  ")).toEqual([]);
    expect(parseEventTypes("session.status_idled, session.deleted\nsession.status_idled"))
      .toEqual(["session.status_idled", "session.deleted"]);
  });

  /** Cause/effect matrix: authored disable and durable delivery failures are
   * independent causes; all four combinations must remain distinguishable so
   * an operator never mistakes an automatic safety stop for a manual pause. */
  it("distinguishes authored pause from delivery degradation and auto-disable", () => {
    expect(webhookHealth({ disabled: false, consecutive_failures: 0 })).toBe("active");
    expect(webhookHealth({ disabled: false, consecutive_failures: 2 })).toBe("degraded");
    expect(webhookHealth({ disabled: true, consecutive_failures: 0 })).toBe("paused");
    expect(webhookHealth({ disabled: true, consecutive_failures: 20 })).toBe("delivery_failed");
  });
});
