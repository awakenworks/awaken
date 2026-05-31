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

function fetchHref(input: string | URL | Request): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
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
    expect(tabs.length).toBe(7);

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

  it("redacts credential patterns from metadata error messages", async () => {
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
          return jsonResponse(
            { error: "metadata failed with Cookie: session=raw-session-id" },
            { ok: false, status: 403 },
          );
        }
        if (href.endsWith("/v1/config/agents/agent-a")) {
          return jsonResponse(agentSpec("agent-a"));
        }
        return jsonResponse({ error: "not found" }, { ok: false, status: 404 });
      }),
    );

    const { container } = renderEditorRoute("/agents/agent-a");
    await screen.findByText(/Agent metadata unavailable: metadata failed with Cookie: \*\*\*/i);
    expect(container.textContent ?? "").not.toContain("raw-session-id");
  });
});

describe("agent editor numeric inputs", () => {
  it("keeps previous Basics numeric values while fields are blank or half-typed", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const maxRounds = screen.getByLabelText("Max rounds") as HTMLInputElement;
    const retries = screen.getByLabelText("Max continuation retries") as HTMLInputElement;

    fireEvent.change(maxRounds, { target: { value: "7" } });
    expect(maxRounds.value).toBe("7");
    fireEvent.change(maxRounds, { target: { value: "" } });
    expect(maxRounds.value).toBe("7");

    fireEvent.change(retries, { target: { value: "0" } });
    expect(retries.value).toBe("0");
    fireEvent.change(retries, { target: { value: "" } });
    expect(retries.value).toBe("0");
  });
});

describe("agent editor preview Recent runs wiring", () => {
  it("shows Recent runs for a loaded saved agent and opens traces with the saved id", async () => {
    const agent = agentSpec("existing-agent");
    vi.stubGlobal(
      "fetch",
      buildEditorFetchMock("existing-agent", agent, {
        source: { kind: "user" },
        hidden: false,
        user_overrides: null,
        created_at: 0,
        updated_at: 0,
      }),
    );

    renderEditorRoute("/agents/existing-agent");
    await screen.findByText(/Edit existing-agent/i);

    fireEvent.click(screen.getByTestId("open-recent-traces"));

    const drawer = screen.getByRole("dialog", { name: "Recent runs" });
    expect(drawer).toBeTruthy();
    expect(drawer.textContent ?? "").toContain("existing-agent");
  });

  it("does not show Recent runs for a new unsaved agent after the user enters an id", async () => {
    renderEditorRoute("/agents/new");
    const idInput = await screen.findByLabelText("Agent ID");

    fireEvent.change(idInput, { target: { value: "my-new-agent" } });

    expect(screen.queryByTestId("open-recent-traces")).toBeNull();
  });
});

