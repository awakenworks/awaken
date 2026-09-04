import { useCallback, useEffect } from "react";
/**
 * Protects dirty browser state and returns one confirmation gate for in-app
 * dismissal/navigation actions. Products own the localized confirmation copy
 * and any router-specific blocker integration.
 */
export function useUnsavedChangesGuard(dirty, message) {
    useEffect(() => {
        if (!dirty)
            return;
        const handler = (event) => {
            event.preventDefault();
            event.returnValue = "";
        };
        window.addEventListener("beforeunload", handler);
        return () => window.removeEventListener("beforeunload", handler);
    }, [dirty]);
    return useCallback((action) => {
        if (dirty && !window.confirm(message))
            return false;
        action();
        return true;
    }, [dirty, message]);
}
