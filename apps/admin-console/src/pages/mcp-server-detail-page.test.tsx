// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";

import { ConfirmDialogProvider } from "@/components/confirm-dialog";
import { ToastProvider } from "@/components/toast-provider";
import { withQueryClient } from "@/test/query";
import { McpServerDetailPage } from "./mcp-server-detail-page";

const server = {
  id: "my-server",
  transport: "stdio",
  command: "node",
  args: ["server.js"],
  timeout_secs: 30,
  config: {},
  restart_policy: { enabled: false },
  updated_at: 1_700_000_000,
};

function hrefOf(input: string | URL | Request): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

function jsonResponse(data: unknown) {
  return {
    ok: true,
    status: 200,
    text: async () => JSON.stringify(data),
  };
}

function renderDetail() {
  return render(
    withQueryClient(
      <ToastProvider>
        <ConfirmDialogProvider>
          <MemoryRouter initialEntries={["/mcp-servers/my-server"]}>
            <Routes>
              <Route path="/mcp-servers/:id" element={<McpServerDetailPage />} />
            </Routes>
          </MemoryRouter>
        </ConfirmDialogProvider>
      </ToastProvider>,
    ),
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("McpServerDetailPage", () => {
  it("keeps status pending distinct from disconnected", async () => {
    const pendingStatus = new Promise(() => undefined);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string | URL | Request) => {
        const href = hrefOf(url);
        if (href.endsWith("/v1/config/mcp-servers/my-server")) {
          return jsonResponse(server);
        }
        if (href.endsWith("/v1/config/agents")) {
          return jsonResponse({ namespace: "agents", items: [], offset: 0, limit: 100 });
        }
        if (href.endsWith("/v1/mcp-servers/my-server/inventory")) {
          await pendingStatus;
        }
        return { ok: false, status: 404, text: async () => JSON.stringify({ error: "missing" }) };
      }),
    );

    renderDetail();

    await screen.findByText("my-server");
    expect(screen.getByText("status loading")).toBeDefined();
    expect(screen.getByText("loading tools")).toBeDefined();
    expect(screen.queryByText("disconnected")).toBeNull();
    expect(screen.queryByText("not connected")).toBeNull();
  });
});
