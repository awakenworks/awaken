// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { RouterProvider, createMemoryRouter, createRoutesFromElements } from "react-router";
import { appRoutes } from "../app";
import { AuthProvider } from "../components/auth-provider";
import { ConfirmDialogProvider } from "../components/confirm-dialog";
import { ToastProvider } from "../components/toast-provider";
import { DiffModal } from "./agent-editor-page";
import { ADMIN_TOKEN_STORAGE_KEY } from "../lib/config-api";
import type { AgentSpec } from "../lib/config-api";
import { __resetAuthInterceptorForTesting } from "../lib/auth-interceptor";
import { withQueryClient } from "../test/query";

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

function jsonResponse(data: unknown, init?: { ok?: boolean; status?: number }) {
  return {
    ok: init?.ok ?? true,
    status: init?.status ?? 200,
    text: async () => JSON.stringify(data),
  };
}

function agentSpec(id: string) {
  return {
    id,
    model_id: "model-a",
    system_prompt: "Loaded prompt",
    max_rounds: 16,
    max_continuation_retries: 2,
    plugin_ids: [],
    sections: {},
    delegates: [],
  };
}

function stubExistingAgentWithMetaError() {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string | URL | Request) => {
      const href = typeof url === "string" ? url : url instanceof URL ? url.href : url.url;
      if (href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [],
          providers: [],
          namespaces: [],
        });
      }
      if (href.endsWith("/v1/config/agents/agent-a/meta")) {
        return jsonResponse({ error: "metadata forbidden" }, { ok: false, status: 403 });
      }
      if (href.endsWith("/v1/config/agents/agent-a")) {
        return jsonResponse(agentSpec("agent-a"));
      }
      return jsonResponse({ error: "not found" }, { ok: false, status: 404 });
    }),
  );
}

function renderEditorRoute(path = "/agents/new") {
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
  globalThis.localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
  stubCapabilitiesFetch();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  __resetAuthInterceptorForTesting();
});

