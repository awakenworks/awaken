import { Suspense, useEffect, useLayoutEffect, useState } from "react";
import {
  DialogSurface,
  useCommandPalette,
  useCommandPaletteShortcut,
} from "@awaken/ui";
import { Outlet, useLocation, useNavigate } from "react-router";
import { useApp } from "../../lib/app-state";
import { navPath, visibleNavigation } from "../../lib/navigation/paths";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";
import { ConfirmProvider } from "../ui/Confirm";
import { ToastProvider } from "../ui/Toast";
import AssistantFab from "./AssistantFab";
import PageIntentHeader from "./PageIntentHeader";
import Sidebar, { GROUP_CAPTIONS } from "./Sidebar";
import TopChrome from "./TopChrome";

export function navItemMatchesQuery(
  item: ReturnType<typeof visibleNavigation>[number],
  query: string,
): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return item.label.toLowerCase().includes(needle)
    || item.labelZh.toLowerCase().includes(needle)
    || GROUP_CAPTIONS[item.group].some((caption) => caption.toLowerCase().includes(needle));
}

/** ⌘K command palette. Opens on the shortcut or the topbar search box's event. */
function CommandPalette() {
  const app = useApp();
  const nav = useNavigate();
  const capabilities = useConfigCapabilities();
  const [open, setOpen] = useState(false);
  const palette = useCommandPalette({
    open,
    items: visibleNavigation(capabilities.data),
    filterItems: (items, query) =>
      items
        .filter((item) => navItemMatchesQuery(item, query))
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
          placeholder={app.t("Search pages and workflows…", "搜索页面与流程…")}
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
              {app.t(...GROUP_CAPTIONS[n.group])}
            </span>
          </button>
        ))}
        {palette.query && palette.filteredItems.length === 0 && (
          <div className="empty-inline" role="status">
            <strong>{app.t("No matching page", "没有匹配的页面")}</strong>
            <span className="mut">{app.t("Try a page name or workflow such as Build, Run, or Connect.", "可尝试页面名称，或“构建”“运行”“连接”等流程。")}</span>
          </div>
        )}
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

export function resetRouteScroll(container: { scrollTop: number } | null): void {
  if (container) container.scrollTop = 0;
}

/** The shell, not window, owns vertical scrolling. Every new page starts at its
 * intent header instead of inheriting the previous page's reading position. */
function RouteScrollReset() {
  const { pathname, search } = useLocation();
  useLayoutEffect(() => {
    resetRouteScroll(document.querySelector<HTMLElement>(".content"));
  }, [pathname, search]);
  return null;
}

function RouteFallback() {
  const app = useApp();
  return (
    <div className="skeleton" style={{ height: 160 }} role="status">
      <span className="sr-only">{app.t("Loading page…", "正在加载页面…")}</span>
    </div>
  );
}

export default function AppShell() {
  return (
    <ToastProvider>
      <ConfirmProvider>
        <div className="shell">
          <RouteScope />
          <RouteScrollReset />
          <div className="dawn" />
          <TopChrome />
          <div className="shell-body">
            <Sidebar />
            <main className="main">
              <div className="content">
                <div className="content-inner">
                  <PageIntentHeader />
                  <Suspense fallback={<RouteFallback />}>
                    <Outlet />
                  </Suspense>
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
