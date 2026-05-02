// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { ADMIN_TOKEN_STORAGE_KEY } from "../lib/config-api";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";

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

function renderEditorRoute(path = "/agents/new") {
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
  globalThis.localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
  stubCapabilitiesFetch();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("agent editor tab ARIA semantics", () => {
  it("renders a tablist with correct role", async () => {
    renderEditorRoute("/agents/new");
    // Wait for the page to render (Agent ID field indicates Basics panel is shown)
    await screen.findByLabelText("Agent ID");
    const tablist = screen.getByRole("tablist");
    expect(tablist).toBeDefined();
  });

  it("each tab has role=tab and aria-selected reflects active state", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const tabs = screen.getAllByRole("tab");
    expect(tabs.length).toBe(6);

    // "basics" is the default active tab
    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(basicsTab.getAttribute("aria-selected")).toBe("true");

    // All other tabs are not selected
    for (const tab of tabs) {
      if (tab !== basicsTab) {
        expect(tab.getAttribute("aria-selected")).toBe("false");
      }
    }
  });

  it("active tab has tabIndex=0 and inactive tabs have tabIndex=-1", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(basicsTab.getAttribute("tabindex")).toBe("0");

    const toolsTab = screen.getByRole("tab", { name: "Tools" });
    expect(toolsTab.getAttribute("tabindex")).toBe("-1");
  });

  it("tab has aria-controls pointing to the corresponding panel id", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(basicsTab.getAttribute("aria-controls")).toBe("panel-basics");
    expect(basicsTab.getAttribute("id")).toBe("tab-basics");
  });

  it("active panel has role=tabpanel and aria-labelledby matching the active tab id", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    // By default the basics panel is visible (not hidden)
    const panels = screen.getAllByRole("tabpanel");
    // Only one panel should be "visible" (not hidden) at a time
    // getAllByRole returns only visible roles by default
    expect(panels.length).toBe(1);
    const panel = panels[0];
    expect(panel.getAttribute("aria-labelledby")).toBe("tab-basics");
    expect(panel.getAttribute("id")).toBe("panel-basics");
  });
});

describe("agent editor tab keyboard navigation", () => {
  it("ArrowRight moves focus and activates next tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    basicsTab.focus();
    fireEvent.keyDown(basicsTab, { key: "ArrowRight" });

    const toolsTab = screen.getByRole("tab", { name: "Tools" });
    expect(document.activeElement).toBe(toolsTab);
    expect(toolsTab.getAttribute("aria-selected")).toBe("true");
    expect(basicsTab.getAttribute("aria-selected")).toBe("false");
  });

  it("ArrowLeft from first tab wraps to last tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    basicsTab.focus();
    fireEvent.keyDown(basicsTab, { key: "ArrowLeft" });

    const historyTab = screen.getByRole("tab", { name: "History" });
    expect(document.activeElement).toBe(historyTab);
    expect(historyTab.getAttribute("aria-selected")).toBe("true");
  });

  it("Home key jumps to the first tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    // Click Tools tab first to make it active, then press Home
    const toolsTab = screen.getByRole("tab", { name: "Tools" });
    fireEvent.click(toolsTab);
    toolsTab.focus();
    fireEvent.keyDown(toolsTab, { key: "Home" });

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(document.activeElement).toBe(basicsTab);
    expect(basicsTab.getAttribute("aria-selected")).toBe("true");
  });

  it("End key jumps to the last tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    basicsTab.focus();
    fireEvent.keyDown(basicsTab, { key: "End" });

    const historyTab = screen.getByRole("tab", { name: "History" });
    expect(document.activeElement).toBe(historyTab);
    expect(historyTab.getAttribute("aria-selected")).toBe("true");
  });

  it("ArrowRight wraps from last tab to first tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    // Click History tab to make it active
    const historyTab = screen.getByRole("tab", { name: "History" });
    fireEvent.click(historyTab);
    historyTab.focus();
    fireEvent.keyDown(historyTab, { key: "ArrowRight" });

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(document.activeElement).toBe(basicsTab);
    expect(basicsTab.getAttribute("aria-selected")).toBe("true");
  });
});

describe("agent editor History tab", () => {
  it("shows 'Save first' empty state for new agents", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const historyTab = screen.getByRole("tab", { name: "History" });
    fireEvent.click(historyTab);

    await screen.findByText(/Save the agent first to see its history/i);
  });

  it("renders audit list rows when auditLog returns events for an existing agent", async () => {
    const auditEvents = [
      {
        id: "evt-abc123",
        ts: "2026-01-01T00:00:00Z",
        actor: "hash1/admin",
        action: "update",
        resource: "agents/existing-agent",
        before: { id: "existing-agent", model_id: "old-model", system_prompt: "", max_rounds: 8 },
        after: { id: "existing-agent", model_id: "new-model", system_prompt: "", max_rounds: 8 },
      },
    ];

    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (String(url).includes("/v1/capabilities")) {
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
        if (String(url).includes("/v1/config/agents/existing-agent") && !String(url).includes("audit")) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({
                id: "existing-agent",
                model_id: "new-model",
                system_prompt: "",
                max_rounds: 8,
              }),
          };
        }
        if (String(url).includes("/v1/audit-log")) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ items: auditEvents, next_cursor: undefined }),
          };
        }
        return { ok: false, status: 404, text: async () => "" };
      }),
    );

    renderEditorRoute("/agents/existing-agent");
    await screen.findByText(/Edit existing-agent/i);

    const historyTab = screen.getByRole("tab", { name: "History" });
    fireEvent.click(historyTab);

    // Wait for the audit row to appear (actor hash)
    await screen.findByText("hash1");
  });
});
