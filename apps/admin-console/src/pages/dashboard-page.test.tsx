// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { DashboardPage } from "./dashboard-page";
import type { DashboardData } from "@/lib/query/hooks/dashboard";
import type { RunCountsResult } from "@/lib/query/hooks/run-counts";
import type { AgentRuntimeSnapshot } from "@/lib/config-api";

const dashboardState = vi.hoisted(() => ({
  value: {
    data: undefined as DashboardData | undefined,
    error: null as unknown,
  },
  ranges: [] as string[],
}));

const runCountsState = vi.hoisted(() => ({
  value: {
    data: undefined as RunCountsResult | undefined,
    error: null as unknown,
  },
}));

const runtimeStatsState = vi.hoisted(() => ({
  value: {
    data: undefined as AgentRuntimeSnapshot[] | null | undefined,
    error: null as unknown,
  },
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
  // Minimal Trans shim — the dashboard only uses Trans for the
  // FeatureDisabledNotice "To enable, X" copy, which carries one
  // interpolated value. Surface the key + values the same way `t` does
  // so existing string-match assertions keep working.
  Trans: ({ i18nKey, values }: { i18nKey?: string; values?: Record<string, unknown> }) => {
    if (!values || Object.keys(values).length === 0) return i18nKey ?? "";
    const formatted = Object.entries(values)
      .map(([name, value]) => `${name}=${String(value)}`)
      .join(" ");
    return `${i18nKey} ${formatted}`;
  },
}));

vi.mock("@/lib/query/hooks/dashboard", async () => {
  const actual = await vi.importActual<typeof import("@/lib/query/hooks/dashboard")>(
    "@/lib/query/hooks/dashboard",
  );
  return {
    ...actual,
    useDashboardQuery: (range: string) => {
      dashboardState.ranges.push(range);
      return dashboardState.value;
    },
  };
});

vi.mock("@/lib/query/hooks/run-counts", () => ({
  useRunCountsQuery: () => runCountsState.value,
}));

vi.mock("@/lib/query/hooks/runtime-stats", () => ({
  useRuntimeStatsQuery: () => runtimeStatsState.value,
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
      scope_id: "workspace-a",
      uptime_seconds: 90_000,
      config_store_enabled: true,
      audit_log_enabled: true,
      runtime_stats_enabled: false,
    },
    degraded: {},
    ...overrides,
  };
}

function runtimeStatsFixture(): AgentRuntimeSnapshot[] {
  return [
    {
      agent_id: "agent-0",
      window_seconds: 3600,
      bucket_window_seconds: 60,
      bucket_count: 60,
      inference_count: 80,
      error_count: 2,
      input_tokens: 12000,
      output_tokens: 4500,
      avg_inference_duration_ms: 0,
      min_inference_duration_ms: 0,
      max_inference_duration_ms: 0,
      p50_inference_duration_ms: 0,
      p95_inference_duration_ms: 0,
      p99_inference_duration_ms: 0,
      suspensions: 1,
      handoffs: 0,
      delegations: 0,
      tool_calls_by_tool: [
        {
          tool: "echo",
          call_count: 30,
          failure_count: 1,
          total_duration_ms: 0,
          avg_duration_ms: 0,
          min_duration_ms: 0,
          max_duration_ms: 0,
          p50_duration_ms: 0,
          p95_duration_ms: 0,
          p99_duration_ms: 0,
        },
      ],
    },
    {
      agent_id: "agent-1",
      window_seconds: 3600,
      bucket_window_seconds: 60,
      bucket_count: 60,
      inference_count: 20,
      error_count: 0,
      input_tokens: 3000,
      output_tokens: 1500,
      avg_inference_duration_ms: 0,
      min_inference_duration_ms: 0,
      max_inference_duration_ms: 0,
      p50_inference_duration_ms: 0,
      p95_inference_duration_ms: 0,
      p99_inference_duration_ms: 0,
      suspensions: 0,
      handoffs: 1,
      delegations: 0,
      tool_calls_by_tool: [
        {
          tool: "search",
          call_count: 5,
          failure_count: 0,
          total_duration_ms: 0,
          avg_duration_ms: 0,
          min_duration_ms: 0,
          max_duration_ms: 0,
          p50_duration_ms: 0,
          p95_duration_ms: 0,
          p99_duration_ms: 0,
        },
      ],
    },
  ];
}

