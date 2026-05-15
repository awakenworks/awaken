// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { RouterProvider, createMemoryRouter, createRoutesFromElements } from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";
import { withQueryClient } from "../test/query";

function agentItem(id: string, modelId = "bootstrap") {
  return {
    id,
    model_id: modelId,
    system_prompt: "test",
    max_rounds: 8,
    plugin_ids: [] as string[],
    delegates: [] as string[],
    updated_at: 1_700_000_000,
  };
}

function runtimeSnapshot(agentId: string) {
  return {
    agent_id: agentId,
    window_seconds: 86_400,
    bucket_window_seconds: 300,
    bucket_count: 12,
    inference_count: 12,
    error_count: 1,
    input_tokens: 120,
    output_tokens: 80,
    avg_inference_duration_ms: 50,
    min_inference_duration_ms: 10,
    max_inference_duration_ms: 120,
    p50_inference_duration_ms: 40,
    p95_inference_duration_ms: 95,
    p99_inference_duration_ms: 110,
    inference_duration_histogram: [
      { upper_bound_ms: 10, count: 1 },
      { upper_bound_ms: 50, count: 3 },
      { upper_bound_ms: 100, count: 8 },
    ],
    suspensions: 0,
    handoffs: 0,
    delegations: 0,
    tool_calls_by_tool: [],
  };
}

function metaItem(
  id: string,
  sourceKind: "builtin" | "user",
  userOverrides?: Record<string, unknown>,
) {
  return {
    id,
    meta: {
      source:
        sourceKind === "builtin" ? { kind: "builtin", binary_version: "test" } : { kind: "user" },
      hidden: false,
      user_overrides: userOverrides ?? null,
      created_at: 0,
      updated_at: 0,
    },
  };
}

function buildFetchMock(
  agents: ReturnType<typeof agentItem>[],
  metaItems: ReturnType<typeof metaItem>[],
  options?: {
    runtimeStats?: unknown;
    runtimeStatus?: number;
    deleteStatus?: number;
  },
) {
  const deletedIds = new Set<string>();
  return vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const href =
      typeof url === "string" ? url : url instanceof URL ? url.href : (url as Request).url;
    const parsed = new URL(href);
    const method = init?.method?.toUpperCase() ?? "GET";

    if (href.includes("/v1/capabilities")) {
      return {
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
            namespaces: [],
          }),
      };
    }

    // Runtime stats — not enabled in test
    if (parsed.pathname === "/v1/agents/runtime-stats") {
      const status = options?.runtimeStatus ?? 503;
      return {
        ok: status >= 200 && status < 300,
        status,
        text: async () => JSON.stringify(options?.runtimeStats ?? { agents: [] }),
      };
    }

    // List agents meta
    if (method === "GET" && parsed.pathname === "/v1/config/agents/meta") {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(metaItems),
      };
    }

    if (method === "DELETE" && parsed.pathname.startsWith("/v1/config/agents/")) {
      const status = options?.deleteStatus ?? 204;
      if (status >= 200 && status < 300) {
        deletedIds.add(decodeURIComponent(parsed.pathname.split("/").at(-1) ?? ""));
      }
      return {
        ok: status >= 200 && status < 300,
        status,
        text: async () => (status === 204 ? "" : JSON.stringify({ error: "delete failed" })),
      };
    }

    // List agents
    if (method === "GET" && parsed.pathname === "/v1/config/agents") {
      return {
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({
            namespace: "agents",
            items: agents.filter((agent) => !deletedIds.has(agent.id)),
            offset: 0,
            limit: 100,
          }),
      };
    }

    return { ok: false, status: 404, text: async () => "" };
  });
}

function renderAgentsPage() {
  const router = createMemoryRouter(createRoutesFromElements(appRoutes()), {
    initialEntries: ["/agents"],
  });
  render(
    withQueryClient(
      <ToastProvider>
        <ConfirmDialogProvider>
          <AuthProvider>
            <RouterProvider router={router} />
          </AuthProvider>
        </ConfirmDialogProvider>
      </ToastProvider>,
    ),
  );
}

