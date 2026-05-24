import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link, useParams } from "react-router";
import { classifyEvalError, evalApi } from "@/lib/api";
import { EmptyState } from "@/components/ui/empty-state";
import { EvalPrivacyNotice } from "@/components/ui/eval-privacy-notice";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { JsonInspector } from "@/components/ui/json-inspector";
import { LoadError } from "@/components/ui/load-error";
import { PageHeader } from "@/components/ui/page-header";
import { Pill } from "@/components/ui/pill";
import { StatCard } from "@/components/ui/stat-card";
import { adminRoutes } from "@/lib/routes";

/** `/eval-runs/:id` — one eval run's metrics + raw JSON.
 *  Backed by `GET /v1/eval/runs/:id`. */
export function EvalRunDetailPage() {
  const { id = "" } = useParams<{ id: string }>();
  const { t } = useTranslation();
  // Parent list query disambiguates 404 — see `classifyEvalError`.
  const runsListQuery = useQuery({
    queryKey: ["eval", "runs"] as const,
    queryFn: () => evalApi.listRuns({ limit: 1 }),
  });
  const listAvailable: boolean | "unknown" = runsListQuery.isPending
    ? "unknown"
    : runsListQuery.data === null
      ? false
      : runsListQuery.error
        ? "unknown"
        : true;

  const runQuery = useQuery({
    queryKey: ["eval", "run", id] as const,
    queryFn: () => evalApi.getRun(id),
    enabled: id.trim().length > 0,
  });

  if (runQuery.error) {
    // Wait for the parent list query before classifying 404 — see
    // dataset detail for the same rationale.
    if (listAvailable === "unknown") {
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <div
            aria-live="polite"
            aria-busy="true"
            className="rounded-sm border border-line bg-surface p-8 text-sm text-fg-soft shadow-card"
          >
            {t("common.loading")}
          </div>
        </div>
      );
    }
    const cat = classifyEvalError(runQuery.error, { listAvailable });
    if (cat === "disabled") {
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <FeatureDisabledNotice
            title={t("evalRuns.disabledTitle")}
            configHint={t("evalRuns.disabledHint")}
          />
        </div>
      );
    }
    if (cat === "store_error") {
      // Transient runtime issue — error tinting + retry, not the
      // disabled-feature chrome.
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <LoadError
            message={`${t("evalRuns.storeUnreachableTitle")} — ${t("evalRuns.storeUnreachableHint")}`}
            onRetry={() => void runQuery.refetch()}
          />
        </div>
      );
    }
    if (cat === "not_found") {
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <EmptyState
            title={t("evalRuns.notFoundTitle", { id })}
            description={t("evalRuns.notFoundHint")}
            actions={
              <Link
                to={adminRoutes.evalRuns}
                className="inline-flex h-9 items-center rounded-sm border border-line-strong px-3 text-sm font-medium text-fg transition hover:bg-soft"
              >
                ← {t("evalRuns.title")}
              </Link>
            }
          />
        </div>
      );
    }
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <LoadError
          message={
            runQuery.error instanceof Error ? runQuery.error.message : String(runQuery.error)
          }
          onRetry={() => void runQuery.refetch()}
        />
      </div>
    );
  }

  if (runQuery.isPending || !runQuery.data) {
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <div className="rounded-sm border border-line bg-surface p-8 text-sm text-fg-soft shadow-card">
          {t("common.loading")}
        </div>
      </div>
    );
  }

  const { run } = runQuery.data;
  const items = Array.isArray(run.items) ? run.items : [];
  const total = items.length;
  // Tri-state per item: passed (report.passed === true), failed
  // (report.passed === false), pending (no report yet, e.g. partial
  // live run). Earlier draft folded pending into failed which made
  // an in-progress run look catastrophic.
  const passed = items.filter((it) => it.report?.passed === true).length;
  const failed = items.filter((it) => it.report?.passed === false).length;
  const pending = total - passed - failed;
  // Pass rate is denominated on *completed* items only — a run that's
  // still streaming shouldn't have its pass rate dragged toward 0%.
  const completed = passed + failed;
  const passRate = completed > 0 ? (passed / completed) * 100 : 0;
  // Typed check + non-negative clamp so 0-epoch sentinels don't read
  // as "missing" and out-of-order timestamps don't render negative ms.
  const durationMs = (() => {
    if (typeof run.started_at_secs !== "number") return null;
    if (typeof run.ended_at_secs !== "number") return null;
    return Math.max(0, (run.ended_at_secs - run.started_at_secs) * 1000);
  })();

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <PageHeader
        eyebrow={
          <Link to={adminRoutes.evalRuns} className="hover:text-fg-strong">
            ← {t("evalRuns.title")}
          </Link>
        }
        title={id}
        description={
          <span className="font-mono text-xs">
            {t("evalRuns.columns.dataset")} ·{" "}
            <Link
              to={adminRoutes.dataset(run.dataset_id)}
              className="text-link hover:text-link-hover"
            >
              {run.dataset_id}
            </Link>
          </span>
        }
        actions={
          <Pill tone={run.execution_mode === "live" ? "info" : "neutral"}>
            {t("evalRuns.mode")}: {run.execution_mode ?? "—"}
          </Pill>
        }
      />

      <section className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          layout="compact"
          label={t("evalRuns.fixtures")}
          value={total.toLocaleString()}
        />
        <StatCard
          layout="compact"
          label={t("evalRuns.passRate")}
          value={completed > 0 ? `${passRate.toFixed(1)}%` : "—"}
          sub={
            pending > 0
              ? t("evalRuns.pendingItems", { count: pending })
              : undefined
          }
          tone={
            completed === 0
              ? "neutral"
              : passRate >= 95
                ? "success"
                : passRate >= 80
                  ? "info"
                  : "warn"
          }
        />
        <StatCard
          layout="compact"
          label={t("evalRuns.failures")}
          value={failed.toLocaleString()}
          tone={failed > 0 ? "error" : "neutral"}
        />
        <StatCard
          layout="compact"
          label={t("evalRuns.finished")}
          value={
            run.ended_at_secs
              ? new Date(run.ended_at_secs * 1000).toLocaleTimeString()
              : "—"
          }
          sub={durationMs !== null ? `${Math.round(durationMs)}ms total` : undefined}
        />
      </section>

      {run.items && Array.isArray(run.items) && run.items.length > 0 && (
        <section className="mt-6">
          <h2 className="mb-2 text-sm font-semibold uppercase tracking-[0.18em] text-fg-faint">
            items
          </h2>
          <EvalPrivacyNotice />
          <JsonInspector value={run.items} title={`run-${id}-items`} />
        </section>
      )}
    </div>
  );
}
