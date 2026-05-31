import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import { type McpServerRecord, type ProviderRecord, type SystemInfo } from "@/lib/config-api";
import { formatActor, isAgentActor, type AuditEvent, type AuditPage } from "@/lib/audit-log";
import { adminRoutes } from "@/lib/routes";
import { formatRelativeTime } from "@/lib/format-time";
import { PageHeader } from "@/components/ui/page-header";
import { Eyebrow } from "@/components/ui/eyebrow";
import { Pill } from "@/components/ui/pill";
import { StatCard } from "@/components/ui/stat-card";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { LoadError } from "@/components/ui/load-error";
import { TimeRangeSwitcher, type TimeRange } from "@/components/ui/time-range-switcher";
import { WorkloadCard, type WorkloadState } from "@/components/dashboard/workload-card";
import {
  RuntimeActivityCard,
  type RuntimeActivityState,
} from "@/components/dashboard/runtime-activity-card";
import { useDashboardQuery } from "@/lib/query/hooks/dashboard";
import { useRunCountsQuery } from "@/lib/query/hooks/run-counts";
import { useRuntimeStatsQuery } from "@/lib/query/hooks/runtime-stats";

export function DashboardPage() {
  const { t } = useTranslation();
  const [range, setRange] = useState<TimeRange>("24h");
  const dashboardQuery = useDashboardQuery(range);
  const runCountsQuery = useRunCountsQuery();
  const runtimeStatsQuery = useRuntimeStatsQuery();
  const data = dashboardQuery.data ?? null;
  const error = dashboardQuery.error
    ? dashboardQuery.error instanceof Error
      ? dashboardQuery.error.message
      : String(dashboardQuery.error)
    : null;

  if (error) {
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <LoadError message={error} onRetry={() => void dashboardQuery.refetch()} />
      </div>
    );
  }
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
    degraded,
  } = data;

  // Workload state — `useRunCountsQuery` now returns a discriminated
  // result so the card can tell route-absent (old build / wrong route
  // layer) from store-unavailable (runtime unhealthy).
  const workloadState: WorkloadState = runCountsQuery.error
    ? { kind: "error", message: errorMessage(runCountsQuery.error) }
    : runCountsQuery.data === undefined
      ? { kind: "loading" }
      : runCountsQuery.data.kind === "ok"
        ? { kind: "ready", counts: runCountsQuery.data.counts }
        : runCountsQuery.data.kind === "route_absent"
          ? { kind: "route_absent" }
          : { kind: "store_unavailable" };

  const runtimeActivityState: RuntimeActivityState = runtimeStatsQuery.error
    ? { kind: "error", message: errorMessage(runtimeStatsQuery.error) }
    : runtimeStatsQuery.data === undefined
      ? { kind: "loading" }
      : runtimeStatsQuery.data === null
        ? { kind: "disabled" }
        : { kind: "ready", snapshots: runtimeStatsQuery.data };

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
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
          <CountRibbon
            stats={[
              {
                label: t("dashboard.counters.agents"),
                count: agents.length,
                to: adminRoutes.agents,
                degraded: degraded.agents,
              },
              {
                label: t("dashboard.counters.skills"),
                count: capabilities.skills.length,
                to: adminRoutes.skills,
              },
              {
                label: t("dashboard.counters.models"),
                count: models.length,
                to: adminRoutes.models,
                degraded: degraded.models,
              },
              {
                label: t("dashboard.counters.providers"),
                count: providers.length,
                to: adminRoutes.providers,
                degraded: degraded.providers,
              },
              {
                label: t("dashboard.counters.mcp"),
                count: mcpServers.length,
                to: adminRoutes.mcpServers,
                degraded: degraded.mcpServers,
              },
              { label: t("dashboard.counters.tools"), count: capabilities.tools.length },
            ]}
          />
        }
      />

      <section className="mb-4">
        <WorkloadCard state={workloadState} />
      </section>

      <section className="mb-4">
        <RuntimeActivityCard state={runtimeActivityState} />
      </section>

      <section className="grid gap-4 lg:grid-cols-2">
        <ActivityTimeline
          auditPage={auditPage}
          disabled={auditDisabled}
          range={range}
          onRangeChange={setRange}
        />
        <HealthCard
          providers={providers}
          mcpServers={mcpServers}
          providersDegraded={!!degraded.providers}
          mcpDegraded={!!degraded.mcpServers}
        />
      </section>

      {systemInfo && (
        <section className="mt-4">
          <SystemCard info={systemInfo} />
        </section>
      )}
    </div>
  );
}