describe("agent editor validate and override guards", () => {
  it("posts the current draft to dry-run validation and reports success", async () => {
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "POST" && href.endsWith("/v1/config/agents/validate")) {
        return jsonResponse({ ok: true, normalized: JSON.parse(String(init?.body)) });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    fireEvent.change(screen.getByLabelText("Agent ID"), { target: { value: "validate-agent" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "model-a" } });
    fireEvent.change(screen.getByLabelText("System prompt"), {
      target: { value: "Validate before saving." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Validate" }));

    await screen.findByText(/Validation passed/i);
    const [, validateInit] = fetchSpy.mock.calls.find(([input, init]) => {
      const href = fetchHref(input as string | URL | Request);
      return init?.method === "POST" && href.endsWith("/v1/config/agents/validate");
    })!;
    expect(JSON.parse(String(validateInit?.body))).toMatchObject({
      id: "validate-agent",
      model_id: "model-a",
      system_prompt: "Validate before saving.",
    });
  });

  it("surfaces dry-run validation errors without saving", async () => {
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "POST" && href.endsWith("/v1/config/agents/validate")) {
        return jsonResponse({ error: "model is not published" }, { ok: false, status: 422 });
      }
      if (method === "POST" && href.endsWith("/v1/config/agents")) {
        return jsonResponse({ error: "should not save" }, { ok: false, status: 500 });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");
    fireEvent.change(screen.getByLabelText("Agent ID"), { target: { value: "invalid-agent" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "model-a" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate" }));

    await screen.findByText(/Validation failed: model is not published/i);
    expect(
      fetchSpy.mock.calls.some(([input, init]) => {
        const href = fetchHref(input as string | URL | Request);
        return init?.method === "POST" && href.endsWith("/v1/config/agents");
      }),
    ).toBe(false);
  });
});

describe("agent editor save API flows", () => {
  it("creates a new agent with the normalized editor payload", async () => {
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "POST" && href.endsWith("/v1/config/agents")) {
        return jsonResponse(JSON.parse(String(init?.body)));
      }
      if (method === "GET" && href.endsWith("/v1/config/agents/new-agent/meta")) {
        return jsonResponse({
          source: { kind: "user" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        });
      }
      if (method === "GET" && href.endsWith("/v1/config/agents/new-agent")) {
        return jsonResponse({
          id: "new-agent",
          model_id: "model-a",
          system_prompt: "Respond with facts only.",
          max_rounds: 12,
          max_continuation_retries: 2,
          plugin_ids: [],
          sections: {},
          delegates: [],
        });
      }
      if (method === "GET" && href.includes("/v1/audit-log")) {
        return jsonResponse({ items: [] });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    fireEvent.change(screen.getByLabelText("Agent ID"), { target: { value: "new-agent" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "model-a" } });
    fireEvent.change(screen.getByLabelText("Max rounds"), { target: { value: "12" } });
    fireEvent.change(screen.getByLabelText("System prompt"), {
      target: { value: "Respond with facts only." },
    });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(
        fetchSpy.mock.calls.filter(([input, init]) => {
          const href = fetchHref(input as string | URL | Request);
          return init?.method === "POST" && href.endsWith("/v1/config/agents");
        }),
      ).toHaveLength(1);
    });
    const [, createInit] = fetchSpy.mock.calls.find(([input, init]) => {
      const href = fetchHref(input as string | URL | Request);
      return init?.method === "POST" && href.endsWith("/v1/config/agents");
    })!;
    expect(JSON.parse(String(createInit?.body))).toEqual({
      id: "new-agent",
      model_id: "model-a",
      system_prompt: "Respond with facts only.",
      max_rounds: 12,
      max_continuation_retries: 2,
      plugin_ids: [],
      sections: {},
      delegates: [],
    });
  });

  it("keeps edited draft visible when user-agent save fails", async () => {
    const agentId = "user-save-fails";
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}/meta`)) {
        return jsonResponse({
          source: { kind: "user" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        });
      }
      if (method === "PUT" && href.endsWith(`/v1/config/agents/${agentId}`)) {
        return jsonResponse({ error: "validation failed" }, { ok: false, status: 422 });
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}`)) {
        return jsonResponse({
          id: agentId,
          model_id: "model-a",
          system_prompt: "original prompt",
          max_rounds: 8,
          max_continuation_retries: 2,
          plugin_ids: [],
          sections: {},
          delegates: [],
        });
      }
      if (method === "GET" && href.includes("/v1/audit-log")) {
        return jsonResponse({ items: [] });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));

    const promptTextarea = screen.getByRole("textbox", { name: /system prompt/i });
    fireEvent.change(promptTextarea, { target: { value: "edited prompt" } });
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await screen.findByText("validation failed");
    expect((promptTextarea as HTMLTextAreaElement).value).toBe("edited prompt");
    const [, updateInit] = fetchSpy.mock.calls.find(([input, init]) => {
      const href = fetchHref(input as string | URL | Request);
      return init?.method === "PUT" && href.endsWith(`/v1/config/agents/${agentId}`);
    })!;
    expect(JSON.parse(String(updateInit?.body))).toMatchObject({
      id: agentId,
      system_prompt: "edited prompt",
    });
  });

  it("saves tool, skill, MCP, plugin, and sub-agent selections from their tabs", async () => {
    let createdAgent: unknown = null;
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: ["new-agent", "helper-agent"],
          tools: [
            {
              id: "tool-alpha",
              name: "Alpha",
              description: "Alpha tool",
              source: { kind: "builtin" },
            },
            {
              id: "tool-beta",
              name: "Beta",
              description: "Beta tool",
              source: { kind: "plugin", id: "files" },
            },
            {
              id: "skill",
              name: "Skill",
              description: "Activate skills",
              source: { kind: "plugin", id: "skills-discovery" },
            },
            {
              id: "load_skill_resource",
              name: "Load Skill Resource",
              description: "Load skill resources",
              source: { kind: "plugin", id: "skills-discovery" },
            },
          ],
          plugins: [
            { id: "logger-plugin", config_schemas: [] },
            { id: "skills-discovery", config_schemas: [] },
            { id: "skills-active-instructions", config_schemas: [] },
          ],
          skills: [
            {
              id: "writer",
              name: "Writer",
              description: "Draft clear prose",
              allowed_tools: [],
              arguments: [],
              user_invocable: true,
              model_invocable: true,
              context: "inline",
              paths: [],
            },
          ],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "GET" && href.includes("/v1/config/mcp-servers")) {
        return jsonResponse({
          namespace: "mcp-servers",
          items: [
            {
              id: "github",
              transport: "http",
              url: "http://mcp.test",
              timeout_secs: 30,
              config: {},
            },
          ],
          offset: 0,
          limit: 100,
        });
      }
      if (method === "GET" && href.endsWith("/v1/mcp-servers/github/status")) {
        return jsonResponse({
          connected: true,
          tools: [{ name: "create_issue", description: "Create issue" }],
          consecutive_failures: 0,
          reconnecting: false,
          permanently_failed: false,
        });
      }
      if (method === "POST" && href.endsWith("/v1/config/agents")) {
        createdAgent = JSON.parse(String(init?.body));
        return jsonResponse(createdAgent);
      }
      if (method === "GET" && href.endsWith("/v1/config/agents/new-agent/meta")) {
        return jsonResponse({
          source: { kind: "user" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        });
      }
      if (method === "GET" && href.endsWith("/v1/config/agents/new-agent")) {
        return jsonResponse(
          createdAgent ?? {
            id: "new-agent",
            model_id: "model-a",
            system_prompt: "",
            max_rounds: 16,
            max_continuation_retries: 2,
            plugin_ids: ["logger-plugin", "skills-discovery", "skills-active-instructions"],
            allowed_tools: ["tool-alpha", "skill", "load_skill_resource"],
            allowed_tool_patterns: ["mcp__github__*"],
            delegates: ["helper-agent"],
            sections: { skills: { allowlist: ["writer"] } },
          },
        );
      }
      if (method === "GET" && href.includes("/v1/audit-log")) {
        return jsonResponse({ items: [] });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    fireEvent.change(screen.getByLabelText("Agent ID"), { target: { value: "new-agent" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "model-a" } });

    fireEvent.click(screen.getByRole("tab", { name: "Tools" }));
    // New ToolSelector has no mode toggle — both literals + patterns sections
    // are always visible. Check tool-alpha into the Allowed literal list.
    const allowedSection = screen.getByTestId("tool-selector-allowed");
    const tools = allowedSection.querySelectorAll(
      "input[type='checkbox']",
    ) as NodeListOf<HTMLInputElement>;
    // Find the checkbox whose sibling shows the "tool-alpha" id.
    const alphaCheckbox = Array.from(tools).find((cb) =>
      cb.closest("label")?.textContent?.includes("tool-alpha"),
    )!;
    fireEvent.click(alphaCheckbox);

    fireEvent.click(screen.getByRole("tab", { name: "Skills" }));
    fireEvent.click(screen.getByRole("button", { name: "Enable discovery" }));
    fireEvent.click(screen.getByRole("button", { name: "Enable instructions" }));
    fireEvent.click(screen.getByRole("button", { name: "Allow `skill`" }));
    fireEvent.click(screen.getByRole("button", { name: "Allow resources" }));
    fireEvent.click(screen.getByText("Selected skills").closest("label")!);
    fireEvent.click(screen.getByLabelText(/writer/i));

    fireEvent.click(screen.getByRole("tab", { name: "Tools" }));
    await screen.findByText("create_issue");
    fireEvent.click(screen.getByLabelText("MCP server github"));

    fireEvent.click(screen.getByRole("tab", { name: "Plugins" }));
    fireEvent.click(screen.getByLabelText(/logger-plugin/i));

    fireEvent.click(screen.getByRole("tab", { name: "Delegates" }));
    fireEvent.click(screen.getByLabelText("helper-agent"));

    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(
        fetchSpy.mock.calls.filter(([input, init]) => {
          const href = fetchHref(input as string | URL | Request);
          return init?.method === "POST" && href.endsWith("/v1/config/agents");
        }),
      ).toHaveLength(1);
    });
    const [, createInit] = fetchSpy.mock.calls.find(([input, init]) => {
      const href = fetchHref(input as string | URL | Request);
      return init?.method === "POST" && href.endsWith("/v1/config/agents");
    })!;
    expect(JSON.parse(String(createInit?.body))).toMatchObject({
      id: "new-agent",
      model_id: "model-a",
      allowed_tools: ["tool-alpha", "skill", "load_skill_resource"],
      allowed_tool_patterns: ["mcp__github__*"],
      plugin_ids: ["skills-discovery", "skills-active-instructions", "logger-plugin"],
      sections: { skills: { allowlist: ["writer"] } },
      delegates: ["helper-agent"],
    });
  });
});

