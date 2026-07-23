import { Drawer as SharedDrawer } from "@awaken/ui";
import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";

/** Awaken always-open API adapter over the shared focus-managed drawer. */
export default function Drawer({
  title,
  onClose,
  children,
}: {
  readonly title: string;
  readonly onClose: () => void;
  readonly children: ReactNode;
}) {
  const app = useApp();
  return (
    <SharedDrawer
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={title}
      closeLabel={app.t("Close", "关闭")}
      className="drawer"
    >
      {children}
    </SharedDrawer>
  );
}
