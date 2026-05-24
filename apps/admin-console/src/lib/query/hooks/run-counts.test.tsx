// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";

import { ConfigApiError, runsApi } from "../../api";
import { createQueryClientWrapper } from "../../../test/query";
import { useRunCountsQuery } from "./run-counts";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("useRunCountsQuery", () => {
  it("returns the running/waiting/created counters from /v1/runs/summary", async () => {
    const summarySpy = vi
      .spyOn(runsApi, "summary")
      .mockResolvedValue({ running: 3, waiting: 2, created: 5 });

    const { result } = renderHook(() => useRunCountsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual({
      kind: "ok",
      counts: { running: 3, waiting: 2, created: 5 },
    });
    expect(summarySpy).toHaveBeenCalledTimes(1);
  });

  it("reports `route_absent` (404) — server doesn't expose /v1/runs/summary", async () => {
    vi.spyOn(runsApi, "summary").mockRejectedValue(new ConfigApiError(404, "not found"));

    const { result } = renderHook(() => useRunCountsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual({ kind: "route_absent" });
  });

  it("reports `store_unavailable` (503) — run store unwired / unhealthy", async () => {
    vi.spyOn(runsApi, "summary").mockRejectedValue(new ConfigApiError(503, "disabled"));

    const { result } = renderHook(() => useRunCountsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual({ kind: "store_unavailable" });
  });

  it("surfaces auth and unexpected errors via the query error state", async () => {
    const failure = new ConfigApiError(401, "unauthorized");
    vi.spyOn(runsApi, "summary").mockRejectedValue(failure);

    const { result } = renderHook(() => useRunCountsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isError).toBe(true), { timeout: 3_000 });
    expect(result.current.error).toBe(failure);
  });
});
