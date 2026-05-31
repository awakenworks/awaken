import { useEffect, useRef, useState } from "react";
import { Link } from "react-router";
import { AssistantChatPanel } from "@/pages/assistant-page";
import { adminRoutes } from "@/lib/routes";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";

export function AdminAssistantBubble() {
  const [open, setOpen] = useState(false);
  const dialogRef = useRef<HTMLElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const previousOpenRef = useRef(false);
  const capabilitiesQuery = useCapabilitiesQuery();
  const assistant = capabilitiesQuery.data?.admin_assistant;
  const enabled = Boolean(assistant?.enabled);
  const reason =
    assistant?.disabled_reason ??
    "Configure and publish a provider-backed model to enable the Admin Assistant.";

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
          aria-label="Admin Assistant"
          tabIndex={-1}
          className="relative h-[min(720px,calc(100dvh-6rem))] w-[min(440px,calc(100vw-2rem))] overflow-hidden rounded-lg border border-line-strong bg-bg shadow-overlay"
        >
          <button
            type="button"
            aria-label="Close Admin Assistant"
            onClick={() => setOpen(false)}
            className="absolute right-3 top-3 z-10 rounded-sm border border-line bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
          >
            Close
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
        aria-label={enabled ? "Open Admin Assistant" : "Configure a model to enable Admin Assistant"}
        title={enabled ? "Admin Assistant" : reason}
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
  return (
    <div className="flex h-full flex-col bg-surface">
      <header className="border-b border-line px-6 py-4">
        <h2 className="text-lg font-semibold text-fg-strong">Admin Assistant</h2>
        <p className="mt-1 text-sm text-fg-soft">
          The assistant becomes available after the first provider-backed model is configured.
        </p>
      </header>
      <div className="flex flex-1 flex-col justify-center gap-4 px-6 text-sm">
        <div className="rounded-sm border border-tone-warning bg-bg px-4 py-3 text-tone-warning">
          {loading ? "Checking configured models..." : reason}
        </div>
        <div className="grid gap-2">
          <Link
            to={adminRoutes.providers}
            className="rounded-sm border border-line-strong bg-bg px-3 py-2 font-medium text-fg hover:bg-muted"
          >
            Configure provider
          </Link>
          <Link
            to={adminRoutes.models}
            className="rounded-sm border border-line-strong bg-bg px-3 py-2 font-medium text-fg hover:bg-muted"
          >
            Configure model
          </Link>
        </div>
      </div>
    </div>
  );
}
