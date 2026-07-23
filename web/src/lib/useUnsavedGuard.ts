// Block navigation away from a dirty form: an in-app router block (confirm modal
// via the browser's blocker prompt) plus a beforeunload guard for tab close/reload.

import { useEffect } from "react";
import { useUnsavedChangesGuard } from "@awaken/ui";
import { useBlocker } from "react-router";

/** Guard the current route while `dirty` is true. `message` is shown when the
 * user tries to leave; returning here lets them stay. */
export function useUnsavedGuard(dirty: boolean, message: string): void {
  const guard = useUnsavedChangesGuard(dirty, message);
  // In-app navigation: block the transition and ask before leaving.
  const blocker = useBlocker(dirty);
  useEffect(() => {
    if (blocker.state === "blocked") {
      if (!guard(blocker.proceed)) blocker.reset();
    }
  }, [blocker, guard]);
}