describe("agent editor review and history flows", () => {
  it("shows a semantic diff for dirty existing agents and closes the modal", async () => {
    const agentId = "diff-agent";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: string | URL | Request) => {
        const href = fetchHref(input);
        if (href.includes("/v1/capabilities")) {
          return jsonResponse({
            agents: [],
            tools: [],
            plugins: [],
            skills: [],
            models: [{ id: "model-a", upstream_model: "gpt-test" }],
            providers: [],
            namespaces: [],
          });
        }
        if (href.endsWith(`/v1/config/agents/${agentId}/meta`)) {
          return jsonResponse({
            source: { kind: "user" },
            hidden: false,
            user_overrides: null,
            created_at: 0,
            updated_at: 0,
          });
        }
        if (href.endsWith(`/v1/config/agents/${agentId}`)) {
          return jsonResponse({
            id: agentId,
            model_id: "model-a",
            system_prompt: "old prompt",
            max_rounds: 8,
            max_continuation_retries: 2,
            plugin_ids: [],
            sections: { nested: { enabled: true } },
            delegates: [],
          });
        }
        if (href.includes("/v1/audit-log")) {
          return jsonResponse({ items: [] });
        }
        throw new Error(`Unexpected fetch: ${href}`);
      }),
    );

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));

    fireEvent.change(screen.getByLabelText("System prompt"), {
      target: { value: "new prompt" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Diff vs published" }));

    await screen.findByRole("dialog", { name: "Diff vs published" });
    expect(screen.getByText("system_prompt")).toBeDefined();
    expect(screen.getByText("old prompt")).toBeDefined();
    expect(screen.getAllByText("new prompt").length).toBeGreaterThanOrEqual(1);

    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog", { name: "Diff vs published" })).toBeNull();
  });

  it("opens audit details and restores an agent version from history", async () => {
    const agentId = "history-agent";
    const event = {
      id: "evt-restore-001",
      ts: "2026-01-02T00:00:00Z",
      actor: "hash9/admin",
      action: "update",
      resource: `agents/${agentId}`,
      before: {
        id: agentId,
        model_id: "model-a",
        system_prompt: "old prompt",
        max_rounds: 8,
      },
      after: {
        id: agentId,
        model_id: "model-a",
        system_prompt: "restored prompt",
        max_rounds: 8,
      },
    };
    let restored = false;
    const fetchSpy = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "model-a", upstream_model: "gpt-test" }],
          providers: [],
          namespaces: [],
        });
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}/meta`)) {
        return jsonResponse({
          source: { kind: "user" },
          hidden: false,
          user_overrides: null,
          created_at: 0,
          updated_at: 0,
        });
      }
      if (method === "POST" && href.endsWith(`/v1/config/agents/${agentId}/restore`)) {
        restored = true;
        return jsonResponse({});
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}`)) {
        return jsonResponse({
          id: agentId,
          model_id: "model-a",
          system_prompt: restored ? "restored prompt" : "current prompt",
          max_rounds: 8,
          max_continuation_retries: 2,
          plugin_ids: [],
          sections: {},
          delegates: [],
        });
      }
      if (method === "GET" && href.includes("/v1/audit-log")) {
        return jsonResponse({ items: [event] });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchSpy);

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));
    fireEvent.click(screen.getByRole("tab", { name: "History" }));

    await screen.findByText("hash9");
    fireEvent.click(screen.getByRole("button", { name: "View" }));
    await screen.findByRole("dialog", { name: "Audit event details" });
    expect(screen.getByText("evt-restore-001")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));

    fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    await screen.findByRole("dialog", { name: "Restore agent to this version?" });
    fireEvent.click(screen.getAllByRole("button", { name: "Restore" }).at(-1)!);

    await screen.findByText("Agent restored to version evt-rest");
    expect(
      fetchSpy.mock.calls.some(([input, init]) => {
        const href = fetchHref(input as string | URL | Request);
        return (
          init?.method === "POST" &&
          href.endsWith(`/v1/config/agents/${agentId}/restore`) &&
          JSON.parse(String(init.body)).version === "evt-restore-001"
        );
      }),
    ).toBe(true);

    fireEvent.click(screen.getByRole("tab", { name: "Basics" }));
    expect((screen.getByLabelText("System prompt") as HTMLTextAreaElement).value).toBe(
      "restored prompt",
    );
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

  it("redacts credential patterns from string diff values", () => {
    const previous = agentSpec("diff-string-agent") as AgentSpec;
    previous.system_prompt = "old prompt";
    const current = agentSpec("diff-string-agent") as AgentSpec;
    current.system_prompt = "new prompt with Authorization: Bearer sk-real-secret-value";

    const { container } = render(
      <DiffModal previous={previous} current={current} onClose={() => {}} />,
    );

    const dom = container.textContent ?? "";
    expect(dom).toContain("Authorization: ***");
    expect(dom).not.toContain("sk-real-secret-value");
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

  it("clears one overridden field and refreshes the displayed draft", async () => {
    const agentId = "field-reset";
    let fieldResetCalled = false;
    const fetchMock = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/capabilities")) {
        return jsonResponse({
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [{ id: "m1", upstream_model: "model-one" }],
          providers: [],
          namespaces: [],
        });
      }
      if (
        method === "DELETE" &&
        href.endsWith(`/v1/config/agents/${agentId}/overrides/system_prompt`)
      ) {
        fieldResetCalled = true;
        return jsonResponse({});
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}/meta`)) {
        return jsonResponse({
          source: { kind: "builtin", binary_version: "1.0" },
          hidden: false,
          user_overrides: fieldResetCalled ? null : { system_prompt: "custom prompt" },
          created_at: 0,
          updated_at: 0,
        });
      }
      if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}`)) {
        return jsonResponse({
          id: agentId,
          model_id: "m1",
          system_prompt: fieldResetCalled ? "default prompt" : "custom prompt",
          max_rounds: 8,
          plugin_ids: [],
          sections: {},
          delegates: [],
        });
      }
      if (method === "GET" && href.includes("/v1/audit-log")) {
        return jsonResponse({ items: [] });
      }
      throw new Error(`Unexpected fetch: ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));
    expect(
      (screen.getByRole("textbox", { name: /system prompt/i }) as HTMLTextAreaElement).value,
    ).toBe("custom prompt");

    fireEvent.click(screen.getByRole("button", { name: /^Reset to default$/i }));

    await waitFor(() => {
      expect(fieldResetCalled).toBe(true);
      expect(
        (screen.getByRole("textbox", { name: /system prompt/i }) as HTMLTextAreaElement).value,
      ).toBe("default prompt");
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

describe("agent editor Raw JSON secret redaction", () => {
  function stubRawJsonAgent(agentBody: AgentSpec, metaBody: Record<string, unknown>) {
    const validateBodies: AgentSpec[] = [];
    const patchBodies: Record<string, unknown>[] = [];
    let currentAgent = JSON.parse(JSON.stringify(agentBody)) as AgentSpec;
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const u = String(url);
      if (u.includes("/v1/capabilities")) {
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
      if (u.includes("/v1/config/agents/validate") && init?.method === "POST") {
        const body = JSON.parse((init.body as string) ?? "{}") as AgentSpec;
        validateBodies.push(body);
        return jsonResponse({ ok: true, normalized: body });
      }
      if (u.endsWith(`/v1/config/agents/${agentBody.id}/meta`)) {
        return jsonResponse(metaBody);
      }
      if (u.includes(`/v1/config/agents/${agentBody.id}/overrides`) && init?.method === "PATCH") {
        const body = JSON.parse((init.body as string) ?? "{}") as Record<string, unknown>;
        patchBodies.push(body);
        if (body.sections && typeof body.sections === "object" && !Array.isArray(body.sections)) {
          currentAgent = {
            ...currentAgent,
            sections: {
              ...(currentAgent.sections ?? {}),
              ...(body.sections as Record<string, unknown>),
            },
          };
        }
        return jsonResponse(currentAgent);
      }
      if (u.endsWith(`/v1/config/agents/${agentBody.id}`)) {
        return jsonResponse(currentAgent);
      }
      if (u.includes("/v1/audit-log")) {
        return jsonResponse({ items: [] });
      }
      return jsonResponse({ error: "not found" }, { ok: false, status: 404 });
    });
    vi.stubGlobal("fetch", fetchMock);
    return { validateBodies, patchBodies };
  }

  async function openRawJsonTextarea() {
    const advancedTab = await screen.findByRole("tab", { name: "Advanced" });
    fireEvent.click(advancedTab);
    await screen.findByText("Raw JSON");
    const textarea = screen
      .getAllByRole("textbox")
      .find(
        (element): element is HTMLTextAreaElement =>
          element instanceof HTMLTextAreaElement && element.value.includes('"sections"'),
      );
    if (!textarea) throw new Error("Raw JSON textarea not found");
    return textarea;
  }

  function rawJsonRedactionMarker(textarea: HTMLTextAreaElement, field: string): string {
    const match = textarea.value.match(
      new RegExp(`"${field}": "(__AWAKEN_REDACTED_SECRET_[a-f0-9]{8}__)"`),
    );
    if (!match) throw new Error(`Redaction marker for ${field} not found`);
    return match[1];
  }

  it("does not render sections secrets into the Raw JSON textarea DOM", async () => {
    const agent = agentSpec("raw-secret-agent") as AgentSpec;
    agent.sections = {
      oauth: { client_secret: "live-client-secret" },
      observability: { api_key: "live-api-key" },
    };
    stubRawJsonAgent(agent, {
      source: { kind: "builtin", binary_version: "1.0" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    });

    renderEditorRoute(`/agents/${agent.id}`);
    await screen.findByText(/Edit raw-secret-agent/i);

    const textarea = await openRawJsonTextarea();
    expect(rawJsonRedactionMarker(textarea, "client_secret")).toMatch(
      /^__AWAKEN_REDACTED_SECRET_[a-f0-9]{8}__$/,
    );
    expect(rawJsonRedactionMarker(textarea, "api_key")).toMatch(
      /^__AWAKEN_REDACTED_SECRET_[a-f0-9]{8}__$/,
    );
    expect(textarea.value).not.toContain('"client_secret": "***"');
    expect(textarea.value).not.toContain("live-client-secret");
    expect(textarea.value).not.toContain("live-api-key");
  });

  it("restores unchanged Raw JSON redaction markers before validating the draft", async () => {
    const agent = agentSpec("raw-restore-agent") as AgentSpec;
    agent.sections = { oauth: { client_secret: "live-client-secret" } };
    const { validateBodies } = stubRawJsonAgent(agent, {
      source: { kind: "builtin", binary_version: "1.0" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    });

    renderEditorRoute(`/agents/${agent.id}`);
    await screen.findByText(/Edit raw-restore-agent/i);

    const textarea = await openRawJsonTextarea();
    fireEvent.change(textarea, { target: { value: `${textarea.value}\n` } });
    fireEvent.click(screen.getByRole("button", { name: /apply to draft/i }));

    await waitFor(() => {
      expect(validateBodies).toHaveLength(1);
    });
    expect(
      ((validateBodies[0].sections?.oauth as Record<string, unknown>) ?? {}).client_secret,
    ).toBe("live-client-secret");
    expect(screen.queryByText(/Save will publish to the runtime config/i)).toBeNull();
  });

  it("blocks Apply when an edited Raw JSON value still contains a redaction marker", async () => {
    const agent = agentSpec("raw-marker-agent") as AgentSpec;
    agent.sections = {
      gateway: {
        note: "Authorization: Bearer raw-token-value-1234567890\nowner=team-a",
      },
    };
    const { validateBodies } = stubRawJsonAgent(agent, {
      source: { kind: "builtin", binary_version: "1.0" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    });

    renderEditorRoute(`/agents/${agent.id}`);
    await screen.findByText(/Edit raw-marker-agent/i);

    const textarea = await openRawJsonTextarea();
    const marker = rawJsonRedactionMarker(textarea, "note");
    fireEvent.change(textarea, {
      target: {
        value: textarea.value.replace(`"note": "${marker}"`, `"note": "${marker}\\nowner=team-b"`),
      },
    });
    fireEvent.click(screen.getByRole("button", { name: /apply to draft/i }));

    await screen.findByText(/Redaction marker `sections\.gateway\.note`/i);
    expect(validateBodies).toHaveLength(0);
  });

  it("keeps a full Raw JSON secret replacement on save", async () => {
    const agent = agentSpec("raw-replace-agent") as AgentSpec;
    agent.sections = { oauth: { client_secret: "old-client-secret" } };
    const { validateBodies, patchBodies } = stubRawJsonAgent(agent, {
      source: { kind: "builtin", binary_version: "1.0" },
      hidden: false,
      user_overrides: null,
      created_at: 0,
      updated_at: 0,
    });

    renderEditorRoute(`/agents/${agent.id}`);
    await screen.findByText(/Edit raw-replace-agent/i);

    const textarea = await openRawJsonTextarea();
    const marker = rawJsonRedactionMarker(textarea, "client_secret");
    fireEvent.change(textarea, {
      target: {
        value: textarea.value.replace(
          `"client_secret": "${marker}"`,
          '"client_secret": "new-secret"',
        ),
      },
    });
    fireEvent.click(screen.getByRole("button", { name: /apply to draft/i }));

    await waitFor(() => {
      expect(validateBodies).toHaveLength(1);
    });
    expect(
      ((validateBodies[0].sections?.oauth as Record<string, unknown>) ?? {}).client_secret,
    ).toBe("new-secret");

    await screen.findByText(/Save will publish to the runtime config/i);
    fireEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await waitFor(() => {
      expect(patchBodies).toHaveLength(1);
    });
    expect(JSON.stringify(patchBodies[0])).not.toContain("__AWAKEN_REDACTED_SECRET_");
    expect(
      (
        ((patchBodies[0].sections as Record<string, unknown>).oauth as Record<string, unknown>) ??
        {}
      ).client_secret,
    ).toBe("new-secret");
  });

  it("allows Raw JSON to set id while creating a new agent", async () => {
    const validateBodies: AgentSpec[] = [];
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const u = String(url);
      if (u.includes("/v1/capabilities")) {
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
      if (u.includes("/v1/config/agents/validate") && init?.method === "POST") {
        const body = JSON.parse((init.body as string) ?? "{}") as AgentSpec;
        validateBodies.push(body);
        return jsonResponse({ ok: true, normalized: body });
      }
      return jsonResponse({ error: "not found" }, { ok: false, status: 404 });
    });
    vi.stubGlobal("fetch", fetchMock);

    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    const textarea = await openRawJsonTextarea();
    fireEvent.change(textarea, {
      target: { value: textarea.value.replace('"id": ""', '"id": "raw-new-agent"') },
    });
    fireEvent.click(screen.getByRole("button", { name: /apply to draft/i }));

    await waitFor(() => {
      expect(validateBodies).toHaveLength(1);
    });
    expect(validateBodies[0].id).toBe("raw-new-agent");

    fireEvent.click(screen.getByRole("tab", { name: "Basics" }));
    await waitFor(() => {
      expect((screen.getByLabelText("Agent ID") as HTMLInputElement).value).toBe("raw-new-agent");
    });
  });
});

describe("agent editor context policy inputs", () => {
  it("keeps the previous autocompact threshold while the number field is blank", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    fireEvent.click(await screen.findByRole("tab", { name: "Advanced" }));
    fireEvent.click(screen.getByLabelText("Apply custom policy"));

    const thresholdField = screen.getByText("Autocompact threshold").closest("label");
    if (!thresholdField) throw new Error("Autocompact threshold field not found");
    const inputs = thresholdField.querySelectorAll("input");
    const enableInput = inputs[0] as HTMLInputElement;
    const thresholdInput = inputs[1] as HTMLInputElement;

    fireEvent.click(enableInput);
    fireEvent.change(thresholdInput, { target: { value: "123456" } });
    expect(thresholdInput.value).toBe("123456");

    fireEvent.change(thresholdInput, { target: { value: "" } });
    expect(thresholdInput.value).toBe("123456");
  });
});

describe("agent editor compaction config inputs", () => {
  it("writes summarizer settings into the compaction section", async () => {
    renderEditorRoute("/agents/new");
    await screen.findByLabelText("Agent ID");

    fireEvent.click(await screen.findByRole("tab", { name: "Advanced" }));
    fireEvent.click(screen.getByLabelText("Customize summarizer"));
    fireEvent.change(screen.getByLabelText(/^Summary upstream model override/), {
      target: { value: "summary-fast" },
    });
    fireEvent.change(screen.getByLabelText("Summary max tokens"), {
      target: { value: "768" },
    });
    fireEvent.change(screen.getByLabelText("Minimum savings ratio"), {
      target: { value: "0.42" },
    });
    fireEvent.change(screen.getByLabelText("Summarizer system prompt"), {
      target: { value: "Preserve user goals and decisions." },
    });
    fireEvent.change(screen.getByLabelText(/^Summarizer user prompt/), {
      target: { value: "Summarize these messages:\n\n{messages}" },
    });

    const textarea = screen
      .getAllByRole("textbox")
      .find(
        (element): element is HTMLTextAreaElement =>
          element instanceof HTMLTextAreaElement && element.value.includes('"sections"'),
      );
    if (!textarea) throw new Error("Raw JSON textarea not found");
    const raw = JSON.parse(textarea.value);
    expect(raw.sections.compaction).toMatchObject({
      mode: "background",
      summary_model: "summary-fast",
      summary_max_tokens: 768,
      min_savings_ratio: 0.42,
      raw_retention: "preserve_durable",
      summarizer_system_prompt: "Preserve user goals and decisions.",
      summarizer_user_prompt: "Summarize these messages:\n\n{messages}",
    });
  });
});

// ── Save → PATCH vs PUT branching ────────────────────────────────────────────

describe("agent editor Save → PATCH vs PUT branching", () => {
  it("does not PATCH a builtin agent when no patchable fields changed", async () => {
    const agentId = "unchanged-builtin";
    const agentBody = {
      id: agentId,
      model_id: "m1",
      system_prompt: "default prompt",
      max_rounds: 8,
      plugin_ids: [],
      sections: {},
      delegates: [],
    };
    let patchCalled = false;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
        const href = fetchHref(input);
        const method = init?.method?.toUpperCase() ?? "GET";
        if (method === "GET" && href.includes("/v1/capabilities")) {
          return jsonResponse({
            agents: [],
            tools: [],
            plugins: [],
            skills: [],
            models: [{ id: "m1", upstream_model: "model-one" }],
            providers: [],
            namespaces: [],
          });
        }
        if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}/meta`)) {
          return jsonResponse({
            source: { kind: "builtin", binary_version: "1.0" },
            hidden: false,
            user_overrides: null,
            created_at: 0,
            updated_at: 0,
          });
        }
        if (method === "PATCH" && href.endsWith(`/v1/config/agents/${agentId}/overrides`)) {
          patchCalled = true;
          return jsonResponse({});
        }
        if (method === "GET" && href.endsWith(`/v1/config/agents/${agentId}`)) {
          return jsonResponse(agentBody);
        }
        if (method === "GET" && href.includes("/v1/audit-log")) {
          return jsonResponse({ items: [] });
        }
        throw new Error(`Unexpected fetch: ${method} ${href}`);
      }),
    );

    renderEditorRoute(`/agents/${agentId}`);
    await screen.findByText(new RegExp(`Edit ${agentId}`, "i"));
    fireEvent.click(screen.getByRole("button", { name: /^Save$/i }));

    await screen.findByText(`Agent "${agentId}" saved (no patchable changes)`);
    expect(patchCalled).toBe(false);
  });

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
