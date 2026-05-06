// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
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
    plugin_ids: [],
    delegates: [],
    updated_at: 1_700_000_000,
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
) {
  return vi.fn(async (url: string | URL | Request) => {
    const href =
      typeof url === "string" ? url : url instanceof URL ? url.href : (url as Request).url;

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
    if (href.includes("/runtime-stats")) {
      return { ok: false, status: 503, text: async () => "" };
    }

    // List agents meta
    if (href.includes("/v1/config/agents/meta")) {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(metaItems),
      };
    }

    // List agents
    if (href.includes("/v1/config/agents")) {
      return {
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({ namespace: "agents", items: agents, offset: 0, limit: 100 }),
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
});
