// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { DashboardPage } from "./dashboard-page";
import type { DashboardData } from "@/lib/query/hooks/dashboard";

const dashboardState = vi.hoisted(() => ({
  value: {
    data: undefined as DashboardData | undefined,
    error: null as unknown,
  },
  ranges: [] as string[],
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) => {
      if (!params || Object.keys(params).length === 0) return key;
      return `${key} ${Object.entries(params)
        .map(([name, value]) => `${name}=${String(value)}`)
        .join(" ")}`;
    },
  }),
}));

vi.mock("@/lib/query/hooks/dashboard", () => ({
  useDashboardQuery: (range: string) => {
    dashboardState.ranges.push(range);
    return dashboardState.value;
  },
}));

function renderDashboard() {
  return render(
    <MemoryRouter initialEntries={["/"]}>
      <DashboardPage />
    </MemoryRouter>,
  );
}

function dashboardData(overrides: Partial<DashboardData> = {}): DashboardData {
  const agents = Array.from({ length: 9 }, (_, index) => ({
    id: `agent-${index}`,
    model_id: index % 2 === 0 ? "gpt-main" : "claude-main",
    system_prompt: "Assist users",
    plugin_ids: [],
    delegates: [],
  }));

  return {
    capabilities: {
      agents: agents.map((agent) => agent.id),
      skills: [
        {
          id: "imagegen",
          name: "Image Generator",
          description: "Generate images",
          allowed_tools: ["image_gen"],
          when_to_use: "bitmap output",
          arguments: [],
          user_invocable: true,
          model_invocable: true,
          context: "inline",
          paths: [],
        },
      ],
      tools: [
        { id: "tool.describe", name: "Describe", description: "Detailed tool" },
        { id: "tool.fallback", name: "Fallback Tool", description: "" },
      ],
      plugins: [
        { id: "permission", config_schemas: [{ key: "approval", schema: {} }] },
        { id: "stateless", config_schemas: [] },
      ],
      models: [],
      providers: [],
      namespaces: [],
    },
    mcpServers: [
      {
        id: "filesystem",
        transport: "stdio",
        command: "node",
        args: ["server.js"],
        timeout_secs: 30,
        config: {},
        restart_policy: { enabled: true },
      },
      {
        id: "docs-http",
        transport: "http",
        url: "https://mcp.example.test",
        timeout_secs: 30,
        config: {},
        restart_policy: { enabled: false },
      },
    ],
    providers: [
      { id: "openai", adapter: "openai", has_api_key: true },
      { id: "local", adapter: "ollama", has_api_key: false },
    ],
    models: [
      { id: "gpt-main", provider_id: "openai", upstream_model: "gpt-4.1" },
      { id: "claude-main", provider_id: "local", upstream_model: "qwen3" },
    ],
    agents,
    auditPage: {
      items: [
        {
          id: "evt-1",
          ts: "2026-05-15T00:00:00.000Z",
          actor: "abc123/research-agent",
          action: "create",
          resource: "agents/research-agent",
        },
        {
          id: "evt-2",
          ts: "2026-05-15T00:01:00.000Z",
          actor: "system",
          action: "delete",
          resource: "models/old-model",
        },
      ],
    },
    auditDisabled: false,
    systemInfo: {
      version: "1.2.3-test",
      uptime_seconds: 90_000,
      config_store_enabled: true,
      audit_log_enabled: true,
      runtime_stats_enabled: false,
    },
    ...overrides,
  };
}

beforeEach(() => {
  dashboardState.value = { data: undefined, error: null };
  dashboardState.ranges = [];
});

afterEach(() => {
  cleanup();
});

describe("DashboardPage", () => {
  it("renders loading and error states with exact messages", () => {
    const { rerender } = render(
      <MemoryRouter>
        <DashboardPage />
      </MemoryRouter>,
    );

    expect(screen.getByText("Loading dashboard...")).toBeTruthy();

    dashboardState.value = { data: undefined, error: new Error("dashboard unavailable") };
    rerender(
      <MemoryRouter>
        <DashboardPage />
      </MemoryRouter>,
    );

    expect(screen.getByText("dashboard unavailable")).toBeTruthy();
  });

  it("renders populated health, activity, graph, system, plugin, and tool sections", () => {
    dashboardState.value = { data: dashboardData(), error: null };

    renderDashboard();

    expect(screen.getByText("dashboard.title")).toBeTruthy();
    expect(screen.getByText("dashboard.counters.agents")).toBeTruthy();
    expect(screen.getByText("dashboard.activity.viewAll")).toBeTruthy();
    expect(screen.getByText("create")).toBeTruthy();
    expect(screen.getByText("agents/research-agent")).toBeTruthy();
    expect(screen.getByText("delete")).toBeTruthy();
    expect(screen.getByText("models/old-model")).toBeTruthy();

    expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.health.keySet")).toBeTruthy();
    expect(screen.getAllByText("local").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.health.noKey")).toBeTruthy();
    expect(screen.getByText("filesystem")).toBeTruthy();
    expect(screen.getByText("dashboard.health.autoRestart")).toBeTruthy();
    expect(screen.getByText("docs-http")).toBeTruthy();
    expect(screen.getByText("dashboard.health.manual")).toBeTruthy();

    expect(screen.getByRole("figure", { name: "agents to models to providers" })).toBeTruthy();
    expect(screen.getByText("dashboard.refGraph.viewAll")).toBeTruthy();
    expect(screen.getByText("1.2.3-test")).toBeTruthy();
    expect(screen.getByText("1d 1h")).toBeTruthy();
    expect(screen.getAllByText("dashboard.system.on").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.system.off")).toBeTruthy();

    expect(screen.getByText("permission")).toBeTruthy();
    expect(screen.getByText(/dashboard\.plugins\.configSections sections=approval/)).toBeTruthy();
    expect(screen.getByText("stateless")).toBeTruthy();
    expect(screen.getByText("tool.describe")).toBeTruthy();
    expect(screen.getByText("Detailed tool")).toBeTruthy();
    expect(screen.getByText("tool.fallback")).toBeTruthy();
    expect(screen.getByText("Fallback Tool")).toBeTruthy();

    fireEvent.click(screen.getByRole("radio", { name: "1h" }));
    expect(dashboardState.ranges).toContain("1h");
  });

  it("renders disabled audit and empty capability states without system info", () => {
    dashboardState.value = {
      data: dashboardData({
        capabilities: {
          agents: [],
          skills: [],
          tools: [],
          plugins: [],
          models: [],
          providers: [],
          namespaces: [],
        },
        mcpServers: [],
        providers: [],
        models: [],
        agents: [],
        auditPage: null,
        auditDisabled: true,
        systemInfo: null,
      }),
      error: null,
    };

    renderDashboard();

    expect(screen.getByText("Audit log is disabled on this server")).toBeTruthy();
    expect(screen.getByText(/AdminApiConfig\.audit_log_enabled/)).toBeTruthy();
    expect(screen.getByText("dashboard.health.noProviders")).toBeTruthy();
    expect(screen.getByText("dashboard.health.noMcp")).toBeTruthy();
    expect(screen.getByText("dashboard.plugins.noConfig")).toBeTruthy();
    expect(screen.getByText("dashboard.tools.empty")).toBeTruthy();
    expect(screen.queryByText("dashboard.system.title")).toBeNull();
  });
});
