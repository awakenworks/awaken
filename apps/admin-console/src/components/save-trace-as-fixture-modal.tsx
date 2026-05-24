import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { ConfigApiError, evalApi, type DatasetSummary } from "@/lib/api";
import { useToast } from "@/components/toast-provider";
import { useDialogKeys } from "@/lib/use-dialog-keys";
import {
  DATASET_ID_MAX_LEN,
  DATASET_ID_PATTERN_SOURCE,
  validateDatasetId,
} from "@/lib/eval-validation";

/** Modal: capture a recorded trace as an eval fixture appended to a
 *  chosen dataset (or a new one created inline).
 *
 *  Wraps `POST /v1/eval/datasets/:id/items`. The backend uses the
 *  recorded trace to derive a `provider_script` and seeds the fixture
 *  with the recorded `user_input` + `source_model_id`; the operator
 *  only has to pick the destination dataset and a non-empty
 *  expectation (the backend's `require_non_empty_expected` guard).
 *
 *  The provider-script mode defaults to `optional`: try to make a
 *  replayable scripted fixture, but fall back to a Live-only fixture
 *  when the trace can't be represented by the script schema. */
export function SaveTraceAsFixtureModal({
  runId,
  onClose,
  onSaved,
}: {
  runId: string;
  onClose: () => void;
  onSaved: () => void;
}) {
  const { t } = useTranslation();
  const toast = useToast();
  const queryClient = useQueryClient();

  const [datasetMode, setDatasetMode] = useState<"existing" | "new">("existing");
  const [datasetId, setDatasetId] = useState("");
  const [newDatasetId, setNewDatasetId] = useState("");
  const [newDatasetDescription, setNewDatasetDescription] = useState("");
  const [fixtureId, setFixtureId] = useState("");
  const [fixtureDescription, setFixtureDescription] = useState("");
  const [mustInclude, setMustInclude] = useState("");
  // Default to `require` so the saved fixture is always replayable by
  // the dataset detail's "Run" button (which fires scripted mode).
  // Operators can downgrade to `optional` / `skip` for traces that
  // can't be scripted (parallel tool calls, partial captures, etc.) —
  // those still produce a fixture, but it'll be live-only.
  const [providerScriptMode, setProviderScriptMode] =
    useState<"optional" | "require" | "skip">("require");
  const dialogRef = useRef<HTMLDivElement>(null);
  useDialogKeys({ dialogRef, onClose });

  const datasetsQuery = useQuery({
    queryKey: ["eval", "datasets"] as const,
    queryFn: evalApi.listDatasets,
  });

  // Payload is built at submit time and frozen into the mutation
  // variables; onSuccess reads from `vars`, not React state, so a
  // user editing the form after submit can't make us invalidate
  // the wrong cache key.
  type SavePayload = {
    mode: "existing" | "new";
    destDatasetId: string;
    newDatasetDescription?: string;
    fixtureId?: string;
    fixtureDescription?: string;
    mustInclude: string;
    providerScriptMode: "optional" | "require" | "skip";
  };

  const save = useMutation({
    mutationFn: async (payload: SavePayload) => {
      const dest = payload.destDatasetId;
      // `created` tracks an inline-created dataset *with the revision
      // we got back* so the rollback delete can compare-and-swap on it
      // (see the catch below) and never destroy a concurrent operator's
      // fixture appended between create and a failed curate.
      let created: { id: string; revision: number } | null = null;
      if (payload.mode === "new") {
        if (!dest) throw new Error("dataset id required");
        const rec = await evalApi.createDataset(dest, {
          description: payload.newDatasetDescription || undefined,
          fixtures: [],
        });
        created = { id: dest, revision: rec.meta.revision };
      }
      if (!dest) throw new Error("destination dataset required");
      try {
        return await evalApi.curateItems(dest, {
          from_run_id: runId,
          fixture_id: payload.fixtureId || undefined,
          description: payload.fixtureDescription || undefined,
          provider_script_mode: payload.providerScriptMode,
          expect: {
            // `require_non_empty_expected` server-side rejects an empty
            // `Expectation`. The operator picks one substring that the
            // assistant's final text must contain. They can edit the
            // fixture in JSON later to add stricter checks.
            final_answer_contains: payload.mustInclude ? [payload.mustInclude] : [],
          },
        });
      } catch (err) {
        // Atomic revision-guarded rollback. Passing the revision we got
        // from `createDataset` makes the delete a server-side CAS: it
        // removes the dataset only if nothing has written to it since
        // (revision unchanged). If a concurrent operator curated a
        // fixture in the gap, the revision moved and the server returns
        // 409 — we swallow it and leave their dataset intact. No
        // client-side TOCTOU window (the check and delete are one op).
        if (created) {
          try {
            await evalApi.deleteDataset(created.id, created.revision);
          } catch {
            // Best-effort; surfacing two errors is worse than one.
          }
        }
        throw err;
      }
    },
    onSuccess: (_record, vars) => {
      toast.success(`Saved fixture from run ${runId.slice(0, 8)}`);
      void queryClient.invalidateQueries({ queryKey: ["eval", "datasets"] });
      // Read destination from frozen `vars`, not current React state,
      // so a user editing the form after submit can't make us
      // invalidate the wrong dataset cache key.
      if (vars.destDatasetId) {
        void queryClient.invalidateQueries({
          queryKey: ["eval", "dataset", vars.destDatasetId],
        });
      }
      onSaved();
    },
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

  // Three distinct query states:
  //   - error                  → render inline LoadError; submit gated.
  //   - data === null          → eval surface disabled; banner + gated.
  //   - data === { datasets }  → normal flow.
  // The earlier draft folded "errored" into "data?.datasets ?? []"
  // which silently let the operator submit an empty dataset id (or pick
  // the new-dataset branch) on a failing list query.
  const listError = datasetsQuery.error;
  const datasets: DatasetSummary[] = datasetsQuery.data?.datasets ?? [];
  const featureDisabled = datasetsQuery.data === null;
  const canSubmit =
    !save.isPending &&
    !featureDisabled &&
    !listError &&
    !datasetsQuery.isPending &&
    mustInclude.trim().length > 0 &&
    (datasetMode === "existing"
      ? datasetId.trim().length > 0
      : validateDatasetId(newDatasetId).ok);

  return (
    <div
      ref={dialogRef}
      role="dialog"
      aria-modal="true"
      aria-label={t("traceCapture.title")}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-overlay backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!canSubmit) return;
          // Snapshot form values *now*. Anything the user edits after
          // submit is ignored by the mutation — onSuccess sees these
          // frozen vars, not the latest React state.
          save.mutate({
            mode: datasetMode,
            destDatasetId:
              datasetMode === "new" ? newDatasetId.trim() : datasetId.trim(),
            newDatasetDescription: newDatasetDescription.trim() || undefined,
            fixtureId: fixtureId.trim() || undefined,
            fixtureDescription: fixtureDescription.trim() || undefined,
            mustInclude: mustInclude.trim(),
            providerScriptMode,
          });
        }}
        className="w-full max-w-lg rounded-sm border border-line bg-surface p-5 shadow-card"
      >
        <h2 className="text-lg font-semibold text-fg-strong">
          {t("traceCapture.title")}
        </h2>
        <p className="mt-1 text-xs text-fg-soft">
          {t("traceCapture.from")} <span className="font-mono">{runId.slice(0, 12)}</span>
        </p>

        {featureDisabled && (
          <p
            className="mt-3 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] p-2 text-xs text-fg-soft"
            role="alert"
          >
            {t("traceCapture.disabledHint")}
          </p>
        )}
        {listError && !featureDisabled && (
          <p
            className="mt-3 rounded-sm border border-tone-error/30 bg-tone-error/[0.06] p-2 text-xs text-tone-error"
            role="alert"
          >
            {t("traceCapture.listError")}:{" "}
            {listError instanceof Error ? listError.message : String(listError)}
          </p>
        )}

        <fieldset className="mt-4 space-y-3">
          <legend className="sr-only">{t("traceCapture.destination")}</legend>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              name="dataset-mode"
              checked={datasetMode === "existing"}
              onChange={() => setDatasetMode("existing")}
              disabled={featureDisabled || save.isPending}
            />
            {t("traceCapture.existingDataset")}
          </label>
          {datasetMode === "existing" && (
            <select
              value={datasetId}
              onChange={(e) => setDatasetId(e.target.value)}
              className="ml-6 w-[calc(100%-1.5rem)] rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none"
              disabled={featureDisabled || save.isPending || datasets.length === 0}
            >
              <option value="">
                {datasets.length === 0
                  ? t("traceCapture.noDatasets")
                  : t("traceCapture.choose")}
              </option>
              {datasets.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.id} ({d.fixture_count})
                </option>
              ))}
            </select>
          )}

          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              name="dataset-mode"
              checked={datasetMode === "new"}
              onChange={() => setDatasetMode("new")}
              disabled={featureDisabled || save.isPending}
            />
            {t("traceCapture.newDataset")}
          </label>
          {datasetMode === "new" && (
            <div className="ml-6 space-y-2">
              <label className="block">
                <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  {t("traceCapture.newDatasetIdLabel")}
                </span>
                <input
                  type="text"
                  value={newDatasetId}
                  onChange={(e) => setNewDatasetId(e.target.value)}
                  placeholder="my-dataset"
                  required
                  aria-required="true"
                  pattern={DATASET_ID_PATTERN_SOURCE}
                  maxLength={DATASET_ID_MAX_LEN}
                  title={t("traceCapture.newDatasetIdHint")}
                  disabled={featureDisabled || save.isPending}
                  className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none disabled:opacity-60"
                />
                <span className="mt-1 block text-[11px] text-fg-soft">
                  {t("traceCapture.newDatasetIdHint")}
                </span>
              </label>
              <label className="block">
                <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  {t("traceCapture.newDatasetDescriptionLabel")}
                </span>
                <input
                  type="text"
                  value={newDatasetDescription}
                  onChange={(e) => setNewDatasetDescription(e.target.value)}
                  placeholder={t("datasets.placeholders.description")}
                  disabled={featureDisabled || save.isPending}
                  className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
                />
              </label>
            </div>
          )}
        </fieldset>

        <div className="mt-4 space-y-3">
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("traceCapture.fixtureId")}
            </span>
            <input
              type="text"
              value={fixtureId}
              onChange={(e) => setFixtureId(e.target.value)}
              placeholder={t("traceCapture.fixtureIdPlaceholder")}
              disabled={featureDisabled || save.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
          </label>
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("traceCapture.fixtureDescription")}
            </span>
            <input
              type="text"
              value={fixtureDescription}
              onChange={(e) => setFixtureDescription(e.target.value)}
              placeholder={t("traceCapture.fixtureDescriptionPlaceholder")}
              disabled={featureDisabled || save.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
          </label>
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("traceCapture.mustInclude")}
            </span>
            <input
              type="text"
              value={mustInclude}
              onChange={(e) => setMustInclude(e.target.value)}
              placeholder={t("traceCapture.mustIncludePlaceholder")}
              required
              disabled={featureDisabled || save.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
            <span className="mt-1 block text-[11px] text-fg-soft">
              {t("traceCapture.mustIncludeHint")}
            </span>
          </label>
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("traceCapture.scriptMode")}
            </span>
            <select
              value={providerScriptMode}
              onChange={(e) =>
                setProviderScriptMode(e.target.value as "optional" | "require" | "skip")
              }
              disabled={featureDisabled || save.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            >
              <option value="optional">{t("traceCapture.scriptOptional")}</option>
              <option value="require">{t("traceCapture.scriptRequire")}</option>
              <option value="skip">{t("traceCapture.scriptSkip")}</option>
            </select>
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
            disabled={!canSubmit}
            data-testid="save-trace-as-fixture-submit"
            className="rounded-sm bg-accent px-4 py-1.5 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {save.isPending ? t("common.loading") : t("traceCapture.save")}
          </button>
        </div>
      </form>
    </div>
  );
}
