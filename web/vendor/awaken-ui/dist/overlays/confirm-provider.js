import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, } from "react";
import { AlertDialog, } from "./alert-dialog.js";
const ConfirmContext = createContext(null);
export function useConfirm() {
    const confirm = useContext(ConfirmContext);
    if (!confirm) {
        throw new Error("useConfirm must be used within ConfirmProvider");
    }
    return confirm;
}
/**
 * Serializes confirmations so concurrent callers cannot replace or orphan an
 * unresolved request. Unmounting settles every pending request as cancelled.
 */
export function ConfirmProvider({ children }) {
    const sequence = useRef(0);
    const queueRef = useRef([]);
    const [active, setActive] = useState(null);
    const activeRef = useRef(null);
    activeRef.current = active;
    const showNext = useCallback(() => {
        setActive((current) => current ?? queueRef.current.shift() ?? null);
    }, []);
    const confirm = useCallback((request) => new Promise((resolve) => {
        queueRef.current.push({
            id: ++sequence.current,
            request,
            resolve,
        });
        showNext();
    }), [showNext]);
    const settle = useCallback((confirmed) => {
        setActive((current) => {
            current?.resolve(confirmed);
            return null;
        });
        queueMicrotask(showNext);
    }, [showNext]);
    useEffect(() => () => {
        activeRef.current?.resolve(false);
        for (const pending of queueRef.current.splice(0))
            pending.resolve(false);
    }, []);
    const value = useMemo(() => confirm, [confirm]);
    return (_jsxs(ConfirmContext.Provider, { value: value, children: [children, active ? (_jsx(AlertDialog, { ...active.request, onConfirm: () => settle(true), onOpenChange: (open) => {
                    if (!open)
                        settle(false);
                }, open: true })) : null] }));
}
//# sourceMappingURL=confirm-provider.js.map