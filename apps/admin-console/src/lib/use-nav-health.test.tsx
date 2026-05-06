// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { createQueryClientWrapper } from "../test/query";
import { useNavHealth } from "./use-nav-health";

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

describe("useNavHealth", () => {
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("does not query while disabled", () => {
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useNavHealth(false), { wrapper });

    expect(result.current).toEqual({
      mcp: { tone: "neutral" },
      providers: { tone: "neutral" },
      agents: { tone: "neutral" },
    });
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("derives chrome health from config namespace counts", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string | URL | Request) => {
        const href = hrefOf(url);
        if (href.includes("/v1/config/mcp-servers")) {
          return jsonResponse({
            namespace: "mcp-servers",
            items: [{ id: "stdio", transport: "stdio", command: "node" }],
            offset: 0,
            limit: 100,
          });
        }
        if (href.includes("/v1/config/providers")) {
          return jsonResponse({
            namespace: "providers",
            items: [{ id: "openai", adapter: "openai", has_api_key: true }],
            offset: 0,
            limit: 100,
          });
        }
        return jsonResponse({
          namespace: "agents",
          items: [{ id: "a" }, { id: "b" }],
          offset: 0,
          limit: 100,
        });
      }),
    );
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useNavHealth(true), { wrapper });

    await waitFor(() => {
      expect(result.current).toEqual({
        mcp: { count: 1, tone: "neutral" },
        providers: { count: 1, tone: "ok" },
        agents: { count: 2, tone: "neutral" },
      });
    });
  });

  it("keeps neutral chrome health when config probes fail", async () => {
    const fetchSpy = vi.fn(async () =>
      jsonResponse({ error: "unavailable" }, { ok: false, status: 503 }),
    );
    vi.stubGlobal("fetch", fetchSpy);
    const wrapper = createQueryClientWrapper();

    const { result } = renderHook(() => useNavHealth(true), { wrapper });

    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledTimes(3);
    });
    expect(result.current).toEqual({
      mcp: { tone: "neutral" },
      providers: { tone: "neutral" },
      agents: { tone: "neutral" },
    });
  });
});
