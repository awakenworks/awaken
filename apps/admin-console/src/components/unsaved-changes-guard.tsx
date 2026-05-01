import { useEffect, useRef } from "react";
import { useBlocker } from "react-router";
import { useConfirmDialog } from "./confirm-dialog";

interface UnsavedChangesGuardOptions {
  enabled: boolean;
  title?: string;
  description?: string;
  confirmLabel?: string;
  cancelLabel?: string;
}

/// Block in-app navigation and the browser-level unload event while
/// `enabled` is true. The in-app block surfaces a styled confirm dialog
/// so the user can review their unsaved changes before navigating away.
export function useUnsavedChangesGuard({
  enabled,
  title = "Discard unsaved changes?",
  description = "Your edits will be lost if you leave this page.",
  confirmLabel = "Discard changes",
  cancelLabel = "Keep editing",
}: UnsavedChangesGuardOptions): void {
  const confirmDialog = useConfirmDialog();

  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      enabled && currentLocation.pathname !== nextLocation.pathname,
  );

  const promptingRef = useRef(false);

  useEffect(() => {
    if (blocker.state !== "blocked") return;
    if (promptingRef.current) return;
    promptingRef.current = true;

    void confirmDialog({
      title,
      description,
      confirmLabel,
      cancelLabel,
      tone: "destructive",
    })
      .then((accepted) => {
        if (accepted) {
          blocker.proceed();
        } else {
          blocker.reset();
        }
      })
      .finally(() => {
        promptingRef.current = false;
      });
  }, [blocker, confirmDialog, title, description, confirmLabel, cancelLabel]);

  useEffect(() => {
    if (!enabled) return;
    function onBeforeUnload(event: BeforeUnloadEvent) {
      event.preventDefault();
      event.returnValue = "";
    }
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [enabled]);
}
