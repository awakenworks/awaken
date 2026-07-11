import { useEffect, useState } from "react";
import { Outlet, useNavigate } from "react-router";
import { useApp } from "../../lib/app-state";
import { NAV, navPath } from "../../lib/navigation/paths";
import { ConfirmProvider } from "../ui/Confirm";
import { ToastProvider } from "../ui/Toast";
import Sidebar from "./Sidebar";
import TopChrome from "./TopChrome";

/** ⌘K command palette. Opens on the shortcut or the topbar search box's event. */
function CommandPalette() {
  const app = useApp();
  const nav = useNavigate();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((v) => !v);
        setQ("");
      }
      if (e.key === "Escape") setOpen(false);
    };
    const onOpen = () => {
      setQ("");
      setOpen(true);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("awaken:open-palette", onOpen);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("awaken:open-palette", onOpen);
    };
  }, []);
  if (!open) return null;
  const hits = NAV.filter(
    (n) => n.label.toLowerCase().includes(q.toLowerCase()) || n.labelZh.includes(q),
  ).slice(0, 8);
  return (
    <div className="overlay" onClick={() => setOpen(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <input
          autoFocus
          className="input"
          placeholder={app.t("Go to…", "跳转…")}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && hits[0]) {
              nav(navPath(hits[0], app.workspaceId));
              setOpen(false);
            }
          }}
        />
        {hits.map((n) => (
          <button
            key={n.key}
            className="nav-item"
            onClick={() => {
              nav(navPath(n, app.workspaceId));
              setOpen(false);
            }}
          >
            {app.t(n.label, n.labelZh)}
            <span className="mut" style={{ marginLeft: "auto", fontSize: 11 }}>
              {n.group}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}

export default function AppShell() {
  return (
    <ToastProvider>
      <ConfirmProvider>
        <div className="shell">
          <div className="dawn" />
          <TopChrome />
          <div className="shell-body">
            <Sidebar />
            <main className="main">
              <div className="content">
                <div className="content-inner">
                  <Outlet />
                </div>
              </div>
            </main>
          </div>
          <CommandPalette />
        </div>
      </ConfirmProvider>
    </ToastProvider>
  );
}
