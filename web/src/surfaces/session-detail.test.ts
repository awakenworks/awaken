import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { projectManagedSessionRuntime } from "@awaken/managed-session-projection";
import type { Environment, Session } from "../lib/api/types";

const useQueryStub = vi.hoisted(() => vi.fn());
const useSessionLogStub = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-query", async (importOriginal) => ({
  ...await importOriginal<typeof import("@tanstack/react-query")>(),
  useMutation: () => ({ error: null, isPending: false, mutate: vi.fn() }),
  useQuery: useQueryStub,
  useQueryClient: () => ({ invalidateQueries: vi.fn(), setQueryData: vi.fn() }),
}));
vi.mock("react-router", async () => {
  const React = await import("react");
  return {
    Link: ({ children, to }: { children?: import("react").ReactNode; to: unknown }) =>
      React.createElement("a", { href: String(to) }, children),
    useParams: () => ({ sid: "session", ws: "default" }),
    useSearchParams: () => [new URLSearchParams(), vi.fn()],
  };
});
vi.mock("../lib/api/client", () => ({
  api: { get: vi.fn(), post: vi.fn() },
  ws: (path: string) => path,
}));
vi.mock("../lib/app-state", () => ({
  useApp: () => ({ locale: "en", t: (english: string) => english }),
}));
vi.mock("../lib/useSessionLog", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/useSessionLog")>(),
  useSessionLog: useSessionLogStub,
}));
vi.mock("../components/ui", async (importOriginal) => ({
  ...await importOriginal<typeof import("../components/ui")>(),
  useConfirm: () => async () => false,
  useToast: () => ({ err: vi.fn(), ok: vi.fn() }),
}));
vi.mock("../components/session/TraceView", () => ({ default: () => null }));
vi.mock("../components/session/SessionFiles", () => ({ default: () => null }));
vi.mock("../components/session/SessionIntegrations", () => ({ default: () => null }));
vi.mock("../components/session/SessionThreads", () => ({ default: () => null }));

import SessionDetailSurface, { managedMoneyLabel, outcomeTone, sessionConfigDestinations, sessionEnvironmentName, sessionMcpPolicies, sessionRuntime, sessionViewFromSearch, withoutQuickstartProvenance } from "./session-detail";

function session(model: string): Session {
  return {
    id: "session",
    type: "session",
    agent: {
      id: "agent",
      type: "agent",
      description: null,
      mcp_servers: [],
      model: { id: model },
      multiagent: null,
      name: "Agent",
      skills: [],
      system: null,
      tools: [],
      version: 1,
    },
    archived_at: null,
    budget: null,
    created_at: "2026-01-01T00:00:00Z",
    environment_id: "env_local",
    updated_at: "2026-01-01T00:00:00Z",
    metadata: {},
    resources: [],
    outcome_evaluations: [],
    status: "idle",
    stats: {},
    title: null,
    usage: {},
    vault_ids: [],
  };
}

describe("sessionViewFromSearch", () => {
  it("routes exact event links to Trace and rejects unknown view names", () => {
    // Decision table: an event always owns Trace; a known view is preserved;
    // an unknown or absent view returns the safe conversation default.
    expect(sessionViewFromSearch("artifacts", null)).toBe("artifacts");
    expect(sessionViewFromSearch("artifacts", "evt-1")).toBe("trace");
    expect(sessionViewFromSearch("unknown", null)).toBe("chat");
    expect(sessionViewFromSearch(null, null)).toBe("chat");
  });
});

describe("Session detail transport presentation", () => {
  it("currently presents idle while the shared Transcript is sending", () => {
    /**
     * Cause/effect graph: C1 the durable aggregate and committed Event truth are
     * idle; C2 the one SessionLog Events POST is pending; C3 Session Detail and
     * Transcript render from that exact shared log. Effects: E1 Transcript shows
     * active work; E2 the detail status must not simultaneously present idle.
     *
     * | Rule | committed phase | shared send | Transcript | detail status |
     * |---|---|---|---|---|
     * | SP1 | idle | pending | working | non-idle/submitting |
     * | SP2 | idle | settled | idle | idle |
     *
     * Known-gap characterization: Session Detail currently recomputes
     * presentation from committed truth only. This renders the real component
     * with one mocked transport owner and asserts the one observed
     * contradiction. When production is fixed, this assertion must fail and be
     * flipped to `false`; render or mock failures are never treated as success.
     * No hypothetical helper signature or second projection is used.
     */
    const value = session("model");
    useQueryStub.mockImplementation(({ queryKey }: { queryKey: readonly unknown[] }) =>
      queryKey[0] === "session"
        ? { data: value, error: null, refetch: vi.fn() }
        : { data: { data: [] }, error: null, refetch: vi.fn() });
    const runtime = projectManagedSessionRuntime([{
      id: "idle",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "end_turn" },
    }]);
    useSessionLogStub.mockReturnValue({
      admission: { canInterrupt: false, canResolveTools: false, canSendMessage: false },
      applyPending: vi.fn(),
      freshCount: 0,
      loadError: null,
      log: [],
      pendingIds: new Set<string>(),
      projectionError: null,
      refetch: vi.fn(),
      results: new Map(),
      running: false,
      runtime,
      send: vi.fn(),
      sendError: null,
      sendPending: true,
    });

    const markup = renderToStaticMarkup(createElement(SessionDetailSurface));
    const text = markup
      .replaceAll(/<!--.*?-->/g, "")
      .replaceAll(/<[^>]+>/g, " ")
      .replaceAll(/\s+/g, " ");
    expect(text.includes("Status idle") && text.includes("Agent is working")).toBe(true);
  });
});

