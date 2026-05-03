import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
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
  formatActor,
  isAgentActor,
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
import {
  TimeRangeSwitcher,
  TIME_RANGE_SECONDS,
  type TimeRange,
} from "@/components/ui/time-range-switcher";

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
  const { t } = useTranslation();
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [range, setRange] = useState<TimeRange>("24h");

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const sinceMs = Date.now() - TIME_RANGE_SECONDS[range] * 1000;
        const since = new Date(sinceMs).toISOString();
        const auditPromise = configApi
          .auditLog({ limit: 50, since })
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
  }, [range]);

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
        title={
          <>
            {t("dashboard.title")}
            <span className="ml-3 align-middle text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
              {t("evals.modeProduction")}
            </span>
          </>
        }
        actions={
          <div className="flex items-center gap-3">
            <TimeRangeSwitcher value={range} onChange={setRange} />
            <CountRibbon stats={[
              { label: t("dashboard.counters.agents"), count: agents.length, to: adminRoutes.agents },
              { label: t("dashboard.counters.skills"), count: capabilities.skills.length, to: adminRoutes.skills },
              { label: t("dashboard.counters.models"), count: models.length, to: adminRoutes.models },
              { label: t("dashboard.counters.providers"), count: providers.length, to: adminRoutes.providers },
              { label: t("dashboard.counters.mcp"), count: mcpServers.length, to: adminRoutes.mcpServers },
              { label: t("dashboard.counters.tools"), count: capabilities.tools.length },
            ]} />
          </div>
        }
      />

      <section className="grid gap-4 lg:grid-cols-2">
        <ActivityTimeline auditPage={auditPage} disabled={auditDisabled} />
        <HealthCard providers={providers} mcpServers={mcpServers} />
      </section>

      <section className="mt-4 rounded-md border border-line bg-surface p-4 shadow-card">
        <div className="flex items-baseline justify-between gap-4">
          <h3 className="text-sm font-semibold text-fg-strong">
            {t("dashboard.refGraph.title")}
            <span className="ml-2 font-normal text-fg-soft">{t("dashboard.refGraph.sub")}</span>
          </h3>
        </div>
        <div className="mt-3">
          <DependencyGraph
            agents={agents}
            models={models}
            providers={providers}
          />
        </div>
        {agents.length > 8 && (
          <div className="mt-3 text-right text-xs text-fg-soft">
            {t("dashboard.refGraph.seeAll", { shown: 8, total: agents.length.toLocaleString() })}{" "}
            <Link to={adminRoutes.agents} className="font-medium text-link hover:text-link-hover">
              {t("dashboard.refGraph.viewAll")}
            </Link>
          </div>
        )}
      </section>

      {systemInfo && (
        <section className="mt-4">
          <SystemCard info={systemInfo} />
        </section>
      )}

      <section className="mt-4 grid gap-4 lg:grid-cols-2">
        <div className="rounded-md border border-line bg-surface p-5 shadow-card">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">{t("dashboard.plugins.title")}</h3>
            <span className="text-sm text-fg-soft">
              {t("dashboard.plugins.meta", { count: capabilities.plugins.length })}
            </span>
          </div>
          {capabilities.plugins.length === 0 ? (
            <p className="mt-4 text-sm text-fg-soft">{t("dashboard.plugins.noConfig")}</p>
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
                      ? t("dashboard.plugins.noConfig")
                      : t("dashboard.plugins.configSections", {
                          sections: plugin.config_schemas.map((s) => s.key).join(", "),
                        })}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="rounded-md border border-line bg-surface p-5 shadow-card">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">{t("dashboard.tools.title")}</h3>
            <span className="text-sm text-fg-soft">{t("dashboard.tools.meta", { count: capabilities.tools.length })}</span>
          </div>
          {capabilities.tools.length === 0 ? (
            <p className="mt-4 text-sm text-fg-soft">{t("dashboard.tools.empty")}</p>
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
  const { t } = useTranslation();
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-fg-strong">{t("dashboard.health.title")}</h3>
        <span className="text-sm text-fg-soft">
          {t("dashboard.health.meta", { providers: providers.length, mcp: mcpServers.length })}
        </span>
      </div>

      <div className="mt-4">
        <Eyebrow>{t("dashboard.health.providers")}</Eyebrow>
        {providers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.noProviders")}</p>
        ) : (
          <ul className="mt-2 space-y-1.5">
            {providers.map((p) => (
              <li key={p.id} className="flex items-center justify-between gap-3 rounded-md border border-line bg-soft px-3 py-2">
                <div className="min-w-0">
                  <div className="font-mono text-sm text-fg-strong">{p.id}</div>
                  <div className="text-xs text-fg-soft">{p.adapter}</div>
                </div>
                <Pill tone={p.has_api_key ? "success" : "warn"}>
                  {p.has_api_key ? t("dashboard.health.keySet") : t("dashboard.health.noKey")}
                </Pill>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="mt-5">
        <Eyebrow>{t("dashboard.health.mcpServers")}</Eyebrow>
        {mcpServers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.noMcp")}</p>
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
                  {s.restart_policy?.enabled ? t("dashboard.health.autoRestart") : t("dashboard.health.manual")}
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
  const { t } = useTranslation();
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <Eyebrow>{t("dashboard.system.title")}</Eyebrow>
      <div className="mt-3 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <SystemStat label={t("dashboard.system.version")} value={info.version} mono />
        <SystemStat label={t("dashboard.system.uptime")} value={formatUptime(info.uptime_seconds)} />
        <SystemStat
          label={t("dashboard.system.configStore")}
          value={info.config_store_enabled ? t("dashboard.system.wired") : t("dashboard.system.none")}
          tone={info.config_store_enabled ? "success" : "neutral"}
        />
        <SystemStat
          label={t("dashboard.system.auditLog")}
          value={info.audit_log_enabled ? t("dashboard.system.on") : t("dashboard.system.off")}
          tone={info.audit_log_enabled ? "success" : "neutral"}
        />
        <SystemStat
          label={t("dashboard.system.runtimeStats")}
          value={info.runtime_stats_enabled ? t("dashboard.system.on") : t("dashboard.system.off")}
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
  const { t } = useTranslation();
  return (
    <div className="rounded-md border border-line bg-surface p-5 shadow-card">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-fg-strong">{t("dashboard.activity.title")}</h3>
        {!disabled && (
          <Link
            to={adminRoutes.auditLog}
            className="text-xs font-medium text-link transition-colors hover:text-link-hover"
          >
            {t("dashboard.activity.viewAll")}
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
  const fromAgent = isAgentActor(event.actor);
  const actorMeta = formatActor(event.actor || "system");
  const actorLabel = actorMeta.label
    ? actorMeta.label
    : actorMeta.hash === "system"
      ? "system"
      : actorMeta.hash.slice(0, 6);
  return (
    <li
      className={[
        "flex items-start gap-3 rounded-md border-l-2 px-2 py-1",
        fromAgent
          ? "border-agent-stripe bg-agent-tint"
          : "border-transparent",
      ].join(" ")}
    >
      <span aria-hidden className={`mt-1.5 inline-block h-2 w-2 shrink-0 rounded-pill ${dotClass}`} />
      <div className="min-w-0 flex-1">
        <div className={`text-sm ${fromAgent ? "text-agent-fg" : "text-fg"}`}>
          <span className="font-medium text-fg-strong">{event.action}</span>{" "}
          <span className="font-mono text-fg-soft">{event.resource}</span>
        </div>
        <div
          title={event.actor || "system"}
          className={`mt-0.5 text-xs ${fromAgent ? "text-agent-fg/80" : "text-fg-faint"}`}
        >
          <span className={fromAgent ? "" : "font-mono"}>{actorLabel}</span>
          {" · "}
          {formatRelativeTime(Date.parse(event.ts))}
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

function CountRibbon({ stats }: { stats: { label: string; count: number; to?: string }[] }) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-xs text-fg-soft">
      {stats.map((s, idx) => {
        const inner = (
          <span className="tabular-nums">
            <span className="font-semibold text-fg-strong">{s.count.toLocaleString()}</span>{" "}
            <span className="text-fg-soft">{s.label}</span>
          </span>
        );
        return (
          <span key={s.label} className="flex items-center gap-x-4">
            {idx > 0 && <span aria-hidden className="text-fg-faint">·</span>}
            {s.to ? (
              <Link to={s.to} className="transition-colors hover:text-fg-strong">
                {inner}
              </Link>
            ) : (
              inner
            )}
          </span>
        );
      })}
    </div>
  );
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
