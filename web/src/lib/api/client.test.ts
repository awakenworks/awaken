import { afterEach, beforeAll, beforeEach, describe, expect, expectTypeOf, it, vi } from "vitest";
import type {
  BetaManagedAgentsSessionResource,
  BetaManagedAgentsSessionResourcesPageCursor,
  BetaManagedAgentsSessionThread,
  BetaManagedAgentsSessionThreadsPageCursor,
  BetaManagedAgentsSessionThreadUsage,
} from "@awaken/managed-sdk-oracle/current-types";
import {
  API_BETAS,
  ApiClientError,
  IdempotencyScope,
  api,
  betaForPath,
  createManagedSession,
  getToken,
  setResolvedWorkspace,
  workspaceFromPath,
  workspaceIdForRequest,
  ws,
} from "./client";
import type {
  ListSessionResourcesResponse,
  ListSessionThreadsResponse,
  Session,
  SessionResource,
  SessionThread,
  SessionThreadUsage,
} from "./types";

describe("idempotent mutation identity", () => {
  it("reuses an exact payload after timeout and rotates when intent changes", () => {
    // Cause/effect graph: C1 retry payload is exact/changed; C2 no server
    // receipt is available after a transport timeout. Effects: E1 exact retry
    // keeps one key; E2 changed intent gets a new key; E3 known success closes
    // the operation so a later identical user intent gets a new key. The
    // backend remains the receipt/conflict authority.
    // | Rule | Payload/lifecycle | Effect |
    // |---|---|---|
    // | I1 | same, result unknown | same Idempotency-Key |
    // | I2 | changed | new Idempotency-Key |
    // | I3 | same, after complete | new Idempotency-Key |
    const scope = new IdempotencyScope("session-create");
    const first = scope.headersFor({ agent: "a", title: "one" });
    const retry = scope.headersFor({ agent: "a", title: "one" });
    const changed = scope.headersFor({ agent: "a", title: "two" });
    expect(retry).toEqual(first);
    expect(changed["idempotency-key"]).not.toBe(first["idempotency-key"]);
    scope.complete();
    const later = scope.headersFor({ agent: "a", title: "two" });
    expect(later["idempotency-key"]).not.toBe(changed["idempotency-key"]);
  });
});

describe("Managed Session create protocol decision table", () => {
  afterEach(() => vi.unstubAllGlobals());

  /**
   * Cause/effect graph: C1 an official create body and one retry identity enter
   * the canonical client; C2 the standard route is synchronous. Effects: E1
   * POST exactly one SDK-compatible body; E2 add Managed beta/idempotency only;
   * E3 return the Session directly. Rule S1 forbids Prefer and any preparation
   * response shape, eliminating the former parallel async protocol.
   */
  it("uses only the synchronous official create contract", async () => {
    setResolvedWorkspace("");
    vi.stubGlobal("location", { pathname: "/" });
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      id: "session-1",
      type: "session",
      status: "idle",
    }), {
      status: 200,
      headers: { "content-type": "application/json" },
    }));
    vi.stubGlobal("fetch", fetch);
    const request = {
      agent: "agent-1",
      environment_id: "env_local",
      initial_events: [{
        type: "user.message" as const,
        content: [{ type: "text" as const, text: "start atomically" }],
      }],
    };

    const created = await createManagedSession(
      request,
      new IdempotencyScope("session-create"),
    );

    expect(created.id).toBe("session-1");
    expect(fetch).toHaveBeenCalledOnce();
    const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/v1/sessions");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual(request);
    expect(init.headers).toMatchObject({
      "anthropic-beta": API_BETAS.managed,
      "content-type": "application/json",
    });
    expect(init.headers).toHaveProperty("idempotency-key");
    expect(init.headers).not.toHaveProperty("Prefer");
    expect(init.headers).not.toHaveProperty("prefer");
    expectTypeOf<"preparation" extends keyof Session ? true : false>()
      .toEqualTypeOf<false>();
  });
});

