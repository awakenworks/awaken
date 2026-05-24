import { useTranslation } from "react-i18next";

/** Inline privacy reminder rendered above eval / trace surfaces.
 *
 *  Eval runs and trace captures persist `request_messages`, tool
 *  arguments, and assistant text — anything the operator put into a
 *  prompt, including PII or secrets accidentally pasted in by users.
 *  Admin Console users with eval/trace access can read all of it, and
 *  there is no field-level redaction in this PR.
 *
 *  Surfacing this on every list/detail page is the lowest-friction
 *  way to keep the privacy boundary visible until a dedicated
 *  RBAC / retention / redaction story exists. Pages that don't
 *  display trace bodies can pass `compact` to use the short variant. */
export function EvalPrivacyNotice({ compact = false }: { compact?: boolean }) {
  const { t } = useTranslation();
  return (
    <div
      role="note"
      className="mb-4 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] px-3 py-2 text-xs text-fg-soft"
    >
      <span className="font-medium text-tone-warn">
        {t("privacyNotice.label")}:
      </span>{" "}
      {compact
        ? t("privacyNotice.compact")
        : t("privacyNotice.full")}
    </div>
  );
}
