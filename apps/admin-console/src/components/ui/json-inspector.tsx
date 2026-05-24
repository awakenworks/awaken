import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

const DEFAULT_PREVIEW_CHARS = 4000;

/** Read-only JSON inspector with size cap + collapse/expand.
 *
 *  Pages that need to surface raw eval / trace payloads (eval-run
 *  detail items, model-test fallback) used to drop a bare `<pre>` of
 *  `JSON.stringify(node, null, 2)`. That had two problems:
 *
 *  - **Performance** — multi-MB traces froze layout while the browser
 *    rendered every byte even when the operator only wanted a glance.
 *  - **Privacy** — full prompts / tool args were visible to anyone
 *    landing on the page; an explicit "Show full payload" click is a
 *    minimum acknowledgement gesture before exposing the body.
 *
 *  This component collapses by default, shows the byte count, and
 *  exposes a Copy / Download button so the operator never has to
 *  copy-paste truncated text. Field-level redaction is a follow-up. */
export function JsonInspector({
  value,
  previewChars = DEFAULT_PREVIEW_CHARS,
  title,
}: {
  value: unknown;
  previewChars?: number;
  title?: string;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);

  const serialized = useMemo(() => {
    try {
      return JSON.stringify(value, null, 2);
    } catch {
      return String(value);
    }
  }, [value]);

  const bytes = serialized.length;
  const truncated = !expanded && bytes > previewChars;
  const text = truncated ? serialized.slice(0, previewChars) : serialized;

  function copy() {
    void navigator.clipboard?.writeText(serialized);
  }
  function download() {
    const blob = new Blob([serialized], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${(title ?? "payload").replace(/[^a-z0-9_-]+/gi, "-")}.json`;
    document.body.append(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  }

  return (
    <div className="overflow-hidden rounded-sm border border-line bg-soft">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line bg-surface px-3 py-1.5 text-[11px] text-fg-soft">
        <span className="font-medium text-fg">
          {title ?? t("jsonInspector.title")}
          <span className="ml-2 font-mono text-fg-faint">
            {t("jsonInspector.bytes", { count: bytes })}
          </span>
        </span>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="rounded-sm border border-line-strong px-2 py-0.5 text-fg transition hover:bg-soft"
          >
            {expanded ? t("jsonInspector.collapse") : t("jsonInspector.expand")}
          </button>
          <button
            type="button"
            onClick={copy}
            className="rounded-sm border border-line-strong px-2 py-0.5 text-fg transition hover:bg-soft"
          >
            {t("jsonInspector.copy")}
          </button>
          <button
            type="button"
            onClick={download}
            className="rounded-sm border border-line-strong px-2 py-0.5 text-fg transition hover:bg-soft"
          >
            {t("jsonInspector.download")}
          </button>
        </div>
      </div>
      <pre className="max-h-[60vh] overflow-auto p-3 text-xs leading-relaxed text-fg">
        {text}
        {truncated && (
          <>
            {"\n…"}
            <span className="text-fg-faint">
              {" "}
              {t("jsonInspector.truncated", {
                shown: previewChars.toLocaleString(),
                total: bytes.toLocaleString(),
              })}
            </span>
          </>
        )}
      </pre>
    </div>
  );
}
