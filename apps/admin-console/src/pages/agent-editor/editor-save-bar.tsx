import { useState } from "react";
import { useTranslation } from "react-i18next";
import { type AgentSpec, configApi } from "@/lib/config-api";
import { useToast } from "@/components/toast-provider";
import { DiffModal } from "./diff-modal";

export function EditorSaveBar({
  isDirty,
  isNew,
  saving,
  saveDisabled,
  spec,
  savedSpec,
  onSave,
}: {
  isDirty: boolean;
  isNew: boolean;
  saving: boolean;
  saveDisabled: boolean;
  spec: AgentSpec;
  savedSpec: AgentSpec | null;
  onSave: () => void;
}) {
  const { t } = useTranslation();
  const toast = useToast();
  const [validating, setValidating] = useState(false);
  const [diffOpen, setDiffOpen] = useState(false);

  if (!isDirty && !isNew) return null;

  async function handleValidate() {
    setValidating(true);
    try {
      await configApi.validateConfig("agents", spec, isNew ? undefined : { id: spec.id });
      toast.success("Validation passed — payload is safe to publish.");
    } catch (err) {
      toast.error(`Validation failed: ${err instanceof Error ? err.message : String(err)}`);
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
            <span className="ml-2 text-fg-soft">Save will publish to the runtime config.</span>
          </div>
          <div className="ml-auto flex items-center gap-2">
            {!isNew && savedSpec && (
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

      {diffOpen && savedSpec && (
        <DiffModal current={spec} previous={savedSpec} onClose={() => setDiffOpen(false)} />
      )}
    </>
  );
}
