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
    expect(tabs.length).toBe(5);

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

    const advancedTab = screen.getByRole("tab", { name: "Advanced" });
    expect(document.activeElement).toBe(advancedTab);
    expect(advancedTab.getAttribute("aria-selected")).toBe("true");
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

    const advancedTab = screen.getByRole("tab", { name: "Advanced" });
    expect(document.activeElement).toBe(advancedTab);
    expect(advancedTab.getAttribute("aria-selected")).toBe("true");
  });

  it("ArrowRight wraps from last tab to first tab", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    // Click Advanced tab to make it active
    const advancedTab = screen.getByRole("tab", { name: "Advanced" });
    fireEvent.click(advancedTab);
    advancedTab.focus();
    fireEvent.keyDown(advancedTab, { key: "ArrowRight" });

    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    expect(document.activeElement).toBe(basicsTab);
    expect(basicsTab.getAttribute("aria-selected")).toBe("true");
  });
});
