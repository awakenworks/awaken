// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import {
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";

function toolItem(id: string, name: string, description?: string, category?: string) {
  return { id, name, description: description ?? `${name} description`, category };
}

function metaItem(
  id: string,
  userOverrides?: Record<string, unknown>,
) {
  return {
    id,
    meta: {
      source: { kind: "builtin", binary_version: "v1" },
      hidden: false,
      user_overrides: userOverrides ?? null,
      created_at: 0,
      updated_at: 0,
    },
  };
}

function buildFetchMock(
  tools: ReturnType<typeof toolItem>[],
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

    if (href.includes("/runtime-stats")) {
      return { ok: false, status: 503, text: async () => "" };
    }

    if (href.includes("/v1/config/tools/meta")) {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(metaItems),
      };
    }

    if (href.includes("/v1/config/tools")) {
      return {
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({ namespace: "tools", items: tools, offset: 0, limit: 100 }),
      };
    }

    return { ok: false, status: 404, text: async () => "" };
  });
}

function renderToolsPage() {
  const router = createMemoryRouter(createRoutesFromElements(appRoutes()), {
    initialEntries: ["/tools"],
  });
  render(
    <ToastProvider>
      <ConfirmDialogProvider>
        <AuthProvider>
          <RouterProvider router={router} />
        </AuthProvider>
      </ConfirmDialogProvider>
    </ToastProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("ToolsPage", () => {
  beforeEach(() => vi.clearAllMocks());

  it("renders one row per tool with the override indicator", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [
          toolItem("echo", "Echo", "Stock echo", "debug"),
          toolItem("shell", "Shell", "Run a shell command"),
        ],
        [
          metaItem("echo", { description: "patched" }),
          metaItem("shell"),
        ],
      ),
    );

    renderToolsPage();

    await waitFor(() => {
      expect(screen.getByText("echo")).toBeDefined();
      expect(screen.getByText("shell")).toBeDefined();
    });

    // The "echo" row carries the override badge; "shell" does not.
    const echoRow = screen.getByText("echo").closest("tr")!;
    expect(echoRow.textContent).toMatch(/customized|overridden/i);
  });

  it("shows builtin label for tools without overrides", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [toolItem("grep", "Grep", "Search files")],
        [metaItem("grep")],
      ),
    );

    renderToolsPage();

    await waitFor(() => {
      expect(screen.getByText("grep")).toBeDefined();
    });

    const grepRow = screen.getByText("grep").closest("tr")!;
    expect(grepRow.textContent).toMatch(/builtin/i);
  });

  it("renders category column and dash for missing category", async () => {
    vi.stubGlobal(
      "fetch",
      buildFetchMock(
        [
          toolItem("echo", "Echo", "Stock echo", "debug"),
          toolItem("shell", "Shell", "Run a shell command", undefined),
        ],
        [metaItem("echo"), metaItem("shell")],
      ),
    );

    renderToolsPage();

    await waitFor(() => {
      expect(screen.getByText("echo")).toBeDefined();
    });

    expect(screen.getByText("debug")).toBeDefined();
    // shell has no category — should show "—"
    const cells = screen.getAllByText("—");
    expect(cells.length).toBeGreaterThan(0);
  });
});
