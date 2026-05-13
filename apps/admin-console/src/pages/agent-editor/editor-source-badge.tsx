import { useTranslation } from "react-i18next";
import { type ConfigSourceState } from "@/lib/config-api";

export function EditorSourceBadge({ state }: { state: ConfigSourceState }) {
  const { t } = useTranslation();
  if (state === "builtin") {
    return (
      <span className="rounded-full bg-muted px-2 py-0.5 text-xs font-medium text-fg-soft">
        {t("agents.source.builtin")}
      </span>
    );
  }
  if (state === "customized") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-blue-100 px-2 py-0.5 text-xs font-medium text-blue-800 dark:bg-blue-900/30 dark:text-blue-300">
        <span aria-hidden className="h-1.5 w-1.5 rounded-full bg-blue-500" />
        {t("agents.source.customized")}
      </span>
    );
  }
  return (
    <span className="rounded-full bg-soft px-2 py-0.5 text-xs font-medium text-fg">
      {t("agents.source.userDefined")}
    </span>
  );
}
