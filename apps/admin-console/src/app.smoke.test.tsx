// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Suspense } from "react";
import {
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router";
import { appRoutes } from "./app";
import { AuthProvider } from "./components/auth-provider";
import { ConfirmDialogProvider } from "./components/confirm-dialog";
import { ToastProvider } from "./components/toast-provider";
import { ADMIN_TOKEN_STORAGE_KEY } from "./lib/config-api";
import { __resetAuthInterceptorForTesting } from "./lib/auth-interceptor";

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
  const memRouter = createMemoryRouter(
    createRoutesFromElements(appRoutes()),
    { initialEntries: [path] },
  );
  return render(
    <ToastProvider>
      <ConfirmDialogProvider>
        <AuthProvider>
          <RouterProvider router={memRouter} />
        </AuthProvider>
      </ConfirmDialogProvider>
    </ToastProvider>,
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
    expect(await screen.findByText("Admin Console")).toBeDefined();
  });

  it("renders the agent editor for /agents/new (regression: useBlocker invariant)", async () => {
    renderRoute("/agents/new");
    // The Basics tab renders an "Agent ID" labelled input — the same
    // selector that broke when the AgentEditorPage crashed because the
    // legacy BrowserRouter could not satisfy useBlocker.
    expect(await screen.findByLabelText("Agent ID")).toBeDefined();
    expect(await screen.findByRole("heading", { name: /New Agent/ })).toBeDefined();
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
    expect(
      await screen.findByRole("heading", { level: 2, name: header }),
    ).toBeDefined();
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
    const memRouter = createMemoryRouter(
      createRoutesFromElements(appRoutes()),
      { initialEntries: ["/"] },
    );
    expect(() =>
      render(
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
    ).not.toThrow();
  });
});
