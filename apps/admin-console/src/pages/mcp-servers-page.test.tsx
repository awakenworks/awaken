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

// Minimal MCP server record returned by the list endpoint.
function mcpServerItem(id: string) {
  return {
    id,
    transport: "stdio",
    command: "node",
    args: ["server.js"],
    timeout_secs: 30,
    config: {},
    restart_policy: { enabled: false },
    updated_at: 1_700_000_000_000,
  };
}

function mcpStatusResponse(
  connected: boolean,
  tools: Array<{ name: string; description?: string }> = [],
) {
  return { connected, last_error: connected ? null : "connection refused", tools };
}

function buildFetchMock(overrides?: {
  listItems?: unknown[];
  statusConnected?: boolean;
  statusTools?: Array<{ name: string; description?: string }>;
}) {
  const items = overrides?.listItems ?? [mcpServerItem("my-server")];
  const connected = overrides?.statusConnected ?? true;
  const tools = overrides?.statusTools ?? [{ name: "ping", description: "ping tool" }];

  return vi.fn(async (url: string | URL | Request) => {
    const href = typeof url === "string" ? url : url instanceof URL ? url.href : url.url;

    // capabilities
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

    // list mcp-servers
    if (
      href.includes("/v1/config/mcp-servers") &&
      !href.includes("/status") &&
      !href.includes("/restart")
    ) {
      return {
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({ namespace: "mcp-servers", items, offset: 0, limit: 100 }),
      };
    }

    // per-server status
    if (href.includes("/v1/mcp-servers/") && href.includes("/status")) {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(mcpStatusResponse(connected, tools)),
      };
    }

    // fallback: 404
    return { ok: false, status: 404, text: async () => JSON.stringify({ error: "not found" }) };
  });
}

function renderMcpPage() {
  const memRouter = createMemoryRouter(createRoutesFromElements(appRoutes()), {
    initialEntries: ["/mcp-servers"],
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
  if (typeof globalThis.localStorage !== "undefined") {
    globalThis.localStorage.clear();
  }
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("MCP servers page — status badge", () => {
  it("renders a green status badge when the server is connected", async () => {
    vi.stubGlobal("fetch", buildFetchMock({ statusConnected: true }));
    renderMcpPage();

    // Wait for the server row to appear.
    await screen.findByText("my-server");

    // Wait for status fetch to complete; the connected badge has title="Connected".
    await waitFor(() => {
      const badge = document.querySelector('[title="Connected"]');
      expect(badge).not.toBeNull();
    });
  });

  it("renders a red status badge when the server is disconnected", async () => {
    vi.stubGlobal("fetch", buildFetchMock({ statusConnected: false, statusTools: [] }));
    renderMcpPage();

    await screen.findByText("my-server");

    await waitFor(() => {
      const badge = document.querySelector('[title^="Error:"]');
      expect(badge).not.toBeNull();
    });
  });

  it("shows a loading badge (grey) before status is fetched", async () => {
    // Use a fetch that hangs for status to observe the loading state.
    let resolveStatus!: () => void;
    const statusPending = new Promise<void>((r) => {
      resolveStatus = r;
    });

    const fetchMock = vi.fn(async (url: string | URL | Request) => {
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
              skills: [],
              models: [],
              providers: [],
              namespaces: [],
            }),
        };
      }
      if (href.includes("/v1/config/mcp-servers")) {
        return {
          ok: true,
          status: 200,
          text: async () =>
            JSON.stringify({
              namespace: "mcp-servers",
              items: [mcpServerItem("my-server")],
              offset: 0,
              limit: 100,
            }),
        };
      }
      if (href.includes("/v1/mcp-servers/") && href.includes("/status")) {
        await statusPending;
        return {
          ok: true,
          status: 200,
          text: async () => JSON.stringify(mcpStatusResponse(true, [])),
        };
      }
      return { ok: false, status: 404, text: async () => '{"error":"not found"}' };
    });

    vi.stubGlobal("fetch", fetchMock);
    renderMcpPage();

    await screen.findByText("my-server");

    // While status is pending, a grey/loading dot should be present.
    const loadingBadge = document.querySelector('[title="Loading status..."]');
    expect(loadingBadge).not.toBeNull();

    resolveStatus();
  });

  it("renders discovered tools in the editor when status is loaded", async () => {
    const tools = [
      { name: "list_files", description: "List directory files" },
      { name: "read_file", description: "Read a file" },
    ];
    vi.stubGlobal("fetch", buildFetchMock({ statusConnected: true, statusTools: tools }));
    renderMcpPage();

    await screen.findByText("my-server");

    // Click Edit to open the editor.
    const editButton = await screen.findByRole("button", { name: "Edit" });
    editButton.click();

    // Wait for the tools list to appear in the editor.
    await waitFor(() => {
      expect(screen.getByText("list_files")).toBeDefined();
      expect(screen.getByText("read_file")).toBeDefined();
    });
  });
});
