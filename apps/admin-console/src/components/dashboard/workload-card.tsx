import { useTranslation } from "react-i18next";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { StatCard } from "@/components/ui/stat-card";
import { DASHBOARD_REFETCH_MS } from "@/lib/query/hooks/dashboard";
import type { RunCounts } from "@/lib/query/hooks/run-counts";

/** Discriminated state for the workload card. The dashboard derives
 *  this from `useRunCountsQuery` so the card visually distinguishes
 *  loading / route-absent / store-unavailable / generic-error / ready.
 *  The reviewer flagged that the older `disabled` state collapsed two
 *  distinct backend conditions (no route vs store unwired) into one
 *  notice, hiding diagnostic info for operators.
 *
 *  - `loading`            — query in flight, render skeleton tiles
 *  - `route_absent`       — 404 from `/v1/runs/summary` (old build)
 *  - `store_unavailable`  — 503 (run store unwired / unhealthy)
 *  - `error`              — anything else (auth, network, 5xx); surface
 *  - `ready`              — counts available, render the live workload */
export type WorkloadState =
  | { kind: "loading" }
  | { kind: "route_absent" }
  | { kind: "store_unavailable" }
  | { kind: "error"; message: string }
  | { kind: "ready"; counts: RunCounts };

/** Live workload card — the operator's first signal that the system is
 *  doing work, blocked on a decision, or has a backlog forming.
 *  Waiting is the visual hero: HITL is the only one a human must act
 *  on, so it gets larger type, a warn-tinted card, and an aria-live
 *  region so the count is announced when it goes from 0 to N. */
export function WorkloadCard({ state }: { state: WorkloadState }) {
  const { t } = useTranslation();

  // All variants share `aria-live="polite"` so transitions between
  // loading → ready, ready → error, etc. are announced once. The
  // skeleton variant additionally sets `aria-busy="true"` so SR users
  // know the data isn't final yet.
  if (state.kind === "loading") {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
        aria-busy="true"
        aria-label={t("dashboard.workload.loading")}
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.workload.title")}</h2>
        <div className="mt-4 grid gap-3 sm:grid-cols-[2fr_1fr_1fr]">
          <div className="h-20 animate-pulse rounded-sm bg-soft" />
          <div className="h-14 animate-pulse rounded-sm bg-soft" />
          <div className="h-14 animate-pulse rounded-sm bg-soft" />
        </div>
      </div>
    );
  }

  if (state.kind === "route_absent") {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.workload.title")}</h2>
        <FeatureDisabledNotice
          title={t("dashboard.workload.routeAbsentTitle")}
          configHint={t("dashboard.workload.routeAbsentHint")}
        />
      </div>
    );
  }

  if (state.kind === "store_unavailable") {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.workload.title")}</h2>
        <FeatureDisabledNotice
          title={t("dashboard.workload.unavailableTitle")}
          configHint={t("dashboard.workload.unavailableHint")}
        />
      </div>
    );
  }

  if (state.kind === "error") {
    return (
      <div
        className="rounded-sm border border-tone-error/30 bg-tone-error/[0.06] p-5 shadow-card"
        aria-live="polite"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.workload.title")}</h2>
        <div className="mt-3 text-sm text-tone-error">
          <span className="font-medium">{t("dashboard.workload.errorTitle")}: </span>
          {state.message}
        </div>
      </div>
    );
  }

  const { counts: runCounts } = state;
  const idle = runCounts.running === 0 && runCounts.waiting === 0 && runCounts.created === 0;
  const hitlAlert = runCounts.waiting > 0;
  // Tint the entire card warn when HITL is queued — color-only signals
  // miss color-blind operators, the border + tinted surface adds shape.
  const cardClass = hitlAlert
    ? "rounded-sm border border-tone-warn/40 bg-tone-warn/[0.06] p-5 shadow-card"
    : "rounded-sm border border-line bg-surface p-5 shadow-card";
  return (
    <div className={cardClass} aria-live="polite">
      <div className="flex items-baseline justify-between">
        <h2 className="text-lg font-semibold text-fg-strong">
          {hitlAlert && <span className="sr-only">{t("dashboard.workload.actionNeeded")}: </span>}
          {t("dashboard.workload.title")}
        </h2>
        <span className="text-xs text-fg-soft">
          {t("dashboard.workload.sub", { seconds: DASHBOARD_REFETCH_MS / 1000 })}
        </span>
      </div>
      {/* Waiting is the hero — operators must see HITL queue at a glance.
          Running + created fall back to a smaller right column. */}
      <div className="mt-4 grid gap-3 sm:grid-cols-[2fr_1fr_1fr]">
        <StatCard
          layout="compact"
          emphasis="lg"
          label={t("dashboard.workload.waiting")}
          value={runCounts.waiting.toLocaleString()}
          tone={hitlAlert ? "warn" : "neutral"}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.workload.running")}
          value={runCounts.running.toLocaleString()}
          tone={runCounts.running > 0 ? "success" : "neutral"}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.workload.created")}
          value={runCounts.created.toLocaleString()}
        />
      </div>
      {idle && <p className="mt-3 text-xs text-fg-soft">{t("dashboard.workload.idle")}</p>}
    </div>
  );
}
