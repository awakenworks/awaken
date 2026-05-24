import { useTranslation } from "react-i18next";

/** Page-level "failed to load" surface. Replaces the bare red text
 *  box that pages used to render on query failure with something
 *  operators can actually act on: a title, the error message, a hint,
 *  and a retry control.
 *
 *  Used by Dashboard, Audit Log, and Eval Reports — the surfaces the
 *  operator hits first when the backend goes down. */
export function LoadError({
  title,
  message,
  onRetry,
}: {
  /** Optional title. Defaults to the localised "Failed to load" copy. */
  title?: string;
  /** The underlying error message, surfaced verbatim. */
  message: string;
  /** When provided, render a Retry button that calls this. Pages back
   *  by react-query should pass `() => query.refetch()`. */
  onRetry?: () => void;
}) {
  const { t } = useTranslation();
  // Bare "Failed to fetch" is the native browser fetch error and is
  // almost always a network / backend-down condition rather than a
  // server-returned message. Replace it with operator-actionable copy.
  const looksLikeNetworkError =
    /^failed to fetch$/i.test(message.trim()) ||
    /networkerror|net::|fetch failed/i.test(message);
  const heading = title ?? t("loadError.title");
  const hint = looksLikeNetworkError
    ? t("loadError.networkHint")
    : t("loadError.genericHint");
  return (
    <div
      className="rounded-sm border border-tone-error/30 bg-tone-error/[0.06] p-5 shadow-card"
      role="alert"
      aria-live="polite"
    >
      <div className="flex flex-wrap items-baseline gap-3">
        <h2 className="text-base font-semibold text-tone-error">{heading}</h2>
        <span className="font-mono text-xs text-fg-soft">{message}</span>
      </div>
      <p className="mt-2 text-sm text-fg-soft">{hint}</p>
      {onRetry && (
        <div className="mt-3">
          <button
            type="button"
            onClick={onRetry}
            className="inline-flex items-center rounded-sm border border-line-strong bg-surface px-3 py-1.5 text-sm font-medium text-fg-strong transition hover:bg-soft"
          >
            {t("loadError.retry")}
          </button>
        </div>
      )}
    </div>
  );
}
