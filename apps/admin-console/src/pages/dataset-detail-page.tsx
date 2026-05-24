import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link, useNavigate, useParams } from "react-router";
import {
  ConfigApiError,
  classifyEvalError,
  evalApi,
  type EvalErrorCategory,
  type Fixture,
} from "@/lib/api";
import { useToast } from "@/components/toast-provider";
import { EmptyState } from "@/components/ui/empty-state";
import { EvalPrivacyNotice } from "@/components/ui/eval-privacy-notice";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { LoadError } from "@/components/ui/load-error";
import { PageHeader } from "@/components/ui/page-header";
import { Pill } from "@/components/ui/pill";
import { adminRoutes } from "@/lib/routes";

/** `/datasets/:id` — drill into one dataset's fixtures and trigger evals. */
export function DatasetDetailPage() {
  const { id = "" } = useParams<{ id: string }>();
  const { t } = useTranslation();
  const navigate = useNavigate();
  const toast = useToast();
  const queryClient = useQueryClient();
  const [running, setRunning] = useState(false);

  // Lightweight parent-list query so we can disambiguate 404 on the
  // detail call: if the list resolves, a 404 on `/datasets/:id` means
  // *that id* is missing (not the whole feature). If the list returned
  // null (route absent), we propagate the disabled story consistently.
  const datasetsListQuery = useQuery({
    queryKey: ["eval", "datasets"] as const,
    queryFn: evalApi.listDatasets,
  });
  const listAvailable: boolean | "unknown" = datasetsListQuery.isPending
    ? "unknown"
    : datasetsListQuery.data === null
      ? false
      : datasetsListQuery.error
        ? "unknown"
        : true;

  const datasetQuery = useQuery({
    queryKey: ["eval", "dataset", id] as const,
    queryFn: () => evalApi.getDataset(id),
    enabled: id.trim().length > 0,
  });

  const triggerRun = useMutation({
    mutationFn: () =>
      evalApi.startRun({
        dataset_id: id,
        // Scripted mode replays the recorded provider_script; safest
        // default for a "run now" button. Live mode requires picking
        // model bindings — surface that in the dedicated runner UI.
        mode: "scripted",
      }),
    onMutate: () => setRunning(true),
    onSettled: () => setRunning(false),
    onSuccess: (response) => {
      toast.success(t("evalRuns.triggeredFor", { dataset: id }));
      void queryClient.invalidateQueries({ queryKey: ["eval", "runs"] });
      navigate(adminRoutes.evalRun(response.run.id));
    },
    onError: (err) => {
      const cat = classifyEvalError(err, { listAvailable });
      const msg = mutationErrorMessage(cat, err, t);
      toast.error(msg);
    },
  });

  if (datasetQuery.error) {
    // If the parent list query is still in flight we can't yet tell
    // route-absent (disabled) from id-missing (not_found) on a 404;
    // show a generic loading state until the list resolves so a deep
    // link to a real `not_found` doesn't flash a misleading
    // "feature disabled" notice first.
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
    const cat = classifyEvalError(datasetQuery.error, { listAvailable });
    if (cat === "disabled") {
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <FeatureDisabledNotice
            title={t("datasets.disabledTitle")}
            configHint={t("datasets.disabledHint")}
          />
        </div>
      );
    }
    if (cat === "store_error") {
      // Distinct visual treatment from `disabled` — a 503 is a
      // *transient* runtime issue, not a flag the operator forgot
      // to flip. Use the LoadError chrome (error-tinted, with retry)
      // and the storeUnreachable copy.
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <LoadError
            message={`${t("datasets.storeUnreachableTitle")} — ${t("datasets.storeUnreachableHint")}`}
            onRetry={() => void datasetQuery.refetch()}
          />
        </div>
      );
    }
    if (cat === "not_found") {
      return (
        <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
          <EmptyState
            title={t("datasets.notFoundTitle", { id })}
            description={t("datasets.notFoundHint")}
            actions={
              <Link
                to={adminRoutes.datasets}
                className="inline-flex h-9 items-center rounded-sm border border-line-strong px-3 text-sm font-medium text-fg transition hover:bg-soft"
              >
                ← {t("datasets.title")}
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
            datasetQuery.error instanceof Error
              ? datasetQuery.error.message
              : String(datasetQuery.error)
          }
          onRetry={() => void datasetQuery.refetch()}
        />
      </div>
    );
  }

  const record = datasetQuery.data;
  const fixtures: Fixture[] = record?.spec.fixtures ?? [];
  // Scripted run replays each fixture's `provider_script`. Live-only
  // fixtures have an empty `provider_script`, so a scripted run on
  // them is guaranteed to fail or skip. Bucket fixtures so the
  // operator gets a clear choice: run only the scriptable ones (the
  // safe path that doesn't burn LLM tokens), or — if every fixture is
  // live-only — surface that explicitly and disable the button.
  const scriptableCount = fixtures.filter(
    (f) => (f.provider_script?.length ?? 0) > 0,
  ).length;
  const liveOnlyCount = fixtures.length - scriptableCount;
  const hasMixedScripts = scriptableCount > 0 && liveOnlyCount > 0;
  const allLiveOnly = scriptableCount === 0 && liveOnlyCount > 0;
  const scriptedRunDisabled =
    running ||
    triggerRun.isPending ||
    fixtures.length === 0 ||
    allLiveOnly;

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <PageHeader
        eyebrow={
          <Link to={adminRoutes.datasets} className="hover:text-fg-strong">
            ← {t("datasets.title")}
          </Link>
        }
        title={id}
        description={record?.spec.description}
        actions={
          record && (
            <button
              type="button"
              disabled={scriptedRunDisabled}
              onClick={() => triggerRun.mutate()}
              title={
                allLiveOnly
                  ? t("datasets.scriptedDisabledHint")
                  : hasMixedScripts
                    ? t("datasets.scriptedMixedHint", { count: liveOnlyCount })
                    : undefined
              }
              className="inline-flex h-9 items-center rounded-sm bg-accent px-3 text-sm font-medium text-accent-text transition-colors hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {triggerRun.isPending ? t("evalRuns.running") : t("evalRuns.trigger")}
            </button>
          )
        }
      />

      <EvalPrivacyNotice compact />

      {allLiveOnly && (
        <p
          className="mb-4 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] px-3 py-2 text-xs text-fg-soft"
          role="status"
        >
          {t("datasets.scriptedDisabledHint")}
        </p>
      )}
      {hasMixedScripts && (
        <p
          className="mb-4 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] px-3 py-2 text-xs text-fg-soft"
          role="status"
        >
          {t("datasets.scriptedMixedHint", {
            count: liveOnlyCount,
            scriptable: scriptableCount,
          })}
        </p>
      )}

      {datasetQuery.isPending ? (
        <div className="rounded-sm border border-line bg-surface p-8 text-sm text-fg-soft shadow-card">
          {t("common.loading")}
        </div>
      ) : fixtures.length === 0 ? (
        <div className="rounded-sm border border-line bg-surface p-5 shadow-card">
          <EmptyState
            title={t("datasets.fixturesEmptyTitle")}
            description={t("datasets.fixturesEmptyDesc")}
          />
        </div>
      ) : (
        <div className="space-y-3">
          {fixtures.map((f, idx) => (
            <FixtureRow key={f.id ?? idx} fixture={f} />
          ))}
        </div>
      )}
    </div>
  );
}