beforeEach(() => {
  dashboardState.value = { data: undefined, error: null };
  dashboardState.ranges = [];
  runCountsState.value = {
    data: { kind: "ok", counts: { running: 3, waiting: 1, created: 2 } },
    error: null,
  };
  runtimeStatsState.value = { data: runtimeStatsFixture(), error: null };
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

    // Loading state is a skeleton — assert by aria role + busy.
    expect(screen.getByLabelText("Loading dashboard").getAttribute("aria-busy")).toBe("true");

    dashboardState.value = { data: undefined, error: new Error("dashboard unavailable") };
    rerender(
      <MemoryRouter>
        <DashboardPage />
      </MemoryRouter>,
    );

    expect(screen.getByText("dashboard unavailable")).toBeTruthy();
  });

  it("renders workload hero, activity, health, and system sections", () => {
    dashboardState.value = { data: dashboardData(), error: null };

    renderDashboard();

    expect(screen.getByText("dashboard.title")).toBeTruthy();
    expect(screen.getByText("dashboard.counters.agents")).toBeTruthy();
    expect(screen.getByText("dashboard.activity.viewAll")).toBeTruthy();
    expect(screen.getByText("create")).toBeTruthy();
    expect(screen.getByText("agents/research-agent")).toBeTruthy();
    expect(screen.getByText("delete")).toBeTruthy();
    expect(screen.getByText("models/old-model")).toBeTruthy();

    // Workload card: waiting is hero (=1), running=3, created=2. HITL
    // tile gets the warn screen-reader prefix when waiting > 0. Scope
    // the number assertions inside the card (the global CountRibbon
    // also renders small digits).
    const workloadHeading = screen.getByRole("heading", {
      name: /dashboard\.workload\.title/,
    });
    const workloadCard = workloadHeading.parentElement!.parentElement!;
    expect(within(workloadCard).getByText(/dashboard\.workload\.actionNeeded/)).toBeTruthy();
    expect(within(workloadCard).getByText("3")).toBeTruthy();
    expect(within(workloadCard).getByText("1")).toBeTruthy();
    expect(within(workloadCard).getByText("2")).toBeTruthy();
    expect(within(workloadCard).getByText("dashboard.workload.running")).toBeTruthy();
    expect(within(workloadCard).getByText("dashboard.workload.waiting")).toBeTruthy();
    expect(within(workloadCard).getByText("dashboard.workload.created")).toBeTruthy();
    expect(within(workloadCard).getByText(/dashboard\.workload\.sub seconds=30/)).toBeTruthy();

    // Runtime activity card surfaces aggregates + top lists.
    expect(screen.getByText("dashboard.agentActivity.title")).toBeTruthy();
    expect(screen.getByText("100")).toBeTruthy();
    expect(screen.getByText(/dashboard\.agentActivity\.errorRate pct=2\.0/)).toBeTruthy();
    expect(
      screen.getByText(/dashboard\.agentActivity\.tokensInOut input=15,000 output=6,000/),
    ).toBeTruthy();
    expect(screen.getByText("dashboard.agentActivity.topAgents")).toBeTruthy();
    expect(screen.getByText("agent-0")).toBeTruthy();
    expect(screen.getByText("dashboard.agentActivity.topTools")).toBeTruthy();
    expect(screen.getByText("echo")).toBeTruthy();
    expect(screen.getByText(/dashboard\.agentActivity\.toolFailures failures=1/)).toBeTruthy();

    expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.health.keySet")).toBeTruthy();
    expect(screen.getAllByText("local").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.health.noKey")).toBeTruthy();
    expect(screen.getByText("filesystem")).toBeTruthy();
    expect(screen.getByText("dashboard.health.autoRestart")).toBeTruthy();
    expect(screen.getByText("docs-http")).toBeTruthy();
    expect(screen.getByText("dashboard.health.manual")).toBeTruthy();

    expect(screen.getByText("1.2.3-test")).toBeTruthy();
    expect(screen.getByText("workspace-a")).toBeTruthy();
    expect(screen.getByText("1d 1h")).toBeTruthy();
    expect(screen.getAllByText("dashboard.system.on").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("dashboard.system.off")).toBeTruthy();

    // Plugins / tools sections were removed from the dashboard — they
    // remain reachable from the sidebar but no longer take page space.
    expect(screen.queryByText("dashboard.plugins.title")).toBeNull();
    expect(screen.queryByText("dashboard.tools.title")).toBeNull();

    // Range switcher now lives inside the Activity timeline card.
    fireEvent.click(screen.getByRole("radio", { name: "1h" }));
    expect(dashboardState.ranges).toContain("1h");
  });

  it("renders disabled audit empty without system info", () => {
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

    expect(screen.getByText("dashboard.activity.disabledTitle")).toBeTruthy();
    // featureDisabled.toEnable is a Trans render: the interpolated hint
    // appears inside the surrounding "To enable, {{hint}}" frame.
    expect(screen.getByText(/dashboard\.activity\.disabledHint/)).toBeTruthy();
    expect(screen.getByText("dashboard.health.noProviders")).toBeTruthy();
    expect(screen.getByText("dashboard.health.noMcp")).toBeTruthy();
    expect(screen.queryByText("dashboard.system.title")).toBeNull();
  });

  it("renders disabled notices when runtime stats and run counts are unavailable (404/503)", () => {
    dashboardState.value = {
      data: dashboardData(),
      error: null,
    };
    runCountsState.value = { data: { kind: "store_unavailable" }, error: null };
    runtimeStatsState.value = { data: null, error: null };

    renderDashboard();

    expect(screen.getByText("dashboard.workload.unavailableTitle")).toBeTruthy();
    expect(screen.getByText(/dashboard\.workload\.unavailableHint/)).toBeTruthy();
    expect(screen.getByText("dashboard.agentActivity.disabledTitle")).toBeTruthy();
    expect(screen.getByText(/dashboard\.agentActivity\.disabledHint/)).toBeTruthy();
    expect(screen.queryByText("dashboard.agentActivity.topAgents")).toBeNull();
  });

  it("renders skeleton tiles while runtime stats and run counts are loading", () => {
    dashboardState.value = { data: dashboardData(), error: null };
    runCountsState.value = { data: undefined, error: null };
    runtimeStatsState.value = { data: undefined, error: null };

    renderDashboard();

    expect(screen.getByLabelText("dashboard.workload.loading").getAttribute("aria-busy")).toBe(
      "true",
    );
    expect(screen.getByLabelText("dashboard.agentActivity.loading").getAttribute("aria-busy")).toBe(
      "true",
    );
    // Loading must not be confused with the disabled notice.
    expect(screen.queryByText("dashboard.workload.unavailableTitle")).toBeNull();
    expect(screen.queryByText("dashboard.agentActivity.disabledTitle")).toBeNull();
  });

  it("surfaces auth/server errors inline instead of swallowing them as disabled", () => {
    dashboardState.value = { data: dashboardData(), error: null };
    runCountsState.value = { data: undefined, error: new Error("401 unauthorised") };
    runtimeStatsState.value = { data: undefined, error: new Error("network refused") };

    renderDashboard();

    expect(screen.getByText("dashboard.workload.errorTitle:")).toBeTruthy();
    expect(screen.getByText("401 unauthorised")).toBeTruthy();
    expect(screen.getByText("dashboard.agentActivity.errorTitle:")).toBeTruthy();
    expect(screen.getByText("network refused")).toBeTruthy();
    // Error state must not be confused with the disabled notice either.
    expect(screen.queryByText("dashboard.workload.unavailableTitle")).toBeNull();
    expect(screen.queryByText("dashboard.agentActivity.disabledTitle")).toBeNull();
  });

  it("renders an idle hint when run counts are all zero", () => {
    dashboardState.value = { data: dashboardData(), error: null };
    runCountsState.value = {
      data: { kind: "ok", counts: { running: 0, waiting: 0, created: 0 } },
      error: null,
    };

    renderDashboard();

    expect(screen.getByText("dashboard.workload.idle")).toBeTruthy();
    // When waiting is zero the HITL alert prefix is hidden.
    expect(screen.queryByText(/dashboard\.workload\.actionNeeded/)).toBeNull();
  });

  it("surfaces a degraded notice instead of aggregating snapshots with different window_seconds", () => {
    const snapshots = runtimeStatsFixture();
    // Force a mismatch so the card refuses to aggregate and shows the
    // mixedWindowsDegraded message — silently summing 1h + 24h rows
    // would let an operator read "errors" as one comparable rate.
    snapshots[1].window_seconds = 86_400;
    runtimeStatsState.value = { data: snapshots, error: null };
    dashboardState.value = { data: dashboardData(), error: null };

    renderDashboard();

    expect(screen.getByText("dashboard.agentActivity.mixedWindowsHint")).toBeTruthy();
    expect(screen.getByText("dashboard.agentActivity.mixedWindowsDegraded")).toBeTruthy();
    // No aggregated stat tiles, no top-N lists when degraded.
    expect(screen.queryByText("dashboard.agentActivity.inferences")).toBeNull();
    expect(screen.queryByText("dashboard.agentActivity.topAgents")).toBeNull();
    expect(screen.queryByText(/dashboard\.agentActivity\.window window=/)).toBeNull();
  });

  it("includes the percentage in the tool failure metric extra", () => {
    runtimeStatsState.value = { data: runtimeStatsFixture(), error: null };
    dashboardState.value = { data: dashboardData(), error: null };

    renderDashboard();

    // Fixture: tool "echo" has 30 calls, 1 failure → 3.3% rate. The
    // metric-extra must show both the absolute count AND the rate.
    expect(
      screen.getByText(/dashboard\.agentActivity\.toolFailures failures=1 pct=3\.3/),
    ).toBeTruthy();
  });

  it("surfaces a degraded marker when a config-list slot soft-failed", () => {
    runtimeStatsState.value = { data: [], error: null };
    dashboardState.value = {
      data: dashboardData({
        degraded: { providers: true, mcpServers: true },
      }),
      error: null,
    };

    renderDashboard();

    // CountRibbon shows "?" + warn tone for degraded slots (providers
    // and mcp here). The healthy ones still show their counts.
    const ribbonQuestionMarks = screen.getAllByText("?");
    expect(ribbonQuestionMarks.length).toBeGreaterThanOrEqual(2);
    // HealthCard surfaces the degraded badge + hint per section.
    const degradedPills = screen.getAllByText("dashboard.health.degraded");
    expect(degradedPills.length).toBe(2);
    const degradedHints = screen.getAllByText(/dashboard\.health\.degradedHint/);
    expect(degradedHints.length).toBeGreaterThanOrEqual(2);
  });

  it("renders an empty-activity hint when runtime stats has no inferences", () => {
    runtimeStatsState.value = { data: [], error: null };
    dashboardState.value = { data: dashboardData(), error: null };

    renderDashboard();

    expect(screen.getByText("dashboard.agentActivity.empty")).toBeTruthy();
    expect(screen.queryByText("dashboard.agentActivity.topAgents")).toBeNull();
  });
});
