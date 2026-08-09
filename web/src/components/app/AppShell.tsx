import { useEffect, useState } from "react";
import {
  DialogSurface,
  useCommandPalette,
  useCommandPaletteShortcut,
} from "@awaken/ui";
import { Outlet, useLocation, useNavigate } from "react-router";
import { useApp } from "../../lib/app-state";
import { NAV, navPath } from "../../lib/navigation/paths";
import { ConfirmProvider } from "../ui/Confirm";
import { ToastProvider } from "../ui/Toast";
import AssistantFab from "./AssistantFab";
import PageIntentHeader from "./PageIntentHeader";
import Sidebar from "./Sidebar";
import TopChrome from "./TopChrome";

/** ⌘K command palette. Opens on the shortcut or the topbar search box's event. */
function CommandPalette() {
  const app = useApp();
  const nav = useNavigate();
  const [open, setOpen] = useState(false);
  const palette = useCommandPalette({
    open,
    items: NAV,
    filterItems: (items, query) =>
      items
        .filter(
          (item) =>
            item.label.toLowerCase().includes(query.toLowerCase()) ||
            item.labelZh.includes(query),
        )
        .slice(0, 8),
    onOpenChange: setOpen,
    onSelect: (item) => {
      nav(navPath(item, app.workspaceId));
      setOpen(false);
    },
  });
  useCommandPaletteShortcut({
    onOpenChange: setOpen,
    open,
    openEventName: "awaken:open-palette",
  });
  if (!open) return null;
  return (
    <DialogSurface
      ariaLabel={app.t("Command palette", "命令面板")}
      onOpenChange={setOpen}
      open={open}
      panelClassName="modal"
      rootClassName="overlay"
    >
        <input
          ref={palette.inputRef}
          className="input"
          placeholder={app.t("Go to…", "跳转…")}
          value={palette.query}
          onChange={(e) => palette.setQuery(e.target.value)}
          onKeyDown={palette.onInputKeyDown}
        />
        {palette.filteredItems.map((n, index) => (
          <button
            key={n.key}
            className="nav-item"
            data-active={index === palette.selectedIndex || undefined}
            onMouseEnter={() => palette.setSelectedIndex(index)}
            onClick={() => palette.activate(n)}
          >
            {app.t(n.label, n.labelZh)}
            <span className="mut" style={{ marginLeft: "auto", fontSize: 11 }}>
              {n.group}
            </span>
          </button>
        ))}
    </DialogSurface>
  );
}

/** The route is the UI scope authority. Synchronize the one API addressing seam;
 * never maintain a second client-side Workspace roster. */
function RouteScope() {
  const app = useApp();
  const location = useLocation();
  const workspace = location.pathname.match(/^\/w\/([^/]+)/)?.[1] ?? "default";
  useEffect(() => {
    if (workspace !== app.workspaceId) app.setWorkspaceId(workspace);
  }, [app, workspace]);
  return null;
}

export default function AppShell() {
  return (
    <ToastProvider>
      <ConfirmProvider>
        <div className="shell">
          <RouteScope />
          <div className="dawn" />
          <TopChrome />
          <div className="shell-body">
            <Sidebar />
            <main className="main">
              <div className="content">
                <div className="content-inner">
                  <PageIntentHeader />
                  <Outlet />
                </div>
              </div>
            </main>
          </div>
          <CommandPalette />
          <AssistantFab />
        </div>
      </ConfirmProvider>
    </ToastProvider>
  );
}
