// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { RouterProvider, createMemoryRouter, createRoutesFromElements } from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { ADMIN_TOKEN_STORAGE_KEY } from "../lib/config-api";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";
import { withQueryClient } from "../test/query";
import { callsFor, jsonResponse, listResponse, requestMethod, requestUrl } from "../test/http";

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
  options?: {
    listItems?: unknown[];
    saveResponse?: unknown;
    testStatus?: number;
    testBody?: unknown;
  },
) {
  const providerItems = options?.listItems ?? [PROVIDER_RECORD];
  const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url = requestUrl(input);
    const method = requestMethod(init);
    const path = url.pathname;

    if (method === "GET" && path === "/v1/capabilities") {
      return jsonResponse({
        agents: [],
        tools: [],
        plugins: [],
        skills: [],
        models: [],
        providers: [],
        supported_adapters: supportedAdapters,
        namespaces: [],
      });
    }

    if (method === "GET" && path === "/v1/system/info") {
      return jsonResponse({
        version: "test",
        uptime_seconds: 60,
        config_store_enabled: true,
        audit_log_enabled: true,
        runtime_stats_enabled: true,
      });
    }

    if (method === "GET" && path === "/v1/config/providers") {
      return listResponse("providers", providerItems);
    }

    if (method === "POST" && path === "/v1/config/providers") {
      return jsonResponse(options?.saveResponse ?? JSON.parse(String(init?.body)));
    }

    if (method === "PUT" && path === "/v1/config/providers/my-openai") {
      return jsonResponse(options?.saveResponse ?? JSON.parse(String(init?.body)));
    }

    if (method === "GET" && path === "/v1/config/mcp-servers") {
      return listResponse("mcp-servers");
    }

    if (method === "GET" && path === "/v1/config/agents") {
      return listResponse("agents");
    }

    if (method === "POST" && path === "/v1/providers/my-openai/test") {
      const defaults = { ok: true, latency_ms: 42, network_tested: false };
      return jsonResponse(options?.testBody ?? { ...defaults, ...overrides }, options?.testStatus ?? 200);
    }

    throw new Error(`Unexpected fetch: ${method} ${url.href}`);
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

    const rowTestButton = await screen.findByRole(
      "button",
      { name: /^test$/i },
      { timeout: 5_000 },
    );
    await act(async () => {
      fireEvent.click(rowTestButton);
    });

    await screen.findByText(/my-openai config OK · 64ms/i, undefined, { timeout: 5_000 });
    expect(callsFor(fetchSpy, "/v1/providers/my-openai/test", "POST")).toHaveLength(1);
  });

  it("filters the provider list by search text", async () => {
    stubFetch();
    renderProviders();

    await screen.findByText("my-openai", undefined, { timeout: 5_000 });
    const search = screen.getByPlaceholderText(/search by id/i);

    fireEvent.change(search, { target: { value: "missing" } });
    expect(screen.getByText("No providers match the current filter.")).not.toBeNull();
    expect(screen.queryByText("my-openai")).toBeNull();

    fireEvent.change(search, { target: { value: "openai" } });
    expect(screen.getByText("my-openai")).not.toBeNull();
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
    expect(callsFor(fetchSpy, "/v1/config/providers", "POST")).toHaveLength(0);
  });

  it("requires client_email and private_key in service-account JSON before save", async () => {
    const fetchSpy = stubFetch(undefined, ["anthropic", "openai", "vertex"]);
    renderProviders();

    fireEvent.click(
      await screen.findByRole("button", { name: /new provider/i }, { timeout: 5_000 }),
    );
    await screen.findByText(/create provider/i, undefined, { timeout: 5_000 });

    fireEvent.change(screen.getByLabelText("Provider ID"), { target: { value: "vertex-main" } });
    fireEvent.change(screen.getByLabelText("Adapter"), { target: { value: "vertex" } });
    fireEvent.change(screen.getByLabelText("Credentials type"), {
      target: { value: "service_account_json" },
    });
    fireEvent.change(screen.getByPlaceholderText(/paste the full json/i), {
      target: { value: JSON.stringify({ project_id: "proj" }) },
    });

    await act(async () => {
      fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);
    });

    expect(screen.getByRole("alert").textContent).toContain(
      "JSON must include client_email and private_key",
    );
    expect(callsFor(fetchSpy, "/v1/config/providers", "POST")).toHaveLength(0);
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
    expect(
      screen.getByRole("button", { name: /test connection/i }).hasAttribute("disabled"),
    ).toBe(false);
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

    expect(callsFor(fetchSpy, "/v1/providers/my-openai/test", "POST")).toHaveLength(1);
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
    expect(callsFor(fetchSpy, "/v1/config/providers", "POST")).toHaveLength(0);
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

  it("shows server errors from the edit form connection test", async () => {
    stubFetch(undefined, ["openai", "anthropic"], {
      testStatus: 503,
      testBody: { error: "provider runtime unavailable" },
    });
    renderProviders();

    fireEvent.click(await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 }));
    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /test connection/i }));
    });

    await screen.findByText(/Failed: provider runtime unavailable/i, undefined, {
      timeout: 5_000,
    });
  });

  it("reports row-level provider test failures without opening the editor", async () => {
    stubFetch({ ok: false, latency_ms: 9, error: "missing api key" });
    renderProviders();

    await act(async () => {
      fireEvent.click(
        await screen.findByRole("button", { name: /^test$/i }, { timeout: 5_000 }),
      );
    });

    await screen.findByText(/my-openai: missing api key/i, undefined, { timeout: 5_000 });
    expect(screen.queryByText(/edit provider/i)).toBeNull();
  });

  it("drops service-account metadata when switching a Vertex provider back to bearer auth", async () => {
    const fetchSpy = stubFetch(undefined, ["openai", "vertex"], {
      listItems: [
        {
          ...PROVIDER_RECORD,
          adapter: "vertex",
          adapter_options: { credentials_kind: "service_account_json" },
        },
      ],
    });
    renderProviders();

    fireEvent.click(await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 }));
    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });
    fireEvent.change(screen.getByLabelText("Adapter"), { target: { value: "openai" } });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")).toHaveLength(1);
    });
    const [, updateInit] = callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")[0];
    expect(JSON.parse(String(updateInit?.body))).toEqual({
      id: "my-openai",
      adapter: "openai",
      has_api_key: true,
      timeout_secs: 300,
      created_at: 1000,
      updated_at: 2000,
    });
  });

  it("creates a Vertex provider with service-account JSON credentials", async () => {
    const serviceAccountJson = JSON.stringify({
      client_email: "svc@example.iam.gserviceaccount.com",
      private_key: "test-private-key",
      project_id: "proj",
    });
    const fetchSpy = stubFetch(undefined, ["anthropic", "openai", "vertex"], {
      listItems: [],
    });
    renderProviders();

    fireEvent.click(
      await screen.findByRole("button", { name: /new provider/i }, { timeout: 5_000 }),
    );
    await screen.findByText(/create provider/i, undefined, { timeout: 5_000 });

    fireEvent.change(screen.getByLabelText("Provider ID"), { target: { value: "vertex-main" } });
    fireEvent.change(screen.getByLabelText("Adapter"), { target: { value: "vertex" } });
    fireEvent.change(screen.getByLabelText("Credentials type"), {
      target: { value: "service_account_json" },
    });
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://us-central1-aiplatform.googleapis.com" },
    });
    fireEvent.change(screen.getByLabelText("Timeout (seconds)"), { target: { value: "120" } });
    fireEvent.change(screen.getByPlaceholderText(/paste the full json/i), {
      target: { value: serviceAccountJson },
    });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(callsFor(fetchSpy, "/v1/config/providers", "POST")).toHaveLength(1);
    });
    const [, createInit] = callsFor(fetchSpy, "/v1/config/providers", "POST")[0];
    expect(JSON.parse(String(createInit?.body))).toEqual({
      id: "vertex-main",
      adapter: "vertex",
      base_url: "https://us-central1-aiplatform.googleapis.com",
      timeout_secs: 120,
      adapter_options: { credentials_kind: "service_account_json" },
      api_key: serviceAccountJson,
    });
  });

  it("preserves, clears, and replaces existing provider credentials explicitly", async () => {
    const fetchSpy = stubFetch();
    renderProviders();

    fireEvent.click(await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 }));
    await screen.findByText(/edit provider/i, undefined, { timeout: 5_000 });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")).toHaveLength(1);
    });
    let [, updateInit] = callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")[0];
    expect(JSON.parse(String(updateInit?.body))).not.toHaveProperty("api_key");

    fireEvent.click(await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 }));
    fireEvent.click(screen.getByRole("radio", { name: "Clear key" }));
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")).toHaveLength(2);
    });
    [, updateInit] = callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")[1];
    expect(JSON.parse(String(updateInit?.body))).toMatchObject({ api_key: "" });

    fireEvent.click(await screen.findByRole("button", { name: /edit/i }, { timeout: 5_000 }));
    fireEvent.click(screen.getByRole("radio", { name: "Set new key" }));
    fireEvent.change(screen.getByPlaceholderText("sk-…"), { target: { value: "rotated-test-key" } });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")).toHaveLength(3);
    });
    [, updateInit] = callsFor(fetchSpy, "/v1/config/providers/my-openai", "PUT")[2];
    expect(JSON.parse(String(updateInit?.body))).toMatchObject({ api_key: "rotated-test-key" });
  });
});
