// The always-on Admin Assistant FAB: a floating button that opens the authoring copilot
// as a NON-MODAL panel (the console stays usable behind it; Escape closes; open state
// persists across navigation). It reads the route so that, opened over an agent editor,
// it targets THAT agent (patch) instead of drafting a new one — the copilot knows what
// you're working on. Reuses <AssistantPanel>, so it's the same engine as the /assistant
// page. Gated the same way (the panel shows the no-model / not-installed states itself).

import { useEffect, useState } from "react";
import { useLocation } from "react-router";
import { AssistantPanel } from "../../surfaces/assistant";
import { useApp } from "../../lib/app-state";

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
  useEffect(() => {
    try {
      localStorage.setItem(OPEN_KEY, open ? "1" : "0");
    } catch {
      /* best effort */
    }
  }, [open]);
  // Escape closes (non-modal: focus is never trapped, so the console stays usable).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  // The assistant is a workspace tool; hide the FAB on the workspace picker / root.
  if (!location.pathname.startsWith("/w/")) return null;
  const { wsId, targetAgentId } = routeContext(location.pathname);

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
            <button className="assistant-fab-x" onClick={() => setOpen(false)} aria-label={app.t("Close", "关闭")}>
              ✕
            </button>
          </header>
          <div className="assistant-fab-body">
            {/* Remount the panel per (workspace, target) so a fresh session picks up the
                current context — cheap, and keeps "refining X" honest as you navigate. */}
            <AssistantPanel key={`${wsId}:${targetAgentId ?? ""}`} wsId={wsId} targetAgentId={targetAgentId} />
          </div>
        </section>
      )}
      <button
        className="assistant-fab"
        data-open={open}
        onClick={() => setOpen((v) => !v)}
        title={app.t("Draft or refine an agent with AI", "用 AI 起草或修改 agent")}
        aria-label={app.t("Admin Assistant", "控制台助手")}
      >
        {open ? "✕" : "✦"}
      </button>
    </>
  );
}