describe("agent editor tab ARIA semantics", () => {
  async function waitForBasicsPanel() {
    await screen.findByLabelText("Agent ID", undefined, { timeout: 5000 });
  }

  it("renders a tablist with correct role", async () => {
    renderEditorRoute("/agents/new");
    // Wait for the page to render (Agent ID field indicates Basics panel is shown)
    await waitForBasicsPanel();
    const tablist = screen.getByRole("tablist");
    expect(tablist).toBeDefined();
  });

  it("each tab has role=tab and aria-selected reflects active state", async () => {
    renderEditorRoute("/agents/new");
    await waitForBasicsPanel();

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
    await waitForBasicsPanel();

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

describe("agent editor save validation", () => {
  it("blocks Save when required fields are empty and surfaces inline Required errors", async () => {
    const fetchSpy = stubCapabilitiesFetch();
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const saveBtn = screen.getAllByRole("button", { name: /^save$/i })[0];
    fireEvent.click(saveBtn);

    const alerts = await screen.findAllByRole("alert");
    expect(alerts.some((node) => /required/i.test(node.textContent ?? ""))).toBe(true);

    // No POST issued — Save was gated client-side.
    const postCall = fetchSpy.mock.calls.find((call) => {
      const [url, init] = call as unknown as Parameters<typeof fetch>;
      const method = (init as RequestInit | undefined)?.method?.toUpperCase();
      return method === "POST" && String(url).includes("/v1/config/agents");
    });
    expect(postCall).toBeUndefined();
  });

  it("hydrates existing agent data when metadata loading fails", async () => {
    stubExistingAgentWithMetaError();
    renderEditorRoute("/agents/agent-a");

    const idInput = await screen.findByLabelText("Agent ID", undefined, { timeout: 5_000 });
    await waitFor(() => {
      expect((idInput as HTMLInputElement).value).toBe("agent-a");
      expect((screen.getByLabelText("System prompt") as HTMLTextAreaElement).value).toBe(
        "Loaded prompt",
      );
    });
    expect(
      await screen.findAllByText(/Agent metadata unavailable: metadata forbidden/i),
    ).not.toHaveLength(0);
    expect(
      screen
        .getAllByRole("button", { name: /^save$/i })
        .every((button) => button.hasAttribute("disabled")),
    ).toBe(true);
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
        if (
          String(url).includes("/v1/config/agents/existing-agent") &&
          !String(url).includes("audit")
        ) {
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

describe("agent editor History tab auto-refresh after Save", () => {
  it("re-fetches history after a successful save", async () => {
    let auditCallCount = 0;
    const newEvent = {
      id: "evt-new001",
      ts: "2026-01-02T00:00:00Z",
      actor: "hash2/dev",
      action: "update",
      resource: "agents/refresh-agent",
      before: { id: "refresh-agent", model_id: "m1", system_prompt: "old", max_rounds: 8 },
      after: { id: "refresh-agent", model_id: "m1", system_prompt: "new", max_rounds: 8 },
    };

    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const u = String(url);
        if (u.includes("/v1/capabilities")) {
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
        if (
          u.includes("/v1/config/agents/refresh-agent") &&
          !u.includes("audit") &&
          init?.method === "PUT"
        ) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({
                id: "refresh-agent",
                model_id: "m1",
                system_prompt: "new",
                max_rounds: 8,
              }),
          };
        }
        if (u.includes("/v1/config/agents/refresh-agent") && !u.includes("audit")) {
          return {
            ok: true,
            status: 200,
            text: async () =>
              JSON.stringify({
                id: "refresh-agent",
                model_id: "m1",
                system_prompt: "old",
                max_rounds: 8,
              }),
          };
        }
        if (u.includes("/v1/audit-log")) {
          auditCallCount += 1;
          const items = auditCallCount >= 2 ? [newEvent] : [];
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ items }),
          };
        }
        return { ok: false, status: 404, text: async () => "" };
      }),
    );

    renderEditorRoute("/agents/refresh-agent");
    await screen.findByText(/Edit refresh-agent/i);

    // Switch to History — first audit fetch (returns 0 events)
    const historyTab = screen.getByRole("tab", { name: "History" });
    fireEvent.click(historyTab);
    await screen.findByText(/No history yet/i);

    // Switch to Basics, make the form dirty, then save
    const basicsTab = screen.getByRole("tab", { name: "Basics" });
    fireEvent.click(basicsTab);
    await screen.findByLabelText("Agent ID");

    // Modify system prompt to mark the form as dirty
    const promptTextarea = screen.getByRole("textbox", { name: /system prompt/i });
    fireEvent.change(promptTextarea, { target: { value: "new" } });

    const saveButton = screen.getByRole("button", { name: /^save$/i });
    fireEvent.click(saveButton);

    // Switch back to History — should trigger second audit fetch (returns 1 event)
    fireEvent.click(historyTab);
    await screen.findByText("hash2");

    expect(auditCallCount).toBeGreaterThanOrEqual(2);
  });
});

describe("DiffModal", () => {
  it("shows secret-only section changes without rendering raw secret values", () => {
    const previous = agentSpec("secret-agent") as AgentSpec;
    previous.sections = { oauth: { client_secret: "old-client-secret" } };
    const current = agentSpec("secret-agent") as AgentSpec;
    current.sections = { oauth: { client_secret: "new-client-secret" } };

    render(<DiffModal previous={previous} current={current} onClose={() => {}} />);

    const dialog = screen.getByRole("dialog", { name: "Diff vs published" });
    expect(screen.getByText("sections.oauth.client_secret")).toBeTruthy();
    expect(screen.getByTestId("diff-redacted-value-changed")).toBeTruthy();
    expect(dialog.textContent).toContain("*** (changed)");
    expect(dialog.textContent).not.toContain("old-client-secret");
    expect(dialog.textContent).not.toContain("new-client-secret");
    expect(screen.queryByText(/No semantic changes/i)).toBeNull();
  });
});

// ── Source badge + reset button ──────────────────────────────────────────────

function buildEditorFetchMock(
  agentId: string,
  agentBody: Record<string, unknown>,
  metaBody: Record<string, unknown> | null,
) {
  return vi.fn(async (url: string) => {
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
    // GET meta endpoint — must be checked before the general agent URL check
    if (String(url).endsWith(`/v1/config/agents/${agentId}/meta`)) {
      if (!metaBody) return { ok: false, status: 404, text: async () => "" };
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(metaBody),
      };
    }
    // GET / PUT agent
    if (String(url).includes(`/v1/config/agents/${agentId}`)) {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(agentBody),
      };
    }
    // DELETE overrides
    if (String(url).includes(`/v1/config/agents/${agentId}/overrides`)) {
      return { ok: true, status: 200, text: async () => JSON.stringify(agentBody) };
    }
    if (String(url).includes("/v1/audit-log")) {
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify({ items: [] }),
      };
    }
    return { ok: false, status: 404, text: async () => "" };
  });
}

