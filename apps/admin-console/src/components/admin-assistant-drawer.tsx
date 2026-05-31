import { useState } from "react";
import { Link } from "react-router";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import { adminRoutes } from "@/lib/routes";
import { AssistantChatPanel } from "@/pages/assistant-page";

export function AdminAssistantDrawer() {
  const [open, setOpen] = useState(false);
  const capabilitiesQuery = useCapabilitiesQuery();
  const assistant = capabilitiesQuery.data?.admin_assistant;
  const enabled = Boolean(assistant?.enabled);
  const disabledReason =
    assistant?.disabled_reason ?? "Configure the first model to enable the admin assistant.";
  const hasProvider = (capabilitiesQuery.data?.providers ?? []).length > 0;
  const setupRoute = hasProvider ? adminRoutes.models : adminRoutes.providers;
  const setupLabel = hasProvider ? "Configure models" : "Configure provider";

  return (
    <>
      <div className="fixed bottom-4 right-4 z-40 flex flex-col items-end gap-2">
        {!enabled ? (
          <div className="max-w-xs rounded-sm border border-line bg-surface px-3 py-2 text-xs text-fg-soft shadow-card">
            <div className="font-medium text-fg-strong">Admin Assistant locked</div>
            <div className="mt-1">{disabledReason}</div>
            <Link
              to={setupRoute}
              className="mt-2 inline-flex rounded-sm border border-line-strong px-2 py-1 font-medium text-fg transition hover:bg-soft"
            >
              {setupLabel}
            </Link>
          </div>
        ) : null}
        <button
          type="button"
          onClick={() => {
            if (enabled) setOpen(true);
          }}
          disabled={!enabled}
          className="rounded-sm border border-line-strong bg-accent px-4 py-2 text-sm font-medium text-accent-text shadow-card-lift transition hover:opacity-90 disabled:cursor-not-allowed disabled:bg-muted disabled:text-fg-soft"
          aria-label="Open Admin Assistant"
        >
          Admin Assistant
        </button>
      </div>

      {open ? (
        <div className="fixed inset-0 z-50 bg-overlay backdrop-blur-sm">
          <button
            type="button"
            aria-label="Close Admin Assistant"
            className="absolute inset-0 cursor-default"
            onClick={() => setOpen(false)}
          />
          <aside
            role="dialog"
            aria-modal="true"
            aria-label="Admin Assistant"
            className="absolute right-0 top-0 flex h-full w-full max-w-2xl flex-col border-l border-line bg-bg shadow-overlay"
          >
            <div className="flex items-center justify-between border-b border-line bg-surface px-4 py-3">
              <div>
                <div className="text-sm font-semibold text-fg-strong">Admin Assistant</div>
                <div className="mt-0.5 text-xs text-fg-soft">
                  Model:{" "}
                  <span className="font-mono">{assistant?.model_id ?? "unconfigured"}</span>
                  <span className="mx-2 text-fg-faint">·</span>
                  Tools locked
                </div>
              </div>
              <button
                type="button"
                onClick={() => setOpen(false)}
                className="rounded-sm border border-line bg-soft px-2 py-1 text-xs font-medium text-fg-soft hover:text-fg"
              >
                Close
              </button>
            </div>
            <div className="min-h-0 flex-1">
              <AssistantChatPanel variant="drawer" />
            </div>
          </aside>
        </div>
      ) : null}
    </>
  );
}
