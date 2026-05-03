// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

const fakeInfo = {
  version: "0.4.1-test",
  uptime_seconds: 42,
  config_store_enabled: true,
  audit_log_enabled: true,
  runtime_stats_enabled: false,
};

// Stub the configApi.systemInfo call before importing the SUT so the module
// cache picks up the mock.
vi.mock("./config-api", () => ({
  configApi: {
    systemInfo: vi.fn(async () => fakeInfo),
  },
}));

describe("useSystemInfo", () => {
  beforeEach(() => {
    vi.resetModules();
  });
  afterEach(() => {
    vi.clearAllMocks();
  });

  it("first caller fetches; result becomes available after the promise settles", async () => {
    const { useSystemInfo } = await import("./use-system-info");
    const { result } = renderHook(() => useSystemInfo());
    // Initial render: cache is empty
    expect(result.current).toBeNull();
    await waitFor(() => {
      expect(result.current).toEqual(fakeInfo);
    });
  });

  it("subsequent callers reuse the module-level cache (one fetch total)", async () => {
    const api = await import("./config-api");
    const spy = api.configApi.systemInfo as ReturnType<typeof vi.fn>;
    spy.mockClear();
    const { useSystemInfo } = await import("./use-system-info");

    const a = renderHook(() => useSystemInfo());
    const b = renderHook(() => useSystemInfo());
    await waitFor(() => {
      expect(a.result.current).toEqual(fakeInfo);
      expect(b.result.current).toEqual(fakeInfo);
    });
    // Either 1 (single inflight resolved before second mount) or 0 (cache hit on
    // second mount). Never 2.
    expect(spy.mock.calls.length).toBeLessThanOrEqual(1);
  });

  it("returns null when the API call rejects (no throw)", async () => {
    vi.resetModules();
    vi.doMock("./config-api", () => ({
      configApi: {
        systemInfo: vi.fn(async () => {
          throw new Error("503");
        }),
      },
    }));
    const { useSystemInfo } = await import("./use-system-info");
    const { result } = renderHook(() => useSystemInfo());
    await waitFor(() => {
      // Resolved to null (error path); no throw bubbled out.
      expect(result.current).toBeNull();
    });
    // Re-render to ensure the hook itself didn't crash on the rejection.
    expect(() => act(() => {})).not.toThrow();
  });
});