describe("agent editor source badges", () => {
  it("shows Built-in badge for a builtin agent", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "my-builtin",
        { id: "my-builtin", model_id: "m1", system_prompt: "", max_rounds: 8 },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/my-builtin");
    await screen.findByText(/Edit my-builtin/i);
    expect(screen.getByText("Built-in")).toBeDefined();
  });

  it("shows Customized badge and Reset to defaults button for customized agent", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "my-customized",
        { id: "my-customized", model_id: "m1", system_prompt: "custom", max_rounds: 8 },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: { system_prompt: "custom" },
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/my-customized");
    await screen.findByText(/Edit my-customized/i);
    expect(screen.getByText("Customized")).toBeDefined();
    expect(screen.getByRole("button", { name: /reset to defaults/i })).toBeDefined();
  });

  it("does not show Reset button for builtin agent with no overrides", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "pure-builtin",
        { id: "pure-builtin", model_id: "m1", system_prompt: "", max_rounds: 8 },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/pure-builtin");
    await screen.findByText(/Edit pure-builtin/i);
    expect(screen.queryByRole("button", { name: /reset to defaults/i })).toBeNull();
  });

  it("shows User-defined badge for user-created agent", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "user-def",
        { id: "user-def", model_id: "m1", system_prompt: "", max_rounds: 8 },
        {
          source: { kind: "user" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/user-def");
    await screen.findByText(/Edit user-def/i);
    expect(screen.getByText("User-defined")).toBeDefined();
    expect(screen.queryByRole("button", { name: /reset to defaults/i })).toBeNull();
  });

  it("calls clearAgentOverrides and refetches on reset confirm", async () => {
    let deleteOverridesCalled = false;
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
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
      if (
        String(url).includes("/v1/config/agents/reset-me/overrides") &&
        (init as RequestInit | undefined)?.method === "DELETE"
      ) {
        deleteOverridesCalled = true;
        return {
          ok: true,
          status: 200,
          text: async () =>
            JSON.stringify({ id: "reset-me", model_id: "m1", system_prompt: "", max_rounds: 8 }),
        };
      }
      if (String(url).endsWith("/v1/config/agents/reset-me/meta")) {
        return {
          ok: true,
          status: 200,
          text: async () =>
            JSON.stringify({
              source: { kind: "builtin", binary_version: "1.0" },
              hidden: false,
              user_overrides: { system_prompt: "custom" },
              created_at: 0,
              updated_at: 0,
            }),
        };
      }
      if (String(url).includes("/v1/config/agents/reset-me")) {
        return {
          ok: true,
          status: 200,
          text: async () =>
            JSON.stringify({
              id: "reset-me",
              model_id: "m1",
              system_prompt: "custom",
              max_rounds: 8,
            }),
        };
      }
      if (String(url).includes("/v1/audit-log")) {
        return { ok: true, status: 200, text: async () => JSON.stringify({ items: [] }) };
      }
      return { ok: false, status: 404, text: async () => "" };
    });

    vi.stubGlobal("fetch", fetchMock);

    renderEditorRoute("/agents/reset-me");
    await screen.findByText(/Edit reset-me/i);
    expect(screen.getByText("Customized")).toBeDefined();

    const resetBtn = screen.getByRole("button", { name: /reset to defaults/i });
    fireEvent.click(resetBtn);

    // Confirm dialog appears — wait for it, then click the last "Reset to defaults" button
    // (the dialog's confirm button, not the page trigger).
    await waitFor(() => {
      const buttons = screen.getAllByRole("button", { name: /reset to defaults/i });
      expect(buttons.length).toBeGreaterThan(1);
    });
    const allReset = screen.getAllByRole("button", { name: /reset to defaults/i });
    fireEvent.click(allReset[allReset.length - 1]);

    await waitFor(() => {
      expect(deleteOverridesCalled).toBe(true);
    });
  });
});

// ── endpoint override banner (R14) ───────────────────────────────────────────
//
// The admin-console editor treats `endpoint` as locked and exposes no UI
// for editing it. The server-side `AgentSpecPatch.endpoint` field is
// still patchable through `PATCH /v1/config/agents/:id/overrides`, so
// programmatic clients (CLI, scripts) can install endpoint overrides
// the editor wouldn't otherwise reveal. The banner surfaces that
// existence so the editor's "endpoint is fixed" appearance doesn't
// silently lie to operators.

describe("agent editor endpoint override banner (R14)", () => {
  it("renders banner when user_overrides.endpoint exists", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "patched-endpoint",
        { id: "patched-endpoint", model_id: "m1", system_prompt: "", max_rounds: 8 },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: {
            endpoint: {
              backend: "a2a",
              base_url: "https://staging.example.com",
              target: "remote-agent",
            },
          },
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/patched-endpoint");
    await screen.findByText(/Edit patched-endpoint/i);
    expect(screen.getByTestId("endpoint-override-banner")).toBeDefined();
  });

  it("renders banner even when override is explicit null (clears base endpoint)", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "cleared-endpoint",
        { id: "cleared-endpoint", model_id: "m1", system_prompt: "", max_rounds: 8 },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: { endpoint: null },
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/cleared-endpoint");
    await screen.findByText(/Edit cleared-endpoint/i);
    // `endpoint: null` is a valid override (means "this customized
    // record clears the base endpoint"). The banner must surface this
    // too — operators should see ANY endpoint override exists.
    expect(screen.getByTestId("endpoint-override-banner")).toBeDefined();
  });

  it("does not render banner when no endpoint override is set", async () => {
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock(
        "no-endpoint-override",
        {
          id: "no-endpoint-override",
          model_id: "m1",
          system_prompt: "custom",
          max_rounds: 8,
        },
        {
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          // Customized record, but the override is on a different field.
          user_overrides: { system_prompt: "custom" },
          created_at: 0,
          updated_at: 0,
        },
      ),
    );

    renderEditorRoute("/agents/no-endpoint-override");
    await screen.findByText(/Edit no-endpoint-override/i);
    expect(screen.queryByTestId("endpoint-override-banner")).toBeNull();
  });
});

