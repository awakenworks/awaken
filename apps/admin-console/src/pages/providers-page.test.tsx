// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, act } from "@testing-library/react";
import { RouterProvider, createMemoryRouter, createRoutesFromElements } from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { ADMIN_TOKEN_STORAGE_KEY } from "../lib/config-api";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";
import { withQueryClient } from "../test/query";

const PROVIDER_RECORD = {
  id: "my-openai",
  adapter: "openai",
  has_api_key: true,
  timeout_secs: 300,
  created_at: 1000,
  updated_at: 2000,
};

function stubFetch(
  overrides?: Record<string, unknown>,
  supportedAdapters = ["openai", "anthropic"],
) {
  const fetchSpy = vi.fn(async (url: string, init?: RequestInit) => {
    const method = init?.method?.toUpperCase() ?? "GET";
    const urlStr = String(url);

    // capabilities
    if (urlStr.includes("/v1/capabilities")) {
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
            supported_adapters: supportedAdapters,
            namespaces: [],
          }),
      };
    }

    // list providers
    if (urlStr.includes("/v1/config/providers") && method === "GET") {
      return {
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({
            namespace: "providers",
            items: [PROVIDER_RECORD],
            offset: 0,
            limit: 100,
          }),
      };
    }

    // test provider
    if (urlStr.includes("/v1/providers/") && urlStr.endsWith("/test") && method === "POST") {
      const defaults = { ok: true, latency_ms: 42, network_tested: false };
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify({ ...defaults, ...overrides }),
      };
    }

    // fallback
    return {
      ok: true,
      status: 200,
      text: async () => JSON.stringify({}),
    };
  });
  vi.stubGlobal("fetch", fetchSpy);
  return fetchSpy;
}

