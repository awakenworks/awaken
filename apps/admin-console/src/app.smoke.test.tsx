// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Suspense } from "react";
import { RouterProvider, createMemoryRouter, createRoutesFromElements } from "react-router";
import { appRoutes } from "./app";
import { AuthProvider } from "./components/auth-provider";
import { ConfirmDialogProvider } from "./components/confirm-dialog";
import { ToastProvider } from "./components/toast-provider";
import { ADMIN_TOKEN_STORAGE_KEY } from "./lib/config-api";
import { __resetAuthInterceptorForTesting } from "./lib/auth-interceptor";
import { withQueryClient } from "./test/query";

// AuthProvider probes /v1/capabilities on mount. The smoke tests are
// purely about rendering the route tree; stub fetch so the probe
// resolves with an empty capability set instead of going to a real
// network. Each test installs its own mock so we don't leak state.

function stubCapabilitiesFetch() {
  const fetchSpy = vi.fn(async () => ({
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
  }));
  vi.stubGlobal("fetch", fetchSpy);
  return fetchSpy;
}

function renderRoute(path: string) {
  const memRouter = createMemoryRouter(createRoutesFromElements(appRoutes()), {
    initialEntries: [path],
  });
  return render(
    withQueryClient(
      <ToastProvider>
        <ConfirmDialogProvider>
          <AuthProvider>
            <RouterProvider router={memRouter} />
          </AuthProvider>
        </ConfirmDialogProvider>
      </ToastProvider>,
    ),
  );
}

beforeEach(() => {
  // The token modal listens to localStorage; jsdom provides a sane impl
  // but we wipe it between tests to keep them deterministic.
  globalThis.localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
  stubCapabilitiesFetch();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("router smoke", () => {
  it("renders the dashboard route without throwing", async () => {
    renderRoute("/");
    // The lazy AdminLayout always shows "Admin Console" once mounted.
    // findBy* waits for the suspense fallback to resolve.
    expect(await screen.findByText("Admin Console", undefined, { timeout: 5_000 })).toBeDefined();
  });

  it("renders the agent editor for /agents/new (regression: useBlocker invariant)", async () => {
    renderRoute("/agents/new");
    // The Basics tab renders an "Agent ID" labelled input — the same
    // selector that broke when the AgentEditorPage crashed because the
    // legacy BrowserRouter could not satisfy useBlocker.
    expect(await screen.findByLabelText("Agent ID", undefined, { timeout: 5_000 })).toBeDefined();
    expect(await screen.findByRole("heading", { name: /New Agent/ })).toBeDefined();
  });

  it("renders a skill detail route from cached capabilities and config queries", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string | URL | Request) => {
        const href = typeof url === "string" ? url : url instanceof URL ? url.href : url.url;
        if (href.includes("/v1/capabilities")) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({
                agents: [],
                tools: [],
                plugins: [],
                skills: [
                  {
                    id: "writer",
                    name: "Writer",
                    description: "Drafts user-facing text.",
                    allowed_tools: [],
                    arguments: [],
                    user_invocable: true,
                    model_invocable: false,
                    context: "inline",
                    paths: [],
                  },
                ],
                models: [],
                providers: [],
                namespaces: [],
              }),
          };
        }
        if (href.includes("/v1/config/agents")) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({
                namespace: "agents",
                items: [
                  {
                    id: "agent-a",
                    model_id: "bootstrap",
                    system_prompt: "test",
                    sections: { skills: { allowlist: ["writer"] } },
                  },
                ],
                offset: 0,
                limit: 100,
              }),
          };
        }
        return {
          ok: true,
          status: 200,
          text: async () =>
            JSON.stringify({ namespace: "empty", items: [], offset: 0, limit: 100 }),
        };
      }),
    );

    renderRoute("/skills/writer");

    expect(await screen.findByRole("heading", { level: 1, name: "Writer" })).toBeDefined();
    expect(await screen.findByText("agent-a")).toBeDefined();
  });

  it("renders an agent runtime dashboard from the stats query", async () => {
    const snapshot = {
      agent_id: "alpha",
      window_seconds: 3600,
      bucket_window_seconds: 60,
      bucket_count: 60,
      inference_count: 3,
      error_count: 0,
      input_tokens: 120,
      output_tokens: 80,
      avg_inference_duration_ms: 42,
      min_inference_duration_ms: 30,
      max_inference_duration_ms: 60,
      p50_inference_duration_ms: 40,
      p95_inference_duration_ms: 58,
      p99_inference_duration_ms: 60,
      inference_duration_histogram: [],
      suspensions: 0,
      handoffs: 0,
      delegations: 0,
      tool_calls_by_tool: [],
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string | URL | Request) => {
        const href = typeof url === "string" ? url : url instanceof URL ? url.href : url.url;
        if (href.includes("/runtime-stats")) {
          return {
            ok: true,
            status: 200,
            json: async () => snapshot,
            text: async () => JSON.stringify(snapshot),
          };
        }
        if (href.includes("/v1/config/")) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({ namespace: "empty", items: [], offset: 0, limit: 100 }),
          };
        }
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
      }),
    );

    renderRoute("/agents/alpha/dashboard");

    expect(
      await screen.findByRole("heading", { level: 1, name: "Dashboard · alpha" }),
    ).toBeDefined();
    expect(await screen.findByText(/Rolling-window snapshot/)).toBeDefined();
  });

  it.each([
    { path: "/agents", header: "Agents" },
    { path: "/skills", header: "Skill Registry" },
    { path: "/models", header: "Models" },
    { path: "/providers", header: "Providers" },
    { path: "/mcp-servers", header: "MCP Servers" },
    { path: "/eval-reports", header: "Eval Reports" },
  ])("renders $path without crashing", async ({ path, header }) => {
    renderRoute(path);
    expect(await screen.findByRole("heading", { level: 1, name: header })).toBeDefined();
  });
});

