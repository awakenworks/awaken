// Centered overlay dialog. The counterpart to Drawer (right slide-over): use
// Modal for focused, transient interactions (a live model Test, a restore
// preview, a confirm). Click-outside and the ✕ dismiss; an optional footer holds
// the action buttons. Confirm is built on top of this.

import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";

export default function Modal({
  title,
  onClose,
  children,
  footer,
  width,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: string;
}) {
  const app = useApp();
  return (
    <div className="overlay" onClick={onClose}>
      <div
        className="modal"
        style={{ width: width ?? "min(560px, 92vw)" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>{title}</h3>
          <button
            className="btn ghost"
            style={{ height: 24 }}
            onClick={onClose}
            aria-label={app.t("Close", "关闭")}
          >
            ✕
          </button>
        </div>
        {children}
        {footer && (
          <div className="row" style={{ justifyContent: "flex-end" }}>
            {footer}
          </div>
        )}
      </div>
    </div>
  );
}
