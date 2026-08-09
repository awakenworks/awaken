import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { API_BETAS, api, betaForPath, getToken, workspaceIdForRequest } from "./client";

describe("cloud product session bearer", () => {
  // Cause/effect decision table:
  // cloud session bearer -> Authorization header; no bearer -> cookie-only.
  // A localStorage management token is never consulted.
  beforeAll(() => {
    const storage = () => {
      const values = new Map<string, string>();
      return {
        clear: () => values.clear(),
        getItem: (key: string) => values.get(key) ?? null,
        removeItem: (key: string) => values.delete(key),
        setItem: (key: string, value: string) => values.set(key, value),
      };
    };
    Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: storage() });
    Object.defineProperty(globalThis, "localStorage", { configurable: true, value: storage() });
  });

  beforeEach(() => {
    globalThis.sessionStorage.clear();
    globalThis.localStorage.clear();
  });

  it("uses the short-lived IAM product token", () => {
    globalThis.sessionStorage.setItem("awaken.product.session-bearer", "cloud-token");
    expect(getToken()).toBe("cloud-token");
  });

  it("does not read a management bearer from local storage", () => {
    globalThis.localStorage.setItem("awaken.console.token", "legacy-token");
    expect(getToken()).toBe("");
  });

  it("treats unavailable browser storage as an absent credential", () => {
    const original = globalThis.sessionStorage;
    Object.defineProperty(globalThis, "sessionStorage", {
      configurable: true,
      get: () => { throw new DOMException("blocked", "SecurityError"); },
    });
    expect(getToken()).toBe("");
    Object.defineProperty(globalThis, "sessionStorage", {
      configurable: true,
      value: original,
    });
  });
});

describe("API beta cause/effect graph", () => {
  // Cause graph:
  //   API family ─┬─ memory ────────> memory beta only
  //               ├─ skills ────────> skills beta only
  //               ├─ managed ───────> managed beta
  //               ├─ dreams ────────> managed + dreaming betas
  //               └─ admin/other ───> no beta
  //   workspace prefix ─────────────> does not change the selected family
  //   bearer / body / multipart ────> compose with, never replace, the beta
  const cases: Array<[string, string | undefined]> = [
    ["/v1/memory_stores", API_BETAS.memory],
    ["/v1/memory_stores/store-1/memories?view=full", API_BETAS.memory],
    ["/v1/workspaces/team%20a/memory_stores/store-1/config", API_BETAS.memory],
    ["/v1/skills", API_BETAS.skills],
    ["/v1/workspaces/team-a/skills/skill-1/versions/latest", API_BETAS.skills],
    ["/v1/sessions", API_BETAS.managed],
    ["/v1/workspaces/team-a/environments/env-1", API_BETAS.managed],
    ["/v1/deployments/run-1", API_BETAS.managed],
    ["/v1/dreams", `${API_BETAS.managed},${API_BETAS.dreaming}`],
    ["/v1/config/agents/agent-1", undefined],
    ["/v1/files", undefined],
  ];

  it.each(cases)("maps %s to its exact beta", (path, expected) => {
    expect(betaForPath(path)).toBe(expected);
  });

  afterEach(() => vi.unstubAllGlobals());

  it("sends the Memory beta with bearer and JSON content headers", async () => {
    globalThis.sessionStorage.setItem("awaken.product.session-bearer", "cloud-token");
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ data: [] }), {
      status: 200,
      headers: { "content-type": "application/json" },
    }));
    vi.stubGlobal("fetch", fetch);

    await api.post("/v1/memory_stores", { name: "project" });

    expect(fetch).toHaveBeenCalledOnce();
    const init = fetch.mock.calls[0][1] as RequestInit;
    expect(init.headers).toMatchObject({
      "anthropic-beta": API_BETAS.memory,
      authorization: "Bearer cloud-token",
      "content-type": "application/json",
    });
  });

  it("adds the Skills beta to multipart and byte transports without forcing content-type", async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ id: "skill-1" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }))
      .mockResolvedValueOnce(new Response(new Uint8Array([1, 2, 3]), { status: 200 }));
    vi.stubGlobal("fetch", fetch);

    await api.uploadMany("/v1/skills", [{ path: "SKILL.md", blob: new Blob(["# Skill"]) }]);
    await api.bytes("/v1/skills/skill-1/versions/latest/files/SKILL.md");

    for (const call of fetch.mock.calls) {
      const headers = call[1]?.headers as Record<string, string>;
      expect(headers["anthropic-beta"]).toBe(API_BETAS.skills);
      expect(headers["content-type"]).toBeUndefined();
    }
  });
});

describe("workspace identity cause/effect graph", () => {
  it("never sends the presentation-only default label as an IAM target", () => {
    // Route scope > authenticated context > real named display fallback.
    expect(workspaceIdForRequest("default", "team-path", "team-session")).toBe("team-path");
    expect(workspaceIdForRequest("default", "", "workspace_local_1")).toBe("workspace_local_1");
    expect(workspaceIdForRequest("team-display", "", "")).toBe("team-display");
    expect(workspaceIdForRequest("default", "", "")).toBeUndefined();
  });
});
