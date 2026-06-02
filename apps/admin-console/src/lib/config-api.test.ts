import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ADMIN_TOKEN_STORAGE_KEY,
  BACKEND_URL,
  configApi,
  deriveSourceState,
  type RecordMeta,
  type RestoreResponse,
} from "./config-api";
import { __resetAuthInterceptorForTesting, setUnauthorizedHandler } from "./auth-interceptor";

describe("restoreConfig", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("encodes namespace and id with special chars in the URL", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ id: "my-agent", version: "v2" }),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.restoreConfig("agents", "alpha/beta", "evt-123");

    const calledUrl: string = fetchSpy.mock.calls[0][0] as string;
    expect(calledUrl).toBe(`${BACKEND_URL}/v1/config/agents/alpha%2Fbeta/restore`);
  });

  it("encodes namespace with special chars", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({}),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.restoreConfig("my/ns", "simple-id", "evt-456");

    const calledUrl: string = fetchSpy.mock.calls[0][0] as string;
    expect(calledUrl).toBe(`${BACKEND_URL}/v1/config/my%2Fns/simple-id/restore`);
  });

  it("sends version in the POST body", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ id: "agent-1" }),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.restoreConfig("agents", "agent-1", "evt-789");

    const init: RequestInit = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe("POST");
    expect(init.body).toBe(JSON.stringify({ version: "evt-789" }));
  });

  it("returns the parsed response on success", async () => {
    const payload = { id: "agent-1", model_id: "gpt-4" };
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () => JSON.stringify(payload),
      }),
    );

    const result: RestoreResponse = await configApi.restoreConfig("agents", "agent-1", "evt-1");
    expect(result).toEqual(payload);
  });

  it("throws ConfigApiError with detail string for 404 with reason", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 404,
        text: async () => JSON.stringify({ error: "not found", reason: "version missing" }),
      }),
    );

    await expect(configApi.restoreConfig("agents", "agent-1", "bad-version")).rejects.toMatchObject(
      {
        name: "ConfigApiError",
        status: 404,
      },
    );
  });

  it("throws ConfigApiError for 422 with resolver message", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 422,
        text: async () => JSON.stringify({ error: "validation failed" }),
      }),
    );

    await expect(configApi.restoreConfig("agents", "agent-1", "evt-1")).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 422,
      message: "validation failed",
    });
  });

  it("throws ConfigApiError for 503 audit log not configured", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 503,
        text: async () => JSON.stringify({ error: "service unavailable" }),
      }),
    );

    await expect(configApi.restoreConfig("agents", "agent-1", "evt-1")).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 503,
    });
  });
});

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
    vi.unstubAllEnvs();
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
      getItem: (key: string) => (key === ADMIN_TOKEN_STORAGE_KEY ? "stored-token" : null),
    });

    await configApi.list("agents");

    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer stored-token");
  });

  it("uses stored bearer token before the dev env token", async () => {
    vi.stubEnv("VITE_ADMIN_BEARER_TOKEN", "stale-env-token");
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ namespace: "agents", items: [] }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => (key === ADMIN_TOKEN_STORAGE_KEY ? "fresh-stored-token" : null),
    });

    await configApi.list("agents");

    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer fresh-stored-token");
  });

  it("ignores VITE_ADMIN_BEARER_TOKEN in production builds", async () => {
    vi.stubEnv("PROD", true);
    vi.stubEnv("VITE_ADMIN_BEARER_TOKEN", "must-not-ship-token");
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ namespace: "agents", items: [] }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    vi.stubGlobal("localStorage", {
      getItem: () => null,
    });

    await configApi.list("agents");

    const init = fetchSpy.mock.calls[0][1] as RequestInit | undefined;
    expect(init?.headers).toBeUndefined();
  });

  it("retries with a fresh token when the unauthorized handler returns one", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce({
        ok: false,
        status: 401,
        text: async () => "Unauthorized",
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: async () => JSON.stringify({ namespace: "agents", items: [] }),
      });
    vi.stubGlobal("fetch", fetchSpy);
    setUnauthorizedHandler(async () => "rotated-token");

    await expect(configApi.list("agents")).resolves.toMatchObject({
      items: [],
    });

    expect(fetchSpy).toHaveBeenCalledTimes(2);
    const retryInit = fetchSpy.mock.calls[1][1] as RequestInit;
    expect(new Headers(retryInit.headers).get("authorization")).toBe("Bearer rotated-token");

    __resetAuthInterceptorForTesting();
  });

  it("propagates 401 when no handler is installed", async () => {
    __resetAuthInterceptorForTesting();
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: false,
      status: 401,
      text: async () => "Unauthorized",
    });
    vi.stubGlobal("fetch", fetchSpy);

    await expect(configApi.list("agents")).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 401,
    });
    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });

  it("propagates 401 when the handler refuses to provide a new token", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: false,
      status: 401,
      text: async () => "Unauthorized",
    });
    vi.stubGlobal("fetch", fetchSpy);
    setUnauthorizedHandler(async () => null);

    await expect(configApi.list("agents")).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 401,
    });
    expect(fetchSpy).toHaveBeenCalledTimes(1);

    __resetAuthInterceptorForTesting();
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
      kind: "ok",
      capabilities: {
        backends: [],
        skills: [
          {
            id: "greeting",
            allowed_tools: [],
            arguments: [],
            paths: [],
          },
        ],
      },
    });
  });

  it("preserves backend config schemas from capabilities", async () => {
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
            skills: [],
            models: [],
            providers: [],
            backends: [
              {
                kind: "a2a",
                version: 1,
                schema: { type: "object", required: ["base_url"] },
                default_config: { base_url: "" },
                ui_schema: { auth: { token: { "ui:widget": "password" } } },
              },
            ],
            namespaces: [],
          }),
      }),
    );

    await expect(configApi.capabilities()).resolves.toMatchObject({
      kind: "ok",
      capabilities: {
        backends: [
          {
            kind: "a2a",
            version: 1,
            schema: { required: ["base_url"] },
            default_config: { base_url: "" },
          },
        ],
      },
    });
  });

  it("reports absent capabilities route", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 404,
        text: async () => JSON.stringify({ error: "not found" }),
        headers: new Headers({ "content-type": "application/json" }),
      }),
    );

    await expect(configApi.capabilities()).resolves.toEqual({ kind: "route_absent" });
  });

  it("reports unavailable capabilities route errors", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 503,
        text: async () => JSON.stringify({ error: "unavailable" }),
        headers: new Headers({ "content-type": "application/json" }),
      }),
    );

    await expect(configApi.capabilities()).resolves.toMatchObject({
      kind: "registry_unavailable",
      message: "unavailable",
    });
  });

});

