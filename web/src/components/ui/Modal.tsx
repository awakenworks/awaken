import { Dialog } from "@awaken/ui";
import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";

/** Awaken always-open API adapter over the shared focus-managed dialog. */
export default function Modal({
  title,
  onClose,
  children,
  footer,
  width,
}: {
  readonly title: ReactNode;
  readonly onClose: () => void;
  readonly children: ReactNode;
  readonly footer?: ReactNode;
  readonly width?: string;
}) {
  const app = useApp();
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={title}
      closeLabel={app.t("Close", "关闭")}
      footer={footer}
      className="modal"
      style={{ width: width ?? "min(560px, 92vw)" }}
    >
      {children}
    </Dialog>
  );
}
