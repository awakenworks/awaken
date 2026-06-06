import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import { AssistantChatPanel } from "@/pages/assistant-page";
import { adminRoutes } from "@/lib/routes";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import { capabilitiesFromResult } from "@/lib/api";

export function AdminAssistantBubble() {
  const { t } = useTranslation();
  // Always-on FAB: persist the open/closed state so the floating window stays
  // as the operator left it across reloads and navigation.
  const [open, setOpen] = useState(
    () => typeof localStorage !== "undefined" && localStorage.getItem("awaken.admin.assistantOpen") === "1",
  );
  const dialogRef = useRef<HTMLElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const previousOpenRef = useRef(false);
  const capabilitiesQuery = useCapabilitiesQuery();
  const capabilities = capabilitiesFromResult(capabilitiesQuery.data);
  const assistant = capabilities?.admin_assistant;
  const enabled = Boolean(assistant?.enabled);
  const reason = assistant?.disabled_reason ?? t("assistant.bubble.disabledReason");

  useEffect(() => {
    try {
      localStorage.setItem("awaken.admin.assistantOpen", open ? "1" : "0");
    } catch {
      /* storage unavailable — best effort */
    }
  }, [open]);

  // Non-modal floating window: Escape closes it, but focus is NOT trapped, so
  // the rest of the console stays usable while the assistant is open.
  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open]);

  useEffect(() => {
    const previousOpen = previousOpenRef.current;
    previousOpenRef.current = open;
    // Don't steal focus when opening (especially on a restored-open reload);
    // just return focus to the FAB when the user closes the window.
    if (!open && previousOpen) {
      triggerRef.current?.focus();
    }
  }, [open]);

  return (
    <div className="fixed bottom-5 right-5 z-50 flex flex-col items-end gap-3">
      {open ? (
        <section
          ref={dialogRef}
          role="dialog"
          aria-label={t("assistant.title")}
          tabIndex={-1}
          className="relative h-[min(720px,calc(100dvh-6rem))] w-[min(440px,calc(100vw-2rem))] overflow-hidden rounded-lg border border-line-strong bg-bg shadow-overlay"
        >
          <button
            type="button"
            aria-label={t("assistant.bubble.close")}
            onClick={() => setOpen(false)}
            className="absolute right-3 top-3 z-10 rounded-sm border border-line bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
          >
            {t("assistant.bubble.closeShort")}
          </button>
          {enabled ? (
            <AssistantChatPanel variant="floating" />
          ) : (
            <AssistantSetupNotice loading={capabilitiesQuery.isLoading} reason={reason} />
          )}
        </section>
      ) : null}
      <button
        ref={triggerRef}
        type="button"
        aria-label={enabled ? t("assistant.bubble.open") : t("assistant.bubble.configure")}
        title={enabled ? t("assistant.title") : reason}
        onClick={() => setOpen((value) => !value)}
        className={[
          "flex h-14 w-14 items-center justify-center rounded-2xl border shadow-overlay transition",
          enabled
            ? // Same look as the brand mark, theme-inverted: dark tile on light
              // theme, light tile on dark theme (bg-fg / text-bg auto-flip).
              "border-line bg-fg text-bg hover:opacity-90"
            : "border-tone-warning bg-surface text-tone-warning hover:bg-muted",
        ].join(" ")}
      >
        {/* Awaken brand mark (Λ "A" + red dot), geometry from the canonical
            brand SVG (apps/www/public/favicon.svg). The "A" uses currentColor
            (= text-bg, the inverse of the tile); the dot keeps the brand red. */}
        <svg viewBox="0 0 32 32" aria-hidden="true" className="h-7 w-7" fill="none">
          <path d="M14.2 3.6 H17.8 L27.2 25.8 H24.4 L16 6 L7.6 25.8 H4.8 Z" fill="currentColor" />
          <circle cx="16" cy="19.4" r="2.65" fill="#fa6863" />
        </svg>
      </button>
    </div>
  );
}

function AssistantSetupNotice({ loading, reason }: { loading: boolean; reason: string }) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col bg-surface">
      <header className="border-b border-line px-6 py-4">
        <h2 className="text-lg font-semibold text-fg-strong">{t("assistant.title")}</h2>
        <p className="mt-1 text-sm text-fg-soft">{t("assistant.bubble.setupDescription")}</p>
      </header>
      <div className="flex flex-1 flex-col justify-center gap-4 px-6 text-sm">
        <div className="rounded-sm border border-tone-warning bg-bg px-4 py-3 text-tone-warning">
          {loading ? t("assistant.bubble.checking") : reason}
        </div>
        <div className="grid gap-2">
          <Link
            to={adminRoutes.providers}
            className="rounded-sm border border-line-strong bg-bg px-3 py-2 font-medium text-fg hover:bg-muted"
          >
            {t("assistant.bubble.configureProvider")}
          </Link>
          <Link
            to={adminRoutes.models}
            className="rounded-sm border border-line-strong bg-bg px-3 py-2 font-medium text-fg hover:bg-muted"
          >
            {t("assistant.bubble.configureModel")}
          </Link>
        </div>
      </div>
    </div>
  );
}
