import { useState } from "react";
import { useTranslation } from "react-i18next";
import { type AgentSpec, configApi } from "@/lib/config-api";
import { useToast } from "@/components/toast-provider";
import { DiffModal } from "./diff-modal";
import { type AgentSaveMode } from "./spec-helpers";

export function EditorSaveBar({
  isDirty,
  isNew,
  saving,
  saveDisabled,
  spec,
  savedSpec,
  saveMode,
  savePayload,
  onSave,
}: {
  isDirty: boolean;
  isNew: boolean;
  saving: boolean;
  saveDisabled: boolean;
  spec: AgentSpec;
  savedSpec: AgentSpec | null;
  saveMode: AgentSaveMode;
  savePayload: AgentSpec | Record<string, unknown>;
  onSave: () => void;
}) {
  const { t } = useTranslation();
  const toast = useToast();
  const [validating, setValidating] = useState(false);
  const [diffOpen, setDiffOpen] = useState(false);
  const isPatchMode = saveMode === "patch-overrides";
  const patchPayload =
    isPatchMode && !Array.isArray(savePayload) && savePayload && typeof savePayload === "object"
      ? (savePayload as Record<string, unknown>)
      : {};
  const hasPatchPayload = Object.keys(patchPayload).length > 0;
  const canShowDiff = Boolean(savedSpec) && (!isPatchMode || hasPatchPayload);
  const saveDescription = isPatchMode
    ? t("editor.savePayload.patchDescription")
    : t("editor.savePayload.fullDescription");

  if (!isDirty && !isNew) return null;

  async function handleValidate() {
    setValidating(true);
    try {
      if (isPatchMode) {
        if (!hasPatchPayload) {
          toast.success(t("editor.savePayload.noPatchableChanges"));
          return;
        }
        await configApi.validateAgentOverrides(spec.id, patchPayload);
      } else {
        await configApi.validateConfig("agents", savePayload, isNew ? undefined : { id: spec.id });
      }
      toast.success(t("editor.savePayload.validatePassed"));
    } catch (err) {
      toast.error(
        t("editor.savePayload.validateFailed", {
          message: err instanceof Error ? err.message : String(err),
        }),
      );
    } finally {
      setValidating(false);
    }
  }

  return (
    <>
      <div className="sticky bottom-0 z-20 mx-6 mb-4 rounded-md border border-line bg-surface px-4 py-3 shadow-card-lift md:mx-8">
        <div className="flex flex-wrap items-center gap-3">
          <span aria-hidden className="inline-block h-2 w-2 rounded-pill bg-state-progress" />
          <div className="text-sm text-fg">
            {isNew ? (
              <span className="text-fg-strong">{t("editor.unsavedChanges")}</span>
            ) : (
              <span className="text-fg-strong">{t("editor.unsavedChanges")}</span>
            )}
            <span className="ml-2 text-fg-soft">{saveDescription}</span>
          </div>
          <div className="ml-auto flex items-center gap-2">
            {!isNew && canShowDiff && (
              <button
                type="button"
                onClick={() => setDiffOpen(true)}
                className="inline-flex h-9 items-center rounded-md border border-line bg-surface px-3 text-sm font-medium text-fg-soft transition-colors hover:text-fg"
              >
                {t("editor.diff")}
              </button>
            )}
            <button
              type="button"
              onClick={() => void handleValidate()}
              disabled={validating || saving}
              className="inline-flex h-9 items-center rounded-md border border-line-strong bg-surface px-3 text-sm font-medium text-fg transition-colors hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
            >
              {validating ? t("editor.validating") : t("editor.validate")}
            </button>
            <button
              type="button"
              onClick={onSave}
              disabled={saveDisabled || validating}
              className="inline-flex h-9 items-center rounded-md bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {saving ? t("editor.saving") : t("editor.save")}
            </button>
          </div>
        </div>
      </div>

      {diffOpen && savedSpec && canShowDiff && (
        <DiffModal
          current={isPatchMode ? patchPayload : savePayload}
          previous={isPatchMode ? {} : savedSpec}
          title={isPatchMode ? t("editor.savePayload.patchTitle") : t("editor.diff")}
          onClose={() => setDiffOpen(false)}
        />
      )}
    </>
  );
}
