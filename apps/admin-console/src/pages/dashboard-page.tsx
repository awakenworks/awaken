import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router";
import {
  type AgentSpec,
  type Capabilities,
  type McpServerRecord,
  type ProviderRecord,
  type ModelBindingSpec,
  type SystemInfo,
  configApi,
} from "@/lib/config-api";
import {
  type AuditEvent,
  type AuditPage,
} from "@/lib/audit-log";
import { ConfigApiError } from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";
import { formatRelativeTime } from "@/lib/format-time";
import { PageHeader } from "@/components/ui/page-header";
import { Eyebrow } from "@/components/ui/eyebrow";
import { Pill } from "@/components/ui/pill";
import {
  ReferenceGraph,
  type GraphColumn,
  type GraphEdge,
} from "@/components/ui/reference-graph";

type DashboardData = {
  capabilities: Capabilities;
  mcpServers: McpServerRecord[];
  providers: ProviderRecord[];
  models: ModelBindingSpec[];
  agents: AgentSpec[];
  auditPage: AuditPage | null;
  auditDisabled: boolean;
  systemInfo: SystemInfo | null;
};

export function DashboardPage() {
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const auditPromise = configApi
          .auditLog({ limit: 12 })
          .then((page) => ({ page, disabled: false }))
          .catch((err) => {
            if (err instanceof ConfigApiError && err.status === 503) {
              return { page: null, disabled: true };
            }
            throw err;
          });
        const [capabilities, mcp, providers, models, agents, audit, systemInfo] =
          await Promise.all([
            configApi.capabilities(),
            configApi.list<McpServerRecord>("mcp-servers"),
            configApi.list<ProviderRecord>("providers"),
            configApi.list<ModelBindingSpec>("models"),
            configApi.list<AgentSpec>("agents"),
            auditPromise,
            configApi.systemInfo().catch(() => null),
          ]);
        if (!cancelled) {
          setData({
            capabilities,
            mcpServers: mcp.items,
            providers: providers.items,
            models: models.items,
            agents: agents.items,
            auditPage: audit.page,
            auditDisabled: audit.disabled,
            systemInfo,
          });
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(loadError instanceof Error ? loadError.message : String(loadError));
        }
      }
    }
    void load();
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) return <PageError message={error} />;
  if (!data) return <PageLoading />;

  const {
    capabilities,
    mcpServers,
    providers,
    models,
    agents,
    auditPage,
    auditDisabled,
    systemInfo,
  } = data;

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <PageHeader
        eyebrow="Observe"
        title="Dashboard"
        description="The counts below reflect the currently published runtime snapshot, not just what is stored on disk."
      />

      <section className="grid gap-4 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-6">
        <StatCard label="Agents" count={agents.length} to={adminRoutes.agents} />
        <StatCard label="Skills" count={capabilities.skills.length} to={adminRoutes.skills} />
        <StatCard label="Models" count={models.length} to={adminRoutes.models} />
        <StatCard label="Providers" count={providers.length} to={adminRoutes.providers} />
        <StatCard label="MCP Servers" count={mcpServers.length} to={adminRoutes.mcpServers} />
        <StatCard label="Tools" count={capabilities.tools.length} />
      </section>

      <section className="mt-8 rounded-md border border-line bg-surface p-5 shadow-card">
        <Eyebrow>Reference graph</Eyebrow>
        <h3 className="mt-1 text-lg font-semibold text-fg-strong">
          Agents → Models → Providers
        </h3>
        <p className="mt-1 max-w-2xl text-sm text-fg-soft">
          Each edge is a hard dependency. Deleting a node with inbound edges is
          gated by a reference check.
        </p>
        <div className="mt-5">
          <DependencyGraph
            agents={agents}
            models={models}
            providers={providers}
          />
        </div>
      </section>

      <section className="mt-6 grid gap-6 lg:grid-cols-2">
        <HealthCard providers={providers} mcpServers={mcpServers} />
        <ActivityTimeline auditPage={auditPage} disabled={auditDisabled} />
      </section>

      {systemInfo && (
        <section className="mt-6">
          <SystemCard info={systemInfo} />
        </section>
      )}

      <section className="mt-6 grid gap-6 lg:grid-cols-2">
        <div className="rounded-md border border-line bg-surface p-5 shadow-card">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">Plugins</h3>
            <span className="text-sm text-fg-soft">
              {capabilities.plugins.length} registered
            </span>
          </div>
          {capabilities.plugins.length === 0 ? (
            <p className="mt-4 text-sm text-fg-soft">No plugins registered.</p>
          ) : (
            <ul className="mt-4 space-y-3">
              {capabilities.plugins.map((plugin) => (
                <li
                  key={plugin.id}
                  className="rounded-md border border-line bg-soft px-4 py-3"
                >
                  <div className="font-mono text-sm text-fg-strong">{plugin.id}</div>
                  <div className="mt-1 text-sm text-fg-soft">
                    {plugin.config_schemas.length === 0
                      ? "No config sections"
                      : `Config sections: ${plugin.config_schemas
                          .map((schema) => schema.key)
                          .join(", ")}`}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="rounded-md border border-line bg-surface p-5 shadow-card">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">Published Tools</h3>
            <span className="text-sm text-fg-soft">{capabilities.tools.length} live</span>
          </div>
          {capabilities.tools.length === 0 ? (
            <p className="mt-4 text-sm text-fg-soft">No tools published.</p>
          ) : (
            <ul className="mt-4 max-h-[24rem] space-y-3 overflow-auto">
              {capabilities.tools.map((tool) => (
                <li
                  key={tool.id}
                  className="rounded-md border border-line bg-soft px-4 py-3"
                >
                  <div className="font-mono text-sm text-fg-strong">{tool.id}</div>
                  <div className="mt-1 text-sm text-fg-soft">
                    {tool.description || tool.name}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}

function DependencyGraph({
  agents,
  models,
  providers,
}: {
  agents: AgentSpec[];
  models: ModelBindingSpec[];
  providers: ProviderRecord[];
}) {
  const columns: GraphColumn[] = useMemo(
    () => [
      {
        id: "agents",
        label: "Agents",
        nodes: agents.slice(0, 8).map((a) => ({
          id: `agent:${a.id}`,
          label: a.id,
          sub: a.model_id,
          tone: "agent" as const,
        })),
      },
      {
        id: "models",
        label: "Models",
        nodes: models.slice(0, 8).map((m) => ({
          id: `model:${m.id}`,
          label: m.id,
          sub: `${m.provider_id} · ${m.upstream_model}`,
        })),
      },
      {
        id: "providers",
        label: "Providers",
        nodes: providers.slice(0, 8).map((p) => ({
          id: `provider:${p.id}`,
          label: p.id,
          sub: p.adapter,
          tone: "info" as const,
        })),
      },
    ],
    [agents, models, providers],
  );

  const edges: GraphEdge[] = useMemo(() => {
    const out: GraphEdge[] = [];
    const modelIds = new Set(columns[1].nodes.map((n) => n.id));
    const providerIds = new Set(columns[2].nodes.map((n) => n.id));
    for (const agent of agents.slice(0, 8)) {
      const target = `model:${agent.model_id}`;
      if (modelIds.has(target)) out.push({ from: `agent:${agent.id}`, to: target });
    }
    for (const model of models.slice(0, 8)) {
      const target = `provider:${model.provider_id}`;
      if (providerIds.has(target)) out.push({ from: `model:${model.id}`, to: target });
    }
    return out;
  }, [agents, models, columns]);

  return <ReferenceGraph columns={columns} edges={edges} ariaLabel="agents to models to providers" />;
}

function HealthCard({
  providers,
  mcpServers,
}: {
  providers: ProviderRecord[];
  mcpServers: McpServerRecord[];
}) {
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-fg-strong">Health</h3>
        <span className="text-sm text-fg-soft">
          {providers.length} providers · {mcpServers.length} MCP
        </span>
      </div>

      <div className="mt-4">
        <Eyebrow>Providers</Eyebrow>
        {providers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">No providers configured.</p>
        ) : (
          <ul className="mt-2 space-y-1.5">
            {providers.map((p) => (
              <li key={p.id} className="flex items-center justify-between gap-3 rounded-md border border-line bg-soft px-3 py-2">
                <div className="min-w-0">
                  <div className="font-mono text-sm text-fg-strong">{p.id}</div>
                  <div className="text-xs text-fg-soft">
                    {p.adapter} {p.has_api_key ? "· key set" : "· no key"}
                  </div>
                </div>
                <Pill tone={p.has_api_key ? "success" : "warn"}>
                  {p.has_api_key ? "ok" : "no key"}
                </Pill>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="mt-5">
        <Eyebrow>MCP servers</Eyebrow>
        {mcpServers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">No MCP servers configured.</p>
        ) : (
          <ul className="mt-2 space-y-1.5">
            {mcpServers.map((s) => (
              <li key={s.id} className="flex items-center justify-between gap-3 rounded-md border border-line bg-soft px-3 py-2">
                <div className="min-w-0">
                  <div className="font-mono text-sm text-fg-strong">{s.id}</div>
                  <div className="text-xs text-fg-soft">
                    {s.transport} {s.command ? `· ${s.command}` : ""}
                  </div>
                </div>
                <Pill tone={s.restart_policy?.enabled ? "success" : "neutral"}>
                  {s.restart_policy?.enabled ? "auto-restart" : "manual"}
                </Pill>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function SystemCard({ info }: { info: SystemInfo }) {
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <Eyebrow>System</Eyebrow>
      <div className="mt-3 grid gap-3 md:grid-cols-3 xl:grid-cols-5">
        <SystemStat label="Version" value={info.version} mono />
        <SystemStat label="Uptime" value={formatUptime(info.uptime_seconds)} />
        <SystemStat
          label="Config store"
          value={info.config_store_enabled ? "wired" : "none"}
          tone={info.config_store_enabled ? "success" : "neutral"}
        />
        <SystemStat
          label="Audit log"
          value={info.audit_log_enabled ? "on" : "off"}
          tone={info.audit_log_enabled ? "success" : "neutral"}
        />
        <SystemStat
          label="Runtime stats"
          value={info.runtime_stats_enabled ? "on" : "off"}
          tone={info.runtime_stats_enabled ? "success" : "neutral"}
        />
      </div>
    </div>
  );
}

function SystemStat({
  label,
  value,
  mono = false,
  tone = "neutral",
}: {
  label: string;
  value: string;
  mono?: boolean;
  tone?: "success" | "neutral";
}) {
  return (
    <div className="rounded-md border border-line bg-soft px-3 py-2">
      <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
        {label}
      </div>
      <div
        className={[
          "mt-1 text-sm font-semibold",
          mono ? "font-mono" : "",
          tone === "success" ? "text-tone-success" : "text-fg-strong",
        ]
          .join(" ")
          .trim()}
      >
        {value}
      </div>
    </div>
  );
}

function formatUptime(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

function ActivityTimeline({
  auditPage,
  disabled,
}: {
  auditPage: AuditPage | null;
  disabled: boolean;
}) {
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-fg-strong">Recent activity</h3>
        {!disabled && (
          <Link
            to={adminRoutes.auditLog}
            className="text-xs font-medium text-link transition-colors hover:text-link-hover"
          >
            View all →
          </Link>
        )}
      </div>
      {disabled ? (
        <FeatureDisabledNotice
          title="Audit log is disabled on this server"
          configHint="set AdminApiConfig.audit_log_enabled = true in the server config"
          docsUrl="docs/architecture/admin-audit-log.md"
        />
      ) : !auditPage || auditPage.items.length === 0 ? (
        <p className="mt-4 text-sm text-fg-soft">No recent activity yet.</p>
      ) : (
        <ol className="mt-4 space-y-3">
          {auditPage.items.slice(0, 8).map((event) => (
            <ActivityRow key={event.id} event={event} />
          ))}
        </ol>
      )}
    </div>
  );
}

function FeatureDisabledNotice({
  title,
  configHint,
  docsUrl,
}: {
  title: string;
  configHint: string;
  docsUrl?: string;
}) {
  return (
    <div className="mt-4 rounded-md border border-tone-warn/30 bg-tone-warn/10 px-3 py-3 text-sm">
      <div className="font-medium text-fg-strong">{title}</div>
      <div className="mt-1 text-xs text-fg-soft">
        To enable, <span className="font-mono">{configHint}</span>
        {docsUrl && (
          <>
            {" "}
            (see <span className="font-mono">{docsUrl}</span>)
          </>
        )}
        .
      </div>
    </div>
  );
}

function ActivityRow({ event }: { event: AuditEvent }) {
  const tone = ACTION_TONE[event.action] ?? "neutral";
  const dotClass = TONE_DOT[tone];
  return (
    <li className="flex items-start gap-3">
      <span aria-hidden className={`mt-1.5 inline-block h-2 w-2 shrink-0 rounded-pill ${dotClass}`} />
      <div className="min-w-0 flex-1">
        <div className="text-sm text-fg">
          <span className="font-medium text-fg-strong">{event.action}</span>{" "}
          <span className="font-mono text-fg-soft">{event.resource}</span>
        </div>
        <div className="mt-0.5 text-xs text-fg-faint">
          {event.actor || "system"} · {formatRelativeTime(Date.parse(event.ts))}
        </div>
      </div>
    </li>
  );
}

const ACTION_TONE: Record<string, "info" | "warn" | "success" | "error" | "neutral"> = {
  create: "success",
  update: "info",
  delete: "error",
  restart: "warn",
  publish: "info",
  restore: "warn",
};

const TONE_DOT: Record<"info" | "warn" | "success" | "error" | "neutral", string> = {
  info: "bg-tone-info",
  warn: "bg-tone-warn",
  success: "bg-tone-success",
  error: "bg-tone-error",
  neutral: "bg-fg-faint",
};

function StatCard({
  label,
  count,
  to,
}: {
  label: string;
  count: number;
  to?: string;
}) {
  const card = (
    <div className="group relative rounded-md border border-line bg-surface p-5 shadow-card transition-all hover:-translate-y-0.5 hover:shadow-card-lift">
      <div className="text-3xl font-semibold tracking-tight text-fg-strong">{count}</div>
      <div className="mt-2 flex items-center justify-between text-sm text-fg-soft">
        <span>{label}</span>
        {to && (
          <span aria-hidden className="text-fg-faint transition-colors group-hover:text-link">
            ↗
          </span>
        )}
      </div>
    </div>
  );
  return to ? <Link to={to}>{card}</Link> : card;
}

function PageLoading() {
  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="rounded-md border border-line bg-surface p-6 text-sm text-fg-soft shadow-card">
        Loading dashboard...
      </div>
    </div>
  );
}

function PageError({ message }: { message: string }) {
  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="rounded-md border border-tone-error/30 bg-tone-error/10 p-6 text-sm text-tone-error shadow-card">
        {message}
      </div>
    </div>
  );
}
