import { Trans, useTranslation } from "react-i18next";

/** Inline yellow notice shown when a dashboard panel is missing its
 *  backing subsystem (audit log, runtime stats, run store). Centralised
 *  so the visual language stays consistent and the localised copy
 *  ("To enable, X — see Y") lives in one place. */
export function FeatureDisabledNotice({
  title,
  configHint,
  docsUrl,
}: {
  title: string;
  configHint: string;
  /** Optional doc path. Rendered as "(see PATH)" — not a hyperlink,
   *  since the path is server-side configuration docs. */
  docsUrl?: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="mt-4 rounded-sm border border-tone-warn/30 bg-tone-warn/10 px-3 py-3 text-sm">
      <div className="font-medium text-fg-strong">{title}</div>
      <div className="mt-1 text-xs text-fg-soft">
        <Trans
          i18nKey="dashboard.featureDisabled.toEnable"
          values={{ hint: configHint }}
          components={{ 1: <span className="font-mono" /> }}
        >
          {"To enable, <1>{{hint}}</1>"}
        </Trans>
        {docsUrl && (
          <>
            {" ("}
            {t("dashboard.featureDisabled.see", { doc: docsUrl })}
            {")"}
          </>
        )}
        .
      </div>
    </div>
  );
}