beforeEach(() => {
  // no-op localStorage
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("AgentsPage source badges", () => {
  it("shows Built-in badge for builtin agent", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock([agentItem("builtin-agent")], [metaItem("builtin-agent", "builtin")]),
    );

    renderAgentsPage();

    await waitFor(
      () => {
        expect(screen.getByText("builtin-agent")).toBeDefined();
      },
      { timeout: 5_000 },
    );
    expect(screen.getByText("Built-in")).toBeDefined();
  });

  it("shows Customized badge for builtin agent with overrides", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [agentItem("customized-agent")],
        [metaItem("customized-agent", "builtin", { system_prompt: "custom" })],
      ),
    );

    renderAgentsPage();

    await waitFor(
      () => {
        expect(screen.getByText("customized-agent")).toBeDefined();
      },
      { timeout: 5_000 },
    );
    expect(screen.getByText("Customized")).toBeDefined();
  });

  it("shows User-defined badge for user-created agent", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock([agentItem("user-agent")], [metaItem("user-agent", "user")]),
    );

    renderAgentsPage();

    await waitFor(
      () => {
        expect(screen.getByText("user-agent")).toBeDefined();
      },
      { timeout: 5_000 },
    );
    expect(screen.getByText("User-defined")).toBeDefined();
  });

  it("shows mixed badges when agents have different source states", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [agentItem("a-builtin"), agentItem("a-user"), agentItem("a-custom")],
        [
          metaItem("a-builtin", "builtin"),
          metaItem("a-user", "user"),
          metaItem("a-custom", "builtin", { system_prompt: "x" }),
        ],
      ),
    );

    renderAgentsPage();

    await waitFor(
      () => {
        expect(screen.getByText("a-builtin")).toBeDefined();
      },
      { timeout: 5_000 },
    );
    expect(screen.getByText("Built-in")).toBeDefined();
    expect(screen.getByText("User-defined")).toBeDefined();
    expect(screen.getByText("Customized")).toBeDefined();
  });

  it("renders no badge when meta fetch fails gracefully", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [agentItem("some-agent")],
        // meta list returns empty — simulates endpoint not available
        [],
      ),
    );

    renderAgentsPage();

    await waitFor(
      () => {
        expect(screen.getByText("some-agent")).toBeDefined();
      },
      { timeout: 5_000 },
    );
    // No badge rendered when metaMap has no entry
    expect(screen.queryByText("Built-in")).toBeNull();
    expect(screen.queryByText("User-defined")).toBeNull();
  });

  it("filters by search/model/plugin and deletes only after confirmation", async () => {
    const alpha = { ...agentItem("alpha-agent", "model-a"), plugin_ids: ["plugin-a"] };
    const beta = { ...agentItem("beta-agent", "model-b"), plugin_ids: ["plugin-b"] };
    const fetchMock = buildFetchMock(
      [alpha, beta],
      [metaItem("alpha-agent", "user"), metaItem("beta-agent", "user")],
    );
    vi.stubGlobal("fetch", fetchMock);

    renderAgentsPage();

    await screen.findByText("alpha-agent", undefined, { timeout: 5_000 });
    fireEvent.change(screen.getByPlaceholderText(/search by id/i), {
      target: { value: "beta" },
    });
    expect(screen.getByText("beta-agent")).toBeDefined();
    expect(screen.queryByText("alpha-agent")).toBeNull();

    fireEvent.change(screen.getByPlaceholderText(/search by id/i), {
      target: { value: "" },
    });
    fireEvent.change(screen.getByLabelText("model"), { target: { value: "model-a" } });
    expect(screen.getByText("alpha-agent")).toBeDefined();
    expect(screen.queryByText("beta-agent")).toBeNull();

    fireEvent.change(screen.getByLabelText("model"), { target: { value: "any" } });
    fireEvent.change(screen.getByLabelText("plugin"), { target: { value: "plugin-b" } });
    expect(screen.getByText("beta-agent")).toBeDefined();
    expect(screen.queryByText("alpha-agent")).toBeNull();

    fireEvent.change(screen.getByLabelText("plugin"), { target: { value: "any" } });
    const alphaRow = screen.getByText("alpha-agent").closest("tr");
    expect(alphaRow).not.toBeNull();
    fireEvent.click(within(alphaRow!).getByRole("button", { name: "Delete" }));
    let dialog = await screen.findByRole("dialog", { name: "Delete agent?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(
      fetchMock.mock.calls.filter(([url, init]) => {
        const href =
          typeof url === "string" ? url : url instanceof URL ? url.href : (url as Request).url;
        return init?.method === "DELETE" && new URL(href).pathname === "/v1/config/agents/alpha-agent";
      }),
    ).toHaveLength(0);

    fireEvent.click(within(alphaRow!).getByRole("button", { name: "Delete" }));
    dialog = await screen.findByRole("dialog", { name: "Delete agent?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(screen.queryByText("alpha-agent")).toBeNull());
    expect(screen.getByText('Agent "alpha-agent" deleted')).toBeDefined();
  });

  it("renders runtime stats including errors, p95 latency, unavailable, and missing snapshots", async () => {
    const fetchMock = buildFetchMock(
      [agentItem("with-stats"), agentItem("without-stats")],
      [metaItem("with-stats", "user"), metaItem("without-stats", "user")],
      { runtimeStatus: 200, runtimeStats: { agents: [runtimeSnapshot("with-stats")] } },
    );
    vi.stubGlobal("fetch", fetchMock);

    renderAgentsPage();

    const statsRow = (await screen.findByText("with-stats")).closest("tr");
    expect(statsRow).not.toBeNull();
    expect(within(statsRow!).getByText("12")).toBeDefined();
    expect(within(statsRow!).getByText(/1 err/)).toBeDefined();
    expect(within(statsRow!).getByText("p95 95ms")).toBeDefined();

    const missingRow = screen.getByText("without-stats").closest("tr");
    expect(missingRow).not.toBeNull();
    expect(within(missingRow!).getByText("—")).toBeDefined();

    cleanup();
    vi.stubGlobal(
      "fetch",
      buildFetchMock([agentItem("disabled-stats")], [metaItem("disabled-stats", "user")]),
    );
    renderAgentsPage();
    await screen.findByText("Runtime stats disabled.", undefined, { timeout: 5_000 });
    const disabledRow = screen.getByText("disabled-stats").closest("tr");
    expect(disabledRow).not.toBeNull();
    expect(within(disabledRow!).getByText("n/a")).toBeDefined();
  });
});
