import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ADMIN_TOKEN_STORAGE_KEY,
  BACKEND_URL,
  ConfigApiError,
  configApi,
} from "./config-api";

describe("configUrl encoding", () => {
  it("encodes config ids via the list endpoint", async () => {
    // Verify id encoding by intercepting fetch and inspecting the URL
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({}),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.get("agents", "alpha/beta");

    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/config/agents/alpha%2Fbeta`,
      undefined,
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
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

  it("adds bearer auth from local storage when configured", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ namespace: "agents", items: [] }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    vi.stubGlobal("localStorage", {
      getItem: (key: string) =>
        key === ADMIN_TOKEN_STORAGE_KEY ? "stored-token" : null,
    });

    await configApi.list("agents");

    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(new Headers(init.headers).get("authorization")).toBe(
      "Bearer stored-token",
    );
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
