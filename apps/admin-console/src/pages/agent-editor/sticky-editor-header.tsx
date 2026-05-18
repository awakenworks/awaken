import { useRef } from "react";
import { Link } from "react-router";
import { useTranslation } from "react-i18next";
import { type AgentSpec, type ConfigSourceState } from "@/lib/config-api";
import { AGENT_EDITOR_TABS, type AgentEditorTabId } from "@/lib/editor-tabs";
import { adminRoutes } from "@/lib/routes";
import { EditorSourceBadge } from "./editor-source-badge";

export function StickyEditorHeader({
  isNew,
  agentId,
  spec,
  isDirty,
  saveDisabled,
  onSave,
  activeTab,
  onTabChange,
  sourceState,
  onResetOverrides,
}: {
  isNew: boolean;
  agentId: string;
  spec: AgentSpec;
  isDirty: boolean;
  saveDisabled: boolean;
  onSave: () => void;
  activeTab: AgentEditorTabId;
  onTabChange: (next: AgentEditorTabId) => void;
  sourceState: ConfigSourceState | null;
  onResetOverrides: () => void;
}) {
  const { t } = useTranslation();
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);

  function handleKeyDown(event: React.KeyboardEvent, index: number) {
    const count = AGENT_EDITOR_TABS.length;
    let nextIndex: number | null = null;

    if (event.key === "ArrowRight") {
      nextIndex = (index + 1) % count;
    } else if (event.key === "ArrowLeft") {
      nextIndex = (index - 1 + count) % count;
    } else if (event.key === "Home") {
      nextIndex = 0;
    } else if (event.key === "End") {
      nextIndex = count - 1;
    }

    if (nextIndex !== null) {
      event.preventDefault();
      const nextTab = AGENT_EDITOR_TABS[nextIndex];
      onTabChange(nextTab.id);
      tabRefs.current[nextIndex]?.focus();
    }
  }

  return (
    <div className="sticky top-0 z-30 border-b border-line bg-surface/95 px-6 pt-6 backdrop-blur md:px-8">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <Link
              to={adminRoutes.agents}
              aria-label="Back to agents"
              title="Back to agents"
              className="inline-flex h-7 w-7 items-center justify-center rounded-sm text-fg-soft transition hover:bg-soft hover:text-fg"
            >
              <svg
                aria-hidden
                viewBox="0 0 24 24"
                className="h-4 w-4"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M15 18l-6-6 6-6" />
              </svg>
            </Link>
            {!isNew && agentId && (
              <Link
                to={adminRoutes.auditLogForResource(`agents/${agentId}`)}
                className="rounded-sm border border-line-strong bg-surface px-2.5 py-1 text-xs font-medium text-fg-soft transition hover:bg-soft hover:text-fg"
              >
                {t("editor.history")}
              </Link>
            )}
          </div>
          <h2 className="mt-2 flex flex-wrap items-center gap-3 text-3xl font-semibold text-fg-strong">
            <span>{isNew ? t("editor.newTitle") : t("editor.editTitle", { id: agentId })}</span>
            {isDirty ? (
              <span className="rounded-full bg-tone-warn/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-warn">
                {t("editor.unsavedChanges")}
              </span>
            ) : !isNew ? (
              <span className="rounded-full bg-tone-success/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-success">
                {t("editor.upToDate")}
              </span>
            ) : null}
            {!isNew && sourceState && <EditorSourceBadge state={sourceState} />}
          </h2>
          {!isNew && sourceState === "customized" && (
            <div className="mt-1">
              <button
                type="button"
                onClick={onResetOverrides}
                className="text-xs font-medium text-tone-error transition hover:underline"
              >
                {t("agents.resetOverrides")}
              </button>
            </div>
          )}
        </div>
        {isDirty || isNew ? null : (
          <button
            type="button"
            onClick={onSave}
            disabled={saveDisabled}
            className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {t("editor.save")}
          </button>
        )}
      </div>

      <div
        role="tablist"
        aria-label="Editor sections"
        aria-orientation="horizontal"
        className="mt-4 flex gap-1 overflow-x-auto"
      >
        {AGENT_EDITOR_TABS.map((tab, index) => {
          const active = tab.id === activeTab;
          const badge = tab.badge?.(spec) ?? null;
          return (
            <button
              key={tab.id}
              ref={(el) => {
                tabRefs.current[index] = el;
              }}
              type="button"
              role="tab"
              id={`tab-${tab.id}`}
              aria-selected={active}
              aria-controls={`panel-${tab.id}`}
              tabIndex={active ? 0 : -1}
              onClick={() => onTabChange(tab.id)}
              onKeyDown={(event) => handleKeyDown(event, index)}
              className={[
                "flex shrink-0 items-center gap-2 rounded-t-lg border-b-2 px-4 py-3 text-sm font-medium transition",
                active
                  ? "border-fg-strong text-fg-strong"
                  : "border-transparent text-fg-soft hover:text-fg",
              ].join(" ")}
            >
              <span>{tab.label}</span>
              {badge && (
                <span
                  aria-hidden
                  className={[
                    "rounded-pill px-1.5 font-mono text-[10px]",
                    active ? "bg-muted text-fg-strong" : "bg-soft text-fg-soft",
                  ].join(" ")}
                >
                  {badge}
                </span>
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}
