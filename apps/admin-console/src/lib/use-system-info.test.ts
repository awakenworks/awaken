// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { createQueryClientWrapper } from "../test/query";
import { useSystemInfo } from "./use-system-info";
import { useSystemInfoQuery } from "./query/hooks/system";

const fakeInfo = {
  version: "0.4.1-test",
  scope_id: "default",
  uptime_seconds: 42,
  config_store_enabled: true,
  audit_log_enabled: true,
  runtime_stats_enabled: false,
};

function hrefOf(input: string | URL | Request): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

function jsonResponse(data: unknown, init?: { ok?: boolean; status?: number }) {
  return {
    ok: init?.ok ?? true,
    status: init?.status ?? 200,
    text: async () => JSON.stringify(data),
  };
}

function stubSystemInfoFetch(response = jsonResponse(fakeInfo)) {
  const fetchSpy = vi.fn(async (url: string | URL | Request) => {
    expect(hrefOf(url)).toContain("/v1/system/info");
    return response;
  });
  vi.stubGlobal("fetch", fetchSpy);
  return fetchSpy;
}

describe("useSystemInfo", () => {
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("first caller fetches; result becomes available after the promise settles", async () => {
    stubSystemInfoFetch();
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useSystemInfo(), { wrapper });

    expect(result.current).toBeNull();
    await waitFor(() => {
      expect(result.current).toEqual(fakeInfo);
    });
  });

  it("concurrent callers on the same QueryClient reuse the in-flight request", async () => {
    const fetchSpy = stubSystemInfoFetch();
    const wrapper = createQueryClientWrapper();

    const a = renderHook(() => useSystemInfo(), { wrapper });
    const b = renderHook(() => useSystemInfo(), { wrapper });

    await waitFor(() => {
      expect(a.result.current).toEqual(fakeInfo);
      expect(b.result.current).toEqual(fakeInfo);
    });
    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });

  it("returns null when the optional API responds service unavailable", async () => {
    const fetchSpy = stubSystemInfoFetch(
      jsonResponse({ error: "unavailable" }, { ok: false, status: 503 }),
    );
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useSystemInfo(), { wrapper });

    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledTimes(1);
    });
    expect(result.current).toBeNull();
  });

  it("surfaces non-optional API failures through the query error state", async () => {
    stubSystemInfoFetch(jsonResponse({ error: "forbidden" }, { ok: false, status: 403 }));
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useSystemInfoQuery(), { wrapper });

    await waitFor(() => {
      expect(result.current.isError).toBe(true);
    });
    expect(result.current.error).toBeInstanceOf(Error);
    expect(result.current.error?.message).toBe("forbidden");
  });
});
