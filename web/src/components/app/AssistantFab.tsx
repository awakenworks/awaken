// The always-on Admin Assistant FAB: a floating button that opens the authoring copilot
// as a NON-MODAL panel (the console stays usable behind it; Escape closes; open state
// persists across navigation). It reads the route so that, opened over an agent editor,
// it targets THAT agent (patch) instead of drafting a new one — the copilot knows what
// you're working on. Reuses <AssistantPanel>, so it's the same engine as the /assistant
// page. Gated the same way (the panel shows the no-model / not-installed states itself).

import { useEffect, useState } from "react";
import { useLocation } from "react-router";
import { AssistantPanel } from "../../surfaces/assistant";
import { titleForPath } from "../../lib/navigation/paths";
import { useApp } from "../../lib/app-state";
import {
  AGENT_DRAFT_CHANGED_EVENT,
  ASSISTANT_REPAIR_EVENT,
  ASSISTANT_SETTLED_EVENT,
  type AssistantRepairDetail,
} from "../../lib/assistant-events";

const OPEN_KEY = "awaken.console.assistantOpen";

/** Parse the active workspace and (if on an agent editor) the agent being edited. */
function routeContext(pathname: string): { wsId: string; targetAgentId?: string } {
  const wsId = pathname.match(/^\/w\/([^/]+)/)?.[1] ?? "default";
  const agent = pathname.match(/^\/w\/[^/]+\/agents\/([^/]+)$/)?.[1];
  // "new" is an unsaved draft (no id yet) — nothing to patch.
  return { wsId, targetAgentId: agent && agent !== "new" ? agent : undefined };
}

export default function AssistantFab() {
  const app = useApp();
  const location = useLocation();
  const [open, setOpen] = useState(() => {
    try {
      return localStorage.getItem(OPEN_KEY) === "1";
    } catch {
      return false;
    }
  });
  const [repair, setRepair] = useState<AssistantRepairDetail | null>(null);
  const close = () => {
    setOpen(false);
    setRepair(null);
  };
  useEffect(() => {
    try {
      localStorage.setItem(OPEN_KEY, open ? "1" : "0");
    } catch {
      /* best effort */
    }
  }, [open]);
  useEffect(() => {
    const onRepair = (event: Event) => {
      const detail = (event as CustomEvent<AssistantRepairDetail>).detail;
      if (!detail?.id || !detail.requestId || !detail.message) return;
      setRepair(detail);
      setOpen(true);
    };
    window.addEventListener(ASSISTANT_REPAIR_EVENT, onRepair);
    return () => window.removeEventListener(ASSISTANT_REPAIR_EVENT, onRepair);
  }, []);
  // Escape closes (non-modal: focus is never trapped, so the console stays usable).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  // The assistant is a workspace tool; hide the FAB on the workspace picker / root.
  if (!location.pathname.startsWith("/w/")) return null;
  const { wsId, targetAgentId: routeTargetAgentId } = routeContext(location.pathname);
  const targetAgentId = repair?.id ?? routeTargetAgentId;
  // The current page's name, so a how-to question defaults to explaining this surface.
  const surfaceHint = targetAgentId ? undefined : titleForPath(location.pathname).title || undefined;

  return (
    <>
      {open && (
        <section className="assistant-fab-panel" aria-label={app.t("Admin Assistant", "控制台助手")}>
          <header className="assistant-fab-head">
            <span className="row" style={{ gap: 8 }}>
              <span className="assistant-fab-mark">✦</span>
              <strong>{app.t("Assistant", "助手")}</strong>
              {targetAgentId && <span className="mut" style={{ fontSize: 12 }}>· {targetAgentId}</span>}
            </span>
            <button className="assistant-fab-x" onClick={close} aria-label={app.t("Close", "关闭")}>
              ✕
            </button>
          </header>
          <div className="assistant-fab-body">
            {/* Remount the panel per (workspace, target) so a fresh session picks up the
                current context — cheap, and keeps "refining X" honest as you navigate. */}
            <AssistantPanel
              key={`${wsId}:${targetAgentId ?? surfaceHint ?? ""}`}
              wsId={wsId}
              targetAgentId={targetAgentId}
              surfaceHint={surfaceHint}
              autoMessage={repair ? { id: repair.requestId, text: repair.message } : undefined}
              onAgentChanged={(changedId, paths) => {
                window.dispatchEvent(new CustomEvent(AGENT_DRAFT_CHANGED_EVENT, { detail: { id: changedId, paths } }));
              }}
              onRunSettled={() => {
                window.dispatchEvent(new CustomEvent(ASSISTANT_SETTLED_EVENT, { detail: { id: targetAgentId } }));
              }}
            />
          </div>
        </section>
      )}
      <button
        className="assistant-fab"
        data-open={open}
        onClick={() => {
          if (open) close();
          else setOpen(true);
        }}
        title={app.t("Draft or refine an agent with AI", "用 AI 起草或修改 agent")}
        aria-label={app.t("Admin Assistant", "控制台助手")}
      >
        {open ? "✕" : "✦"}
      </button>
    </>
  );
}