describe("agents-page URL state", () => {
  it("reads search, sort direction, and page size from the URL on initial render", async () => {
    // Override the fetch stub to return proper list responses for config endpoints,
    // so agents-page's configApi.list() gets { items: [] } rather than crashing.
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => ({
        ok: true,
        status: 200,
        text: async () => {
          if (String(url).includes("/v1/config/")) {
            return JSON.stringify({ items: [], total: 0 });
          }
          return JSON.stringify({
            agents: [],
            tools: [],
            plugins: [],
            skills: [],
            models: [],
            providers: [],
            namespaces: [],
          });
        },
      })),
    );

    renderRoute("/agents?q=foo&sort=model_id&dir=desc&size=50&page=2");
    // Wait for the page to mount.
    expect(await screen.findByRole("heading", { level: 1, name: "Agents" })).toBeDefined();
    // The search bar input should reflect the URL param (aria-label from sr-only span).
    const searchInput = screen.getByRole("searchbox");
    expect((searchInput as HTMLInputElement).value).toBe("foo");
    // The page-size selector should show 50.
    const sizeSelect = await screen.findByDisplayValue("50");
    expect(sizeSelect).toBeDefined();
  });
});

describe("provider wiring", () => {
  it("AuthProvider exposes the connected backend URL via the layout", async () => {
    renderRoute("/");
    expect(await screen.findByText(/Connected Backend/)).toBeDefined();
  });

  it("Suspense fallback wraps lazy routes (no plain throw on first render)", () => {
    // Wrapping the smoke render in our own Suspense lets us assert the
    // fallback render path is intact (the app uses RouteLoader for this
    // internally, but a paranoia-level assertion here documents intent).
    const memRouter = createMemoryRouter(createRoutesFromElements(appRoutes()), {
      initialEntries: ["/"],
    });
    expect(() =>
      render(
        withQueryClient(
          <Suspense fallback={<div>loading-shell</div>}>
            <ToastProvider>
              <ConfirmDialogProvider>
                <AuthProvider>
                  <RouterProvider router={memRouter} />
                </AuthProvider>
              </ConfirmDialogProvider>
            </ToastProvider>
          </Suspense>,
        ),
      ),
    ).not.toThrow();
  });
});
