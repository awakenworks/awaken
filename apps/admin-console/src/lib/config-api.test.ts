import { afterEach, describe, expect, it, vi } from "vitest";
import { ConfigApiError, configApi, configUrl } from "./config-api";

describe("configUrl", () => {
  it("encodes config ids", () => {
    expect(configUrl("agents", "alpha/beta")).toContain("alpha%2Fbeta");
  });
});

describe("configApi", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("returns undefined for successful deletes", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 204,
        text: async () => "",
      }),
    );

    await expect(configApi.delete("agents", "demo")).resolves.toBeUndefined();
  });

  it("throws a typed error with server message", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 409,
        text: async () => JSON.stringify({ error: "agents/demo already exists" }),
      }),
    );

    await expect(configApi.get("agents", "demo")).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 409,
      message: "agents/demo already exists",
    });
  });

  it("normalizes omitted skill arrays in capabilities", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({
            agents: [],
            tools: [],
            plugins: [],
            skills: [
              {
                id: "greeting",
                name: "Greeting",
                description: "Friendly opener",
                user_invocable: true,
                model_invocable: true,
                context: "inline",
              },
            ],
            models: [],
            providers: [],
            namespaces: [],
          }),
      }),
    );

    await expect(configApi.capabilities()).resolves.toMatchObject({
      skills: [
        {
          id: "greeting",
          allowed_tools: [],
          arguments: [],
          paths: [],
        },
      ],
    });
  });
});