function renderProviders() {
  const memRouter = createMemoryRouter(createRoutesFromElements(appRoutes()), {
    initialEntries: ["/providers"],
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
  globalThis.localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("ProvidersPage — Test connection button", () => {

  it("runs provider tests from table rows and shows a toast", async () => {
    const fetchSpy = stubFetch({ ok: true, latency_ms: 64, network_tested: false });
    renderProviders();

    const rowTestButton = await screen.findByRole("button", { name: /^test$/i }, { timeout: 5_000 });
    await act(async () => {
      fireEvent.click(rowTestButton);
    });

    await screen.findByText(/my-openai config OK · 64ms/i, undefined, { timeout: 5_000 });
    const testCall = fetchSpy.mock.calls.find(([url, init]) => {
      const method = (init as RequestInit | undefined)?.method?.toUpperCase();
      return method === "POST" && String(url).includes("/v1/providers/my-openai/test");
    });
    expect(testCall).toBeDefined();
  });

  it("filters the provider list by search text", async () => {
    stubFetch();
    renderProviders();

    await screen.findByText("my-openai", undefined, { timeout: 5_000 });
    const search = screen.getByPlaceholderText(/search by id/i);

    fireEvent.change(search, { target: { value: "missing" } });
    expect(screen.getByText("No providers match the current filter.")).toBeTruthy();
    expect(screen.queryByText("my-openai")).toBeNull();

    fireEvent.change(search, { target: { value: "openai" } });
    expect(screen.getByText("my-openai")).toBeTruthy();
  });

  it("validates service-account JSON before saving a Vertex provider", async () => {
    const fetchSpy = stubFetch(undefined, ["anthropic", "openai", "vertex"]);
    renderProviders();

    const newButton = await screen.findByRole(
      "button",
      { name: /new provider/i },
      { timeout: 5_000 },
    );
    fireEvent.click(newButton);
    await screen.findByText(/create provider/i, undefined, { timeout: 5_000 });

    fireEvent.change(screen.getByLabelText("Provider ID"), { target: { value: "vertex-main" } });
    fireEvent.change(screen.getByLabelText("Adapter"), { target: { value: "vertex" } });
    fireEvent.change(screen.getByLabelText("Credentials type"), {
      target: { value: "service_account_json" },
    });
    fireEvent.change(screen.getByPlaceholderText(/paste the full json/i), {
      target: { value: "not json" },
    });

    await act(async () => {
      fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);
    });

    expect(
      screen.getAllByRole("alert").some((node) =>
        (node.textContent ?? "").includes("Not valid JSON"),
      ),
    ).toBe(true);
    const createCall = fetchSpy.mock.calls.find(([url, init]) => {
      const method = (init as RequestInit | undefined)?.method?.toUpperCase();
      return method === "POST" && String(url).includes("/v1/config/providers");
    });
    expect(createCall).toBeUndefined();
  });
  it("does not show Test connection button when creating a new provider", async () => {
    stubFetch();
    renderProviders();

    const newButton = await screen.findByRole(
      "button",
      { name: /new provider/i },
      { timeout: 5_000 },
    );
    fireEvent.click(newButton);

    await screen.findByText(/create provider/i, undefined, { timeout: 5_000 });
    const buttons = screen.queryAllByRole("button", { name: /test connection/i });
    expect(buttons).toHaveLength(0);
  });

  it("shows Test connection button when editing an existing provider", async () => {
    stubFetch();
    renderProviders();

    const editButton = await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 });
    fireEvent.click(editButton);

    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });
    expect(screen.getByRole("button", { name: /test connection/i })).toBeDefined();
  });

  it("calls testProvider and shows config-only success status badge on OK result", async () => {
    const fetchSpy = stubFetch({ ok: true, latency_ms: 55 });
    renderProviders();

    const editButton = await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 });
    fireEvent.click(editButton);

    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });

    const testBtn = screen.getByRole("button", { name: /test connection/i });
    await act(async () => {
      fireEvent.click(testBtn);
    });

    // status badge should appear
    await screen.findByText(/Config OK — 55ms/i, undefined, { timeout: 5_000 });

    // fetch was called with the test endpoint
    const testCall = fetchSpy.mock.calls.find(([url]) =>
      String(url).includes("/v1/providers/my-openai/test"),
    );
    expect(testCall).toBeDefined();
  });

  it("shows connection success status when the provider test reached the network", async () => {
    stubFetch({ ok: true, latency_ms: 77, network_tested: true });
    renderProviders();

    const editButton = await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 });
    fireEvent.click(editButton);

    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });

    const testBtn = screen.getByRole("button", { name: /test connection/i });
    await act(async () => {
      fireEvent.click(testBtn);
    });

    await screen.findByText(/Connection OK — 77ms/i, undefined, { timeout: 5_000 });
  });

  it("blocks Save when required fields are empty and shows inline Required errors", async () => {
    const fetchSpy = stubFetch();
    renderProviders();

    const newButton = await screen.findByRole(
      "button",
      { name: /new provider/i },
      { timeout: 5_000 },
    );
    fireEvent.click(newButton);

    await screen.findByText(/create provider/i, undefined, { timeout: 5_000 });

    // Clear the Provider ID field (defaults to empty for new) and click Save.
    const saveBtn = screen.getAllByRole("button", { name: /^save$/i })[0];
    await act(async () => {
      fireEvent.click(saveBtn);
    });

    // "Required" inline error rendered for the empty Provider ID field.
    const alerts = screen.getAllByRole("alert");
    expect(alerts.some((node) => /required/i.test(node.textContent ?? ""))).toBe(true);

    // No POST issued — Save was gated client-side.
    const postCall = fetchSpy.mock.calls.find(([url, init]) => {
      const method = (init as RequestInit | undefined)?.method?.toUpperCase();
      return method === "POST" && String(url).includes("/v1/config/providers");
    });
    expect(postCall).toBeUndefined();
  });

  it("shows failure status badge when test returns ok=false", async () => {
    stubFetch({ ok: false, latency_ms: 10, error: "unsupported adapter: stub" });
    renderProviders();

    const editButton = await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 });
    fireEvent.click(editButton);

    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });

    const testBtn = screen.getByRole("button", { name: /test connection/i });
    await act(async () => {
      fireEvent.click(testBtn);
    });

    await screen.findByText(/Failed.*unsupported adapter: stub/i, undefined, { timeout: 5_000 });
  });
});
