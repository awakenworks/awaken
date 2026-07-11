import { describe, expect, it } from "vitest";
import { ApiClientError } from "./api/client";
import { toGateState } from "./useGate";

const q = <T>(o: Partial<{ isLoading: boolean; isError: boolean; error: unknown; data: T }>) => ({
  isLoading: false,
  isError: false,
  error: null,
  data: undefined as T | undefined,
  ...o,
});

describe("toGateState", () => {
  it("loading while the probe is in flight", () => {
    expect(toGateState(q({ isLoading: true })).status).toBe("loading");
  });
  it("absent when the face 404/405s (a designed-but-unmounted face)", () => {
    for (const status of [404, 405]) {
      const s = toGateState(q({ isError: true, error: new ApiClientError(status, "x", "no") }));
      expect(s.status).toBe("absent");
    }
  });
  it("error for a real failure (not a missing face)", () => {
    const s = toGateState(q({ isError: true, error: new ApiClientError(500, "x", "boom") }));
    expect(s.status).toBe("error");
    if (s.status === "error") expect(s.error.message).toBe("boom");
  });
  it("live carries the payload when the face answers", () => {
    const s = toGateState(q({ data: { ok: 1 } }));
    expect(s.status).toBe("live");
    if (s.status === "live") expect(s.data).toEqual({ ok: 1 });
  });
});