// ── deriveSourceState ─────────────────────────────────────────────────────────

function makeMeta(overrides?: Partial<RecordMeta>): RecordMeta {
  return {
    source: { kind: "builtin", binary_version: "1.0" },
    hidden: false,
    user_overrides: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  };
}

describe("deriveSourceState", () => {
  it("returns 'builtin' for a builtin record with no overrides", () => {
    const meta = makeMeta({ source: { kind: "builtin", binary_version: "1.0" } });
    expect(deriveSourceState(meta)).toBe("builtin");
  });

  it("returns 'customized' for a builtin record with non-empty user_overrides", () => {
    const meta = makeMeta({
      source: { kind: "builtin", binary_version: "1.0" },
      user_overrides: { system_prompt: "custom" },
    });
    expect(deriveSourceState(meta)).toBe("customized");
  });

  it("returns 'builtin' for a builtin record with empty user_overrides object", () => {
    const meta = makeMeta({
      source: { kind: "builtin", binary_version: "1.0" },
      user_overrides: {},
    });
    expect(deriveSourceState(meta)).toBe("builtin");
  });

  it("returns 'user' for a user-created record", () => {
    const meta = makeMeta({ source: { kind: "user" } });
    expect(deriveSourceState(meta)).toBe("user");
  });

  it("returns 'user' defensively when source is missing (malformed payload)", () => {
    const meta = makeMeta({ source: undefined as unknown as RecordMeta["source"] });
    expect(deriveSourceState(meta)).toBe("user");
  });
});

describe("tool overrides client", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("PATCH posts to /v1/config/tools/:id/overrides", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ id: "echo", description: "patched" }),
    });
    vi.stubGlobal("fetch", fetchMock);
    await configApi.patchToolOverrides("echo", { description: "patched" });
    const url = fetchMock.mock.calls[0]![0] as string;
    expect(url).toMatch(/\/v1\/config\/tools\/echo\/overrides$/);
    expect((fetchMock.mock.calls[0]![1] as RequestInit).method).toBe("PATCH");
  });

  it("DELETE clears all overrides", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ id: "echo", description: "stock" }),
    });
    vi.stubGlobal("fetch", fetchMock);
    await configApi.clearToolOverrides("echo");
    const url = fetchMock.mock.calls[0]![0] as string;
    expect(url).toMatch(/\/v1\/config\/tools\/echo\/overrides$/);
    expect((fetchMock.mock.calls[0]![1] as RequestInit).method).toBe("DELETE");
  });
});

describe("agent overrides client", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("POST validates an agent override patch without persisting", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ ok: true, normalized: { system_prompt: "patched" } }),
    });
    vi.stubGlobal("fetch", fetchMock);
    await configApi.validateAgentOverrides("agent-a", { system_prompt: "patched" });
    const url = fetchMock.mock.calls[0]![0] as string;
    const init = fetchMock.mock.calls[0]![1] as RequestInit;
    expect(url).toMatch(/\/v1\/config\/agents\/agent-a\/overrides$/);
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ system_prompt: "patched" });
  });
});