function HealthCard({
  providers,
  mcpServers,
  providersDegraded = false,
  mcpDegraded = false,
}: {
  providers: ProviderRecord[];
  mcpServers: McpServerRecord[];
  providersDegraded?: boolean;
  mcpDegraded?: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div className="rounded-sm border border-line bg-surface p-5 shadow-card">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.health.title")}</h2>
        <span className="text-sm text-fg-soft">
          {t("dashboard.health.meta", { providers: providers.length, mcp: mcpServers.length })}
        </span>
      </div>

      <div className="mt-4">
        <div className="flex items-center gap-2">
          <Eyebrow>{t("dashboard.health.providers")}</Eyebrow>
          {providersDegraded && (
            <Pill tone="warn" dot>
              {t("dashboard.health.degraded")}
            </Pill>
          )}
        </div>
        {providersDegraded ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.degradedHint")}</p>
        ) : providers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.noProviders")}</p>
        ) : (
          <ul className="mt-2 space-y-1.5">
            {providers.map((p) => (
              <li
                key={p.id}
                className="flex items-center justify-between gap-3 rounded-sm border border-line bg-soft px-3 py-2"
              >
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
        <div className="flex items-center gap-2">
          <Eyebrow>{t("dashboard.health.mcpServers")}</Eyebrow>
          {mcpDegraded && (
            <Pill tone="warn" dot>
              {t("dashboard.health.degraded")}
            </Pill>
          )}
        </div>
        {mcpDegraded ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.degradedHint")}</p>
        ) : mcpServers.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">{t("dashboard.health.noMcp")}</p>
        ) : (
          <ul className="mt-2 space-y-1.5">
            {mcpServers.map((s) => (
              <li
                key={s.id}
                className="flex items-center justify-between gap-3 rounded-sm border border-line bg-soft px-3 py-2"
              >
                <div className="min-w-0">
                  <div className="font-mono text-sm text-fg-strong">{s.id}</div>
                  <div className="text-xs text-fg-soft">
                    {s.transport} {s.command ? `· ${s.command}` : ""}
                  </div>
                </div>
                <Pill tone={s.restart_policy?.enabled ? "success" : "neutral"}>
                  {s.restart_policy?.enabled
                    ? t("dashboard.health.autoRestart")
                    : t("dashboard.health.manual")}
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
  // System metadata is footer material — operators only consult it when
  // something else surfaced a problem. It stays compact and at the
  // bottom of the page so the upper viewport is reserved for live
  // signals (workload, activity, audit, health).
  return (
    <div className="rounded-sm border border-line bg-surface p-5 shadow-card">
      <Eyebrow>{t("dashboard.system.title")}</Eyebrow>
      <div className="mt-3 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <StatCard
          layout="compact"
          label={t("dashboard.system.version")}
          value={info.version}
          mono
        />
        <StatCard
          layout="compact"
          label={t("dashboard.system.uptime")}
          value={formatUptime(info.uptime_seconds)}
          mono={false}
        />
        {info.scope_id && (
          <StatCard
            layout="compact"
            label={t("dashboard.system.scope")}
            value={info.scope_id}
            mono
          />
        )}
        <StatCard
          layout="compact"
          label={t("dashboard.system.configStore")}
          value={
            info.config_store_enabled ? t("dashboard.system.wired") : t("dashboard.system.none")
          }
          tone={info.config_store_enabled ? "success" : "neutral"}
          mono={false}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.system.auditLog")}
          value={info.audit_log_enabled ? t("dashboard.system.on") : t("dashboard.system.off")}
          tone={info.audit_log_enabled ? "success" : "neutral"}
          mono={false}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.system.runtimeStats")}
          value={info.runtime_stats_enabled ? t("dashboard.system.on") : t("dashboard.system.off")}
          tone={info.runtime_stats_enabled ? "success" : "neutral"}
          mono={false}
        />
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
  range,
  onRangeChange,
}: {
  auditPage: AuditPage | null;
  disabled: boolean;
  range: TimeRange;
  onRangeChange: (next: TimeRange) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="rounded-sm border border-line bg-surface p-5 shadow-card">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.activity.title")}</h2>
        {/* Range switcher lives here, not in PageHeader: it only
            filters the audit window, and putting it page-global made
            operators think workload / runtime stats also re-windowed. */}
        {!disabled && <TimeRangeSwitcher value={range} onChange={onRangeChange} />}
      </div>
      {!disabled && (
        <div className="mt-1">
          <Link
            to={adminRoutes.auditLog}
            className="text-xs font-medium text-link transition-colors hover:text-link-hover"
          >
            {t("dashboard.activity.viewAll")}
          </Link>
        </div>
      )}
      {disabled ? (
        <FeatureDisabledNotice
          title={t("dashboard.activity.disabledTitle")}
          configHint={t("dashboard.activity.disabledHint")}
          docsUrl="docs/architecture/admin-audit-log.md"
        />
      ) : !auditPage || auditPage.items.length === 0 ? (
        <p className="mt-4 text-sm text-fg-soft">{t("dashboard.activity.empty")}</p>
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
        "flex items-start gap-3 rounded-sm border-l-2 px-2 py-1",
        fromAgent ? "border-agent-stripe bg-agent-tint" : "border-transparent",
      ].join(" ")}
    >
      <span
        aria-hidden
        className={`mt-1.5 inline-block h-2 w-2 shrink-0 rounded-pill ${dotClass}`}
      />
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

function CountRibbon({
  stats,
}: {
  stats: { label: string; count: number; to?: string; degraded?: boolean }[];
}) {
  const { t } = useTranslation();
  // Hidden on narrow screens: the sidebar already shows these counts,
  // and crowding them into PageHeader at mobile widths forces the title
  // to truncate without buying the operator anything.
  return (
    <div className="hidden flex-wrap items-center gap-x-4 gap-y-1 font-mono text-xs text-fg-soft md:flex">
      {stats.map((s, idx) => {
        // When the underlying list failed, show "?" instead of a
        // misleading 0 — operator should distinguish "no providers" from
        // "providers list 5xx'd".
        const inner = s.degraded ? (
          <span className="tabular-nums" title={t("dashboard.health.degradedHint")}>
            <span className="font-semibold text-tone-warn">?</span>{" "}
            <span className="text-fg-soft">{s.label}</span>
          </span>
        ) : (
          <span className="tabular-nums">
            <span className="font-semibold text-fg-strong">{s.count.toLocaleString()}</span>{" "}
            <span className="text-fg-soft">{s.label}</span>
          </span>
        );
        return (
          <span key={s.label} className="flex items-center gap-x-4">
            {idx > 0 && (
              <span aria-hidden className="text-fg-faint">
                ·
              </span>
            )}
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
  // Skeleton mirrors the real layout so the page doesn't reflow on
  // load: workload hero row + activity 4-tile row + two columns
  // (audit + health). Operators perceive a faster first paint than a
  // single "Loading…" line.
  return (
    <div
      className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8"
      aria-busy="true"
      aria-label="Loading dashboard"
    >
      <div className="mb-4 h-7 w-40 animate-pulse rounded-sm bg-soft" />
      <div className="mb-4 grid gap-3 rounded-sm border border-line bg-surface p-5 shadow-card sm:grid-cols-[2fr_1fr_1fr]">
        <SkeletonTile height="h-20" />
        <SkeletonTile />
        <SkeletonTile />
      </div>
      <div className="mb-4 grid gap-3 rounded-sm border border-line bg-surface p-5 shadow-card sm:grid-cols-2 lg:grid-cols-4">
        <SkeletonTile />
        <SkeletonTile />
        <SkeletonTile />
        <SkeletonTile />
      </div>
      <div className="grid gap-4 lg:grid-cols-2">
        <div className="h-40 animate-pulse rounded-sm border border-line bg-surface shadow-card" />
        <div className="h-40 animate-pulse rounded-sm border border-line bg-surface shadow-card" />
      </div>
    </div>
  );
}

function SkeletonTile({ height = "h-14" }: { height?: string }) {
  return <div className={`${height} animate-pulse rounded-sm bg-soft`} />;
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