// ── Save → PATCH vs PUT branching ────────────────────────────────────────────

describe("agent editor Save → PATCH vs PUT branching", () => {
  it("Save calls PATCH /overrides for Builtin records", async () => {
    const agentId = "builtin-agent";
    const agentBody = {
      id: agentId,
      model_id: "m1",
      system_prompt: "original",
      max_rounds: 8,
      plugin_ids: [],
      sections: {},
      delegates: [],
    };
    const metaBody = {
      source: { kind: "builtin", binary_version: "1.0" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    };

    let patchCalled = false;
    let patchBody: Record<string, unknown> = {};
    let putCalled = false;

    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const u = String(url);
        if (u.includes("/v1/capabilities")) {
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
        if (u.endsWith(`/v1/config/agents/${agentId}/meta`)) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify(metaBody),
          };
        }
        if (u.includes(`/v1/config/agents/${agentId}/overrides`) && init?.method === "PATCH") {
          patchCalled = true;
          patchBody = JSON.parse((init.body as string) ?? "{}") as Record<string, unknown>;
          // Return the updated spec after patch
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ ...agentBody, system_prompt: "updated-prompt" }),
          };
        }
        if (u.includes(`/v1/config/agents/${agentId}`) && init?.method === "PUT") {
          putCalled = true;
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify(agentBody),
          };
        }
        if (u.includes(`/v1/config/agents/${agentId}`)) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify(agentBody),
          };
        }
        if (u.includes("/v1/audit-log")) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ items: [] }),
          };
        }
        return { ok: false, status: 404, text: async () => "" };
      }),
    );

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));

    // Edit the system prompt (a patchable field)
    const promptTextarea = screen.getByRole("textbox", { name: /system prompt/i });
    fireEvent.change(promptTextarea, { target: { value: "updated-prompt" } });

    // Click Save
    const saveBtn = screen.getAllByRole("button", { name: /^save$/i })[0];
    fireEvent.click(saveBtn);

    await waitFor(() => {
      expect(patchCalled).toBe(true);
    });

    expect(putCalled).toBe(false);
    expect(patchBody).toMatchObject({ system_prompt: "updated-prompt" });
  });

  it("Save calls PUT /agents/:id for User records", async () => {
    const agentId = "user-agent";
    const agentBody = {
      id: agentId,
      model_id: "m1",
      system_prompt: "original",
      max_rounds: 8,
      plugin_ids: [],
      sections: {},
      delegates: [],
    };
    const metaBody = {
      source: { kind: "user" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    };

    let putCalled = false;
    let patchCalled = false;

    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const u = String(url);
        if (u.includes("/v1/capabilities")) {
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
        if (u.endsWith(`/v1/config/agents/${agentId}/meta`)) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify(metaBody),
          };
        }
        if (u.includes(`/v1/config/agents/${agentId}/overrides`) && init?.method === "PATCH") {
          patchCalled = true;
          return { ok: true, status: 200, text: async () => JSON.stringify(agentBody) };
        }
        if (u.includes(`/v1/config/agents/${agentId}`) && init?.method === "PUT") {
          putCalled = true;
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ ...agentBody, system_prompt: "updated-prompt" }),
          };
        }
        if (u.includes(`/v1/config/agents/${agentId}`)) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify(agentBody),
          };
        }
        if (u.includes("/v1/audit-log")) {
          return {
            ok: true,
            status: 200,
            text: async () => JSON.stringify({ items: [] }),
          };
        }
        return { ok: false, status: 404, text: async () => "" };
      }),
    );

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));

    // Edit the system prompt
    const promptTextarea = screen.getByRole("textbox", { name: /system prompt/i });
    fireEvent.change(promptTextarea, { target: { value: "updated-prompt" } });

    // Click Save
    const saveBtn = screen.getAllByRole("button", { name: /^save$/i })[0];
    fireEvent.click(saveBtn);

    await waitFor(() => {
      expect(putCalled).toBe(true);
    });

    expect(patchCalled).toBe(false);
  });
});
