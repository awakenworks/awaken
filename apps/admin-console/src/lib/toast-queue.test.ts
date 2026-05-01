import { describe, expect, it } from "vitest";
import {
  appendToast,
  createToast,
  DEFAULT_DURATIONS_MS,
  dismissToast,
  expireToasts,
  MAX_VISIBLE_TOASTS,
  nextExpiryDelay,
  type Toast,
} from "./toast-queue";

function makeToast(overrides: Partial<Toast> = {}): Toast {
  return {
    id: "t1",
    tone: "info",
    message: "Hello",
    durationMs: 1000,
    createdAt: 0,
    ...overrides,
  };
}

describe("createToast", () => {
  it("uses the per-tone default duration when none is provided", () => {
    const toast = createToast({ tone: "success", message: "ok" }, "id-1", 100);
    expect(toast.durationMs).toBe(DEFAULT_DURATIONS_MS.success);
    expect(toast.id).toBe("id-1");
    expect(toast.createdAt).toBe(100);
  });

  it("respects an explicit zero duration (sticky toast)", () => {
    const toast = createToast(
      { tone: "error", message: "Boom", durationMs: 0 },
      "id-2",
      100,
    );
    expect(toast.durationMs).toBe(0);
  });

  it("respects an explicit duration override", () => {
    const toast = createToast(
      { tone: "info", message: "x", durationMs: 250 },
      "id-3",
      0,
    );
    expect(toast.durationMs).toBe(250);
  });

  it("rejects negative durations and falls back to the default", () => {
    const toast = createToast(
      { tone: "info", message: "x", durationMs: -5 },
      "id-4",
      0,
    );
    expect(toast.durationMs).toBe(DEFAULT_DURATIONS_MS.info);
  });
});

describe("appendToast", () => {
  it("appends to the tail when below the cap", () => {
    const a = makeToast({ id: "a" });
    const b = makeToast({ id: "b" });
    expect(appendToast([a], b)).toEqual([a, b]);
  });

  it("evicts the oldest toast when the cap is exceeded", () => {
    const queue = Array.from({ length: MAX_VISIBLE_TOASTS }, (_, idx) =>
      makeToast({ id: `t${idx}` }),
    );
    const incoming = makeToast({ id: "new" });
    const next = appendToast(queue, incoming);
    expect(next).toHaveLength(MAX_VISIBLE_TOASTS);
    expect(next[0].id).toBe("t1");
    expect(next[next.length - 1].id).toBe("new");
  });
});

describe("dismissToast", () => {
  it("removes the matching id and is a no-op when missing", () => {
    const queue = [makeToast({ id: "a" }), makeToast({ id: "b" })];
    expect(dismissToast(queue, "a")).toEqual([makeToast({ id: "b" })]);
    expect(dismissToast(queue, "missing")).toEqual(queue);
  });
});

describe("expireToasts", () => {
  it("keeps toasts whose lifetime has not yet elapsed", () => {
    const queue = [
      makeToast({ id: "a", createdAt: 0, durationMs: 1000 }),
      makeToast({ id: "b", createdAt: 0, durationMs: 500 }),
    ];
    expect(expireToasts(queue, 600).map((t) => t.id)).toEqual(["a"]);
  });

  it("preserves sticky toasts (durationMs === 0) regardless of time", () => {
    const queue = [makeToast({ id: "sticky", durationMs: 0 })];
    expect(expireToasts(queue, 1_000_000)).toEqual(queue);
  });
});

describe("nextExpiryDelay", () => {
  it("returns null for an empty queue", () => {
    expect(nextExpiryDelay([], 0)).toBeNull();
  });

  it("returns null when every toast is sticky", () => {
    expect(
      nextExpiryDelay([makeToast({ durationMs: 0 })], 0),
    ).toBeNull();
  });

  it("returns the soonest remaining lifetime", () => {
    const queue = [
      makeToast({ id: "a", createdAt: 0, durationMs: 1000 }),
      makeToast({ id: "b", createdAt: 0, durationMs: 500 }),
    ];
    expect(nextExpiryDelay(queue, 200)).toBe(300);
  });

  it("clamps to zero when a toast is already overdue", () => {
    const queue = [makeToast({ createdAt: 0, durationMs: 100 })];
    expect(nextExpiryDelay(queue, 999)).toBe(0);
  });
});