describe("Managed Session nested wire type authority", () => {
  /**
   * Cause/effect rule T1: when the current SDK changes a Thread agent variant,
   * required lifecycle/usage field, Resource variant, or PageCursor field, the
   * Web aliases change in the same compile. Effects: advisor and full usage are
   * representable, resource discrimination remains closed, and wire pages keep
   * exactly data/next_page without a handwritten compatibility DTO.
   */
  it("projects Thread, Resource, usage, and cursor types only from the oracle", () => {
    expectTypeOf<SessionThread>().toEqualTypeOf<BetaManagedAgentsSessionThread>();
    expectTypeOf<SessionThreadUsage>().toEqualTypeOf<BetaManagedAgentsSessionThreadUsage>();
    expectTypeOf<SessionResource>().toEqualTypeOf<BetaManagedAgentsSessionResource>();
    expectTypeOf<ListSessionThreadsResponse>().toEqualTypeOf<Pick<
      BetaManagedAgentsSessionThreadsPageCursor,
      "data" | "next_page"
    >>();
    expectTypeOf<ListSessionResourcesResponse>().toEqualTypeOf<Pick<
      BetaManagedAgentsSessionResourcesPageCursor,
      "data" | "next_page"
    >>();
  });
});

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
    setResolvedWorkspace("");
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
    ["/v1/workspaces/team%20a/memory_stores/store-1/memory_versions", API_BETAS.memory],
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
  afterEach(() => vi.unstubAllGlobals());

  it("never sends the presentation-only default label as an IAM target", () => {
    // Route scope > authenticated context > real named display fallback.
    expect(workspaceIdForRequest("default", "team-path", "team-session")).toBe("team-path");
    expect(workspaceIdForRequest("default", "", "workspace_local_1")).toBe("workspace_local_1");
    expect(workspaceIdForRequest("team-display", "", "")).toBe("team-display");
    expect(workspaceIdForRequest("default", "", "")).toBeUndefined();
  });

  it("addresses the first request from the route and ignores stale presentation storage", () => {
    // Decision table: C1 exact /w route, C2 stale local preference, C3 no
    // route. R1(C1+any C2)->encoded route Workspace; R2(!C1+C2)->unscoped
    // until the authenticated server context is resolved. localStorage is a
    // presentation preference and never an IAM target.
    expect(workspaceFromPath("/w/awaken%3Atenant/overview")).toBe("awaken:tenant");
    expect(workspaceFromPath("/w/%E0%A4%A/overview")).toBe("");
    globalThis.localStorage.setItem("awaken.console.workspace", "stale-workspace");
    vi.stubGlobal("location", { pathname: "/w/awaken%3Atenant/overview" });
    expect(ws("/v1/config/workspace-context")).toBe(
      "/v1/workspaces/awaken%3Atenant/config/workspace-context",
    );
    vi.stubGlobal("location", { pathname: "/" });
    expect(ws("/v1/config/workspace-context")).toBe("/v1/config/workspace-context");
  });
});

describe("structured API failure evidence", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("preserves RFC problem code, detail and transport correlation", async () => {
    // Cause/effect rule: C1 non-2xx carries RFC problem fields; C2 ingress adds
    // x-request-id; E1 ApiClientError keeps status/code/detail/requestId so the
    // UI decision table can select an action without string parsing.
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      code: "publication_conflict",
      title: "Publication conflict",
      detail: "refresh revision 7",
    }), {
      status: 409,
      headers: { "content-type": "application/problem+json", "x-request-id": "req-7" },
    })));

    const error = await api.get("/v1/config/agents/a").catch((cause) => cause);
    expect(error).toBeInstanceOf(ApiClientError);
    if (!(error instanceof ApiClientError)) {
      throw new Error("expected structured API client failure");
    }
    expect(error).toMatchObject({
      status: 409,
      code: "publication_conflict",
      requestId: "req-7",
    });
    expect(error.message).toContain("refresh revision 7");
  });
});
