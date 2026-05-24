import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link, useNavigate } from "react-router";
import { ConfigApiError, evalApi, type DatasetSummary } from "@/lib/api";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/ui/empty-state";
import { EvalPrivacyNotice } from "@/components/ui/eval-privacy-notice";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { LoadError } from "@/components/ui/load-error";
import { PageHeader } from "@/components/ui/page-header";
import { SkeletonRows } from "@/components/ui/skeleton";
import { adminRoutes } from "@/lib/routes";
import {
  DATASET_ID_MAX_LEN,
  DATASET_ID_PATTERN_SOURCE,
  validateDatasetId,
} from "@/lib/eval-validation";
import { useDialogKeys } from "@/lib/use-dialog-keys";

/** `/datasets` — list eval datasets with CRUD.
 *
 *  Backed by `/v1/eval/datasets` (gated server-side by
 *  `AdminApiConfig.expose_eval_routes`). When the gate is off,
 *  `evalApi.listDatasets` returns `null` and we render a friendly
 *  "feature not configured" notice rather than crashing the page. */
export function DatasetsPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const queryClient = useQueryClient();
  const [creating, setCreating] = useState(false);

  const listQuery = useQuery({
    queryKey: ["eval", "datasets"] as const,
    queryFn: evalApi.listDatasets,
  });

  const deleteDataset = useMutation({
    mutationFn: (id: string) => evalApi.deleteDataset(id),
    onSuccess: (_result, id) => {
      void queryClient.invalidateQueries({ queryKey: ["eval", "datasets"] });
      toast.success(`Dataset "${id}" deleted`);
    },
    onError: (err) => {
      toast.error(err instanceof Error ? err.message : String(err));
    },
  });

  async function handleDelete(id: string, fixtureCount: number) {
    const accepted = await confirmDialog({
      title: t("datasets.deleteTitle"),
      description: (
        <>
          {t("datasets.deleteBody", { fixtures: fixtureCount })}
          <span className="font-mono"> {id}</span>
        </>
      ),
      confirmLabel: t("common.delete"),
      tone: "destructive",
    });
    if (!accepted) return;
    deleteDataset.mutate(id);
  }

  if (listQuery.error) {
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <LoadError
          message={
            listQuery.error instanceof Error
              ? listQuery.error.message
              : String(listQuery.error)
          }
          onRetry={() => void listQuery.refetch()}
        />
      </div>
    );
  }

  const data = listQuery.data;
  const featureDisabled = data === null;
  const datasets: DatasetSummary[] = data?.datasets ?? [];

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <PageHeader
        title={t("datasets.title")}
        count={featureDisabled ? undefined : datasets.length}
        actions={
          !featureDisabled && (
            <button
              type="button"
              onClick={() => setCreating(true)}
              className="inline-flex h-9 items-center rounded-sm bg-accent px-3 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
            >
              {t("datasets.new")}
            </button>
          )
        }
      />

      {!featureDisabled && <EvalPrivacyNotice compact />}

      {featureDisabled && (
        <FeatureDisabledNotice
          title={t("datasets.disabledTitle")}
          configHint={t("datasets.disabledHint")}
        />
      )}

      {!featureDisabled && (
        <div className="overflow-x-auto rounded-sm border border-line bg-surface shadow-card">
          {listQuery.isPending ? (
            <table className="min-w-full">
              <tbody>
                <SkeletonRows rows={3} cols={4} />
              </tbody>
            </table>
          ) : datasets.length === 0 ? (
            <EmptyState
              title={t("datasets.empty.title")}
              description={t("datasets.empty.desc")}
              actions={
                <button
                  type="button"
                  onClick={() => setCreating(true)}
                  className="inline-flex h-9 items-center rounded-sm bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
                >
                  {t("datasets.new")}
                </button>
              }
            />
          ) : (
            <table className="min-w-full">
              <thead className="bg-soft text-left text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
                <tr>
                  <th className="px-5 py-3">{t("datasets.columns.id")}</th>
                  <th className="px-5 py-3">{t("datasets.columns.description")}</th>
                  <th className="px-5 py-3 text-right">{t("datasets.columns.fixtures")}</th>
                  <th className="px-5 py-3 text-right">{t("datasets.columns.actions")}</th>
                </tr>
              </thead>
              <tbody>
                {datasets.map((d) => (
                  <tr
                    key={d.id}
                    className="cursor-pointer border-t border-line text-sm text-fg transition-colors hover:bg-soft"
                    onClick={() => navigate(adminRoutes.dataset(d.id))}
                  >
                    <td className="px-5 py-4">
                      <div className="font-mono text-sm font-medium text-fg-strong">{d.id}</div>
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {d.description || <span className="text-fg-faint">—</span>}
                    </td>
                    <td className="px-5 py-4 text-right font-mono tabular-nums">{d.fixture_count}</td>
                    <td className="px-5 py-4 text-right">
                      <div className="flex items-center justify-end gap-3">
                        <Link
                          to={adminRoutes.evalRuns + `?dataset=${encodeURIComponent(d.id)}`}
                          onClick={(e) => e.stopPropagation()}
                          className="text-xs font-medium text-link transition-colors hover:text-link-hover"
                        >
                          {t("datasets.actions.runs")}
                        </Link>
                        <button
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation();
                            void handleDelete(d.id, d.fixture_count);
                          }}
                          className="text-xs font-medium text-tone-error transition-colors hover:underline"
                        >
                          {t("common.delete")}
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}

      {creating && (
        <CreateDatasetModal
          onClose={() => setCreating(false)}
          onCreated={(id) => {
            setCreating(false);
            void queryClient.invalidateQueries({ queryKey: ["eval", "datasets"] });
            navigate(adminRoutes.dataset(id));
          }}
        />
      )}
    </div>
  );
}

function CreateDatasetModal({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (id: string) => void;
}) {
  const { t } = useTranslation();
  const toast = useToast();
  const [id, setId] = useState("");
  const [description, setDescription] = useState("");
  // Shared validation rule with SaveTraceAsFixtureModal — see
  // `lib/eval-validation.ts`. Earlier the two forms differed.
  const idValidation = validateDatasetId(id);
  const dialogRef = useRef<HTMLDivElement>(null);
  useDialogKeys({ dialogRef, onClose });
  // Payload is built at submit time and frozen into mutation
  // variables; onSuccess reads from `vars`, never from the latest
  // React state. Without this, editing the id between submit and
  // success would navigate to the wrong page.
  const create = useMutation({
    mutationFn: (payload: { datasetId: string; description?: string }) =>
      evalApi.createDataset(payload.datasetId, {
        description: payload.description,
        fixtures: [],
      }),
    onSuccess: (_record, vars) => onCreated(vars.datasetId),
    onError: (err) => {
      const msg =
        err instanceof ConfigApiError
          ? err.message
          : err instanceof Error
            ? err.message
            : String(err);
      toast.error(msg);
    },
  });
  return (
    <div
      ref={dialogRef}
      role="dialog"
      aria-modal="true"
      aria-label={t("datasets.createTitle")}
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!idValidation.ok || create.isPending) return;
          // Snapshot now — onSuccess reads `vars` so post-submit edits
          // can't redirect to the wrong dataset.
          create.mutate({
            datasetId: idValidation.value,
            description: description.trim() || undefined,
          });
        }}
        className="w-full max-w-md rounded-sm border border-line bg-surface p-5 shadow-card"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("datasets.createTitle")}</h2>
        <div className="mt-4 space-y-3">
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("datasets.columns.id")}
            </span>
            <input
              type="text"
              autoFocus
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="my-dataset"
              required
              pattern={DATASET_ID_PATTERN_SOURCE}
              maxLength={DATASET_ID_MAX_LEN}
              title={t("traceCapture.newDatasetIdHint")}
              disabled={create.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
            <span className="mt-1 block text-[11px] text-fg-soft">
              {t("traceCapture.newDatasetIdHint")}
            </span>
          </label>
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("datasets.columns.description")}
            </span>
            <input
              type="text"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder={t("datasets.placeholders.description")}
              disabled={create.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
          </label>
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-sm border border-line-strong px-3 py-1.5 text-sm font-medium text-fg transition hover:bg-soft"
          >
            {t("common.cancel")}
          </button>
          <button
            type="submit"
            disabled={!idValidation.ok || create.isPending}
            className="rounded-sm bg-accent px-4 py-1.5 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {create.isPending ? t("common.loading") : t("datasets.create")}
          </button>
        </div>
      </form>
    </div>
  );
}
