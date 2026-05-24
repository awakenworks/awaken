import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link, useSearchParams } from "react-router";
import { evalApi, type EvalRunSummary } from "@/lib/api";
import { EmptyState } from "@/components/ui/empty-state";
import { EvalPrivacyNotice } from "@/components/ui/eval-privacy-notice";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { LoadError } from "@/components/ui/load-error";
import { PageHeader } from "@/components/ui/page-header";
import { Pill } from "@/components/ui/pill";
import { adminRoutes } from "@/lib/routes";

/** `/eval-runs` — list of eval runs, optionally filtered by dataset.
 *  Backed by `GET /v1/eval/runs?dataset_id=…`. */
export function EvalRunsPage() {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const datasetId = params.get("dataset") || undefined;
  const runsQuery = useQuery({
    queryKey: ["eval", "runs", datasetId ?? "*"] as const,
    queryFn: () => evalApi.listRuns({ dataset_id: datasetId, limit: 50 }),
  });

  if (runsQuery.error) {
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <LoadError
          message={
            runsQuery.error instanceof Error
              ? runsQuery.error.message
              : String(runsQuery.error)
          }
          onRetry={() => void runsQuery.refetch()}
        />
      </div>
    );
  }

  const data = runsQuery.data;
  const featureDisabled = data === null;
  const runs: EvalRunSummary[] = data?.runs ?? [];

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <PageHeader
        title={t("evalRuns.title")}
        count={featureDisabled ? undefined : runs.length}
        description={datasetId ? `${t("evalRuns.columns.dataset")} · ${datasetId}` : undefined}
      />

      {!featureDisabled && <EvalPrivacyNotice compact />}

      {featureDisabled && (
        <FeatureDisabledNotice
          title={t("evalRuns.disabledTitle")}
          configHint={t("evalRuns.disabledHint")}
        />
      )}

      {!featureDisabled && (
        <div className="overflow-x-auto rounded-sm border border-line bg-surface shadow-card">
          {runsQuery.isPending ? (
            <div className="p-8 text-sm text-fg-soft">{t("common.loading")}</div>
          ) : runs.length === 0 ? (
            <EmptyState title={t("evalRuns.none")} />
          ) : (
            <table className="min-w-full">
              <thead className="bg-soft text-left text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
                <tr>
                  <th className="px-5 py-3">{t("evalRuns.columns.runId")}</th>
                  <th className="px-5 py-3">{t("evalRuns.columns.dataset")}</th>
                  <th className="px-5 py-3">{t("evalRuns.columns.mode")}</th>
                  <th className="px-5 py-3 text-right">{t("evalRuns.columns.fixtures")}</th>
                  <th className="px-5 py-3 text-right">{t("evalRuns.columns.failures")}</th>
                  <th className="px-5 py-3">{t("evalRuns.columns.started")}</th>
                </tr>
              </thead>
              <tbody>
                {runs.map((r) => {
                  // The wire's `failed_count` is the canonical signal —
                  // when it's missing (older server build, or a
                  // partial-run schema where pending items haven't yet
                  // been written) we *don't* derive it as
                  // `item_count - passed_count`, because that conflates
                  // pending with failed and reads pessimistic. Show a
                  // "summary unavailable" indicator instead.
                  const knownFailures = typeof r.failed_count === "number";
                  const failures = r.failed_count ?? 0;
                  const pending = knownFailures
                    ? Math.max(r.item_count - r.passed_count - failures, 0)
                    : 0;
                  return (
                  <tr key={r.id} className="border-t border-line text-sm text-fg">
                    <td className="px-5 py-4">
                      <Link
                        to={adminRoutes.evalRun(r.id)}
                        className="font-mono text-sm font-medium text-link transition-colors hover:text-link-hover"
                      >
                        {r.id.slice(0, 12)}
                      </Link>
                    </td>
                    <td className="px-5 py-4">
                      <Link
                        to={adminRoutes.dataset(r.dataset_id)}
                        className="font-mono text-xs text-fg-soft hover:text-fg-strong"
                      >
                        {r.dataset_id}
                      </Link>
                    </td>
                    <td className="px-5 py-4">
                      <Pill tone={r.execution_mode === "live" ? "info" : "neutral"}>
                        {r.execution_mode ?? "—"}
                      </Pill>
                    </td>
                    <td className="px-5 py-4 text-right font-mono tabular-nums">
                      {r.item_count}
                    </td>
                    <td className="px-5 py-4 text-right font-mono tabular-nums">
                      {knownFailures ? (
                        <>
                          <span
                            className={failures > 0 ? "text-tone-error" : "text-fg-soft"}
                          >
                            {failures}
                          </span>
                          {pending > 0 && (
                            <span className="ml-2 text-[10px] text-fg-faint">
                              {t("evalRuns.pendingItems", { count: pending })}
                            </span>
                          )}
                        </>
                      ) : (
                        <span
                          className="text-fg-faint"
                          title={t("evalRuns.summaryUnavailableHint")}
                        >
                          {t("evalRuns.summaryUnavailable")}
                        </span>
                      )}
                    </td>
                    <td className="px-5 py-4 text-xs text-fg-soft">
                      {new Date(r.started_at_secs * 1000).toLocaleString()}
                    </td>
                  </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      )}
    </div>
  );
}