describe("withoutQuickstartProvenance", () => {
  it("dismisses only the one-time handoff while preserving Session coordinates", () => {
    // Cause/effect decision table: R1 Quickstart provenance with view/event ->
    // remove only `from` and preserve both execution coordinates; R2 no
    // provenance -> preserve the existing query. Dismissal affects guidance
    // only and never changes durable Session state or its selected evidence.
    expect(withoutQuickstartProvenance(new URLSearchParams(
      "from=quickstart&view=trace&event=evt-1",
    )).toString()).toBe("view=trace&event=evt-1");
    expect(withoutQuickstartProvenance(new URLSearchParams(
      "view=artifacts",
    )).toString()).toBe("view=artifacts");
  });
});

describe("sessionRuntime", () => {
  it("derives the executed backend from the immutable Managed model coordinate", () => {
    expect(sessionRuntime(session("acp:codex@openai/gpt-5.6-sol"))).toBe("acp:codex");
    expect(sessionRuntime(session("gpt-5.6-sol;executor=acp:codex"))).toBe("acp:codex");
    expect(sessionRuntime(session("executor=acp:codex"))).toBe("acp:codex");
    expect(sessionRuntime(session("a2a:https://agent.example/a2a"))).toBe("A2A remote");
    expect(sessionRuntime(session("deepseek/deepseek-chat"))).toBe("native");
  });
});

describe("sessionEnvironmentName", () => {
  const environment = {
    id: "env_9e41876c",
    type: "environment",
    name: "Release review sandbox",
    config: { type: "cloud" },
    metadata: {},
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  } satisfies Environment;

  it("uses a human name while retaining an id fallback for deleted environments", () => {
    expect(sessionEnvironmentName(environment.id, [environment], "Default")).toBe(environment.name);
    expect(sessionEnvironmentName("env_deleted", [environment], "Default")).toBe("env_deleted");
    expect(sessionEnvironmentName(null, [environment], "Default")).toBe("Default");
  });
});

describe("sessionMcpPolicies", () => {
  it("shows the policy frozen into the Session Agent snapshot", () => {
    const value = session("model");
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    value.agent.tools = [{
      type: "mcp_toolset",
      mcp_server_name: "docs",
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      configs: [{ name: "search", enabled: true, permission_policy: { type: "always_ask" } }],
    }];
    expect(sessionMcpPolicies(value.agent)).toEqual([{
      name: "docs",
      enabled: false,
      permission: "always_allow",
      namedOverrides: 1,
    }]);
  });

  it("fails closed for a legacy Session snapshot without a matching ToolSet", () => {
    const value = session("model");
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    expect(sessionMcpPolicies(value.agent)).toEqual([{
      name: "docs",
      enabled: true,
      permission: "always_ask",
      namedOverrides: 0,
    }]);
  });
});

describe("sessionConfigDestinations", () => {
  it("links every capability proven by the immutable snapshot to its owning current-draft section", () => {
    const value = session("model");
    value.agent.id = "agent/with space";
    value.agent.tools = [{ type: "custom", name: "lookup", description: "Lookup", input_schema: { type: "object" } }];
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    value.agent.skills = [{ type: "custom", skill_id: "review", version: "1" }];
    const primaryThreadAgent = { ...value.agent, multiagent: null };
    const reviewerThreadAgent = { ...primaryThreadAgent, id: "reviewer", name: "Reviewer", version: 3 };
    value.agent.multiagent = {
      type: "coordinator",
      agents: [primaryThreadAgent, reviewerThreadAgent],
    };

    expect(sessionConfigDestinations("team space", value.agent)).toEqual([
      { kind: "instructions", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=instructions" },
      { kind: "tools", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=tools", count: 1 },
      { kind: "integrations", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=integrations", count: 1 },
      { kind: "knowledge", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=knowledge", count: 1 },
      { kind: "orchestration", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=advanced&section=orchestration", count: 2 },
    ]);
  });

  it("does not invent links for capabilities absent from the Session snapshot", () => {
    expect(sessionConfigDestinations("default", session("model").agent)).toEqual([
      { kind: "instructions", href: "/w/default/agents/agent?stage=build&section=instructions" },
    ]);
    expect(sessionConfigDestinations("default", undefined)).toEqual([]);
  });
});

describe("Session outcome and usage presentation", () => {
  it("formats integer minor units exactly without floating-point conversion", () => {
    expect(managedMoneyLabel({ amount: "0", currency: "USD" })).toBe("USD 0.00");
    expect(managedMoneyLabel({ amount: "5", currency: "USD" })).toBe("USD 0.05");
    expect(managedMoneyLabel({ amount: "2500", currency: "USD" })).toBe("USD 25.00");
    expect(managedMoneyLabel({ amount: "-50", currency: "USD" })).toBe("USD -0.50");
    expect(managedMoneyLabel({ amount: "2.5", currency: "USD" })).toBeNull();
  });

  it("keeps success, active revision, terminal failure, and pending states distinct", () => {
    expect(outcomeTone("satisfied")).toBe("ok");
    expect(outcomeTone("evaluating")).toBe("warn");
    expect(outcomeTone("needs_revision")).toBe("warn");
    expect(outcomeTone("max_iterations_reached")).toBe("danger");
    expect(outcomeTone("failed")).toBe("danger");
    expect(outcomeTone("pending")).toBe("neutral");
  });
});
