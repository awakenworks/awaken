import { useTranslation } from "react-i18next";
import { type AgentSaveMode, prettyStableStringify } from "../spec-helpers";

export function AdvancedPanel({
  saveMode,
  savePayload,
}: {
  saveMode: AgentSaveMode;
  savePayload: unknown;
}) {
  const { t } = useTranslation();
  const modeLabel =
    saveMode === "patch-overrides"
      ? t("editor.savePayload.patchTitle")
      : saveMode === "create"
        ? t("editor.savePayload.createTitle")
        : t("editor.savePayload.fullTitle");
  const description =
    saveMode === "patch-overrides"
      ? t("editor.savePayload.advancedPatch")
      : t("editor.savePayload.advancedFull");

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">{modeLabel}</h3>
      <p className="mt-2 text-sm text-fg-soft">{description}</p>
      <pre className="mt-4 max-h-[36rem] overflow-auto rounded-sm bg-code-bg p-4 text-xs text-code-fg">
        {prettyStableStringify(savePayload)}
      </pre>
    </section>
  );
}
