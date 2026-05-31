import { afterEach, describe, expect, it, vi } from "vitest";

import { BACKEND_URL } from "./http";
import { capabilitiesApi, capabilitiesFromResult } from "./capabilities";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("capabilitiesApi", () => {
  it("returns ok capabilities and normalizes optional skill arrays", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        agents: ["a"],
        tools: [],
        plugins: [],
        skills: [{ id: "skill-a", name: "Skill A", description: "test" }],
        models: [],
        providers: [],
        namespaces: [],
      }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    const result = await capabilitiesApi.capabilities();

    expect(fetchSpy).toHaveBeenCalledWith(`${BACKEND_URL}/v1/capabilities`, undefined);
    expect(result).toEqual({
      kind: "ok",
      capabilities: {
        agents: ["a"],
        tools: [],
        plugins: [],
        skills: [
          {
            id: "skill-a",
            name: "Skill A",
            description: "test",
            allowed_tools: [],
            arguments: [],
            paths: [],
          },
        ],
        models: [],
        providers: [],
        namespaces: [],
      },
    });
    expect(capabilitiesFromResult(result)?.agents).toEqual(["a"]);
  });

  it("reports route_absent on 404 instead of returning empty capabilities", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse({ error: "not found" }, 404)));

    await expect(capabilitiesApi.capabilities()).resolves.toEqual({ kind: "route_absent" });
  });

  it("reports registry_unavailable on 503 instead of returning empty capabilities", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(jsonResponse({ error: "registry unavailable" }, 503)),
    );

    await expect(capabilitiesApi.capabilities()).resolves.toEqual({
      kind: "registry_unavailable",
      message: "registry unavailable",
    });
  });
});
