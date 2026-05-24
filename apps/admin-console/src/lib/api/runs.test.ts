import { afterEach, describe, expect, it, vi } from "vitest";
import { BACKEND_URL } from "./http";
import { runsApi } from "./runs";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("runsApi", () => {
  it("issues a status-filtered list request and returns the page envelope", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ items: [], total: 3, has_more: false }));
    vi.stubGlobal("fetch", fetchSpy);

    const page = await runsApi.list({ status: "running", limit: 1 });

    expect(page.total).toBe(3);
    expect(page.has_more).toBe(false);
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const url = fetchSpy.mock.calls[0][0] as string;
    expect(url).toBe(`${BACKEND_URL}/v1/runs?status=running&limit=1`);
  });

  it("omits the query string when no params are supplied", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ items: [], total: 0, has_more: false }));
    vi.stubGlobal("fetch", fetchSpy);

    await runsApi.list();

    const url = fetchSpy.mock.calls[0][0] as string;
    expect(url).toBe(`${BACKEND_URL}/v1/runs`);
  });

  it("summary issues a single request to /v1/runs/summary and returns the counters", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ running: 4, waiting: 1, created: 2 }));
    vi.stubGlobal("fetch", fetchSpy);

    const summary = await runsApi.summary();

    expect(summary).toEqual({ running: 4, waiting: 1, created: 2 });
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    expect(fetchSpy.mock.calls[0][0]).toBe(`${BACKEND_URL}/v1/runs/summary`);
  });
});
