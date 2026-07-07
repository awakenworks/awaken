// Block navigation away from a dirty form: an in-app router block (confirm modal
// via the browser's blocker prompt) plus a beforeunload guard for tab close/reload.

import { useEffect } from "react";
import { useBlocker } from "react-router";

/** Guard the current route while `dirty` is true. `message` is shown when the
 * user tries to leave; returning here lets them stay. */
export function useUnsavedGuard(dirty: boolean, message: string): void {
  // In-app navigation: block the transition and ask before leaving.
  const blocker = useBlocker(dirty);
  useEffect(() => {
    if (blocker.state === "blocked") {
      if (window.confirm(message)) blocker.proceed();
      else blocker.reset();
    }
  }, [blocker, message]);

  // Tab close / reload / external nav: the native prompt.
  useEffect(() => {
    if (!dirty) return;
    const handler = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", handler);
    return () => window.removeEventListener("beforeunload", handler);
  }, [dirty]);
}
