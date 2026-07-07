// Right slide-over. The IA's "Manage ↗" surface: open a management page in a
// drawer without losing the form you're filling underneath (design handoff §4a).

import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";

export default function Drawer({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const app = useApp();
  return (
    <div
      className="overlay"
      style={{ padding: 0, alignItems: "stretch", justifyContent: "flex-end", zIndex: 70 }}
      onClick={onClose}
    >
      <div className="drawer" onClick={(e) => e.stopPropagation()}>
        <div className="drawer-head">
          <span>{title}</span>
          <span style={{ flex: 1 }} />
          <button className="btn ghost" style={{ height: 24 }} onClick={onClose}>
            {app.t("Close", "关闭")}
          </button>
        </div>
        <div className="drawer-body">{children}</div>
      </div>
    </div>
  );
}
