import { afterEach, describe, expect, it, vi } from "vitest";
import { BACKEND_URL } from "./http";
import { toolsApi } from "./tools";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("toolsApi", () => {
  it("lists and fetches tools with encoded ids", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ namespace: "tools", items: [] }))
      .mockResolvedValueOnce(
        jsonResponse({ id: "tool/a", name: "Tool A", description: "Search" }),
      );
    vi.stubGlobal("fetch", fetchSpy);

    await expect(toolsApi.listTools()).resolves.toEqual({ namespace: "tools", items: [] });
    await expect(toolsApi.getTool("tool/a")).resolves.toMatchObject({ id: "tool/a" });

    expect(fetchSpy).toHaveBeenNthCalledWith(1, `${BACKEND_URL}/v1/config/tools`, undefined);
    expect(fetchSpy).toHaveBeenNthCalledWith(
      2,
      `${BACKEND_URL}/v1/config/tools/tool%2Fa`,
      undefined,
    );
  });

  it("patches tool overrides with JSON and encoded ids", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({ id: "builtin/search", name: "search", description: "Updated" }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    await toolsApi.patchToolOverrides("builtin/search", { description: "Updated" });

    expect(fetchSpy).toHaveBeenCalledTimes(1);
    expect(fetchSpy.mock.calls[0][0]).toBe(
      `${BACKEND_URL}/v1/config/tools/builtin%2Fsearch/overrides`,
    );
    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe("PATCH");
    expect(new Headers(init.headers).get("content-type")).toBe("application/json");
    expect(init.body).toBe(JSON.stringify({ description: "Updated" }));
  });

  it("clears all overrides or a single override field", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ id: "tool/a" }))
      .mockResolvedValueOnce(jsonResponse({ id: "tool/a" }));
    vi.stubGlobal("fetch", fetchSpy);

    await toolsApi.clearToolOverrides("tool/a");
    await toolsApi.clearToolOverrideField("tool/a", "display/name");

    expect(fetchSpy.mock.calls[0]).toEqual([
      `${BACKEND_URL}/v1/config/tools/tool%2Fa/overrides`,
      { method: "DELETE" },
    ]);
    expect(fetchSpy.mock.calls[1]).toEqual([
      `${BACKEND_URL}/v1/config/tools/tool%2Fa/overrides/display%2Fname`,
      { method: "DELETE" },
    ]);
  });
});