function FixtureRow({ fixture }: { fixture: Fixture }) {
  const { t } = useTranslation();
  const liveOnly = (fixture.provider_script?.length ?? 0) === 0;
  const turnCount = (fixture.continued_turns?.length ?? 0) + 1;
  // Expose the operator-authored pass/fail criteria. Without this the
  // dataset detail page lied about what a "run" is checking — operator
  // could only see it by editing the fixture JSON elsewhere.
  const expect = fixture.expect as
    | {
        final_answer_contains?: string[];
        final_answer_excludes?: string[];
        tool_sequence?: string[];
        min_judge_score?: number;
      }
    | undefined;
  const includes = expect?.final_answer_contains ?? [];
  const excludes = expect?.final_answer_excludes ?? [];
  const toolSeq = expect?.tool_sequence ?? [];
  return (
    <div className="rounded-sm border border-line bg-surface p-4 shadow-card">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <div className="font-mono text-sm font-medium text-fg-strong">{fixture.id}</div>
        <div className="flex items-center gap-2 text-xs text-fg-soft">
          {liveOnly ? (
            <Pill tone="info" dot>
              {t("datasets.liveOnly")}
            </Pill>
          ) : (
            <Pill tone="neutral">{t("datasets.scriptable")}</Pill>
          )}
          {turnCount > 1 && (
            <Pill tone="neutral">{t("datasets.turns", { count: turnCount })}</Pill>
          )}
          {fixture.source_run_id && (
            <span className="font-mono text-fg-faint">{fixture.source_run_id.slice(0, 8)}</span>
          )}
        </div>
      </div>
      {fixture.description && (
        <p className="mt-1 text-sm text-fg-soft">{fixture.description}</p>
      )}
      <div className="mt-3">
        <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
          {t("datasets.userInput")}
        </div>
        <p className="mt-1 max-w-3xl whitespace-pre-wrap break-words text-sm text-fg">
          {fixture.user_input || <span className="text-fg-faint">—</span>}
        </p>
      </div>
      <div className="mt-3">
        <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
          {t("datasets.expectation")}
        </div>
        {includes.length === 0 && excludes.length === 0 && toolSeq.length === 0 &&
        expect?.min_judge_score === undefined ? (
          <p className="mt-1 text-sm text-fg-faint">{t("datasets.expectationEmpty")}</p>
        ) : (
          <ul className="mt-1 space-y-0.5 text-xs text-fg">
            {includes.map((s, i) => (
              <li key={`inc-${i}`}>
                <span className="text-fg-faint">contains:</span>{" "}
                <code className="font-mono">{s}</code>
              </li>
            ))}
            {excludes.map((s, i) => (
              <li key={`exc-${i}`}>
                <span className="text-fg-faint">excludes:</span>{" "}
                <code className="font-mono">{s}</code>
              </li>
            ))}
            {toolSeq.length > 0 && (
              <li>
                <span className="text-fg-faint">tools:</span>{" "}
                <code className="font-mono">{toolSeq.join(" → ")}</code>
              </li>
            )}
            {typeof expect?.min_judge_score === "number" && (
              <li>
                <span className="text-fg-faint">judge ≥</span>{" "}
                <code className="font-mono">{expect.min_judge_score.toFixed(2)}</code>
              </li>
            )}
          </ul>
        )}
      </div>
      {fixture.provider_script_error && (
        <p
          className="mt-3 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] px-3 py-2 text-xs text-fg-soft"
          role="note"
        >
          <span className="font-medium text-tone-warn">
            {t("datasets.providerScriptError")}:
          </span>{" "}
          {fixture.provider_script_error}
        </p>
      )}
    </div>
  );
}

function mutationErrorMessage(
  cat: EvalErrorCategory,
  err: unknown,
  t: (k: string, opts?: Record<string, unknown>) => string,
): string {
  if (cat === "disabled") {
    return `${t("datasets.disabledTitle")} — ${t("datasets.disabledHint")}`;
  }
  if (cat === "store_error") {
    return `${t("datasets.storeUnreachableTitle")} — ${t("datasets.storeUnreachableHint")}`;
  }
  if (cat === "not_found") {
    return t("datasets.notFoundHint");
  }
  return err instanceof ConfigApiError
    ? err.message
    : err instanceof Error
      ? err.message
      : String(err);
}
