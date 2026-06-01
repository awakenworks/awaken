import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import { AssistantChatPanel } from "@/pages/assistant-page";
import { adminRoutes } from "@/lib/routes";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import { capabilitiesFromResult } from "@/lib/api";

export function AdminAssistantBubble() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const dialogRef = useRef<HTMLElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const previousOpenRef = useRef(false);
  const capabilitiesQuery = useCapabilitiesQuery();
  const capabilities = capabilitiesFromResult(capabilitiesQuery.data);
  const assistant = capabilities?.admin_assistant;
  const enabled = Boolean(assistant?.enabled);
  const reason = assistant?.disabled_reason ?? t("assistant.bubble.disabledReason");

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setOpen(false);
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = dialogRef.current;
      if (!dialog) return;
      const focusable = focusableElements(dialog);
      if (focusable.length === 0) {
        event.preventDefault();
        dialog.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open]);

  useEffect(() => {
    const previousOpen = previousOpenRef.current;
    previousOpenRef.current = open;
    if (open) {
      dialogRef.current?.focus();
    } else if (previousOpen) {
      triggerRef.current?.focus();
    }
  }, [open]);

  return (
    <div className="fixed bottom-5 right-5 z-50 flex flex-col items-end gap-3">
      {open ? (
        <section
          ref={dialogRef}
          role="dialog"
          aria-modal="true"
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
          "flex h-14 w-14 items-center justify-center rounded-full border text-sm font-semibold shadow-overlay transition",
          enabled
            ? "border-cyan-500 bg-cyan-700 text-bg hover:bg-cyan-600"
            : "border-tone-warning bg-surface text-tone-warning hover:bg-muted",
        ].join(" ")}
      >
        AI
      </button>
    </div>
  );
}

function focusableElements(root: HTMLElement): HTMLElement[] {
  return Array.from(
    root.querySelectorAll<HTMLElement>(
      [
        "a[href]",
        "button:not([disabled])",
        "textarea:not([disabled])",
        "input:not([disabled])",
        "select:not([disabled])",
        "[tabindex]:not([tabindex='-1'])",
      ].join(","),
    ),
  ).filter((element) => !element.hasAttribute("disabled") && element.offsetParent !== null);
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
